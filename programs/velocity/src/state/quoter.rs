//! # Quoter interfaces for the shared orderbook
//!
//! Velocity matches across several liquidity sources. They are DLOB maker
//! orders, the vAMM, and external quoter programs such as the CLOB and
//! PropAMMs, which velocity reaches over CPI. Every source publishes discrete
//! price levels and the router splits a take across them.
//!
//! The interface is [`RouterQuoter`]. `quote` returns a source's book as
//! [`PriceLevel`]s. `execute` fills an allocation against the source and
//! settles the source's own bytes. A source is the sole authority on how its
//! bytes change, and the router only hands it the allocation it won.
//! `priority` places it in a tier. At a shared price, a lower priority number
//! fills first, and a tie inside one tier splits pro rata.
//!
//! Two implementations live in this program. [`DlobOrderQuoter`] bridges one
//! resting DLOB order as a single discrete level. `AmmQuoter` has its ladder
//! built by `vlp::amm::router_adapter::vamm_quote_levels`, so the continuous
//! curve is reduced to levels before the router sees it. An external quoter is
//! not a `RouterQuoter` implementation. It is quoted and executed over CPI
//! through [`crate::state::prop_amm::QuoterConfigV0`].
//!
//! [`QuoteContext`] is the snapshot every source quotes against. A quote
//! method reads from it, and `execute` reads it again, so a source reaches the
//! same conclusion at execute time as it did at quote time. Refresh cost, such
//! as an AMM repeg, comes back through [`QuoterFill::refresh_cost`] for the
//! fill controller to apply to the `PerpMarket`.
//!
//! Each source picks its own fee handling. [`FillFeePolicy`] selects the
//! schedule a fill settles on. A resting order uses `DlobMatch` and the vAMM
//! uses `AmmHouse`. The vAMM is exempt from the maker schedule, because it
//! earns from the spread it quotes rather than from a rebate.
//!
//! See `docs/amm-decoupling-and-maker-interface.md` for the full design,
//! including pro-rata policy and snapshot consistency rules.

use crate::{
    controller::position::PositionDirection,
    error::{ErrorCode, VelocityResult},
    math::{safe_math::SafeMath, time::SlotClock},
    state::{
        oracle::{MMOraclePriceData, OraclePriceData},
        perp_market::MarketStats,
        prop_amm::WireDirectionExt,
    },
};

/// Inputs the matcher shares with every maker during a single match.
///
/// Quote methods read from this; `commit_fill` reads from it again to
/// re-derive any conditional state updates (e.g. AMM repeg) so the maker
/// reaches the same conclusion at commit time as it did at quote time.
#[derive(Copy, Clone)]
pub struct QuoteContext<'a> {
    /// Historic market data — TWAPs, volatility, volume, mm-oracle. Written
    /// by `controller/market_stats.rs` from every fill path; read by makers
    /// when computing their quotes.
    pub stats: &'a MarketStats,
    /// Current oracle reading for the market.
    pub oracle: &'a OraclePriceData,
    /// MM-wrapped oracle reading. Required for makers whose `setup` derives
    /// post-refresh state from the same MM oracle the orchestrator's keeper
    /// crank uses (the AMM); `None` for callers that only need the plain
    /// `OraclePriceData` (DLOB, JIT) or that aren't invoking setup.
    pub mm_oracle: Option<&'a MMOraclePriceData>,
    /// Oracle validity classification, computed by the orchestrator. Threaded
    /// here so `setup` can decide whether to apply a curve update (AMM) or
    /// skip it. `None` mirrors the Settlement/Delisted passthrough.
    pub oracle_validity: Option<crate::math::oracle::OracleValidity>,
    /// Available protocol fee budget the AMM can consume for repeg / k-update.
    /// The fill controller reads this off the AMM's bookkeeping and passes
    /// it in as a scalar — the AMM itself does not reach into PerpMarket
    /// state.
    pub fee_budget: u64,
    /// The market's price tick — minimum **price** increment. Sourced from
    /// `PerpMarket::order_tick_size`.
    pub tick: u64,
    /// The market's base step — minimum **base-amount** increment a fill
    /// can take. Used by the AMM's `cumulative_size` (the sole-vAMM
    /// limit cap) to standardise its analytic-inverse output into valid lot
    /// sizes. Sourced from `PerpMarket::order_step_size`.
    pub step_size: u64,
    /// Current slot. Used by DLOB-order makers to determine auction state
    /// (an order in active auction prices differently than the same order
    /// resting post-auction).
    pub slot: u64,
    /// Cluster slot clock, from `State::slot_clock()`. It scales the
    /// slot-denominated windows that are calibrated to the 400ms baseline,
    /// such as the reference price offset smoothing budget, across an IBRL
    /// transition.
    pub slot_clock: SlotClock,
    /// Base-asset precision divisor: when computing `quote_amount` from a
    /// base amount filled at a price, the formula is
    /// `quote = base * price / base_precision` to convert from raw base
    /// units to QUOTE_PRECISION-scaled quote. For perps this is `1e9`
    /// (BASE_PRECISION). Step makers (DLOB orders) and the matcher's
    /// credit/marginal accumulation steps use this to keep precision
    /// consistent. AMM-style makers using try_fill_solo bypass this since
    /// they compute quote via the AMM's own swap math, which is already
    /// precision-correct.
    pub base_precision: u64,
    /// PerpMarket status — threaded to the AMM's projection so the curve
    /// update can relax its k-down precondition when the market is
    /// `ReduceOnly` (matching legacy behaviour). AMM-only.
    pub market_status: crate::state::market_status::MarketStatus,
    /// Raw `PerpMarket::market_config` byte — the AMM tests
    /// `MarketConfigFlag::DisableFormulaicKUpdate` against it during k
    /// adjustment. AMM-only.
    pub market_config: u8,
}

