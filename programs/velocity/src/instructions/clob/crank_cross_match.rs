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
        crank_common::{ResolveClobCrank, MAX_CROSS_MAKERS},
        crank_taker_origin_cross::stage_taker_origin_cross,
    },
    crate::{
        controller,
        error::ErrorCode,
        instructions::{
            constraints::*,
            relay_harness::{resolve_into, StagedCall},
            router::{cpi_executor::CpiQuoterExecutor, quoted_route::QuotedEntry},
        },
        load, msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            pdas,
            perp_market_map::MarketSet,
            prop_amm::{
                find_account, occupied_slots, quoter_slab_slots, PriceLevel, QuoterSlabV0,
                QuoterType,
            },
            state::State,
            user::{User, UserStats},
            user_map::load_user_maps,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CrankCrossMatchArgs {
    pub market_index: u16,
    /// Base to take from each leg. The executor reverts unless both legs
    /// balance exactly at this size.
    pub size: u64,
    /// Legs, as slab slot indexes: the entry whose ask the pass-through
    /// taker buys, and the one whose bid it sells into. The book is slot 0.
    pub buy_quoter_index: u8,
    pub sell_quoter_index: u8,
}

#[derive(Accounts)]
#[instruction(args: CrankCrossMatchArgs)]
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
            args.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    /// The crossed market. Named rather than read out of the maps section:
    /// the cross serves exactly one market, so the account is structural.
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump,
        has_one = quoter_slab
    )]
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
    /// The market's approved quoters — both legs' configs, and the identity
    /// every quoter CPI signs as. Bound by the market's `has_one`, which is a
    /// memcmp where a seeds constraint pays a PDA derivation.
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_crank_cross_match<'c: 'info, 'info>(
    ctx: Context<'info, CrankCrossMatch<'info>>,
    args: CrankCrossMatchArgs,
) -> Result<()> {
    let CrankCrossMatchArgs {
        market_index,
        size,
        buy_quoter_index,
        sell_quoter_index,
    } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    // The perp market is a named account, so the maps section carries only
    // the oracle and the quote spot market.
    let mut oracle_map = crate::state::oracle_map::OracleMap::load(
        remaining_accounts_iter,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;
    let spot_market_map = crate::state::spot_market_map::SpotMarketMap::load(
        &MarketSet::new(),
        remaining_accounts_iter,
    )?;
    let perp_market_map = crate::state::perp_market_map::PerpMarketMap::load_one(
        ctx.accounts.perp_market.as_ref(),
        true,
    )?;
    crate::instructions::optional_accounts::update_prelaunch_oracle(
        std::ops::Deref::deref(&perp_market_map.get_ref(&market_index)?),
        &oracle_map,
        clock.slot,
    )?;
    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;

    // The account tail: the union of the consulted quoters' registered CPI
    // accounts, same shape as the router fill's. A slot is consulted when its
    // response account rides the tail. This crank does not quote, so each
    // consulted slot carries no levels.
    // Borrow the tail in place: the executor and the guard read it by
    // reference, so nothing needs an owned clone of every account.
    let tail_from = ctx.remaining_accounts.len() - remaining_accounts_iter.len();
    let tail = &ctx.remaining_accounts[tail_from..];
    let mut quoted: Vec<QuotedEntry<'info>> = Vec::new();
    {
        let loader = &ctx.accounts.quoter_slab;
        let slots = quoter_slab_slots(loader)?;
        for (index, slot) in occupied_slots(&slots) {
            if find_account(tail, &slot.config.response_account).is_none() {
                continue;
            }
            quoted.push(QuotedEntry {
                // The crank quotes the book unrestricted, so it never falls
                // short of a user set.
                withheld: crate::state::prop_amm::PriceLevel::default(),
                slab: loader.clone(),
                slot: index,
                entry_key: slot.entry,
                quoter_type: slot.config.quoter_type,
                user: slot.config.user,
                response_account: slot.config.response_account,
                priority: slot.config.priority,
                max_oracle_deviation_bps: slot.config.max_oracle_deviation_bps,
                levels: 0..0,
            });
        }
    }
    // Legs are named by slab slot index — stable whatever else rides the
    // tail — and resolved to consulted entries here.
    let leg = |slot: u8| -> Result<usize> {
        quoted
            .iter()
            .position(|entry| entry.slot == slot as usize)
            .ok_or_else(|| {
                msg!(
                    "cross leg names slab slot {}, which the tail does not consult",
                    slot
                );
                error!(ErrorCode::DefaultError)
            })
    };
    let buy_index = leg(buy_quoter_index)?;
    let sell_index = leg(sell_quoter_index)?;

    let taker_ref = {
        let taker = load!(ctx.accounts.taker)?;
        taker.clob_user_ref()
    };
    let users = crate::state::prop_amm::quoter_wire_users(
        makers_and_referrer
            .user_ref_index()?
            .into_keys()
            .map(
                |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                    authority,
                    sub_account_id,
                },
            ),
    )?;
    // One set of CPI buffers for the whole crank, as the router fill uses.
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
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
    // The read is the same L3 walk the crank itself runs, not the two heads.
    // A remainder one level down is still a remainder the taker-origin crank
    // owns, and it is invisible to a read that reports only the best order on
    // each side.
    //
    // Runs before the executor takes the CPI buffers, so it reads through the
    // one set the crank already built. A set per book would be an allocation
    // per book, and the runtime's allocator never gives one back.
    //
    // Refusing rather than skipping, because the caller has a correct
    // instruction to send instead: `crank_taker_origin_cross` resolves this
    // book and pays the taker the difference.
    // Whether every book order this cross could consume has measurably
    // rested (`served_window`). The cranks cannot vouch by construction: on
    // a zero-delay book, place-then-crank is two back-to-back transactions,
    // and fresh informed flow would wear the protected flag into a quoter
    // that only serves protected flow. Measured over the consumable depth —
    // the first `size` base of each side — on the same L3 walk the guard
    // below already runs, so a fresh order beyond what the cross could
    // touch defers nothing.
    let mut flow_served = true;
    for clob in quoted
        .iter()
        .filter(|quoted| quoted.quoter_type == QuoterType::Clob)
    {
        let find = |key: &Pubkey| {
            crate::state::prop_amm::find_account(tail, key)
                .ok_or_else(|| error!(ErrorCode::DefaultError))
        };
        let slots = quoter_slab_slots(&clob.slab)?;
        let config = &slots[clob.slot].config;
        let sides = [
            find(&clob.response_account)?.clone(),
            find(&config.program_id)?.clone(),
        ];
        let (bids, asks) = super::helpers::crank_common::book_l3_sides(
            config,
            &clob.slab,
            market_index,
            CROSS_ROWS_PER_SIDE,
            &sides,
            &mut cpi_scratch,
        )?
        .unwrap_or_default();
        for side in [&bids, &asks] {
            let mut depth = 0u64;
            for order in side {
                if depth >= size {
                    break;
                }
                depth = depth.saturating_add(order.base_asset_amount);
                flow_served = flow_served
                    && crate::math::crosses::served_window(order.placed_slot, clock.slot);
            }
        }
        validate!(
            !strips_taker_origin_gate(&bids, &asks, size),
            ErrorCode::CrossedTakerRemainderPending,
            "a crossed taker remainder must be resolved by crank_taker_origin_cross"
        )?;
    }

    let mut executor = CpiQuoterExecutor {
        scratch: &mut cpi_scratch,
        caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
        reference_price: 0,
        quoted: &quoted,
        market_index,
        accounts: tail,
        users: &users,
        taker: taker_ref,
        // Not by construction: measured rest over the consumable depth,
        // computed on the guard walk above.
        taker_served_window: flow_served,
        slot: clock.slot,
        now: clock.unix_timestamp,
    };

    // Raise the surplus floor to cover the keeper's lamport payment valued in
    // quote, so a cross the reservoir pays for never nets the protocol less
    // than it costs to land. The two figures are in different units; the SOL
    // oracle bridges them. When no SOL market rides the crank or its oracle is
    // unusable, the admin's `min_cross_surplus` stands alone.
    let cross_floor = {
        let (min_surplus, payment_lamports) = {
            let conditions = ctx.accounts.crank_conditions.load()?;
            (
                conditions.min_cross_surplus,
                u64::from(conditions.crank_payments.cross),
            )
        };
        let payment_quote = sol_oracle_price(&state, &spot_market_map, &mut oracle_map)
            .and_then(|sol_price| {
                crate::state::clob_crank::CrankPaymentsV0::lamports_to_quote(
                    payment_lamports,
                    sol_price,
                )
            })
            .unwrap_or(0);
        min_surplus.max(payment_quote)
    };

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
        cross_floor,
    )?;

    // The keeper's fee, so relay's assert_paid_v0 has a balance to measure.
    let payment = u64::from(
        crate::load_mut!(ctx.accounts.crank_conditions)?
            .crank_payments
            .cross,
    );
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

