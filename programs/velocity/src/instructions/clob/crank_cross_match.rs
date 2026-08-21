//! Cross-match crank: fill two crossed resting sources against each other.
//!
//! Nothing else matches two *resting* books — router matching only happens
//! when a taker fills through — so a CLOB bid at/above a CLOB ask, or a
//! PropAMM quoting through the CLOB's best, would rest crossed forever.
//! Trigger placements make the first routine and PropAMM reprices the
//! second; both strand user orders, which is the UX this crank exists for.
//!
//! The protocol `User` is the pass-through taker (the arb bot): buy the
//! crossed ask, sell into the crossed bid, both legs through the standard
//! external-match settlement, so every maker experiences an ordinary fill.
//! The executor is the authoritative predicate — it reverts unless the legs
//! balance exactly and the spread nets positive after both legs' taker
//! fees, so a simulation that succeeds implies a profitable cross and books
//! whose cross is inside the fee gulf simply rest. The surplus lands in the
//! protocol `User` — the sink the crank incentive loop drains — and the
//! caller's `authority` is paid reservoir lamports; no signature is
//! required anywhere (relay turners submit executors unsigned).

use {
    super::{
        crank_common::{next_matchable, validate_linkage, ResolveClobCrank, MAX_CROSS_MAKERS},
        crank_taker_origin_cross::stage_taker_origin_cross,
    },
    crate::{
        controller,
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
            relay_harness::resolve_into,
            router::{cpi_executor::CpiQuoterExecutor, quoted_route::QuotedEntry},
        },
        load, msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            pdas,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{ClobNodeView, PriceLevel, QuoterType, QuoterV0},
            state::State,
            user::{User, UserStats},
            user_map::load_user_maps,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
};

#[cfg(test)]
mod tests;

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct CrankCrossMatch<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: the lamport payout target — relay's keeper-placeholder slot.
    /// No signature: the executor's own profitability predicate is the gate.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    /// The protocol-owned pass-through taker. Locked to the protocol `User`
    /// so the reservoir never pays for someone else's private arb.
    #[account(
        mut,
        constraint = is_protocol_user(&taker, &state)?
    )]
    pub taker: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&taker, &taker_stats)?
    )]
    pub taker_stats: AccountLoader<'info, UserStats>,
    /// The market's conditions account: the reservoir that pays the keeper.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
}

