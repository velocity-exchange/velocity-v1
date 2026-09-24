//! # Quoter interfaces for the shared orderbook
//!
//! Velocity matches across several liquidity sources. They are the vAMM and
//! external quoter programs such as the CLOB and PropAMMs, which velocity
//! reaches over CPI. Every source publishes discrete price levels and the
//! router splits a take across them.
//!
//! The interface is [`RouterQuoter`]. `quote` returns a source's book as
//! [`PriceLevelV0`]s. `execute` fills an allocation against the source and
//! settles the source's own bytes. A source is the sole authority on how its
//! bytes change, and the router only hands it the allocation it won.
//! `priority` places it in a tier. At a shared price, a lower priority number
//! fills first, and a tie inside one tier splits pro rata.
//!
//! One implementation lives in this program. `AmmQuoter` has its ladder built
//! by `vlp::amm::router_adapter::vamm_quote_levels`, so the continuous curve is
//! reduced to levels before the router sees it. An external quoter is not a
//! `RouterQuoter` implementation. It is quoted and executed over CPI
//! through [`crate::state::prop_amm::QuoterConfigV0`].
//!
//! [`QuoteContext`] is the snapshot every source quotes against. A quote
//! method reads from it, and `execute` reads it again, so a source reaches the
//! same conclusion at execute time as it did at quote time.
//!
//! The vAMM settles on the house fee schedule and an external quoter on the
//! match schedule. The vAMM is exempt from the maker schedule, because it
//! earns from the spread it quotes rather than from a rebate.
//!
//! See `docs/amm-decoupling-and-maker-interface.md` for the full design,
//! including pro-rata policy and snapshot consistency rules.

use crate::{
    controller::position::PositionDirection,
    error::VelocityResult,
    math::time::SlotClock,
    state::{
        oracle::{MMOraclePriceData, OraclePriceData},
        perp_market::MarketStats,
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
    /// `OraclePriceData` or that aren't invoking setup.
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
    /// Current slot. Used by order-backed makers to determine auction state
    /// (an order in active auction prices differently than the same order
    /// resting post-auction).
    pub slot: u64,
    /// Cluster slot clock, from `State::slot_clock()`. It scales the
    /// slot-denominated windows that are calibrated to the 400ms baseline,
    /// such as the reference price offset smoothing budget, across an IBRL
    /// transition.
    pub slot_clock: SlotClock,
    /// Base-asset precision divisor. `quote = base * price / base_precision`
    /// converts a raw base amount to QUOTE_PRECISION-scaled quote. `1e9`
    /// (BASE_PRECISION) for perps. An AMM maker computes quote through its
    /// own swap math instead and does not use this field.
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

/// The result of a single maker's portion of a match, built by the matcher
/// during segment-walk and pro-rata, or returned by a maker's solo-fill
/// path. Fields are protocol-level: the fill controller reads them without
/// maker interpretation. Maker-specific state changes happen inside
/// [`RouterQuoter::execute`].
#[derive(Debug, Clone, Copy)]
pub struct QuoterFill {
    /// Which side of the book this fill is on (from the taker's perspective).
    pub side: PositionDirection,
    /// Base asset amount this maker filled.
    pub base_filled: u64,
    /// Quote asset amount this maker filled.
    pub quote_filled: u64,
    /// Maker-specific quote surplus, or deficit if negative. For the AMM
    /// this is the gap between the spread-adjusted and no-spread swap
    /// result: the spread profit or loss on this fill. Zero for a maker
    /// order, which quotes a single price and has no spread to capture.
    pub quote_asset_amount_surplus: i64,
}

impl QuoterFill {
    pub const ZERO: QuoterFill = QuoterFill {
        side: PositionDirection::Long,
        base_filled: 0,
        quote_filled: 0,
        quote_asset_amount_surplus: 0,
    };
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

    /// Discrete best-first levels for a taker of `direction` and `size`.
    /// This is the in-program `quote_v0`. `rival_books` carries the books
    /// already built in this fill (external CPI books and worse-tier
    /// internal ones), letting a quoter run a last look. Most ignore it.
    fn quote(
        &self,
        ctx: &QuoteContext,
        direction: crate::state::prop_amm::DirectionV0,
        size: u64,
        rival_books: &[crate::math::router::QuoterBook],
    ) -> VelocityResult<Vec<crate::state::prop_amm::PriceLevelV0>>;

    /// Commit a fill of up to `size`, which is the routed allocation. This is
    /// the in-program `execute_v0`. It applies the quoter's own state changes
    /// and reports the fill. Taker-side settlement stays with the fill
    /// controller.
    fn execute(
        &mut self,
        ctx: &QuoteContext,
        direction: crate::state::prop_amm::DirectionV0,
        size: u64,
    ) -> VelocityResult<QuoterFill>;
}

// ============================================================================
// AMM Quoter impl (v1)
// ============================================================================
//
// `AmmQuoter` exposes the existing `AMM` struct as a router quoter so the
// matcher can drive it. It reports `is_prio = true` and `is_fee_exempt =
// true`. The AMM fills ahead of a maker book at a tied price and pays no maker
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

/// An owned snapshot of everything a [`QuoteContext`] needs from a
/// `PerpMarket`, so a caller can build the context once and hand out borrows.
///
/// The fill's fulfillment pass and the router's quote view both need the
/// same fields pulled off the market before borrowing its AMM mutably.
/// Assembling them by hand at each site lets the two drift apart.
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