/// The validity-gated SOL oracle price, for pricing a lamport crank payment in
/// quote. `None` when no SOL market is configured, it is not loaded on this
/// crank, or its oracle is not valid — the caller then falls back to the
/// admin-set floor rather than block the cross.
fn sol_oracle_price(
    state: &State,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
) -> Option<i64> {
    if state.sol_spot_market_index == 0 {
        return None;
    }
    let sol_market = spot_market_map.get_ref(&state.sol_spot_market_index).ok()?;
    let (oracle_data, validity) = oracle_map
        .get_price_data_and_validity(
            crate::state::user::MarketType::Spot,
            sol_market.market_index,
            &sol_market.oracle_id(),
            sol_market.historical_oracle_data.last_oracle_price_twap,
            sol_market.get_max_confidence_interval_multiplier().ok()?,
            -1,
            0,
            None,
        )
        .ok()?;
    matches!(validity, crate::math::oracle::OracleValidity::Valid).then_some(oracle_data.price)
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
/// The cross and activation slots' answer: a crossed taker remainder if
/// there is one, otherwise a maker-against-maker cross worth taking.
pub(super) fn stage_cross(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    if let Some(call) = stage_taker_origin_cross(ctx)? {
        return Ok(Some(call));
    }
    let cross = find_clob_cross(ctx)?;
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
        perp_market: pdas::perp_market(market_index),
        quoter_slab: ctx.accounts.quoter_slab.key(),
    })
    .map_section_named_perp(oracle, quote_spot_market_index)
    .maker_refs(cross.makers.iter().copied());
    Ok(Some(
        call.account(ctx.accounts.clob_market.key(), true)
            .account(crate::ids::clob_program::id(), false)
            // Both legs are the book's slot.
            .arg(CrankCrossMatchArgs {
                market_index,
                size: cross.size,
                buy_quoter_index: 0,
                sell_quoter_index: 0,
            })?,
    ))
}