pub fn handle_crank_cross_match<'c: 'info, 'info>(
    ctx: Context<'info, CrankCrossMatch<'info>>,
    market_index: u16,
    size: u64,
    buy_quoter_index: u8,
    sell_quoter_index: u8,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        Some(state.oracle_guard_rails),
    )?;
    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;

    // The account tail: registry entries plus the union of their registered CPI
    // accounts, same shape as the router fill's. This crank does not quote, so
    // each entry carries no levels.
    let leftover: Vec<&AccountInfo<'info>> = remaining_accounts_iter.collect();
    let accounts: Vec<AccountInfo<'info>> = leftover.iter().map(|info| (*info).clone()).collect();
    let mut quoted: Vec<QuotedEntry<'info>> = Vec::with_capacity(leftover.len());
    for info in &leftover {
        let is_entry = info.owner == &crate::ID
            && info
                .try_borrow_data()
                .is_ok_and(|data| data.get(..8) == Some(QuoterV0::DISCRIMINATOR));
        if !is_entry {
            continue;
        }
        let loader = AccountLoader::<QuoterV0>::try_from(*info)?;
        let quoter = loader.load()?;
        validate!(
            quoter.market == market_index,
            ErrorCode::DefaultError,
            "quoter entry {} is for market {}, cross is for market {}",
            loader.key(),
            quoter.market,
            market_index
        )?;
        validate!(
            quoter.quoter_type != QuoterType::Vamm,
            ErrorCode::DefaultError,
            "the vAMM reprices continuously and cannot rest crossed"
        )?;
        let (quoter_type, user, response_account, priority) = (
            quoter.quoter_type,
            quoter.user,
            quoter.response_account,
            quoter.priority,
        );
        drop(quoter);
        quoted.push(QuotedEntry {
            // The crank quotes the book unrestricted, so it never falls short
            // of a user set.
            withheld: crate::state::prop_amm::PriceLevel::default(),
            entry: loader,
            quoter_type,
            user,
            response_account,
            priority,
            levels: Vec::new(),
        });
    }
    let buy_index = buy_quoter_index as usize;
    let sell_index = sell_quoter_index as usize;
    validate!(
        buy_index < quoted.len() && sell_index < quoted.len(),
        ErrorCode::DefaultError,
        "cross leg index out of range: {} / {} of {}",
        buy_index,
        sell_index,
        quoted.len()
    )?;

    let taker_ref = {
        let taker = load!(ctx.accounts.taker)?;
        crate::state::prop_amm::ClobUserRefV0 {
            authority: taker.authority,
            sub_account_id: taker.sub_account_id.into(),
        }
    };
    let users = crate::state::prop_amm::quoter_wire_users(
        makers_and_referrer
            .user_ref_index()?
            .into_keys()
            .map(
                |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                    authority,
                    sub_account_id: sub_account_id.into(),
                },
            ),
    )?;
    let (quoter_signer, quoter_signer_nonce) = crate::signer::find_quoter_signer();
    let mut executor = CpiQuoterExecutor {
        caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
        reference_price: 0,
        quoted: &quoted,
        market_index,
        accounts: &accounts,
        quoter_signer,
        quoter_signer_nonce,
        users: &users,
        taker: taker_ref,
        slot: clock.slot,
        now: clock.unix_timestamp,
    };

    // A crossed taker remainder is not this crank's to touch. The book's own
    // gate cannot stop it: the first leg can consume the whole opposite side,
    // after which nothing crosses the remainder any more and taking it becomes
    // legitimate as far as the CLOB can tell — so the second leg fills it at
    // its own resting price and the improvement lands with the protocol, which
    // is exactly the outcome the taker-origin path exists to prevent. Relay
    // never stages that (its resolver picks the taker-origin crank when the
    // top pair is a remainder), but this instruction is permissionless, so a
    // hand-built one has to be refused here.
    //
    // Refusing rather than skipping, because the caller has a correct
    // instruction to send instead: `crank_taker_origin_cross` resolves this
    // book and pays the taker the difference.
    for clob in quoted
        .iter()
        .filter(|quoted| quoted.quoter_type == QuoterType::Clob)
    {
        let book = crate::state::prop_amm::find_account(&accounts, &clob.response_account)
            .ok_or_else(|| error!(ErrorCode::DefaultError))?;
        let data = book.try_borrow_data()?;
        validate!(
            super::crank_taker_origin_cross::find_taker_origin_cross(
                &data,
                clock.slot,
                clock.unix_timestamp
            )?
            .is_none(),
            ErrorCode::CrossedTakerRemainderPending,
            "a crossed taker remainder must be resolved by crank_taker_origin_cross"
        )?;
    }

    let (base_matched, surplus) = controller::orders::cross_match(
        &state,
        market_index,
        size,
        buy_index,
        sell_index,
        &ctx.accounts.taker,
        &ctx.accounts.taker_stats,
        &makers_and_referrer,
        &makers_and_referrer_stats,
        &mut executor,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        &clock,
        ctx.accounts.crank_conditions.load()?.min_cross_surplus,
    )?;

    // Repair the wake hints from the post-match book when a leg was the
    // CLOB (a matched activation is exactly when the activation hint fires;
    // this is what sends it forward to the next pending slot).
    let clob_book = quoted
        .iter()
        .find(|quoted| quoted.quoter_type == QuoterType::Clob)
        .map(|quoted| quoted.response_account);
    let payment = {
        let mut conditions = crate::load_mut!(ctx.accounts.crank_conditions)?;
        if let Some(book_key) = clob_book {
            if let Some(book) = crate::state::prop_amm::find_account(&accounts, &book_key) {
                let (min_expiry, min_activation) =
                    crate::state::prop_amm::clob_hint_scan(&book.try_borrow_data()?, clock.slot);
                conditions.repair_expiry(min_expiry)?;
                conditions.repair_activation(min_activation)?;
            }
        }
        // The keeper's fee, so relay's assert_paid_v0 has a balance to
        // measure.
        conditions.keeper_payment_lamports
    };
    let conditions_info = ctx.accounts.crank_conditions.to_account_info();
    let rent_minimum = Rent::get()?.minimum_balance(conditions_info.data_len());
    ClobCrankConditionsV0::pay_keeper_lamports(
        &conditions_info,
        &ctx.accounts.authority.to_account_info(),
        payment,
        rent_minimum,
    )?;

    msg!(
        "cross matched {} base for {} quote surplus on market {}",
        base_matched,
        surplus,
        market_index
    );
    Ok(())
}