/// The result of a single maker's portion of a match. Constructed either by
/// the matcher (during segment-walk + pro-rata) or returned directly by a
/// maker's [`Quoter::try_fill_solo`].
///
/// The fields are protocol-level — meaningful to the fill controller without
/// any maker-specific interpretation. The maker is the sole interpreter of
/// any maker-specific state changes implied by the fill; those happen inside
/// [`RouterQuoter::execute`].
#[derive(Debug, Clone, Copy)]
pub struct QuoterFill {
    /// Which side of the book this fill is on (from the taker's perspective).
    pub side: PositionDirection,
    /// Base asset amount this maker filled.
    pub base_filled: u64,
    /// Quote asset amount this maker filled.
    pub quote_filled: u64,
    /// The clearing price for this maker's portion (the marginal tick the
    /// matcher decided on, or the analytical inverse for sole-maker fills).
    pub clearing_price: u64,
    /// Any cost the maker incurred to produce this fill — for the AMM, this
    /// is the cost of a conditional repeg / k-update that fired as part of
    /// the quote. Summed across the match by the fill controller and
    /// deducted from the appropriate place. Zero for makers without such
    /// costs (DLOB orders, JIT participants).
    pub refresh_cost: u64,
    /// Maker's fee-exempt flag at fill time, copied from `Quoter::is_fee_exempt`.
    /// The fill controller reads this to decide whether to apply the
    /// protocol's maker-fee schedule. AMM = true; DLOB/JIT = false.
    pub is_fee_exempt: bool,
    /// Per-fill fee schedule selector, copied from `Quoter::fee_policy()`.
    /// The unified fulfill orchestrator switches on this to apply the right
    /// fee-calculation path (`calculate_fee_for_fulfillment_with_amm` for
    /// AMM-side fills; `calculate_fee_for_fulfillment_with_match` for
    /// DLOB-side fills) without inspecting the quoter's concrete type.
    pub fee_policy: FillFeePolicy,
    /// Maker-specific quote surplus (or deficit, if negative). For the AMM,
    /// this is the gap between the spread-adjusted swap result and the
    /// no-spread swap result — the bid/ask spread profit the AMM captured
    /// (or lost) on this fill. Zero for makers without such a concept
    /// (DLOB orders quote a single price; there is no spread to capture).
    pub quote_asset_amount_surplus: i64,
}

impl QuoterFill {
    pub const ZERO: QuoterFill = QuoterFill {
        side: PositionDirection::Long,
        base_filled: 0,
        quote_filled: 0,
        clearing_price: 0,
        refresh_cost: 0,
        is_fee_exempt: false,
        fee_policy: FillFeePolicy::DlobMatch,
        quote_asset_amount_surplus: 0,
    };
}

