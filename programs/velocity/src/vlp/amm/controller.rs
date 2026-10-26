#[cfg(test)]
use crate::state::oracle::OraclePriceData;
#[cfg(test)]
use crate::state::perp_market::MarketConfigFlag;
use {
    crate::{
        controller::spot_balance::update_spot_balances,
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast, quote_asset::*, safe_math::SafeMath, spot_balance::get_token_amount,
        },
        msg,
        state::{
            market_status::MarketStatus,
            perp_market::{PerpMarket, AMM},
            spot_market::{SpotBalance, SpotBalanceType, SpotMarket},
        },
        validate,
        vlp::amm::{
            math::{amm, amm::calculate_quote_asset_amount_swapped},
            quoter::AmmContract,
        },
    },
    anchor_lang::prelude::*,
};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapDirection {
    Add,
    Remove,
}

/// The AMM's gain on a fill against the no-spread curve quote. It is negative when the quote
/// crossed the curve, which a nonzero reference price offset allows.
fn calculate_quote_asset_amount_surplus(
    curve_quote_asset_reserve_before: u128,
    curve_quote_asset_reserve_after: u128,
    direction: SwapDirection,
    peg_multiplier: u128,
    quote_asset_amount: u128,
) -> VelocityResult<i128> {
    let curve_quote_asset_amount = match direction {
        // `calculate_quote_asset_amount_swapped` adds one unit to the reserve change and one to
        // the amount when the taker removes base. The curve quote adds only the second.
        SwapDirection::Remove => reserve_to_asset_amount(
            curve_quote_asset_reserve_after.safe_sub(curve_quote_asset_reserve_before)?,
            peg_multiplier,
        )?
        .safe_add(1)?,
        SwapDirection::Add => reserve_to_asset_amount(
            curve_quote_asset_reserve_before.safe_sub(curve_quote_asset_reserve_after)?,
            peg_multiplier,
        )?,
    };

    match direction {
        // The taker buys base, so the AMM gains what the taker pays above the curve.
        SwapDirection::Remove => quote_asset_amount
            .cast::<i128>()?
            .safe_sub(curve_quote_asset_amount.cast()?),
        // The taker sells base, so the AMM gains what it pays below the curve.
        SwapDirection::Add => curve_quote_asset_amount
            .cast::<i128>()?
            .safe_sub(quote_asset_amount.cast()?),
    }
}

/// Output of an AMM swap. Named struct (not a tuple) so call sites
/// document which field is which — there's no excuse for
/// `let (_, _, q, s) = calculate_base_swap_output_with_spread(...)`.
#[derive(Debug, Clone, Copy)]
pub struct AmmSwapOutput {
    /// AMM base reserve after the swap.
    pub new_base_asset_reserve: u128,
    /// AMM quote reserve after the swap.
    pub new_quote_asset_reserve: u128,
    /// Quote amount the taker pays / receives, including the bid/ask spread.
    pub quote_asset_amount: u64,
    /// The AMM's gain on this fill against the no-spread curve quote. A reference price offset
    /// larger than the spread on one side moves that side's quote across the curve, and the
    /// value is then negative.
    pub quote_asset_amount_surplus: i64,
}

/// AMM swap math primitive. Used by [`crate::vlp::amm::AmmQuoter`] and
/// by parity tests. `pub(crate)` because no production code outside the
/// AMM/maker modules needs it — fills go through the matcher and
/// `AmmQuoter`.
///
/// Reads the spread-adjusted ask/bid reserves cached on the AMM (refreshed by
/// [`crate::vlp::amm::math::spread::update_amm_quote_state`] on each crank / fill
/// `setup`).
pub(crate) fn calculate_base_swap_output(
    amm: &AMM,
    base_asset_swap_amount: u64,
    direction: SwapDirection,
) -> VelocityResult<AmmSwapOutput> {
    // first do the swap with spread reserves to figure out how much base asset is acquired
    let (base_asset_reserve_with_spread, quote_asset_reserve_with_spread) = match direction {
        SwapDirection::Add => (amm.bid_base_asset_reserve, amm.bid_quote_asset_reserve),
        SwapDirection::Remove => (amm.ask_base_asset_reserve, amm.ask_quote_asset_reserve),
    };

    let (new_quote_asset_reserve_with_spread, _) = amm::calculate_swap_output(
        base_asset_swap_amount.cast()?,
        base_asset_reserve_with_spread,
        direction,
        amm.sqrt_k,
    )?;

    let quote_asset_amount = calculate_quote_asset_amount_swapped(
        quote_asset_reserve_with_spread,
        new_quote_asset_reserve_with_spread,
        direction,
        amm.peg_multiplier,
    )?;

    let (new_quote_asset_reserve, new_base_asset_reserve) = amm::calculate_swap_output(
        base_asset_swap_amount.cast()?,
        amm.base_asset_reserve,
        direction,
        amm.sqrt_k,
    )?;

    let quote_asset_amount_surplus = calculate_quote_asset_amount_surplus(
        amm.quote_asset_reserve,
        new_quote_asset_reserve,
        direction,
        amm.peg_multiplier,
        quote_asset_amount,
    )?;

    Ok(AmmSwapOutput {
        new_base_asset_reserve,
        new_quote_asset_reserve,
        quote_asset_amount: quote_asset_amount.cast::<u64>()?,
        quote_asset_amount_surplus: quote_asset_amount_surplus.cast::<i64>()?,
    })
}