/// Resolver for the cross conditions: find the book's crossing prefix,
/// estimate profitability with the top (most conservative) taker-fee tier
/// on both legs, and stage the `crank_cross_match` executor — full
/// `(User, UserStats)` pairs, both derived from the node's `(authority,
/// sub_account_id)` identity. Only CLOB×CLOB is discoverable here — a
/// PropAMM crossing the CLOB is the generic quoter-cross resolver's job
/// (it CPIs `quote_v0` through the entry's registered surface), with the
/// book publisher as the fast path. The executor re-verifies profitability
/// exactly either way.
///
/// A taker-origin cross is looked for first and staged as
/// `crank_taker_origin_cross` instead. A `ResolvedCrankV0` names its own
/// executor, so serving both from one condition costs no extra slot and no
/// second wake — the wakes that find a maker×maker cross are the same ones that
/// find a taker-origin cross (see [`stage_taker_origin_cross`]). The order is
/// the economics: the improvement between the two prices belongs to the order
/// that came to trade, so it is handed over before the protocol middles the
/// same crossed book as arbitrage.
pub fn handle_resolve_crank_cross_match(ctx: Context<ResolveClobCrank>) -> Result<()> {
    validate_linkage(&ctx)?;
    resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        if let Some(call) = stage_taker_origin_cross(&ctx, &clock)? {
            return Ok(Some(call));
        }
        let cross = {
            let data = ctx.accounts.clob_market.try_borrow_data()?;
            find_clob_cross(&data, clock.slot, clock.unix_timestamp)?
        };
        if cross.size == 0 {
            return Ok(None);
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
            return Ok(None);
        }

        let (market_index, oracle, quote_spot_market_index) = {
            let conditions = ctx.accounts.crank_conditions.load()?;
            (
                conditions.market_index,
                conditions.oracle,
                conditions.quote_spot_market_index,
            )
        };
        // The CLOB's execute leg is signed by the quoter CPI signer, not the
        // vault authority — stage the one the executor will actually sign as.
        let (quoter_signer, _) = crate::signer::find_quoter_signer();
        let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();

        // Named accounts through the executor's own client struct (compile-time
        // shape check), then the remaining sections: maps, maker
        // `(User, UserStats)` pairs, the quoter section. Both legs are the CLOB:
        // entry index 0.
        let call = crate::staged_call!(CrankCrossMatch {
            state: ctx.accounts.state.key(),
            authority: pdas::keeper_placeholder(),
            taker: protocol_user,
            taker_stats: protocol_user_stats,
            crank_conditions: ctx.accounts.crank_conditions.key(),
        })
        .map_section(oracle, quote_spot_market_index, market_index)
        .maker_refs(cross.makers.iter().copied());
        Ok(Some(
            call.account(ctx.accounts.quoter.key(), false)
                .account(ctx.accounts.clob_market.key(), true)
                .account(quoter_signer, false)
                .account(ctx.accounts.quoter.load()?.program_id, false)
                .arg(market_index)?
                .arg(cross.size)?
                .arg(0u8)? // buy leg: the CLOB entry
                .arg(0u8)?, // sell leg: the CLOB entry
        ))
    })
}

/// The crossing prefix of the book: total matchable size, the gross quote
/// of each leg, and the (deduped, capped) makers it touches.
struct ClobCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::ClobUserRefV0>,
}