/// Per-fill fee schedule. Returned by `Quoter::fee_policy()` and copied into
/// each `QuoterFill` so the unified fulfill orchestrator can switch on it
/// without knowing the concrete quoter type.
///
/// - `AmmHouse` — the AMM is the counterparty. Taker pays the AMM-house fee
///   schedule (`calculate_fee_for_fulfillment_with_amm`); no maker rebate.
///   The AMM's `total_fee` / `total_fee_minus_distributions` /
///   `net_revenue_since_last_funding` get credited via
///   `AmmContract::apply_fill_fees`.
/// - `DlobMatch` — a DLOB resting order is the counterparty. Taker pays the
///   match fee schedule (`calculate_fee_for_fulfillment_with_match`); the
///   maker receives a rebate. AMM-side counters are NOT touched (the AMM
///   was not party to this fill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillFeePolicy {
    AmmHouse,
    DlobMatch,
}

/// A market-level signal a maker may want to react to.
///
/// The variants carry every input a maker needs to handle the event in
/// isolation — the maker should never need to reach back into PerpMarket
/// state from within its handler. PerpMarket-level fields the AMM's k-update
/// formerly read (the protocol fee floor) are pre-computed by the
/// orchestrator and threaded through the event.
#[derive(Debug, Clone, Copy)]
pub enum MarketEvent<'a> {
    /// Funding has just been applied to the market — cumulative rates have
    /// been bumped on `PerpMarket`. Carries everything a position-holding
    /// participant needs to settle its own funding payment from cum-rate
    /// deltas (`(market_cum_rate − own_last_cum_rate) × position`) and,
    /// for the AMM, to run its eager k-update.
    FundingUpdated {
        /// PerpMarket index — threaded through so the AMM can emit
        /// `AmmCurveChanged` (which carries market_index) from inside the
        /// handler.
        market_index: u16,
        /// Post-update cumulative funding rates from `PerpMarket`. The AMM
        /// settles against `(new − own_last) × counterparty_position` —
        /// same math shape user positions use via `settle_funding_payment`.
        cumulative_funding_rate_long: i128,
        cumulative_funding_rate_short: i128,
        /// User-position aggregates so the AMM can decompose its
        /// counterparty exposure (long-side vs short-side). Lets the AMM
        /// match master's asymmetric-cap behaviour without reaching back
        /// into PerpMarket. Snapshot copied at dispatch time.
        base_asset_amount_long: i128,
        base_asset_amount_short: i128,
        /// This period's funding_rate scalar — used by the k-update
        /// affordability / direction logic. Cum-rate deltas alone don't
        /// recover it (capping splits long vs short asymmetrically).
        funding_rate: i128,
        oracle_price_data: &'a OraclePriceData,
        now: i64,
        /// AMM bid/ask spread snapshot at the moment funding was computed.
        /// The k-update branch compares these against the AMM's base spread.
        long_spread: u32,
        short_spread: u32,
        /// Formulaic k-update enabled (curve_update_intensity + the
        /// `DisableFormulaicKUpdate` config flag). If false, AMM still
        /// settles funding + resets the rolling window but skips k-update.
        k_update_eligible: bool,
        /// `market.status` — threaded so `get_update_k_result` can relax
        /// its k-down precondition when the market is `ReduceOnly`.
        market_status: crate::state::market_status::MarketStatus,
        /// `market_stats.min_order_size` — used by the AMM's `can_lower_k`
        /// check during k-update.
        min_order_size: u64,
    },
}

// `AmmContract` trait moved to `crate::vlp::amm::quoter` so the AMM-side contract
// definition co-locates with the only impl (`impl AmmContract for AMM`).
// Callers import it from there directly.

/// A liquidity source the router fill reads and executes. It is the
/// in-program mirror of the registry's external `QuoterConfigV0` CPI legs,
/// and it keeps the same shape. `quote` returns discrete best-first levels.
/// `execute` commits a fill of the routed allocation and reports it. An
/// implementation that needs a per-fill refresh, such as the AMM's projection
/// and curve update, runs it in its own setup step before the router quotes
/// it. Setup is not part of this contract, which is how an external quoter
/// refreshes its own state too.
pub trait RouterQuoter {
    /// Routing tier, with the same meaning as the registry's
    /// `QuoterConfigV0::priority`. A lower number fills first at a shared
    /// price, and one tier splits pro rata.
    fn priority(&self) -> u8;

