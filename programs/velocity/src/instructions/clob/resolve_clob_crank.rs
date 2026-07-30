//! Resolvers for the CLOB crank conditions: simulation-only instructions a
//! relay turner runs to discover work and learn the executor call that does
//! it. Each stages a `ResolvedCrankV0` — the full executor account list plus
//! its trailing args — into the conditions account's scratch region and
//! returns a `ResponsePointerV0` locating it; the turner reads the staged
//! bytes out of post-simulation account state, so nothing here ever lands on
//! chain.
//!
//! The account list is fixed (registered per condition at initialization) and
//! deliberately tiny: everything the executor needs is either passed here,
//! read off the book's bytes (the maker's `User` is inline on the order
//! node), read from the registry entry (the CLOB program), or derived — the
//! protocol `User`/`UserStats` PDAs from the signer's authority, the perp
//! market PDA from the market index. Deriving PDAs costs real CU but only in
//! simulation. The keeper payout slot is staged as [`KEEPER_PLACEHOLDER`],
//! which the turner substitutes; it is the only non-static entry besides the
//! maker.
//!
//! Resolvers are advisory: the executor re-verifies everything (the CLOB
//! fails removals that aren't due, and velocity fails the crank if the
//! removal hit a different maker), so a stale or lying simulation filters
//! itself out.

use {
    crate::{
        error::ErrorCode,
        load_mut,
        state::{
            clob_crank::ClobCrankConditionsV0,
            prop_amm::{
                clob_find_expired, read_clob_node, read_clob_u32, ClobNodeView, ClobOrderRefV0,
                ClobSide, QuoterType, QuoterV0, CLOB_ASK_COUNT_OFFSET, CLOB_BEST_ASK_OFFSET,
                CLOB_BEST_BID_OFFSET, CLOB_BID_COUNT_OFFSET, CLOB_EVICT_THRESHOLD_OFFSET, CLOB_NIL,
                CLOB_WORST_ASK_OFFSET, CLOB_WORST_BID_OFFSET,
            },
            state::State,
        },
        validate,
    },
    anchor_lang::prelude::*,
    relay_spec::{AccountRefV0, ResolvedCrankV0, ResponsePointerV0, KEEPER_PLACEHOLDER},
    solana_program::program::set_return_data,
};

/// Account order is the contract with `initialize_clob_crank_conditions`'s
/// registered `resolver_accounts` — the conditions account first (index 0 is
/// where the response pointer says the payload lives).
#[derive(Accounts)]
pub struct ResolveClobCrank<'info> {
    /// Writable only because the payload is staged in its scratch region;
    /// the instruction is otherwise read-only and only ever simulated.
    #[account(mut)]
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts, same as the executor it stages.
    pub clob_market: UncheckedAccount<'info>,
    pub quoter: AccountLoader<'info, QuoterV0>,
    pub state: AccountLoader<'info, State>,
}

pub fn handle_resolve_clob_crank_evict(ctx: Context<ResolveClobCrank>) -> Result<()> {
    validate_linkage(&ctx)?;
    let (side, maker) = {
        let data = ctx.accounts.clob_market.try_borrow_data()?;
        let read = |offset: usize| {
            read_clob_u32(&data, offset).ok_or_else(|| error!(ErrorCode::DefaultError))
        };
        let threshold = read(CLOB_EVICT_THRESHOLD_OFFSET)?;
        let bid_count = read(CLOB_BID_COUNT_OFFSET)?;
        let ask_count = read(CLOB_ASK_COUNT_OFFSET)?;
        // The fuller side at/above the soft cap; the CLOB itself re-checks
        // the threshold at execution.
        let side = match (bid_count >= threshold, ask_count >= threshold) {
            (true, true) if ask_count > bid_count => ClobSide::Ask,
            (true, _) => ClobSide::Bid,
            (_, true) => ClobSide::Ask,
            _ => return no_work(),
        };
        let tail_offset = match side {
            ClobSide::Bid => CLOB_WORST_BID_OFFSET,
            ClobSide::Ask => CLOB_WORST_ASK_OFFSET,
        };
        let tail = read(tail_offset)?;
        if tail == CLOB_NIL {
            return no_work();
        }
        let node = crate::state::prop_amm::read_clob_node(&data, tail)
            .ok_or_else(|| error!(ErrorCode::DefaultError))?;
        (side, derive_user_pdas(&node.user_ref()).0)
    };

    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let mut args = Vec::with_capacity(3);
    market_index.serialize(&mut args)?;
    side.serialize(&mut args)?;
    stage(&ctx, maker, args)
}