/// The crossing prefix of the book: total matchable size, the gross quote
/// of each leg, and the (deduped, capped) makers it touches.
struct ClobCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::ClobUserRefV0>,
}

/// How deep either side of the crossing prefix is read.
///
/// The walk ends on [`MAX_CROSS_MAKERS`] distinct makers anyway, so this only
/// has to be past the point where a crossing prefix could still be one
/// maker's ladder. A prefix longer than this stages a smaller cross, which the
/// next wake continues.
const CROSS_ROWS_PER_SIDE: u16 = 32;

/// Whether this cross would take a taker-origin remainder's protection away
/// part-way through the instruction.
///
/// The book withholds a remainder for as long as a live counterparty crosses
/// it, and that gate is the only thing keeping this crank's legs off it. The
/// legs consume `size` from each side. A remainder whose whole crossing depth
/// the cross takes stops being crossed before the second leg runs; the gate
/// then goes quiet and that leg fills the remainder at its own resting price,
/// which is the outcome the taker-origin path exists to prevent.
///
/// Depth counts only the rows the legs can actually consume. A remainder on
/// the far side is withheld too, so it stays and keeps the gate firing.
///
/// A remainder nothing crosses has no gate to lose. It is an ordinary resting
/// order at its own price, which is what an unmatched remainder is for.
fn strips_taker_origin_gate(
    bids: &[crate::math::crosses::RestingOrder],
    asks: &[crate::math::crosses::RestingOrder],
    size: u64,
) -> bool {
    let exposed = |rows: &[crate::math::crosses::RestingOrder],
                   opposite: &[crate::math::crosses::RestingOrder],
                   crosses: fn(u64, u64) -> bool| {
        rows.iter().filter(|row| row.taker_origin).any(|row| {
            let depth: u64 = opposite
                .iter()
                .filter(|other| !other.taker_origin && crosses(row.price, other.price))
                .map(|other| other.base_asset_amount)
                .sum();
            depth > 0 && size >= depth
        })
    };
    exposed(bids, asks, |bid, ask| bid >= ask) || exposed(asks, bids, |ask, bid| bid >= ask)
}