    /// Discrete best-first levels for a taker of `direction` and `size`. This
    /// is the in-program `quote_v0`. `rival_books` carries the books already
    /// built in this fill, which are the external CPI books and the
    /// worse-tier internal ones. It lets a quoter run a last look. Most
    /// quoters ignore it.
    fn quote(
        &self,
        ctx: &QuoteContext,
        direction: crate::state::prop_amm::Direction,
        size: u64,
        rival_books: &[crate::math::router::QuoterBook],
    ) -> VelocityResult<Vec<crate::state::prop_amm::PriceLevel>>;

    /// Commit a fill of up to `size`, which is the routed allocation. This is
    /// the in-program `execute_v0`. It applies the quoter's own state changes
    /// and reports the fill. Taker-side settlement stays with the fill
    /// controller.
    fn execute(
        &mut self,
        ctx: &QuoteContext,
        direction: crate::state::prop_amm::Direction,
        size: u64,
    ) -> VelocityResult<QuoterFill>;
}

impl RouterQuoter for DlobOrderQuoter<'_> {
    /// The DLOB bridges into the router at the CLOB's tier during migration.
    fn priority(&self) -> u8 {
        crate::state::prop_amm::QuoterType::Clob.default_priority()
    }

    /// A resting order is a single level at its effective limit price, sized
    /// by what remains of it.
    fn quote(
        &self,
        ctx: &QuoteContext,
        direction: crate::state::prop_amm::Direction,
        size: u64,
        _rival_books: &[crate::math::router::QuoterBook],
    ) -> VelocityResult<Vec<crate::state::prop_amm::PriceLevel>> {
        let side = direction.to_position_direction();
        if !self.quotes_on(side) {
            return Ok(vec![]);
        }
        let Some(price) = self.effective_price(ctx)? else {
            return Ok(vec![]);
        };
        let size = self.remaining().min(size);
        if size == 0 {
            return Ok(vec![]);
        }
        Ok(vec![crate::state::prop_amm::PriceLevel { price, size }])
    }

    fn execute(
        &mut self,
        ctx: &QuoteContext,
        direction: crate::state::prop_amm::Direction,
        size: u64,
    ) -> VelocityResult<QuoterFill> {
        self.fill(ctx, direction.to_position_direction(), size)
    }
}

// ============================================================================
// DLOB resting-order Quoter impl
// ============================================================================

use crate::state::user::Order;

/// A [`RouterQuoter`] view over a single resting DLOB order.
///
/// The order's direction says which side of the book it offers liquidity on.
/// Long is a bid and Short is an ask. A maker offers liquidity to the opposite
/// taker side. `best_price` returns the order's effective limit price from
/// `Order::get_limit_price`, which handles oracle-offset orders. The order
/// presents as one discrete level at `best_price`, sized by what remains of
/// it.
///
/// DLOB makers do not implement `is_prio` or `is_fee_exempt` (defaults of
/// `false`). The matcher sorts them with the vAMM by price; at tied prices
/// the vAMM (prio) wins, otherwise non-prio makers pro-rata.
///
/// `try_fill_solo` is implemented because a single resting order has a
/// trivial closed-form fill: take `min(target, remaining)` at the limit
/// price.
pub struct DlobOrderQuoter<'a> {
    pub order: &'a mut Order,
    /// Upper bound on how much base this order may fill, on top of the
    /// order's own unfilled amount. Callers pass the position-capped
    /// unfilled (`Order::get_base_asset_amount_unfilled(Some(position))`)
    /// so a reduce-only order can only shrink the maker's position, never
    /// grow or flip it.
    max_fill: u64,
}

impl<'a> DlobOrderQuoter<'a> {
    /// A DLOB resting order is never fee-exempt. It pays or receives maker
    /// fees under the protocol schedule. The vAMM is the exempt one, because
    /// it earns from the spread it quotes.
    pub fn is_fee_exempt(&self) -> bool {
        false
    }