pub fn handle_resolve_clob_crank_remove_expired(ctx: Context<ResolveClobCrank>) -> Result<()> {
    validate_linkage(&ctx)?;
    let now = Clock::get()?.unix_timestamp;
    let (node_index, node) = {
        let data = ctx.accounts.clob_market.try_borrow_data()?;
        match clob_find_expired(&data, now) {
            Some(found) => found,
            None => return no_work(),
        }
    };

    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let mut args = Vec::with_capacity(14);
    market_index.serialize(&mut args)?;
    ClobOrderRefV0 {
        node_index,
        order_id: node.order_id,
    }
    .serialize(&mut args)?;
    stage(&ctx, derive_user_pdas(&node.user_ref()).0, args)
}

/// Makers one profitable cross touches, bounded so the staged executor
/// stays inside the conditions account's scratch region. The walk stops
/// before admitting a maker past the cap, so the staged size only covers
/// staged makers and the executor's loaded-user set is always sufficient.
const MAX_CROSS_MAKERS: usize = 8;

/// The crossing prefix of the book: total matchable size, the gross quote
/// of each leg, and the (deduped, capped) makers it touches.
struct ClobCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::ClobUserRefV0>,
}

/// Advance a cursor to the next node that is live and matchable right now.
fn next_matchable(
    data: &[u8],
    mut cursor: u32,
    slot: u64,
    now: i64,
) -> Option<(u32, ClobNodeView)> {
    while cursor != CLOB_NIL {
        let node = read_clob_node(data, cursor)?;
        if node.is_matchable(slot, now) {
            return Some((cursor, node));
        }
        cursor = node.next;
    }
    None
}

/// Two-pointer walk over the crossing prefix (bid price >= ask price),
/// best-first on both sides — exactly the orders the executor's two legs
/// will consume.
fn find_clob_cross(data: &[u8], slot: u64, now: i64) -> Result<ClobCross> {
    let base_precision = crate::math::constants::BASE_PRECISION_U64 as u128;
    let mut cross = ClobCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
        makers: Vec::new(),
    };
    let read =
        |offset: usize| read_clob_u32(data, offset).ok_or_else(|| error!(ErrorCode::DefaultError));
    let mut bid = next_matchable(data, read(CLOB_BEST_BID_OFFSET)?, slot, now);
    let mut ask = next_matchable(data, read(CLOB_BEST_ASK_OFFSET)?, slot, now);
    let mut bid_remaining = bid.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
    let mut ask_remaining = ask.map(|(_, node)| node.base_asset_amount).unwrap_or(0);

    while let (Some((_, bid_node)), Some((_, ask_node))) = (bid, ask) {
        if bid_node.price < ask_node.price {
            break;
        }
        // Admit both makers before taking; stop at the cap instead of
        // taking size whose maker is not staged.
        let mut admit =
            |user: crate::state::prop_amm::ClobUserRefV0,
             makers: &mut Vec<crate::state::prop_amm::ClobUserRefV0>| {
                if makers.contains(&user) {
                    true
                } else if makers.len() < MAX_CROSS_MAKERS {
                    makers.push(user);
                    true
                } else {
                    false
                }
            };
        if !admit(bid_node.user_ref(), &mut cross.makers)
            || !admit(ask_node.user_ref(), &mut cross.makers)
        {
            break;
        }

        let take = bid_remaining.min(ask_remaining);
        cross.size = cross.size.saturating_add(take);
        cross.buy_quote = cross
            .buy_quote
            .saturating_add(ask_node.price as u128 * take as u128 / base_precision);
        cross.sell_quote = cross
            .sell_quote
            .saturating_add(bid_node.price as u128 * take as u128 / base_precision);

        bid_remaining -= take;
        ask_remaining -= take;
        if bid_remaining == 0 {
            bid = next_matchable(data, bid_node.next, slot, now);
            bid_remaining = bid.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
        }
        if ask_remaining == 0 {
            ask = next_matchable(data, ask_node.next, slot, now);
            ask_remaining = ask.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
        }
    }
    Ok(cross)
}

