//! The funding-rate crank and the bid/ask TWAP crank.
//!
//! Both read the market's oracle and move market-level statistics. Neither
//! advances an oracle TWAP before the gate that reads it, because a crank that
//! refreshes its own anchor can clear its own gate.
//!
//! The bid/ask crank measures the market from the market's own book, read
//! through `quote_l3_v0`. It reads no account the caller chooses, so the caller
//! chooses no part of what moves the mark.

use super::*;

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
    funding_not_paused(&ctx.accounts.state)
    valid_oracle_for_perp_market(&ctx.accounts.oracle, &ctx.accounts.perp_market)
)]
pub fn handle_update_funding_rate(
    ctx: Context<UpdateFundingRate>,
    perp_market_index: u16,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let clock_slot = clock.slot;
    let state = ctx.accounts.state.load()?;
    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock_slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    refresh_quote_state_for_funding(perp_market, &state, &mut oracle_map, clock_slot)?;

    validate!(
        matches!(
            perp_market.status,
            MarketStatus::Active | MarketStatus::ReduceOnly
        ),
        ErrorCode::MarketActionPaused,
        "Market funding is paused",
    )?;

    let funding_paused =
        state.funding_paused()? || perp_market.is_operation_paused(PerpOperation::UpdateFunding);

    let is_updated = controller::funding::update_funding_rate(
        perp_market_index,
        perp_market,
        &mut oracle_map,
        now,
        clock_slot,
        &state.oracle_guard_rails,
        funding_paused,
        None,
    )?;

    if !is_updated {
        let time_until_next_update = crate::math::helpers::on_the_hour_update(
            now,
            perp_market.last_funding_rate_ts,
            perp_market.market_stats.funding_period,
        )?;

        msg!(
            "time_until_next_update = {:?} seconds",
            time_until_next_update
        );

        return Err(ErrorCode::FundingWasNotUpdated.into());
    }

    Ok(())
}

/// Refresh the market's cached quote state against this slot's oracle.
///
/// The AMM refresh happens inside `update_funding_rate`'s AmmQuoter setup, not here. This is the
/// TWAP-free half, by design: the gate there reads `last_oracle_price_twap` for the too-volatile
/// check and `last_oracle_price_twap_5min` for the mark-divergence check, and advancing either TWAP
/// here would pull it toward the live price. A too-volatile or too-divergent oracle could then
/// clear its own gate inside this same instruction and mutate cumulative funding. The skip loses
/// nothing. `update_funding_rate` advances the TWAPs itself when funding updates, and reverts the
/// whole instruction with `FundingWasNotUpdated` when it does not. The TWAPs also keep advancing
/// through `update_amms`, perp fills, and `update_perp_bid_ask_twap`, so a too-volatile market
/// still recovers without this crank relaxing its own gate.
fn refresh_quote_state_for_funding(
    perp_market: &mut PerpMarket,
    state: &State,
    oracle_map: &mut OracleMap,
    slot: u64,
) -> Result<()> {
    let oracle_price_data = *oracle_map.get_price_data(&perp_market.oracle_id())?;
    let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
        oracle_price_data,
        slot,
        &state.oracle_guard_rails.validity,
        state.slot_clock(),
    )?;
    let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity(
        perp_market,
        &mm_oracle_price_data,
        state,
        slot,
    )?;

    perp_market.refresh_amm_quote_state(
        &mm_oracle_price_data,
        validity,
        slot,
        state.slot_clock(),
    )?;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
    funding_not_paused(&ctx.accounts.state)
    valid_oracle_for_perp_market(&ctx.accounts.oracle, &ctx.accounts.perp_market)
)]
pub fn handle_update_perp_bid_ask_twap<'c: 'info, 'info>(
    ctx: Context<'info, UpdatePerpBidAskTwap<'info>>,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    // Stop this crank while funding is paused. `funding_not_paused` blocks only the exchange
    // pause. This crank also feeds `OrderParams::get_perp_baseline_start_price_offset`'s stop-loss
    // band from an estimated CLOB book, not real fills, so a paused, untrusted market skips it.
    // Fills still write the same TWAPs, since they carry real capital.
    if perp_market.is_operation_paused(PerpOperation::UpdateFunding) {
        return Ok(());
    }

    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let keeper_stats = load!(ctx.accounts.keeper_stats)?;
    require_twap_keeper(&keeper_stats)?;

    let oracle_price_data = *oracle_map.get_price_data(&perp_market.oracle_id())?;
    let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
        oracle_price_data,
        clock.slot,
        &state.oracle_guard_rails.validity,
        state.slot_clock(),
    )?;

    // This crank writes `PerpMarket`-level oracle stats only. It reads the book
    // to estimate the bid and ask TWAP. It reads neither the AMM peg nor the
    // AMM reserves. `update_oracle_derived_stats` below
    // keeps the oracle TWAP and the reference price offset current.
    let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity(
        perp_market,
        &mm_oracle_price_data,
        &state,
        clock.slot,
    )?;

    perp_market.update_oracle_derived_stats(
        &mm_oracle_price_data,
        validity,
        clock.unix_timestamp,
        clock.slot,
        state.slot_clock(),
    )?;

    let book = book_source(&ctx, perp_market)?;
    let estimates = estimate_book(
        perp_market,
        &oracle_price_data,
        book.as_ref(),
        &state,
        &clock,
    )?;

    // This crank does not apply funding. One instruction that refreshed the mark TWAP from resting
    // depth and then applied funding would let a caller stamp `last_mark_price_twap_ts = now`, so
    // funding would read that just-written TWAP back at zero elapsed time. Funding runs on its own
    // `update_funding_rate` crank, and on fills.
    apply_bid_ask_twap(
        perp_market,
        &mm_oracle_price_data,
        &oracle_price_data,
        estimates,
        &state,
        &clock,
    )
}