    /// DLOB resting orders settle on the match fee schedule.
    pub fn fee_policy(&self) -> FillFeePolicy {
        FillFeePolicy::DlobMatch
    }

    pub fn new(order: &'a mut Order, max_fill: u64) -> Self {
        DlobOrderQuoter { order, max_fill }
    }

    /// Fill up to `size` at this order's price and apply it to the order. That
    /// is everything a router allocation does to a resting maker. Returns
    /// [`QuoterFill::ZERO`] when the order does not quote this side or has
    /// nothing left.
    pub fn fill(
        &mut self,
        ctx: &QuoteContext,
        taker_side: PositionDirection,
        size: u64,
    ) -> VelocityResult<QuoterFill> {
        let fill = self
            .solo_fill(ctx, taker_side, size)?
            .unwrap_or(QuoterFill::ZERO);
        if fill.base_filled > 0 {
            self.apply_fill(&fill)?;
        }
        Ok(fill)
    }

    /// Apply a fill to the order's own counters. Position and fee accounting
    /// belong to the fill controller.
    pub fn apply_fill(&mut self, fill: &QuoterFill) -> VelocityResult {
        self.order.base_asset_amount_filled = self
            .order
            .base_asset_amount_filled
            .safe_add(fill.base_filled)?;
        self.order.quote_asset_amount_filled = self
            .order
            .quote_asset_amount_filled
            .safe_add(fill.quote_filled)?;
        Ok(())
    }

    /// Sentinel "doesn't quote on this side" price.
    fn no_quote(side: PositionDirection) -> u64 {
        match side {
            PositionDirection::Long => u64::MAX,
            PositionDirection::Short => 0,
        }
    }

    /// Whether this order is on the maker side opposite the given taker side.
    fn quotes_on(&self, taker_side: PositionDirection) -> bool {
        self.order.direction != taker_side
    }

    /// Effective limit price for this order at the current matcher context.
    /// Returns `None` if the order has no usable price (e.g. an unanchored
    /// market order with no fallback).
    fn effective_price(&self, ctx: &QuoteContext) -> VelocityResult<Option<u64>> {
        // Slot 0 + valid_oracle_price = ctx.oracle.price; this drives
        // get_limit_price's auction / oracle-offset handling.
        self.order.get_limit_price(
            Some(ctx.oracle.price),
            None,
            ctx.slot,
            ctx.tick.max(1),
            ctx.slot_clock,
        )
    }

    fn remaining(&self) -> u64 {
        self.order
            .base_asset_amount
            .saturating_sub(self.order.base_asset_amount_filled)
            .min(self.max_fill)
    }
}

impl<'a> DlobOrderQuoter<'a> {
    pub fn best_price(&self, ctx: &QuoteContext, side: PositionDirection) -> VelocityResult<u64> {
        if !self.quotes_on(side) {
            return Ok(Self::no_quote(side));
        }
        Ok(self.effective_price(ctx)?.unwrap_or(Self::no_quote(side)))
    }

    pub fn level_capacity(
        &self,
        ctx: &QuoteContext,
        side: PositionDirection,
    ) -> VelocityResult<u64> {
        if !self.quotes_on(side) || self.effective_price(ctx)?.is_none() {
            return Ok(0);
        }
        Ok(self.remaining())
    }

    pub fn try_fill_solo(
        &self,
        ctx: &QuoteContext,
        side: PositionDirection,
        target_size: u64,
    ) -> VelocityResult<Option<QuoterFill>> {
        self.solo_fill(ctx, side, target_size)
    }
}