/// The crossing prefix of a book against itself: total matchable size, the
/// gross quote of each leg, and the (deduped, capped) makers it touches.
///
/// Two-pointer walk over the two sides, best-first — exactly the orders the
/// executor's two legs will consume. Both sides come from the book's own
/// `quote_l3_v0`, which has already applied its rules about which of its
/// orders are matchable right now, so nothing here reads the market account.
///
/// Taker-origin rows are dropped rather than stopping the walk, and admitting
/// one is wrong in two separate ways. Usually the book withholds it from
/// `execute_v0`, so a leg sized to include its base comes back short, the two
/// legs imbalance, and the ordinary cross resting in *front* of the remainder
/// cannot clear either for as long as it is there. And admitting it also
/// stages its owner's `(User, UserStats)` pair, which is what would let the
/// book hand the remainder over at its own resting price: once the first leg
/// has consumed the whole opposite side nothing crosses the remainder any
/// more, so the gate protecting it stops firing, and the improvement lands in
/// the protocol `User` instead of the taker's. Leaving its owner unloaded
/// means the book passes over the order as unsettleable even then.
///
/// A crossed remainder is [`stage_taker_origin_cross`]'s to resolve, at the
/// counterparty's price. Whatever rests behind it is an ordinary cross and
/// stays in.
fn cross_prefix(
    bid_rows: &[crate::state::prop_amm::L3RowV0],
    ask_rows: &[crate::state::prop_amm::L3RowV0],
) -> ClobCross {
    let base_precision = crate::math::constants::BASE_PRECISION_U64 as u128;
    let mut cross = ClobCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
        makers: Vec::new(),
    };
    let crossable = |rows: &[crate::state::prop_amm::L3RowV0]| -> Vec<_> {
        rows.iter()
            .copied()
            .filter(|row| row.flags & crate::state::prop_amm::L3_ROW_FLAG_TAKER_ORIGIN == 0)
            .collect()
    };
    let (bids, asks) = (crossable(bid_rows), crossable(ask_rows));

    // `Σ price·base` per leg, divided into quote units once at the end.
    let (mut scaled_buy, mut scaled_sell) = (0u128, 0u128);
    let (mut bid_index, mut ask_index) = (0usize, 0usize);
    let mut bid_remaining = bids.first().map(|row| row.size).unwrap_or(0);
    let mut ask_remaining = asks.first().map(|row| row.size).unwrap_or(0);
    while let (Some(bid_row), Some(ask_row)) = (bids.get(bid_index), asks.get(ask_index)) {
        if bid_row.price < ask_row.price {
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
        if !admit(bid_row.user, &mut cross.makers) || !admit(ask_row.user, &mut cross.makers) {
            break;
        }

        let take = bid_remaining.min(ask_remaining);
        cross.size = cross.size.saturating_add(take);
        // The products accumulate and the division happens once, after the
        // walk. `Σ(price·take)/precision` is the same number as the sum of the
        // per-row quotients only up to rounding, and it is the cheaper one:
        // u128 division is a helper call on this target, and this loop runs
        // once per row on both sides.
        scaled_buy = scaled_buy.saturating_add(ask_row.price as u128 * take as u128);
        scaled_sell = scaled_sell.saturating_add(bid_row.price as u128 * take as u128);

        bid_remaining -= take;
        ask_remaining -= take;
        if bid_remaining == 0 {
            bid_index += 1;
            bid_remaining = bids.get(bid_index).map(|row| row.size).unwrap_or(0);
        }
        if ask_remaining == 0 {
            ask_index += 1;
            ask_remaining = asks.get(ask_index).map(|row| row.size).unwrap_or(0);
        }
    }
    cross.buy_quote = scaled_buy / base_precision;
    cross.sell_quote = scaled_sell / base_precision;
    cross
}

