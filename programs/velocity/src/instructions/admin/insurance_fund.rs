//! The insurance fund of a market, and the revenue that feeds it.
//!
//! Each handler moves one setting: the unstaking period, the lending-gain
//! carveouts, the settle period, the paused operations, and the ceilings on how
//! much a perp market may claim.

use super::*;

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_max_imbalances(
    ctx: Context<AdminUpdatePerpMarket>,
    unrealized_max_imbalance: u64,
    max_revenue_withdraw_per_period: u64,
    quote_max_insurance: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating perp market {} max imbalances",
        perp_market.market_index
    );

    validate_perp_insurance_maxes(
        perp_market,
        max_revenue_withdraw_per_period,
        unrealized_max_imbalance,
        quote_max_insurance,
    )?;

    msg!(
        "market.max_revenue_withdraw_per_period: {:?} -> {:?}",
        perp_market.insurance_claim.max_revenue_withdraw_per_period,
        max_revenue_withdraw_per_period
    );

    msg!(
        "market.unrealized_max_imbalance: {:?} -> {:?}",
        perp_market.unrealized_pnl_max_imbalance,
        unrealized_max_imbalance
    );

    msg!(
        "market.quote_max_insurance: {:?} -> {:?}",
        perp_market.insurance_claim.quote_max_insurance,
        quote_max_insurance
    );

    perp_market.insurance_claim.max_revenue_withdraw_per_period = max_revenue_withdraw_per_period;
    perp_market.unrealized_pnl_max_imbalance = unrealized_max_imbalance;
    perp_market.insurance_claim.quote_max_insurance = quote_max_insurance;

    // ensure altered max_revenue_withdraw_per_period doesn't break invariant check
    crate::validation::perp_market::validate_perp_market(perp_market)?;

    Ok(())
}

/// The largest insurance claim a market of each tier may hold. A riskier tier
/// gets a smaller claim on the fund.
fn max_insurance_for_tier(contract_tier: ContractTier) -> u64 {
    match contract_tier {
        ContractTier::A => INSURANCE_A_MAX,
        ContractTier::B => INSURANCE_B_MAX,
        ContractTier::C => INSURANCE_C_MAX,
        ContractTier::Speculative => INSURANCE_SPECULATIVE_MAX,
        ContractTier::HighlySpeculative => INSURANCE_SPECULATIVE_MAX,
        ContractTier::Isolated => INSURANCE_SPECULATIVE_MAX,
    }
}

/// Holds the three insurance ceilings to the market's tier, and to what the
/// market has already claimed. A ceiling below the settled claim would make the
/// market's own accounting invalid.
fn validate_perp_insurance_maxes(
    perp_market: &PerpMarket,
    max_revenue_withdraw_per_period: u64,
    unrealized_max_imbalance: u64,
    quote_max_insurance: u64,
) -> Result<()> {
    let max_insurance_for_tier = max_insurance_for_tier(perp_market.contract_tier);

    validate!(
        max_revenue_withdraw_per_period
            <= max_insurance_for_tier.max(FEE_POOL_TO_REVENUE_POOL_THRESHOLD.cast()?)
            && unrealized_max_imbalance <= max_insurance_for_tier + 1
            && quote_max_insurance <= max_insurance_for_tier,
        ErrorCode::DefaultError,
        "all maxs must be less than max_insurance for ContractTier ={}",
        max_insurance_for_tier
    )?;

    validate!(
        perp_market.insurance_claim.quote_settled_insurance <= quote_max_insurance,
        ErrorCode::DefaultError,
        "quote_max_insurance must be above market.insurance_claim.quote_settled_insurance={}",
        perp_market.insurance_claim.quote_settled_insurance
    )?;

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_insurance_fund_unstaking_period(
    ctx: Context<AdminUpdateSpotMarket>,
    insurance_fund_unstaking_period: i64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    msg!("updating spot market {} IF unstaking period");
    msg!(
        "spot_market.insurance_fund.unstaking_period: {:?} -> {:?}",
        spot_market.insurance_fund.unstaking_period,
        insurance_fund_unstaking_period
    );

    spot_market.insurance_fund.unstaking_period = insurance_fund_unstaking_period;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
/// Set the lending-gain carveouts: `if_fee_factor` (to the insurance fund) and
/// `protocol_fee_factor` (to the withdrawable protocol fee pool). Lenders receive
/// deposit interest net of both.
pub fn handle_update_spot_market_if_factor(
    ctx: Context<AdminUpdateSpotMarket>,
    spot_market_index: u16,
    if_fee_factor: u32,
    protocol_fee_factor: u32,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    msg!("spot market {}", spot_market.market_index);

    validate!(
        spot_market.market_index == spot_market_index,
        ErrorCode::DefaultError,
        "spot_market_index dne spot_market.index"
    )?;

    // The combined carveout stays below 100%, so lenders keep a configured share.
    // `split_deposit_interest` relies on this bound. It divides the deposit
    // interest by IF_FACTOR_PRECISION with the combined factor as the numerator.
    // A combined factor below IF_FACTOR_PRECISION keeps that quotient at or below
    // the interval gain, so the two cuts never take more than the market earned.
    //
    // The bound does not by itself keep the lender share above zero. A carried
    // remainder can raise the cuts to the whole gain on a short interval. The
    // accrual commits anyway in that case, so a zero lender share is safe.
    //
    // A lower pair can leave a carried remainder at or above the new combined
    // factor, which is the divisor of the insurance-fund-vs-protocol split.
    // `split_deposit_interest` reduces that remainder below the divisor in force, so
    // this handler does not have to settle or rescale it. The reduction costs less
    // than one index unit.
    validate!(
        if_fee_factor.safe_add(protocol_fee_factor)? < IF_FACTOR_PRECISION.cast()?,
        ErrorCode::DefaultError,
        "if_fee_factor + protocol_fee_factor must be < 100%"
    )?;

    msg!(
        "spot_market.if_fee_factor: {:?} -> {:?}",
        spot_market.insurance_fund.if_fee_factor,
        if_fee_factor
    );

    msg!(
        "spot_market.protocol_fee_factor: {:?} -> {:?}",
        spot_market.protocol_fee_factor,
        protocol_fee_factor
    );

    spot_market.insurance_fund.if_fee_factor = if_fee_factor;
    spot_market.protocol_fee_factor = protocol_fee_factor;

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_revenue_settle_period(
    ctx: Context<AdminUpdateSpotMarket>,
    revenue_settle_period: i64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate!(revenue_settle_period > 0, ErrorCode::DefaultError)?;
    msg!(
        "spot_market.revenue_settle_period: {:?} -> {:?}",
        spot_market.insurance_fund.revenue_settle_period,
        revenue_settle_period
    );
    spot_market.insurance_fund.revenue_settle_period = revenue_settle_period;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_if_paused_operations(
    ctx: Context<PauseAdminUpdateSpotMarket>,
    paused_operations: u8,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let signer = ctx.accounts.admin.key();
    let state = ctx.accounts.state.load()?;
    require_pause_only_added(
        &signer,
        &state,
        spot_market.if_paused_operations,
        paused_operations,
    )?;
    drop(state);
    spot_market.if_paused_operations = paused_operations;
    msg!("spot market {}", spot_market.market_index);
    InsuranceFundOperation::log_all_operations_paused(paused_operations);
    Ok(())
}