/// Advance to the next node a cross leg may consume: a taker-origin order is
/// never one, and neither its base nor its owner belongs in a staged cross.
///
/// Every taker-origin order this walk reaches is crossed, since the walk only
/// visits the prefix where the two sides cross, and admitting one is wrong in
/// two separate ways. Usually the book withholds it from `execute_v0`, so a leg
/// sized to include its base comes back short, the two legs imbalance, and the
/// ordinary cross resting in *front* of the remainder cannot clear either for as
/// long as it is there. And admitting it also stages its owner's
/// `(User, UserStats)` pair, which is what would let the book hand the remainder
/// over at its own resting price: once the first leg has consumed the whole
/// opposite side nothing crosses the remainder any more, so the gate protecting
/// it stops firing, and the improvement lands in the protocol `User` instead of
/// the taker's. Leaving its owner unloaded means the book passes over the order
/// as unsettleable even then.
///
/// A crossed remainder is [`stage_taker_origin_cross`]'s to resolve, at the
/// counterparty's price. Whatever rests behind it is an ordinary cross and stays
/// in.
fn next_crossable(data: &[u8], cursor: u32, slot: u64, now: i64) -> Option<(u32, ClobNodeView)> {
    let mut cursor = cursor;
    loop {
        let (index, node) = next_matchable(data, cursor, slot, now)?;
        if !node.is_taker_origin() {
            return Some((index, node));
        }
        cursor = node.next;
    }
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
    let read = |offset: usize| {
        crate::state::prop_amm::read_clob_u32(data, offset)
            .ok_or_else(|| error!(ErrorCode::DefaultError))
    };
    let mut bid = next_crossable(
        data,
        read(crate::state::prop_amm::CLOB_BEST_BID_OFFSET)?,
        slot,
        now,
    );
    let mut ask = next_crossable(
        data,
        read(crate::state::prop_amm::CLOB_BEST_ASK_OFFSET)?,
        slot,
        now,
    );
    let mut bid_remaining = bid.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
    let mut ask_remaining = ask.map(|(_, node)| node.base_asset_amount).unwrap_or(0);

    while let (Some((_, bid_node)), Some((_, ask_node))) = (bid, ask) {
        if bid_node.price < ask_node.price {
            break;
        }
        // Admit both makers before taking; stop at the cap instead of
        // taking size whose maker is not staged.
        let admit = |user: crate::state::prop_amm::ClobUserRefV0,
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
            bid = next_crossable(data, bid_node.next, slot, now);
            bid_remaining = bid.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
        }
        if ask_remaining == 0 {
            ask = next_crossable(data, ask_node.next, slot, now);
            ask_remaining = ask.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
        }
    }
    Ok(cross)
}

/// The generic quoter-cross resolver's accounts. The entry's registered
/// quote surface (plus its program) rides `remaining_accounts` — registered
/// per condition at attach time, so it is whatever `quote_v0` needs for
/// *any* Custom quoter. Nothing here is program-specific.
#[derive(Accounts)]
pub struct ResolveCrankCrossMatchQuoter<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Writable only for the staging region; simulation-only.
    /// Read-only: resolvers stage into the shared scratch account.
    #[account(has_one = quoter)]
    pub cross_conditions: AccountLoader<'info, crate::state::quoter_cross::QuoterCrossConditionsV0>,
    /// CHECK: locked to the CLOB book captured at attach.
    #[account(address = cross_conditions.load()?.clob_market)]
    pub clob_market: UncheckedAccount<'info>,
    pub state: AccountLoader<'info, State>,
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// The entry's quoted user — the maker every staged balance change
    /// lands on; its identity derives the staged `(User, UserStats)` pair.
    #[account(address = quoter.load()?.user)]
    pub user: AccountLoader<'info, User>,
}