/// Quote both sides of the market's book and cross them against each other.
fn find_clob_cross(ctx: &Context<ResolveClobCrank>) -> Result<ClobCross> {
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let book_slot =
        crate::state::prop_amm::quoter_slab_clob(&ctx.accounts.quoter_slab, market_index)?;
    if !book_slot.quotes() {
        // A killed or unvetted book has no cross to stage; the conditions go
        // quiet rather than erroring forever.
        return Ok(cross_prefix(&[], &[]));
    }
    let quoter = &book_slot.config;
    let accounts = [
        ctx.accounts.clob_market.to_account_info(),
        ctx.accounts.clob_program.to_account_info(),
    ];
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    // One side at a time: both responses land in the same region of the book's
    // response tail, so the first is copied out before the second CPI
    // overwrites it. A buyer consumes the asks.
    let asks = super::helpers::crank_common::book_l3_side(
        quoter,
        &ctx.accounts.quoter_slab,
        market_index,
        crate::state::prop_amm::Direction::Long,
        CROSS_ROWS_PER_SIDE,
        &accounts,
        &mut cpi_scratch,
        |row| *row,
    )?
    .unwrap_or_default();
    let bids = super::helpers::crank_common::book_l3_side(
        quoter,
        &ctx.accounts.quoter_slab,
        market_index,
        crate::state::prop_amm::Direction::Short,
        CROSS_ROWS_PER_SIDE,
        &accounts,
        &mut cpi_scratch,
        |row| *row,
    )?
    .unwrap_or_default();
    Ok(cross_prefix(&bids, &asks))
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
    /// Read-only: resolvers stage into the shared scratch account.
    pub cross_conditions: AccountLoader<'info, crate::state::quoter_cross::QuoterCrossConditionsV0>,
    /// CHECK: locked to the CLOB book captured at attach. Writable for the
    /// book's response tail, which is where `quote_l3_v0` streams the resting
    /// orders this resolver crosses the entry against.
    #[account(mut, address = cross_conditions.load()?.clob_market)]
    pub clob_market: UncheckedAccount<'info>,
    pub state: AccountLoader<'info, State>,
    /// The market's slab: both legs' approved configs — the entry the
    /// conditions name, and the book at slot 0.
    #[account(
        seeds = [
            crate::state::prop_amm::QUOTER_SLAB_PDA_SEED,
            cross_conditions.load()?.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub quoter_slab: AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    /// The entry's quoted user — the maker every staged balance change
    /// lands on; its identity derives the staged `(User, UserStats)` pair.
    /// Checked against the entry's approved config in the handler.
    pub user: AccountLoader<'info, User>,
    /// CHECK: locked to the program the CLOB entry was registered with.
    #[account(address = cross_conditions.load()?.clob_program)]
    pub clob_program: UncheckedAccount<'info>,
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
        let quoter_entry_key = ctx.accounts.cross_conditions.load()?.quoter;
        let slots = crate::state::prop_amm::quoter_slab_slots(&ctx.accounts.quoter_slab)?;
        let Some(quoter_slot_index) =
            crate::state::prop_amm::slot_for_entry(&slots, &quoter_entry_key)
        else {
            // A revoked quoter has no discoverable work; the conditions go
            // quiet rather than erroring forever.
            return Ok(None);
        };
        let quoter_slot = &slots[quoter_slot_index];
        if !quoter_slot.quotes() {
            // A killed or suspended quoter has no discoverable work either.
            return Ok(None);
        }
        let quoter = &quoter_slot.config;
        validate!(
            ctx.accounts.user.key() == quoter.user,
            ErrorCode::InvalidQuoterConfig,
            "user {} is not the entry's quoted user",
            ctx.accounts.user.key()
        )?;
        let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
        // The resolver's own tail, searched rather than indexed: it is a
        // handful of accounts and this reads a few of them.
        let accounts: Vec<AccountInfo<'info>> = ctx.remaining_accounts.to_vec();

        // Quote both sides. An empty user set is discovery mode
        // (unrestricted); no taker (the executor's taker is the protocol
        // User, which quotes nothing anywhere).
        let market_index = ctx.accounts.cross_conditions.load()?.market_index;
        let mut quote = |direction: crate::state::prop_amm::Direction| -> Result<Vec<PriceLevel>> {
            let mut levels = Vec::new();
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
                        // A cross is found by comparing the two sides, so
                        // neither side has a price to stop at until the other
                        // has been read.
                        limit_price: 0,
                        // A crank's discovery read: what it stages settles only
                        // orders that rested through placement.
                        taker_served_window: true,
                    },
                    &ctx.accounts.quoter_slab,
                    &accounts,
                    &mut cpi_scratch,
                    &mut levels,
                )
                // The crank routes the book against itself; there is no
                // caller-supplied user set for it to fall short of.
                .map(|_| levels)
        };
        let quoter_asks = sanitize_levels(quote(crate::state::prop_amm::Direction::Long)?, true);
        let quoter_bids = sanitize_levels(quote(crate::state::prop_amm::Direction::Short)?, false);

        let maker_ref = {
            let user = crate::load!(ctx.accounts.user)?;
            user.clob_user_ref()
        };

        // Both cross directions against the book, quoted the same way the
        // entry was; keep the better one. buy leg = the entry whose ask is
        // consumed.
        //
        // One side at a time: both responses land in the same region of the
        // book's response tail, so the first is copied out before the second
        // CPI overwrites it.
        let clob_accounts = [
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ];
        let book_slot = crate::state::prop_amm::clob_slot_index(&slots).ok_or_else(|| {
            msg!("quoter slab holds no book slot");
            error!(ErrorCode::QuoterNotOnSlab)
        })?;
        let mut clob_book = |direction: crate::state::prop_amm::Direction| -> Result<Vec<_>> {
            Ok(super::helpers::crank_common::book_l3_side(
                &slots[book_slot].config,
                &ctx.accounts.quoter_slab,
                market_index,
                direction,
                CROSS_ROWS_PER_SIDE,
                &clob_accounts,
                &mut cpi_scratch,
                |row| *row,
            )?
            .unwrap_or_default())
        };
        // The entry's asks cross the book's bids, which is what a seller
        // consumes.
        let a = find_quoter_clob_cross(
            &clob_book(crate::state::prop_amm::Direction::Short)?,
            &quoter_asks,
            true,
        )?;
        let b = find_quoter_clob_cross(
            &clob_book(crate::state::prop_amm::Direction::Long)?,
            &quoter_bids,
            false,
        )?;
        // (cross, buy_index, sell_index): legs are slab slot indexes — the
        // book at slot 0, the entry at its own slot.
        let quoter_leg = quoter_slot_index as u8;
        let (cross, buy_index, sell_index) =
            if a.surplus(&ctx.accounts.state)? >= b.surplus(&ctx.accounts.state)? {
                (a, quoter_leg, 0u8)
            } else {
                (b, 0u8, quoter_leg)
            };
        if cross.size == 0 || cross.surplus(&ctx.accounts.state)? == 0 {
            return Ok(None);
        }

        let (oracle, quote_spot_market_index, clob_program) = {
            let conditions = ctx.accounts.cross_conditions.load()?;
            (
                conditions.oracle,
                conditions.quote_spot_market_index,
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
            perp_market: pdas::perp_market(market_index),
            quoter_slab: ctx.accounts.quoter_slab.key(),
        })
        .map_section_named_perp(oracle, quote_spot_market_index);
        // Maker pairs: the quoter's user first, then the CLOB-side makers.
        let mut staged = vec![maker_ref];
        for maker in &cross.makers {
            if !staged.contains(maker) {
                staged.push(*maker);
            }
        }
        // The union of both execute surfaces: the CLOB's book plus everything
        // the quoter registered, programs included. Both legs sign as the
        // slab, which is staged as a named account.
        let slab_key = ctx.accounts.quoter_slab.key();
        let mut call = call.maker_refs(staged.iter().copied());
        let mut union: std::collections::BTreeMap<Pubkey, bool> = Default::default();
        *union.entry(ctx.accounts.clob_market.key()).or_default() |= true;
        union.entry(clob_program).or_default();
        for meta in quoter.leg_metas(quoter.execute_leg_indexes())? {
            *union.entry(meta.pubkey).or_default() |= meta.is_writable;
        }
        *union.entry(quoter.response_account).or_default() |= true;
        union.entry(quoter.program_id).or_default();
        for (key, writable) in union.iter().filter(|(key, _)| **key != slab_key) {
            call = call.account(*key, *writable);
        }
        Ok(Some(call.arg(CrankCrossMatchArgs {
            market_index,
            size: cross.size,
            buy_quoter_index: buy_index,
            sell_quoter_index: sell_index,
        })?))
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