/// Only a keeper that holds an insurance-fund stake may move a TWAP from its
/// own estimate.
fn require_twap_keeper(keeper_stats: &UserStats) -> Result<()> {
    validate!(
        keeper_stats.can_update_bid_ask_twap(),
        ErrorCode::CantUpdatePerpBidAskTwap,
        "Keeper stats can_update_bid_ask_twap is false"
    )?;

    let min_if_stake = 1000 * QUOTE_PRECISION_U64;
    validate!(
        keeper_stats.if_staked_quote_asset_amount >= min_if_stake,
        ErrorCode::CantUpdatePerpBidAskTwap,
        "Keeper doesnt have min if stake. stake = {} min if stake = {}",
        keeper_stats.if_staked_quote_asset_amount,
        min_if_stake
    )?;

    Ok(())
}

/// Rows the estimate reads from one side of the book. The same cap the
/// `User` walk keeps on its levels, so neither source outweighs the other by
/// depth alone.
const TWAP_ROWS_PER_SIDE: u16 = 32;

/// The market's book, bound for the two reads the estimate makes.
struct BookSource<'a, 'info> {
    config: crate::state::prop_amm::QuoterConfigV0,
    slab: &'a AccountLoader<'info, QuoterSlabV0>,
    /// The book and its program. The entry's registered CPI list is resolved
    /// against these by address.
    accounts: [AccountInfo<'info>; 2],
}

impl<'info> BookSource<'_, 'info> {
    /// One side of the book as levels, best price first.
    ///
    /// `direction` is the taker's, so `Long` reads the asks.
    ///
    /// An order that has not rested for `BID_ASK_TWAP_MIN_QUOTE_REST` is
    /// dropped, for the reason the `User` walk drops one: a caller could
    /// otherwise place a quote, crank the mark off it and cancel it in one
    /// transaction at no risk.
    fn side(
        &self,
        market_index: u16,
        direction: Direction,
        state: &State,
        clock: &Clock,
        scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> Result<Vec<Level>> {
        let slot_clock = state.slot_clock();
        let rows = crate::instructions::clob::helpers::crank_common::book_l3_side(
            &self.config,
            self.slab,
            market_index,
            direction,
            TWAP_ROWS_PER_SIDE,
            &self.accounts,
            scratch,
            // A crossing remainder's cover is depth the book holds for one
            // crank. Reading it here would not take it, but it is not depth
            // this mark can claim either.
            false,
            |row| (row.price, row.size, row.placed_slot),
        )?
        .unwrap_or_default();
        Ok(rows
            .into_iter()
            .filter(|(_, _, placed_slot)| quote_has_rested(*placed_slot, clock.slot, slot_clock))
            .map(|(price, size, _)| Level {
                price,
                base_asset_amount: size,
            })
            .collect())
    }
}

/// The best bid and ask the market stands behind, to the depth the market's
/// funding rate is measured over.
///
/// The book holds every resting order the market has, so the estimate reads it
/// through the same `quote_l3_v0` leg every other book reader uses. Nothing
/// rests anywhere else, so the caller chooses no part of what moves the mark.
fn estimate_book<'info>(
    perp_market: &PerpMarket,
    oracle_price_data: &crate::state::oracle::OraclePriceData,
    book: Option<&BookSource<'_, 'info>>,
    state: &State,
    clock: &Clock,
) -> Result<(Option<u64>, Option<u64>)> {
    let depth = perp_market.get_market_depth_for_funding_rate()?;
    let market_index = perp_market.market_index;

    let mut scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let (book_bids, book_asks) = match book {
        // Two calls, one per side. Both answers land in the same region of
        // the book's response tail, so the first is copied out before the
        // second overwrites it. A taker of `Short` sweeps the bids.
        Some(book) => (
            book.side(market_index, Direction::Short, state, clock, &mut scratch)?,
            book.side(market_index, Direction::Long, state, clock, &mut scratch)?,
        ),
        None => (Vec::new(), Vec::new()),
    };

    let (bids, asks) = filter_bids_asks_by_oracle_divergence(
        book_bids,
        book_asks,
        oracle_price_data.price,
        BID_ASK_TWAP_MAX_ORACLE_DIVERGENCE_PERCENT,
    )?;
    let estimated_bid = estimate_price_from_side(&bids, depth)?;
    let estimated_ask = estimate_price_from_side(&asks, depth)?;

    msg!(
        "estimated_bid = {:?} estimated_ask = {:?}",
        estimated_bid,
        estimated_ask
    );

    Ok((estimated_bid, estimated_ask))
}

