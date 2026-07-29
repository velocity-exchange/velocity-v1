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
                clob_find_expired, read_clob_u32, ClobOrderRefV0, ClobSide, QuoterType, QuoterV0,
                CLOB_ASK_COUNT_OFFSET, CLOB_BID_COUNT_OFFSET, CLOB_EVICT_THRESHOLD_OFFSET,
                CLOB_NIL, CLOB_WORST_ASK_OFFSET, CLOB_WORST_BID_OFFSET,
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
        (side, node.user)
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
    stage(&ctx, node.user, args)
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