/// Resolver for the cross conditions: find the book's crossing prefix,
/// estimate profitability with the top (most conservative) taker-fee tier
/// on both legs, and stage the `crank_cross_match` executor — full
/// `(User, UserStats)` pairs, both derived from the node's `(authority,
/// sub_account_id)` identity. Only CLOB×CLOB is discoverable here — a
/// PropAMM crossing the CLOB has no account a fixed four-account resolver
/// can quote through, so that case is the book publisher's (it re-simulates
/// the quote view on every registered quote-account change and submits the
/// executor directly). The executor re-verifies profitability exactly
/// either way.
pub fn handle_resolve_clob_crank_cross(ctx: Context<ResolveClobCrank>) -> Result<()> {
    validate_linkage(&ctx)?;
    let clock = Clock::get()?;
    let cross = {
        let data = ctx.accounts.clob_market.try_borrow_data()?;
        find_clob_cross(&data, clock.slot, clock.unix_timestamp)?
    };
    if cross.size == 0 {
        return no_work();
    }

    // Conservative estimate: tier-0 taker fee on both legs. The executor
    // measures the real thing; this only avoids staging obvious losers.
    let (fee_numerator, fee_denominator) = {
        let state = ctx.accounts.state.load()?;
        let tier = state.perp_fee_structure.fee_tiers[0];
        (
            tier.fee_numerator as u128,
            (tier.fee_denominator as u128).max(1),
        )
    };
    let fees = (cross.buy_quote * fee_numerator).div_ceil(fee_denominator)
        + (cross.sell_quote * fee_numerator).div_ceil(fee_denominator);
    if cross.sell_quote <= cross.buy_quote.saturating_add(fees) {
        return no_work();
    }

    let (market_index, oracle, quote_spot_market_index) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            conditions.market_index,
            conditions.oracle,
            conditions.quote_spot_market_index,
        )
    };
    let signer = ctx.accounts.state.load()?.signer;
    let (protocol_user, _) = Pubkey::find_program_address(
        &[b"user", signer.as_ref(), 0u16.to_le_bytes().as_ref()],
        &crate::ID,
    );
    let (protocol_user_stats, _) =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &crate::ID);
    let (perp_market, _) = Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        &crate::ID,
    );
    let (quote_spot_market, _) = Pubkey::find_program_address(
        &[
            b"spot_market",
            quote_spot_market_index.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    );

    // `CrankCrossMatch`'s account order: named accounts, the map section,
    // maker `(User, UserStats)` pairs, then the quoter section. Both legs
    // are the CLOB: entry index 0.
    let mut accounts = vec![
        AccountRefV0::readonly(ctx.accounts.state.key().to_bytes()),
        AccountRefV0::writable(KEEPER_PLACEHOLDER),
        AccountRefV0::writable(protocol_user.to_bytes()),
        AccountRefV0::writable(protocol_user_stats.to_bytes()),
        AccountRefV0::writable(ctx.accounts.crank_conditions.key().to_bytes()),
        AccountRefV0::readonly(oracle.to_bytes()),
        AccountRefV0::writable(quote_spot_market.to_bytes()),
        AccountRefV0::writable(perp_market.to_bytes()),
    ];
    for maker in &cross.makers {
        let (user_pda, stats_pda) = derive_user_pdas(maker);
        accounts.push(AccountRefV0::writable(user_pda.to_bytes()));
        accounts.push(AccountRefV0::writable(stats_pda.to_bytes()));
    }
    accounts.extend([
        AccountRefV0::readonly(ctx.accounts.quoter.key().to_bytes()),
        AccountRefV0::writable(ctx.accounts.clob_market.key().to_bytes()),
        AccountRefV0::readonly(signer.to_bytes()),
        AccountRefV0::readonly(ctx.accounts.quoter.load()?.program_id.to_bytes()),
    ]);

    let mut args = Vec::with_capacity(12);
    market_index.serialize(&mut args)?;
    cross.size.serialize(&mut args)?;
    0u8.serialize(&mut args)?; // buy leg: the CLOB entry
    0u8.serialize(&mut args)?; // sell leg: the CLOB entry
    let resolved = ResolvedCrankV0 {
        accounts,
        data: args,
    };
    let pointer = load_mut!(ctx.accounts.crank_conditions)?.stage(&resolved)?;
    set_return_data(&pointer);
    Ok(())
}