/// The market's book, when this crank carries it.
///
/// A market that names a book is cranked with it. The estimate is only as
/// good as the depth it reads, and a caller free to leave the book out would
/// choose which liquidity moves the mark.
///
/// `None` is a market that names no book, or one whose slot cannot quote. A
/// suspended or de-listed book holds depth nobody can take, so it must not
/// move the mark either.
fn book_source<'a, 'info>(
    ctx: &'a Context<'info, UpdatePerpBidAskTwap<'info>>,
    perp_market: &PerpMarket,
) -> Result<Option<BookSource<'a, 'info>>> {
    if perp_market.clob_market == Pubkey::default() {
        return Ok(None);
    }

    let (Some(slab), Some(book), Some(program)) = (
        ctx.accounts.quoter_slab.as_ref(),
        ctx.accounts.clob_market.as_ref(),
        ctx.accounts.clob_program.as_ref(),
    ) else {
        msg!(
            "market {} names CLOB book {}; the crank must carry it",
            perp_market.market_index,
            perp_market.clob_market
        );

        return Err(ErrorCode::DefaultError.into());
    };
    let slot = slab.clob_slot(perp_market.market_index)?;
    slot.config
        .validate_clob_book(perp_market.market_index, &book.key())?;
    if !slot.quotes() {
        return Ok(None);
    }

    Ok(Some(BookSource {
        config: slot.config,
        slab,
        accounts: [book.to_account_info(), program.to_account_info()],
    }))
}

/// The TWAP state a crank starts from, kept so the crank can prove it moved.
struct TwapSnapshot {
    bid: u64,
    ask: u64,
    mark_ts: i64,
}

/// Fold the estimate, plus the AMM's own spread, into the market's mark TWAP.
fn apply_bid_ask_twap(
    perp_market: &mut PerpMarket,
    mm_oracle_price_data: &crate::state::oracle::MMOraclePriceData,
    oracle_price_data: &crate::state::oracle::OraclePriceData,
    estimates: (Option<u64>, Option<u64>),
    state: &State,
    clock: &Clock,
) -> Result<()> {
    let before = TwapSnapshot {
        bid: perp_market.market_stats.last_bid_price_twap,
        ask: perp_market.market_stats.last_ask_price_twap,
        mark_ts: perp_market.market_stats.last_mark_price_twap_ts,
    };

    let sanitize_clamp_denominator = perp_market.get_sanitize_clamp_denominator()?;
    {
        let reserve_price = perp_market.amm.reserve_price()?;
        let crate::state::perp_market::PerpMarket {
            amm, market_stats, ..
        } = &mut *perp_market;

        // Refresh the AMM's cached spread state against this slot's oracle,
        // then fold it (plus the resting liquidity) into the mark TWAP.
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            mm_oracle_price_data,
            reserve_price,
            clock.slot,
            state.slot_clock(),
        )?;

        market_stats.update_mark_twap_crank(
            amm,
            clock.unix_timestamp,
            oracle_price_data,
            estimates.0,
            estimates.1,
            sanitize_clamp_denominator,
        )?;
    }

    msg!(
        "after amm bid twap = {} -> {}
        ask twap = {} -> {}
        ts = {} -> {}",
        before.bid,
        perp_market.market_stats.last_bid_price_twap,
        before.ask,
        perp_market.market_stats.last_ask_price_twap,
        before.mark_ts,
        perp_market.market_stats.last_mark_price_twap_ts
    );

    require_twap_moved(perp_market, &before, estimates)
}