impl DlobOrderQuoter<'_> {
    /// Closed-form fill of `target_size` at this order's effective limit
    /// price. It takes `min(target, remaining)` and rounds the quote in the
    /// maker's favor. Returns `None` when the order does not quote this side.
    fn solo_fill(
        &self,
        ctx: &QuoteContext,
        side: PositionDirection,
        target_size: u64,
    ) -> VelocityResult<Option<QuoterFill>> {
        if !self.quotes_on(side) {
            return Ok(None);
        }
        let limit_price = match self.effective_price(ctx)? {
            Some(p) => p,
            None => return Ok(None),
        };
        let base = target_size.min(self.remaining());
        // Precision-correct quote = base * price / base_precision, rounded in
        // the maker's favor (ceiling for Short maker, floor for Long maker).
        // Matches `calculate_quote_asset_amount_for_maker_order`, which the
        // legacy match path used. Plain floor division understates the price
        // on the Short side and fails `validate_fill_price` against the
        // maker's limit.
        let bp = ctx.base_precision.max(1);
        let base_u128 = base as u128;
        let price_u128 = limit_price as u128;
        let bp_u128 = bp as u128;
        let quote_u128 = match self.order.direction {
            PositionDirection::Long => base_u128.safe_mul(price_u128)?.safe_div(bp_u128)?,
            PositionDirection::Short => base_u128.safe_mul(price_u128)?.safe_div_ceil(bp_u128)?,
        };
        if quote_u128 > u64::MAX as u128 {
            return Err(ErrorCode::MathError);
        }
        let quote = quote_u128 as u64;
        Ok(Some(QuoterFill {
            side,
            base_filled: base,
            quote_filled: quote,
            clearing_price: limit_price,
            refresh_cost: 0,
            is_fee_exempt: self.is_fee_exempt(),
            fee_policy: self.fee_policy(),
            // DLOB orders quote a single price; no spread surplus.
            quote_asset_amount_surplus: 0,
        }))
    }
}

impl<'a> DlobOrderQuoter<'a> {
    pub fn commit_fill(&mut self, _ctx: &QuoteContext, fill: &QuoterFill) -> VelocityResult<()> {
        // Update only the order's bytes. Per-user position / fee accounting
        // is the fill controller's responsibility, applied after the match
        // resolves using the public `QuoterFill` fields.
        self.order.base_asset_amount_filled = self
            .order
            .base_asset_amount_filled
            .safe_add(fill.base_filled)?;
        self.order.quote_asset_amount_filled = self
            .order
            .quote_asset_amount_filled
            .safe_add(fill.quote_filled)?;
        Ok(())
    }
}

// ============================================================================
// AMM Quoter impl (v1)
// ============================================================================
//
// `AmmQuoter` exposes the existing `AMM` struct as a router quoter so the
// matcher can drive it. It reports `is_prio = true` and `is_fee_exempt =
// true`. The AMM fills ahead of the DLOB at a tied price and pays no maker
// fee.
//
// **Design choice: repeg / k-update happens via `_update_amm`, not inside
// `try_fill_solo`.** The AmmQuoter quote reflects the AMM's CURRENT reserves
// + peg. Production callers (controller/repeg.rs::_update_amm, called from
// every fill path that reaches the matcher) apply repeg / k-update BEFORE
// matching runs, so by the time AmmQuoter is queried, the AMM is already at
// its "fresh" state.
//
// Folding the conditional triggers INTO `try_fill_solo` was considered and
// rejected for v1 because it would require:
//   1. Threading `State + MMOraclePriceData + slot` into QuoteContext (the
//      matcher would need access to oracle guard rails, hot/cold authority
//      flags, etc. that don't belong in a maker abstraction).
//   2. A shadow-mutate-then-rollback pattern in try_fill_solo (simulate the
//      repeg, quote against the simulated state) plus a re-commit in
//      commit_fill — the two must reach identical conclusions for the
//      matcher's bisection convergence to hold.
//   3. Threading `refresh_cost` back through `apply_clearing` (currently
//      surfaced only by the sole-maker fast path).
//
// The current pattern works correctly because every production matcher
// invocation runs through a code path that has already called `_update_amm`.
// If a future caller needs the matcher to fire without that precondition,
// re-open this design choice.
#[cfg(test)]
mod dlob_order_maker_tests {
    use {
        super::*,
        crate::state::user::{MarketType, Order, OrderStatus, OrderType},
    };

    fn make_ctx<'a>(stats: &'a MarketStats, oracle: &'a OraclePriceData) -> QuoteContext<'a> {
        QuoteContext {
            stats,
            oracle,
            mm_oracle: None,
            oracle_validity: None,
            fee_budget: 0,
            tick: 1,
            step_size: 1,
            slot: 100,
            slot_clock: SlotClock::baseline(),
            base_precision: crate::math::constants::BASE_PRECISION as u64,
            market_status: crate::state::market_status::MarketStatus::default(),
            market_config: 0,
        }
    }