/// Walk the quoter's (sanitized) levels against the CLOB's resting rows:
/// `quoter_is_ask_side` crosses quoter asks with CLOB bids (CLOB bid price
/// >= quoter ask price), else CLOB asks with quoter bids.
fn find_quoter_clob_cross(
    clob_rows: &[crate::state::prop_amm::L3RowV0],
    quoter_levels: &[PriceLevel],
    quoter_is_ask_side: bool,
) -> Result<QuoterCross> {
    let base_precision = crate::math::constants::BASE_PRECISION_U64 as u128;
    let mut cross = QuoterCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
        makers: Vec::new(),
    };
    let mut rows = clob_rows.iter();
    let mut row = rows.next();
    let mut clob_remaining = row.map(|row| row.size).unwrap_or(0);
    let mut levels = quoter_levels.iter();
    let mut level = levels.next();
    let mut level_remaining = level.map(|l| l.size).unwrap_or(0);

    while let (Some(r), Some(l)) = (row, level) {
        let crossed = if quoter_is_ask_side {
            r.price >= l.price
        } else {
            l.price >= r.price
        };
        if !crossed {
            break;
        }
        // Reserve one maker slot for the quoter's user (staged first).
        if !cross.makers.contains(&r.user) {
            if cross.makers.len() + 1 >= MAX_CROSS_MAKERS {
                break;
            }
            cross.makers.push(r.user);
        }
        let take = clob_remaining.min(level_remaining);
        let (ask_price, bid_price) = if quoter_is_ask_side {
            (l.price, r.price)
        } else {
            (r.price, l.price)
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
            row = rows.next();
            clob_remaining = row.map(|row| row.size).unwrap_or(0);
        }
        if level_remaining == 0 {
            level = levels.next();
            level_remaining = level.map(|l| l.size).unwrap_or(0);
        }
    }
    Ok(cross)
}
