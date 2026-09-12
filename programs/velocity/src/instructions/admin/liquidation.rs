//! What a liquidation costs and how fast it runs.
//!
//! The per-market handlers set the fees a liquidator and the insurance fund
//! take. The exchange-wide handlers set the speed of a liquidation, the margin
//! buffer that starts one, and what the protocol pays to get one cranked.

use super::*;

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_liquidation_fee(
    ctx: Context<AdminUpdatePerpMarket>,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    protocol_liquidation_fee: u32,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating perp market {} liquidation fee",
        perp_market.market_index
    );

    validate!(
        liquidator_fee
            .safe_add(if_liquidation_fee)?
            .safe_add(protocol_liquidation_fee)?
            < LIQUIDATION_FEE_PRECISION,
        ErrorCode::DefaultError,
        "Total liquidation fee must be less than 100%"
    )?;

    validate!(
        if_liquidation_fee < LIQUIDATION_FEE_PRECISION,
        ErrorCode::DefaultError,
        "If liquidation fee must be less than 100%"
    )?;

    validate!(
        protocol_liquidation_fee <= LIQUIDATION_FEE_PRECISION / 10,
        ErrorCode::DefaultError,
        "protocol_liquidation_fee must be <= 10%"
    )?;

    perp_market.amm.validate_compatible_with_liquidation_fee(
        perp_market.margin_ratio_initial,
        perp_market.margin_ratio_maintenance,
        liquidator_fee,
        if_liquidation_fee,
    )?;

    msg!(
        "perp_market.liquidator_fee: {:?} -> {:?}",
        perp_market.liquidator_fee,
        liquidator_fee
    );

    msg!(
        "perp_market.if_liquidation_fee: {:?} -> {:?}",
        perp_market.if_liquidation_fee,
        if_liquidation_fee
    );

    msg!(
        "perp_market.protocol_liquidation_fee: {:?} -> {:?}",
        perp_market.protocol_liquidation_fee,
        protocol_liquidation_fee
    );

    perp_market.liquidator_fee = liquidator_fee;
    perp_market.if_liquidation_fee = if_liquidation_fee;
    perp_market.protocol_liquidation_fee = protocol_liquidation_fee;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_liquidation_fee(
    ctx: Context<AdminUpdateSpotMarket>,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    protocol_liquidation_fee: u32,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!(
        "updating spot market {} liquidation fee",
        spot_market.market_index
    );

    validate!(
        liquidator_fee
            .safe_add(if_liquidation_fee)?
            .safe_add(protocol_liquidation_fee)?
            < LIQUIDATION_FEE_PRECISION,
        ErrorCode::DefaultError,
        "Total liquidation fee must be less than 100%"
    )?;

    validate!(
        if_liquidation_fee <= LIQUIDATION_FEE_PRECISION / 10,
        ErrorCode::DefaultError,
        "if_liquidation_fee must be <= 10%"
    )?;

    validate!(
        protocol_liquidation_fee <= LIQUIDATION_FEE_PRECISION / 10,
        ErrorCode::DefaultError,
        "protocol_liquidation_fee must be <= 10%"
    )?;

    msg!(
        "spot_market.liquidator_fee: {:?} -> {:?}",
        spot_market.liquidator_fee,
        liquidator_fee
    );

    msg!(
        "spot_market.if_liquidation_fee: {:?} -> {:?}",
        spot_market.if_liquidation_fee,
        if_liquidation_fee
    );

    msg!(
        "spot_market.protocol_liquidation_fee: {:?} -> {:?}",
        spot_market.protocol_liquidation_fee,
        protocol_liquidation_fee
    );

    spot_market.liquidator_fee = liquidator_fee;
    spot_market.if_liquidation_fee = if_liquidation_fee;
    spot_market.protocol_liquidation_fee = protocol_liquidation_fee;
    Ok(())
}

/// Set what the protocol will spend getting a liquidation cranked, and the
/// market whose oracle prices it.
///
/// A liquidation crank repays the priority fee its keeper paid, so the crank
/// stays worth landing when the fee market moves — and this bounds that at a
/// share of what the liquidation recovers, so a recovery too small to cover
/// its own gas is simply left. Both halves are needed: a share with no SOL
/// market has no way to turn quote into lamports, and a market with no share
/// spends nothing.
///
/// Zero in either field is a valid setting. It leaves the flat payment, which
/// is where every market starts.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateLiquidationCrankReimbursementArgs {
    /// The liquidator fee share paid back to a cranked liquidation's payer,
    /// in basis points of the fee.
    pub share_bps: u16,
    /// The SOL spot market the reimbursement is priced through.
    pub sol_spot_market_index: u16,
}

pub fn handle_update_liquidation_crank_reimbursement(
    ctx: Context<AdminUpdateState>,
    args: UpdateLiquidationCrankReimbursementArgs,
) -> Result<()> {
    let UpdateLiquidationCrankReimbursementArgs {
        share_bps,
        sol_spot_market_index,
    } = args;
    validate!(
        share_bps <= 10_000,
        ErrorCode::DefaultError,
        "a share of a liquidation cannot exceed the liquidation"
    )?;
    let mut state = ctx.accounts.state.load_mut()?;
    msg!(
        "liquidation crank reimbursement: {}bps market {} -> {}bps market {}",
        state.liquidation_crank_reimbursement_bps,
        state.sol_spot_market_index,
        share_bps,
        sol_spot_market_index
    );
    state.liquidation_crank_reimbursement_bps = share_bps;
    state.sol_spot_market_index = sol_spot_market_index;
    Ok(())
}

pub fn handle_update_initial_pct_to_liquidate(
    ctx: Context<AdminUpdateState>,
    initial_pct_to_liquidate: u16,
) -> Result<()> {
    msg!(
        "initial_pct_to_liquidate: {} -> {}",
        ctx.accounts.state.load()?.initial_pct_to_liquidate,
        initial_pct_to_liquidate
    );

    ctx.accounts.state.load_mut()?.initial_pct_to_liquidate = initial_pct_to_liquidate;
    Ok(())
}

pub fn handle_update_liquidation_duration(
    ctx: Context<AdminUpdateState>,
    liquidation_duration: u8,
) -> Result<()> {
    msg!(
        "liquidation_duration: {} -> {}",
        ctx.accounts.state.load()?.liquidation_duration,
        liquidation_duration
    );

    ctx.accounts.state.load_mut()?.liquidation_duration =
        legacy_slot_duration_u8(liquidation_duration);
    Ok(())
}

pub fn handle_update_liquidation_margin_buffer_ratio(
    ctx: Context<AdminUpdateState>,
    liquidation_margin_buffer_ratio: u32,
) -> Result<()> {
    msg!(
        "liquidation_margin_buffer_ratio: {} -> {}",
        ctx.accounts.state.load()?.liquidation_margin_buffer_ratio,
        liquidation_margin_buffer_ratio
    );

    ctx.accounts
        .state
        .load_mut()?
        .liquidation_margin_buffer_ratio = liquidation_margin_buffer_ratio;
    Ok(())
}