/// Derive the `(User, UserStats)` PDAs from a node's derivable identity —
/// the whole point of the book storing `(authority, sub_account_id)`
/// instead of the `User` key. Costs real CU, but resolvers only ever run
/// under simulation.
fn derive_user_pdas(user: &crate::state::prop_amm::ClobUserRefV0) -> (Pubkey, Pubkey) {
    let (user_pda, _) = Pubkey::find_program_address(
        &[
            b"user",
            user.authority.as_ref(),
            user.sub_account_id.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    );
    let (stats_pda, _) =
        Pubkey::find_program_address(&[b"user_stats", user.authority.as_ref()], &crate::ID);
    (user_pda, stats_pda)
}

fn validate_linkage(ctx: &Context<ResolveClobCrank>) -> Result<()> {
    let quoter = ctx.accounts.quoter.load()?;
    let conditions = ctx.accounts.crank_conditions.load()?;
    validate!(
        quoter.quoter_type == QuoterType::Clob && quoter.market == conditions.market_index,
        ErrorCode::DefaultError,
        "quoter entry does not match the conditions account"
    )?;
    let registered = &quoter.execute_accounts[..quoter.execute_accounts_count as usize];
    validate!(
        registered
            .iter()
            .any(|meta| meta.pubkey == ctx.accounts.clob_market.key()),
        ErrorCode::DefaultError,
        "clob market is not registered on the quoter entry"
    )?;
    Ok(())
}

fn no_work() -> Result<()> {
    set_return_data(&ResponsePointerV0::no_work().to_bytes());
    Ok(())
}

/// Stage the executor call: `CrankClobOrderRemoval`'s exact account order,
/// with the keeper payout slot as the placeholder, followed by the borsh
/// args after the discriminator.
fn stage(ctx: &Context<ResolveClobCrank>, maker: Pubkey, args: Vec<u8>) -> Result<()> {
    let signer = ctx.accounts.state.load()?.signer;
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    // The protocol User is the signer authority's first sub-account, created
    // through the normal initialize_user path.
    let (protocol_user, _) = Pubkey::find_program_address(
        &[b"user", signer.as_ref(), 0u16.to_le_bytes().as_ref()],
        &crate::ID,
    );
    let (protocol_user_stats, _) =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &crate::ID);
    let (perp_market, _) = Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        &crate::ID,
    );

    let resolved = ResolvedCrankV0 {
        accounts: vec![
            AccountRefV0::readonly(ctx.accounts.state.key().to_bytes()),
            AccountRefV0::writable(KEEPER_PLACEHOLDER),
            AccountRefV0::writable(protocol_user.to_bytes()),
            AccountRefV0::writable(protocol_user_stats.to_bytes()),
            AccountRefV0::writable(maker.to_bytes()),
            AccountRefV0::writable(perp_market.to_bytes()),
            AccountRefV0::readonly(ctx.accounts.quoter.key().to_bytes()),
            AccountRefV0::writable(ctx.accounts.clob_market.key().to_bytes()),
            AccountRefV0::readonly(ctx.accounts.quoter.load()?.program_id.to_bytes()),
            AccountRefV0::readonly(signer.to_bytes()),
            AccountRefV0::writable(ctx.accounts.crank_conditions.key().to_bytes()),
        ],
        data: args,
    };
    let pointer = load_mut!(ctx.accounts.crank_conditions)?.stage(&resolved)?;
    set_return_data(&pointer);
    Ok(())
}