/// A crank that moves neither side of the TWAP did no work.
///
/// A long enough time step, or an estimate that already matches the stored
/// value, are the two reasons a side can legitimately stand still.
fn require_twap_moved(
    perp_market: &PerpMarket,
    before: &TwapSnapshot,
    estimates: (Option<u64>, Option<u64>),
) -> Result<()> {
    if perp_market.market_stats.last_bid_price_twap != before.bid
        && perp_market.market_stats.last_ask_price_twap != before.ask
    {
        return Ok(());
    }

    validate!(
        perp_market
            .market_stats
            .last_mark_price_twap_ts
            .safe_sub(before.mark_ts)?
            >= 60
            || estimates.0.unwrap_or(0) == before.bid
            || estimates.1.unwrap_or(0) == before.ask,
        ErrorCode::CantUpdatePerpBidAskTwap,
        "bid or ask twap unchanged from small ts delta update",
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct UpdateFundingRate<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: checked in `update_funding_rate` ix constraint
    pub oracle: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct UpdatePerpBidAskTwap<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: checked in `update_funding_rate` ix constraint
    pub oracle: UncheckedAccount<'info>,
    #[account(has_one = authority)]
    pub keeper_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    /// The market's approved quoter set, which names the book below.
    ///
    /// Optional, for a market that names no book. A market that names one is
    /// refused without it.
    #[account(constraint = quoter_slab.load()?.market == perp_market.load()?.market_index)]
    pub quoter_slab: Option<AccountLoader<'info, QuoterSlabV0>>,
    /// CHECK: held to the book slot's registered response account, so a valid
    /// slab cannot be pointed at an arbitrary account.
    ///
    /// Writable because `quote_l3_v0` streams its answer into the book's own
    /// response tail. The crank changes no order.
    #[account(mut)]
    pub clob_market: Option<UncheckedAccount<'info>>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration, which `validate_clob_book` re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: Option<UncheckedAccount<'info>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: u64, base_asset_amount: u64) -> Level {
        Level {
            price,
            base_asset_amount,
        }
    }

    /// The estimate walks one side best price first and weights each level by
    /// the base behind it, so it reads the average the depth would pay.
    #[test]
    fn the_estimate_weights_each_level_by_its_depth() {
        let side = vec![level(100, 2), level(90, 2)];
        assert_eq!(estimate_price_from_side(&side, 4).unwrap(), Some(95));
    }

    /// A side shallower than the depth still prices what it holds.
    #[test]
    fn a_shallow_side_prices_what_it_holds() {
        let side = vec![level(90, 2), level(80, 2)];
        assert_eq!(estimate_price_from_side(&side, 4).unwrap(), Some(85));
    }
}

/// allow-verbose: carries two independent facts, a manipulation-resistance rationale and a
/// saturating-arithmetic edge case, neither of which the code below states on its own.
///
/// Whether a book row has rested long enough to move the mark.
///
/// A quote that can be posted and cancelled inside one crank is a quote nobody had to stand
/// behind, so the mark must not read it. Requiring [`BID_ASK_TWAP_MIN_QUOTE_REST`] of rest makes a
/// row cost its poster real exposure before it counts.
///
/// The subtraction saturates, so a row stamped at or after the current slot reports no rest rather
/// than wrapping to a large one. A clock that runs behind the book therefore excludes a fresh
/// quote instead of admitting it.
fn quote_has_rested(
    placed_slot: u64,
    now_slot: u64,
    slot_clock: crate::math::time::SlotClock,
) -> bool {
    slot_clock.elapsed_slot_delta(now_slot.saturating_sub(placed_slot), now_slot)
        >= BID_ASK_TWAP_MIN_QUOTE_REST
}

#[cfg(test)]
mod quote_rest_tests {
    use {super::quote_has_rested, crate::math::time::SlotClock};

    /// 9,600ms of rest is 24 slots at the baseline duration.
    const RESTED_SLOTS: u64 = 24;

    #[test]
    fn a_quote_that_has_rested_the_window_counts() {
        assert!(quote_has_rested(
            1_000,
            1_000 + RESTED_SLOTS,
            SlotClock::baseline()
        ));
    }

    #[test]
    fn a_freshly_posted_quote_is_excluded() {
        let now = 1_000;
        assert!(!quote_has_rested(now, now, SlotClock::baseline()));
        assert!(!quote_has_rested(now - 1, now, SlotClock::baseline()));
        assert!(!quote_has_rested(
            now + 1 - RESTED_SLOTS,
            now,
            SlotClock::baseline()
        ));
    }

    /// A row stamped ahead of the reader's clock must read as fresh, not as
    /// having rested since the beginning of time.
    #[test]
    fn a_quote_from_the_future_never_reads_as_rested() {
        assert!(!quote_has_rested(2_000, 1_000, SlotClock::baseline()));
    }
}