    fn make_ask_order(price: u64, size: u64) -> Order {
        Order {
            slot: 0,
            price,
            base_asset_amount: size,
            base_asset_amount_filled: 0,
            quote_asset_amount_filled: 0,
            trigger_price: 0,
            auction_start_price: 0,
            auction_end_price: 0,
            max_ts: 0,
            oracle_price_offset: 0,
            order_id: 0,
            market_index: 0,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            user_order_id: 0,
            existing_position_direction: PositionDirection::Long,
            direction: PositionDirection::Short, // ask = sell
            reduce_only: false,
            post_only: true,
            immediate_or_cancel: false,
            trigger_condition: crate::state::user::OrderTriggerCondition::Above,
            auction_duration: 0,
            posted_slot_tail: 0,
            bit_flags: 0,
            padding: [0; 5],
        }
    }

    fn make_bid_order(price: u64, size: u64) -> Order {
        let mut o = make_ask_order(price, size);
        o.direction = PositionDirection::Long;
        o
    }

    #[test]
    fn ask_order_quotes_to_buying_taker() {
        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let ctx = make_ctx(&stats, &oracle);

        let mut order = make_ask_order(100, 50);
        let maker = DlobOrderQuoter::new(&mut order, u64::MAX);

        // Buying taker should see the ask price.
        let bp = maker.best_price(&ctx, PositionDirection::Long).unwrap();
        assert_eq!(bp, 100);

        // Selling taker should see no quote.
        let bp_no = maker.best_price(&ctx, PositionDirection::Short).unwrap();
        assert_eq!(bp_no, 0);
    }

    #[test]
    fn bid_order_quotes_to_selling_taker() {
        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let ctx = make_ctx(&stats, &oracle);

        let mut order = make_bid_order(95, 30);
        let maker = DlobOrderQuoter::new(&mut order, u64::MAX);

        let bp = maker.best_price(&ctx, PositionDirection::Short).unwrap();
        assert_eq!(bp, 95);

        let bp_no = maker.best_price(&ctx, PositionDirection::Long).unwrap();
        assert_eq!(bp_no, u64::MAX);
    }

    #[test]
    fn discrete_level_is_price_and_remaining() {
        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let ctx = make_ctx(&stats, &oracle);

        let mut order = make_ask_order(100, 50);
        let maker = DlobOrderQuoter::new(&mut order, u64::MAX);

        // The order presents as a single discrete level: price = the ask, and
        // level_capacity = full remaining size.
        assert_eq!(
            maker.best_price(&ctx, PositionDirection::Long).unwrap(),
            100
        );
        assert_eq!(
            maker.level_capacity(&ctx, PositionDirection::Long).unwrap(),
            50
        );
        // Doesn't quote the bid side.
        assert_eq!(maker.best_price(&ctx, PositionDirection::Short).unwrap(), 0);
        assert_eq!(
            maker
                .level_capacity(&ctx, PositionDirection::Short)
                .unwrap(),
            0
        );
    }

    #[test]
    fn max_fill_caps_capacity_and_solo_fill() {
        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let ctx = make_ctx(&stats, &oracle);

        // Reduce-only shape: order asks 60 but the maker position only
        // covers 40, so the caller passes 40 as the position-capped
        // unfilled. Both the advertised level and the solo fill must
        // respect it.
        let mut order = make_ask_order(100, 60);
        let maker = DlobOrderQuoter::new(&mut order, 40);

        assert_eq!(
            maker.level_capacity(&ctx, PositionDirection::Long).unwrap(),
            40
        );
        let fill = maker
            .try_fill_solo(&ctx, PositionDirection::Long, 60)
            .unwrap()
            .unwrap();
        assert_eq!(fill.base_filled, 40);

        // Cap above the order's own remaining changes nothing.
        let mut order = make_ask_order(100, 60);
        let maker = DlobOrderQuoter::new(&mut order, u64::MAX);
        assert_eq!(
            maker.level_capacity(&ctx, PositionDirection::Long).unwrap(),
            60
        );
    }