/// Discover a cross between a Custom quoter and the CLOB *generically*: CPI
/// the entry's registered `quote_v0` (the same interface every fill uses —
/// resolvers only run under simulation, so the CPI is free), walk the
/// CLOB's bytes against the returned levels in both directions, and stage
/// `crank_cross_match` for the profitable side. Works for any quoter
/// program with a registry entry; velocity carries no per-program code.
pub fn handle_resolve_crank_cross_match_quoter<'info>(
    ctx: Context<'info, ResolveCrankCrossMatchQuoter<'info>>,
) -> Result<()> {
    resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        let quoter = ctx.accounts.quoter.load()?;
        if !quoter.is_active || !quoter.is_approved {
            // A killed or unvetted quoter has no discoverable work; the
            // conditions go quiet rather than erroring forever.
            return Ok(None);
        }
        let (quoter_signer, quoter_signer_nonce) = crate::signer::find_quoter_signer();
        // The resolver's own tail, searched rather than indexed: it is a
        // handful of accounts and this reads a few of them.
        let accounts: Vec<AccountInfo<'info>> = ctx
            .remaining_accounts
            .iter()
            .map(|info| info.clone())
            .collect();

        // Quote both sides. An empty user set is discovery mode
        // (unrestricted); no taker (the executor's taker is the protocol
        // User, which quotes nothing anywhere).
        let market_index = ctx.accounts.cross_conditions.load()?.market_index;
        let quote = |direction: crate::state::prop_amm::Direction| -> Result<Vec<PriceLevel>> {
            quoter
                .quote(
                    market_index,
                    crate::state::prop_amm::QuoteArgsV0 {
                        // The crank's taker is the protocol User and the legs it
                        // matches are the book's own; it constrains no one.
                        caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
                        // No budgets to price, so nothing reads this.
                        reference_price: 0,
                        direction,
                        size: u64::MAX / 2,
                        users: &[],
                        taker: None,
                    },
                    &quoter_signer,
                    quoter_signer_nonce,
                    &accounts,
                )
                // The crank routes the book against itself; there is no
                // caller-supplied user set for it to fall short of.
                .map(|quoted| quoted.levels)
        };
        let quoter_asks = sanitize_levels(quote(crate::state::prop_amm::Direction::Long)?, true);
        let quoter_bids = sanitize_levels(quote(crate::state::prop_amm::Direction::Short)?, false);

        let maker_ref = {
            let user = crate::load!(ctx.accounts.user)?;
            crate::state::prop_amm::ClobUserRefV0 {
                authority: user.authority,
                sub_account_id: user.sub_account_id.into(),
            }
        };

        // Both cross directions against the CLOB's bytes; keep the better one.
        // buy leg = the entry whose ask is consumed.
        let clob_data = ctx.accounts.clob_market.try_borrow_data()?;
        let a = find_quoter_clob_cross(
            &clob_data,
            &quoter_asks,
            true,
            clock.slot,
            clock.unix_timestamp,
        )?;
        let b = find_quoter_clob_cross(
            &clob_data,
            &quoter_bids,
            false,
            clock.slot,
            clock.unix_timestamp,
        )?;
        drop(clob_data);
        // (cross, buy_index, sell_index): entry 0 = the CLOB, entry 1 = the quoter.
        let (cross, buy_index, sell_index) =
            if a.surplus(&ctx.accounts.state)? >= b.surplus(&ctx.accounts.state)? {
                (a, 1u8, 0u8)
            } else {
                (b, 0u8, 1u8)
            };
        if cross.size == 0 || cross.surplus(&ctx.accounts.state)? == 0 {
            return Ok(None);
        }

        let (oracle, quote_spot_market_index, clob_quoter, clob_program) = {
            let conditions = ctx.accounts.cross_conditions.load()?;
            (
                conditions.oracle,
                conditions.quote_spot_market_index,
                conditions.clob_quoter,
                conditions.clob_program,
            )
        };
        let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();

        let call = crate::staged_call!(CrankCrossMatch {
            state: ctx.accounts.state.key(),
            authority: pdas::keeper_placeholder(),
            taker: protocol_user,
            taker_stats: protocol_user_stats,
            crank_conditions: pdas::clob_crank_conditions(market_index),
        })
        .map_section(oracle, quote_spot_market_index, market_index);
        // Maker pairs: the quoter's user first, then the CLOB-side makers.
        let mut staged = vec![maker_ref];
        for maker in &cross.makers {
            if !staged.contains(maker) {
                staged.push(*maker);
            }
        }
        // Entries (0 = CLOB, 1 = the quoter), then the union of both execute
        // surfaces: the CLOB's [book, signer] plus everything the quoter
        // registered, programs included.
        let mut call = call
            .maker_refs(staged.iter().copied())
            .account(clob_quoter, false)
            .account(ctx.accounts.quoter.key(), false);
        let mut union: std::collections::BTreeMap<Pubkey, bool> = Default::default();
        *union.entry(ctx.accounts.clob_market.key()).or_default() |= true;
        union.entry(quoter_signer).or_default();
        union.entry(clob_program).or_default();
        for meta in &quoter.execute_accounts[..quoter.execute_accounts_count as usize] {
            *union.entry(meta.pubkey).or_default() |= meta.is_writable;
        }
        *union.entry(quoter.response_account).or_default() |= true;
        union.entry(quoter.program_id).or_default();
        for (key, writable) in &union {
            call = call.account(*key, *writable);
        }
        Ok(Some(
            call.arg(market_index)?
                .arg(cross.size)?
                .arg(buy_index)?
                .arg(sell_index)?,
        ))
    })
}

/// A quoter-vs-CLOB crossing prefix. `quoter_is_ask_side` selects which
/// legs cross: the quoter's asks against the CLOB's bids, or the CLOB's
/// asks against the quoter's bids.
struct QuoterCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::ClobUserRefV0>,
}