// The legacy `update_spreads` / `update_spread_reserves` mutators are now a
// single [`crate::vlp::amm::math::spread::update_amm_quote_state`] that refreshes
// all the cached spread fields on the AMM in one pass. Callers that need a
// spread / spread-reserve view read the cached fields directly off the AMM.

// AMM-mutating helpers (`update_concentration_coef`, `move_price`, `recenter`)
// live as methods on `AMM` in `crate::vlp::amm::state`. Callers go through those.

/// Test-only convenience wrapper that exercises the AMM's eager k-update
/// path directly, bypassing the FundingUpdated event dispatch (which
/// would also do the AMM-as-user settle from cum-rate deltas — orthogonal
/// to what these tests cover). Production code goes through the event
/// path (see `controller/funding.rs::update_funding_rate`).
#[cfg(test)]
pub fn formulaic_update_k(
    market: &mut PerpMarket,
    oracle_price_data: &OraclePriceData,
    funding_imbalance_cost: i128,
    now: i64,
) -> VelocityResult {
    use crate::vlp::amm::AmmQuoter;

    let k_update_eligible =
        !market.has_market_config_flag(MarketConfigFlag::DisableFormulaicKUpdate);
    if !k_update_eligible {
        return Ok(());
    }
    let market_status = market.status;
    let min_order_size = market.market_stats.min_order_size;
    let long_spread = market.amm.long_spread;
    let short_spread = market.amm.short_spread;
    let market_index = market.market_index;
    let mut amm_maker = AmmQuoter::for_amm(&mut market.amm);
    amm_maker.handle_funding_applied(
        funding_imbalance_cost,
        oracle_price_data,
        long_spread,
        short_spread,
        market_status,
        min_order_size,
        market_index,
        now,
    )?;
    Ok(())
}

pub fn get_fee_pool_tokens(amm: &AMM, spot_market: &SpotMarket) -> VelocityResult<i128> {
    get_token_amount(
        amm.fee_pool.balance(),
        spot_market,
        amm.fee_pool.balance_type(),
    )?
    .cast()
}

// Market-level pool accounting lives in `controller::perp_pools` — it reads
// AMM bookkeeping but the operations are protocol-level plumbing. Re-exported
// here so `use crate::vlp::amm::controller::*` star-imports still resolve them.
pub use crate::controller::perp_pools::{
    sweep_market_fees, update_pnl_pool_and_user_balance, update_pool_balances,
};
// `move_price` / `recenter` moved to `impl AMM` in `amm::state`.

// Cross-cutting AMM ↔ PerpMarket summary stat — lives in `math::perp_market`
// because it spans both. Re-exported here so star-imports still resolve.
pub use crate::math::perp_market::calculate_perp_market_amm_summary_stats;

/// The balances a fee pool withdrawal left behind, for the event and the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeePoolWithdrawal {
    pub fee_pool_before: u128,
    pub fee_pool_after: u128,
    pub protected_reserve: u128,
    pub total_fee_minus_distributions_after: i128,
    pub amm_equity_after: i128,
}