    #[test]
    fn commit_fill_increments_filled_counters() {
        let stats = MarketStats::default();
        let oracle = OraclePriceData::default();
        let ctx = make_ctx(&stats, &oracle);

        let mut order = make_ask_order(100, 50);
        {
            let mut maker = DlobOrderQuoter::new(&mut order, u64::MAX);
            let fill = QuoterFill {
                side: PositionDirection::Long,
                base_filled: 20,
                quote_filled: 2000,
                clearing_price: 100,
                refresh_cost: 0,
                is_fee_exempt: false,
                fee_policy: FillFeePolicy::DlobMatch,
                quote_asset_amount_surplus: 0,
            };
            maker.commit_fill(&ctx, &fill).unwrap();
        }
        assert_eq!(order.base_asset_amount_filled, 20);
        assert_eq!(order.quote_asset_amount_filled, 2000);

        // try_fill_solo after partial fill reflects reduced remaining.
        let maker = DlobOrderQuoter::new(&mut order, u64::MAX);
        let fill = maker
            .try_fill_solo(&ctx, PositionDirection::Long, 100)
            .unwrap()
            .unwrap();
        assert_eq!(fill.base_filled, 30); // 50 - 20 = 30 remaining
        assert_eq!(fill.clearing_price, 100);
    }
}

/// An owned snapshot of everything a [`QuoteContext`] needs from a
/// `PerpMarket`, so a caller can build the context once and hand out borrows.
///
/// Both quoting entry points need the same fields pulled off the market before
/// they can borrow its AMM mutably. Those entry points are the fill's
/// fulfillment pass and the router's quote view. Assembling the fields by hand
/// at each site lets the two drift apart.
pub struct MarketQuoteInputs {
    pub stats: MarketStats,
    pub safe_oracle: OraclePriceData,
    pub mm_oracle: MMOraclePriceData,
    pub oracle_validity: Option<crate::math::oracle::OracleValidity>,
    pub oracle_price: i64,
    pub tick_size: u64,
    pub step_size: u64,
    pub market_status: crate::state::market_status::MarketStatus,
    pub market_config: u8,
    pub sanitize_clamp_denominator: Option<i64>,
    pub slot_clock: SlotClock,
}

impl MarketQuoteInputs {
    /// Snapshot `market` for quoting at `slot`.
    pub fn load(
        market: &crate::state::perp_market::PerpMarket,
        oracle_price_data: OraclePriceData,
        slot: u64,
        validity_guard_rails: &crate::state::state::ValidityGuardRails,
        slot_clock: SlotClock,
    ) -> VelocityResult<Self> {
        let mm_oracle = market.get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            validity_guard_rails,
            slot_clock,
        )?;
        // Only `project_and_apply` reads `oracle_validity`, and it returns
        // early when the curve was already refreshed at this slot. The
        // router's own projection, an earlier fill, or a keeper crank can do
        // that refresh. In that common case the value is never read, so the
        // recompute is skipped. It is not cheap.
        let oracle_validity = if market.amm.last_update_slot < slot {
            crate::vlp::amm::refresh::compute_amm_refresh_validity_with_guard_rails(
                market,
                &mm_oracle,
                validity_guard_rails,
                slot,
                slot_clock,
            )?
        } else {
            None
        };
        Ok(MarketQuoteInputs {
            stats: market.market_stats,
            safe_oracle: mm_oracle.get_safe_oracle_price_data(),
            mm_oracle,
            oracle_validity,
            oracle_price: oracle_price_data.price,
            tick_size: market.order_tick_size,
            step_size: market.order_step_size,
            market_status: market.status,
            market_config: market.market_config,
            sanitize_clamp_denominator: market.get_sanitize_clamp_denominator()?,
            slot_clock,
        })
    }

    /// The context quoting reads from. It borrows the snapshot, so it stays
    /// valid while the caller holds the market's AMM mutably.
    pub fn ctx(&self, slot: u64) -> QuoteContext<'_> {
        QuoteContext {
            stats: &self.stats,
            oracle: &self.safe_oracle,
            mm_oracle: Some(&self.mm_oracle),
            oracle_validity: self.oracle_validity,
            fee_budget: 0,
            tick: self.tick_size,
            step_size: self.step_size,
            slot,
            slot_clock: self.slot_clock,
            base_precision: crate::math::constants::BASE_PRECISION_U64,
            market_status: self.market_status,
            market_config: self.market_config,
        }
    }
}