impl QuoterCross {
    /// After-fee surplus at the top (most conservative) taker-fee tier on
    /// both legs; zero when the cross is inside the fee gulf.
    fn surplus(&self, state: &AccountLoader<State>) -> Result<u128> {
        if self.size == 0 {
            return Ok(0);
        }
        let (fee_numerator, fee_denominator) = {
            let state = state.load()?;
            let tier = state.perp_fee_structure.fee_tiers[0];
            (
                tier.fee_numerator as u128,
                (tier.fee_denominator as u128).max(1),
            )
        };
        let fees = (self.buy_quote * fee_numerator).div_ceil(fee_denominator)
            + (self.sell_quote * fee_numerator).div_ceil(fee_denominator);
        Ok(self
            .sell_quote
            .saturating_sub(self.buy_quote.saturating_add(fees)))
    }
}

/// The longest usable best-first prefix of an untrusted `quote_v0` book:
/// positive prices/sizes, monotone (ascending asks / descending bids),
/// truncated at the first violation — the router's sanitization rule.
fn sanitize_levels(levels: Vec<PriceLevel>, ascending: bool) -> Vec<PriceLevel> {
    let mut out: Vec<PriceLevel> = Vec::with_capacity(levels.len());
    for level in levels {
        if level.price == 0 || level.size == 0 {
            break;
        }
        if let Some(previous) = out.last() {
            let monotone = if ascending {
                level.price >= previous.price
            } else {
                level.price <= previous.price
            };
            if !monotone {
                break;
            }
        }
        out.push(level);
    }
    out
}

/// Walk the quoter's (sanitized) levels against the CLOB's matchable side:
/// `quoter_is_ask_side` crosses quoter asks with CLOB bids (CLOB bid price
/// >= quoter ask price), else CLOB asks with quoter bids.
fn find_quoter_clob_cross(
    clob_data: &[u8],
    quoter_levels: &[PriceLevel],
    quoter_is_ask_side: bool,
    slot: u64,
    now: i64,
) -> Result<QuoterCross> {
    let base_precision = crate::math::constants::BASE_PRECISION_U64 as u128;
    let mut cross = QuoterCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
        makers: Vec::new(),
    };
    let head = if quoter_is_ask_side {
        crate::state::prop_amm::CLOB_BEST_BID_OFFSET
    } else {
        crate::state::prop_amm::CLOB_BEST_ASK_OFFSET
    };
    let cursor = crate::state::prop_amm::read_clob_u32(clob_data, head)
        .ok_or_else(|| error!(ErrorCode::DefaultError))?;
    let mut clob = next_matchable(clob_data, cursor, slot, now);
    let mut clob_remaining = clob.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
    let mut levels = quoter_levels.iter();
    let mut level = levels.next();
    let mut level_remaining = level.map(|l| l.size).unwrap_or(0);

    while let (Some((_, node)), Some(l)) = (clob, level) {
        let crossed = if quoter_is_ask_side {
            node.price >= l.price
        } else {
            l.price >= node.price
        };
        if !crossed {
            break;
        }
        // Reserve one maker slot for the quoter's user (staged first).
        if !cross.makers.contains(&node.user_ref()) {
            if cross.makers.len() + 1 >= MAX_CROSS_MAKERS {
                break;
            }
            cross.makers.push(node.user_ref());
        }
        let take = clob_remaining.min(level_remaining);
        let (ask_price, bid_price) = if quoter_is_ask_side {
            (l.price, node.price)
        } else {
            (node.price, l.price)
        };
        cross.size = cross.size.saturating_add(take);
        cross.buy_quote = cross
            .buy_quote
            .saturating_add(ask_price as u128 * take as u128 / base_precision);
        cross.sell_quote = cross
            .sell_quote
            .saturating_add(bid_price as u128 * take as u128 / base_precision);
        clob_remaining -= take;
        level_remaining -= take;
        if clob_remaining == 0 {
            clob = next_matchable(clob_data, node.next, slot, now);
            clob_remaining = clob.map(|(_, n)| n.base_asset_amount).unwrap_or(0);
        }
        if level_remaining == 0 {
            level = levels.next();
            level_remaining = level.map(|l| l.size).unwrap_or(0);
        }
    }
    Ok(cross)
}