/// Debits exactly `amount` from the market's fee pool. Moves no tokens.
///
/// Fails unless the market is Active or ReduceOnly, before expiry, and has no pending
/// bankruptcy claim. After the debit the fee pool must still hold the tokenized bankruptcy
/// provision, stored and recomputed AMM equity must stay above zero, and the lower
/// `total_fee_minus_distributions` must not trip the drawdown breaker. The caller must
/// validate the oracle and accrue quote interest first.
pub fn withdraw_from_fee_pool(
    market: &mut PerpMarket,
    spot_market: &mut SpotMarket,
    amount: u64,
    oracle_price: i64,
    now: i64,
) -> VelocityResult<FeePoolWithdrawal> {
    validate!(
        amount > 0,
        ErrorCode::DefaultError,
        "amount must be positive"
    )?;

    validate!(
        matches!(
            market.status,
            MarketStatus::Active | MarketStatus::ReduceOnly
        ) && !market.is_in_settlement(now),
        ErrorCode::FeePoolWithdrawMarketNotEligible,
        "perp market {} status={:?} expiry_ts={} now={}",
        market.market_index,
        market.status,
        market.expiry_ts,
        now
    )?;

    validate!(
        !market.has_pending_bankruptcy_claim(),
        ErrorCode::FeePoolWithdrawMarketNotEligible,
        "perp market {} has {} pending bankruptcy claims",
        market.market_index,
        market.pending_bankruptcy_claims
    )?;

    validate!(
        market.amm.fee_pool.market_index == spot_market.market_index,
        ErrorCode::DefaultError,
        "fee_pool market mismatch: pool.market={} spot={}",
        market.amm.fee_pool.market_index,
        spot_market.market_index
    )?;

    let fee_pool_before = market.amm.fee_pool_token_amount(spot_market)?;

    // The bankruptcy clawback takes `pending_amm_provision` from the pnl pool first and only
    // the tokenized remainder from the fee pool, so the tokenized remainder must stay here.
    let tokenized_provision = market
        .fee_ledger
        .amm_protocol_fees_received
        .safe_sub(market.fee_ledger.pending_amm_provision)?;
    let protected_reserve = tokenized_provision.min(fee_pool_before);

    validate!(
        amount.cast::<u128>()? <= fee_pool_before,
        ErrorCode::FeePoolWithdrawInsufficientBalance,
        "amount {} exceeds fee pool {}",
        amount,
        fee_pool_before
    )?;

    update_spot_balances(
        amount.cast::<u128>()?,
        &SpotBalanceType::Borrow,
        spot_market,
        &mut market.amm.fee_pool,
        true,
    )?;
    market.amm.record_fee_pool_withdrawal(amount)?;

    // Check the balances the debit actually left, because the scaled debit rounds up.
    let fee_pool_after = market.amm.fee_pool_token_amount(spot_market)?;
    let total_fee_minus_distributions_after = market.amm.total_fee_minus_distributions;
    let amm_equity_after =
        calculate_perp_market_amm_summary_stats(market, spot_market, oracle_price)?;

    msg!(
        "perp market {} fee pool withdraw {}: fee_pool {} -> {}, protected_reserve {}, tfmd_after {}, amm_equity_after {}, oracle_price {}",
        market.market_index,
        amount,
        fee_pool_before,
        fee_pool_after,
        protected_reserve,
        total_fee_minus_distributions_after,
        amm_equity_after,
        oracle_price
    );

    validate!(
        fee_pool_after >= protected_reserve,
        ErrorCode::FeePoolWithdrawInsufficientBalance,
        "fee pool {} would fall below the bankruptcy reserve {}",
        fee_pool_after,
        protected_reserve
    )?;
    validate!(
        total_fee_minus_distributions_after > 0,
        ErrorCode::FeePoolWithdrawInsufficientBalance,
        "total_fee_minus_distributions would be {}",
        total_fee_minus_distributions_after
    )?;
    validate!(
        amm_equity_after > 0,
        ErrorCode::FeePoolWithdrawInsufficientBalance,
        "recomputed amm equity would be {}",
        amm_equity_after
    )?;
    validate!(
        !market.has_too_much_drawdown()?,
        ErrorCode::FeePoolWithdrawInsufficientBalance,
        "perp market {} drawdown breaker would trip at total_fee_minus_distributions {}",
        market.market_index,
        total_fee_minus_distributions_after
    )?;

    Ok(FeePoolWithdrawal {
        fee_pool_before,
        fee_pool_after,
        protected_reserve,
        total_fee_minus_distributions_after,
        amm_equity_after,
    })
}
