//! The kill switches on `State`.
//!
//! Each handler sets or clears one bit. A clear bit makes the instructions that
//! read it fail with a typed error, so an operator can stop one subsystem
//! without an upgrade.

use super::*;

pub fn handle_update_feature_bit_flags_mm_oracle(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting first bit to 1, enabling mm oracle update");
        state.feature_bit_flags |= FeatureBitFlags::MmOracleUpdate as u8;
    } else {
        msg!("Setting first bit to 0, disabling mm oracle update");
        state.feature_bit_flags &= !(FeatureBitFlags::MmOracleUpdate as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_median_trigger_price(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting second bit to 1, enabling median trigger price");
        state.feature_bit_flags |= FeatureBitFlags::MedianTriggerPrice as u8;
    } else {
        msg!("Setting second bit to 0, disabling median trigger price");
        state.feature_bit_flags &= !(FeatureBitFlags::MedianTriggerPrice as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_builder_codes(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can enable feature bit flags"
        )?;

        msg!("Setting 3rd bit to 1, enabling builder codes");
        state.feature_bit_flags |= FeatureBitFlags::BuilderCodes as u8;
    } else {
        msg!("Setting 3rd bit to 0, disabling builder codes");
        state.feature_bit_flags &= !(FeatureBitFlags::BuilderCodes as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_vamm_maker_rebate(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can enable feature bit flags"
        )?;

        msg!("Setting 4th bit to 1, enabling vamm maker rebate");
        state.feature_bit_flags |= FeatureBitFlags::VammMakerRebate as u8;
    } else {
        msg!("Setting 4th bit to 0, disabling vamm maker rebate");
        state.feature_bit_flags &= !(FeatureBitFlags::VammMakerRebate as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_settle_lp_pool(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting first bit to 1, enabling settle LP pool");
        state.lp_pool_feature_bit_flags |= LpPoolFeatureBitFlags::SettleLpPool as u8;
    } else {
        msg!("Setting first bit to 0, disabling settle LP pool");
        state.lp_pool_feature_bit_flags &= !(LpPoolFeatureBitFlags::SettleLpPool as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_swap_lp_pool(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting second bit to 1, enabling swapping with LP pool");
        state.lp_pool_feature_bit_flags |= LpPoolFeatureBitFlags::SwapLpPool as u8;
    } else {
        msg!("Setting second bit to 0, disabling swapping with LP pool");
        state.lp_pool_feature_bit_flags &= !(LpPoolFeatureBitFlags::SwapLpPool as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_mint_redeem_lp_pool(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting third bit to 1, enabling minting and redeeming with LP pool");
        state.lp_pool_feature_bit_flags |= LpPoolFeatureBitFlags::MintRedeemLpPool as u8;
    } else {
        msg!("Setting third bit to 0, disabling minting and redeeming with LP pool");
        state.lp_pool_feature_bit_flags &= !(LpPoolFeatureBitFlags::MintRedeemLpPool as u8);
    }
    Ok(())
}
