//! Moving a perp position to another subaccount, or back to the vAMM.
//!
//! Both handlers price the move at the oracle and record it as a fill between
//! the two sides. Neither may raise open interest, so a transfer can only pass
//! exposure along or retire it.

use super::*;

/// The market state a perp position transfer prices itself against.
struct TransferMarket {
    open_interest_before: u128,
    oracle_price: i64,
    step_size: u64,
    tick_size: u64,
}

/// What each side held before the transfer, as the records report it.
struct TransferExisting {
    from_quote_entry: Option<u64>,
    from_base: Option<u64>,
    to_quote_entry: Option<u64>,
    to_base: Option<u64>,
    to_user_direction: PositionDirection,
}

/// The synthetic fill a perp position transfer records. Both sides trade the
/// same base at the same price, in opposite directions.
struct TransferFill {
    market_index: u16,
    base_asset_amount: u64,
    base_asset_value: u64,
    price: i64,
    oracle_price: i64,
    direction_to_close: PositionDirection,
    slot: u64,
    ts: i64,
}

/// Prove the two accounts may transfer a perp position between them.
fn admit_perp_position_transfer(
    parties: &TransferParties<'_>,
    user_stats: &UserStats,
) -> Result<()> {
    validate!(
        !user_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "equity floor breaker is tripped for this authority"
    )?;

    validate!(
        !parties.to_user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "to_user bankrupt"
    )?;

    validate!(
        !parties.from_user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "from_user bankrupt"
    )?;

    validate!(
        parties.from_user_key != parties.to_user_key,
        ErrorCode::CantTransferBetweenSameUserAccount,
        "cant transfer between the same user account"
    )?;

    Ok(())
}

/// Read the market a transfer moves a position in, and prove it may take one.
///
/// Transfers are rejected once the market is expired or in settlement. The
/// transfer prices its deltas at the live oracle, but expired positions settle
/// at the market's fixed `expiry_price`. A post-expiry transfer would let an
/// authority split a live-oracle gain from the matching fixed-expiry loss
/// across two of its own subaccounts, leaving the source a positive zero-base
/// quote claim while the destination settles the base lower (OtterSec #87).
/// This mirrors the settlement gate the place, fill, and trigger paths enforce.
fn read_transfer_market(
    maps: &mut AccountMaps,
    market_index: u16,
    now: i64,
) -> Result<TransferMarket> {
    let perp_market = maps.perp_market_map.get_ref(&market_index)?;
    let open_interest_before = perp_market.get_open_interest();

    let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
        MarketType::Perp,
        market_index,
        &perp_market.oracle_id(),
        perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        perp_market.get_max_confidence_interval_multiplier()?,
        perp_market.oracle_slot_delay_override,
        perp_market.oracle_low_risk_slot_delay_override,
        Some(LogMode::Margin),
    )?;

    validate!(
        is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::MarginCalc))?,
        ErrorCode::InvalidTransferPerpPosition,
        "oracle is not valid for action"
    )?;

    validate!(
        !perp_market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::InvalidTransferPerpPosition,
        "perp market fills paused"
    )?;

    validate!(
        !perp_market.is_in_settlement(now),
        ErrorCode::InvalidTransferPerpPosition,
        "market is in settlement mode"
    )?;

    Ok(TransferMarket {
        open_interest_before,
        oracle_price: oracle_price_data.price,
        step_size: perp_market.order_step_size,
        tick_size: perp_market.order_tick_size,
    })
}

/// The base a transfer moves, and the direction that closes the source
/// position. A transfer may only reduce the source, so the amount shares its
/// sign and never exceeds it.
fn transfer_base_amount(
    from_user: &mut User,
    market_index: u16,
    amount: Option<i64>,
    step_size: u64,
) -> Result<(i64, PositionDirection)> {
    let position = from_user.force_get_perp_position_mut(market_index)?;
    let direction_to_close = position.get_direction_to_close();

    let Some(amount) = amount else {
        validate!(
            position.base_asset_amount != 0,
            ErrorCode::InvalidTransferPerpPosition,
            "from user has no position"
        )?;

        return Ok((position.base_asset_amount, direction_to_close));
    };

    let existing_base_asset_amount = position.base_asset_amount;

    validate!(
        amount.signum() == existing_base_asset_amount.signum(),
        ErrorCode::InvalidTransferPerpPosition,
        "transfer perp position must reduce position (direction is opposite)"
    )?;

    validate!(
        amount.abs() <= existing_base_asset_amount.abs(),
        ErrorCode::InvalidTransferPerpPosition,
        "transfer perp position amount is greater than existing position"
    )?;

    validate!(
        is_multiple_of_step_size(amount.unsigned_abs(), step_size)?,
        ErrorCode::InvalidTransferPerpPosition,
        "transfer perp position amount is not a multiple of step size"
    )?;

    Ok((amount, direction_to_close))
}

/// Price the transfer at the market's oracle and size the fill it records.
fn price_perp_transfer(
    market: &TransferMarket,
    market_index: u16,
    transfer_amount: i64,
    direction_to_close: PositionDirection,
    clock: &Clock,
) -> Result<TransferFill> {
    let price = standardize_price_i64(
        market.oracle_price,
        market.tick_size.cast()?,
        direction_to_close,
    )?;

    Ok(TransferFill {
        market_index,
        base_asset_amount: transfer_amount.unsigned_abs(),
        base_asset_value: calculate_base_asset_value_with_oracle_price(
            transfer_amount.cast::<i128>()?,
            price,
        )?
        .cast::<u64>()?,
        price,
        oracle_price: market.oracle_price,
        direction_to_close,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    })
}

/// Apply the transfer to both positions and to the market, and report what each
/// side held before it.
fn apply_perp_transfer(
    parties: &mut TransferParties<'_>,
    maps: &mut AccountMaps,
    fill: &TransferFill,
) -> Result<TransferExisting> {
    let from_delta = get_position_delta_for_fill(
        fill.base_asset_amount,
        fill.base_asset_value,
        fill.direction_to_close,
    )?;
    let to_delta = get_position_delta_for_fill(
        fill.base_asset_amount,
        fill.base_asset_value,
        fill.direction_to_close.opposite(),
    )?;

    let to_user_direction = parties
        .to_user
        .force_get_perp_position_mut(fill.market_index)
        .map(|position| position.get_direction())?;

    let mut market = maps.perp_market_map.get_ref_mut(&fill.market_index)?;

    let from_position = parties
        .from_user
        .force_get_perp_position_mut(fill.market_index)?;

    let (from_quote_entry, from_base) = calculate_existing_position_fields_for_order_action(
        fill.base_asset_amount,
        from_position.get_existing_position_params_for_order_action(fill.direction_to_close),
    )?;

    update_position_and_market(from_position, &mut market, &from_delta)?;

    let to_position = parties
        .to_user
        .force_get_perp_position_mut(fill.market_index)?;

    let (to_quote_entry, to_base) = calculate_existing_position_fields_for_order_action(
        fill.base_asset_amount,
        to_position
            .get_existing_position_params_for_order_action(fill.direction_to_close.opposite()),
    )?;

    update_position_and_market(to_position, &mut market, &to_delta)?;

    validate_perp_position_with_perp_market(from_position, &market)?;
    validate_perp_position_with_perp_market(to_position, &market)?;

    Ok(TransferExisting {
        from_quote_entry,
        from_base,
        to_quote_entry,
        to_base,
        to_user_direction,
    })
}

/// Prove both sides still stand after the transfer.
///
/// The source is held to maintenance margin, because it only shed exposure. The
/// recipient takes on risk-increasing exposure, so it is held to initial margin.
/// Both must also stay at or above their own admin-set equity floor: a recipient
/// that passes initial margin can still land below its warm-admin floor, which
/// would otherwise leave the floor unenforced.
fn validate_transfer_margin(
    parties: &mut TransferParties<'_>,
    maps: &mut AccountMaps,
) -> Result<()> {
    let from_margin = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.from_user,
        maps,
        MarginContext::standard(MarginRequirementType::Maintenance),
    )?;

    validate!(
        from_margin.meets_margin_requirement(),
        ErrorCode::InsufficientCollateral,
        "from user margin requirement is greater than total collateral"
    )?;

    if let Some(net_equity) = calculate_net_equity_for_floor(parties.from_user, maps)? {
        net_equity.validate_clears_buffered_floor(parties.from_user)?;
    }

    let to_margin = calculate_margin_requirement_and_total_collateral_and_liability_info(
        parties.to_user,
        maps,
        MarginContext::standard(MarginRequirementType::Initial),
    )?;

    validate!(
        to_margin.meets_margin_requirement(),
        ErrorCode::InsufficientCollateral,
        "to user margin requirement is greater than total collateral"
    )?;

    if let Some(net_equity) = calculate_net_equity_for_floor(parties.to_user, maps)? {
        net_equity.validate_clears_buffered_floor(parties.to_user)?;
    }

    Ok(())
}

/// Record the two synthetic orders the transfer crosses, and return their ids.
fn emit_transfer_orders(
    parties: &mut TransferParties<'_>,
    fill: &TransferFill,
    existing: &TransferExisting,
) -> Result<(u32, u32)> {
    let from_order_id = get_then_update_id!(parties.from_user, next_order_id);
    emit_stack::<_, { OrderRecord::SIZE }>(OrderRecord {
        ts: fill.ts,
        user: parties.from_user_key,
        order: Order {
            slot: fill.slot,
            base_asset_amount: fill.base_asset_amount,
            order_id: from_order_id,
            market_index: fill.market_index,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            price: fill.price.unsigned_abs(),
            direction: fill.direction_to_close,
            existing_position_direction: fill.direction_to_close.opposite(),
            ..Order::default()
        },
    })?;

    let to_order_id = get_then_update_id!(parties.to_user, next_order_id);
    emit_stack::<_, { OrderRecord::SIZE }>(OrderRecord {
        ts: fill.ts,
        user: parties.to_user_key,
        order: Order {
            slot: fill.slot,
            base_asset_amount: fill.base_asset_amount,
            order_id: to_order_id,
            market_index: fill.market_index,
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            price: fill.price.unsigned_abs(),
            direction: fill.direction_to_close.opposite(),
            existing_position_direction: existing.to_user_direction,
            ..Order::default()
        },
    })?;

    Ok((from_order_id, to_order_id))
}

/// Record the transfer as a fill. The recipient is the taker and the source is
/// the maker, because the recipient is the side that takes on exposure.
fn emit_transfer_fill(
    parties: &TransferParties<'_>,
    perp_market: &mut PerpMarket,
    fill: &TransferFill,
    existing: &TransferExisting,
    order_ids: (u32, u32),
) -> Result<()> {
    let (from_order_id, to_order_id) = order_ids;
    let fill_record_id = get_then_update_id!(perp_market, next_fill_record_id);

    Ok(emit_stack::<_, { OrderActionRecord::SIZE }>(
        OrderActionRecord {
            ts: fill.ts,
            action: OrderAction::Fill,
            action_explanation: OrderActionExplanation::TransferPerpPosition,
            market_index: fill.market_index,
            market_type: MarketType::Perp,
            filler: None,
            filler_reward: None,
            fill_record_id: Some(fill_record_id),
            base_asset_amount_filled: Some(fill.base_asset_amount),
            quote_asset_amount_filled: Some(fill.base_asset_value),
            taker_fee: None,
            maker_fee: None,
            referrer_reward: None,
            quote_asset_amount_surplus: None,
            spot_fulfillment_method_fee: None,
            taker: Some(parties.to_user_key),
            taker_order_id: Some(to_order_id),
            taker_order_direction: Some(fill.direction_to_close.opposite()),
            taker_order_base_asset_amount: Some(fill.base_asset_amount),
            taker_order_cumulative_base_asset_amount_filled: Some(fill.base_asset_amount),
            taker_order_cumulative_quote_asset_amount_filled: Some(fill.base_asset_value),
            maker: Some(parties.from_user_key),
            maker_order_id: Some(from_order_id),
            maker_order_direction: Some(fill.direction_to_close),
            maker_order_base_asset_amount: Some(fill.base_asset_amount),
            maker_order_cumulative_base_asset_amount_filled: Some(fill.base_asset_amount),
            maker_order_cumulative_quote_asset_amount_filled: Some(fill.base_asset_value),
            oracle_price: fill.oracle_price,
            bit_flags: 0,
            taker_existing_quote_entry_amount: existing.to_quote_entry,
            taker_existing_base_asset_amount: existing.to_base,
            maker_existing_quote_entry_amount: existing.from_quote_entry,
            maker_existing_base_asset_amount: existing.from_base,
            trigger_price: None,
            builder_idx: None,
            builder_fee: None,
        },
    )?)
}

/// Prove the transfer did not raise open interest. A transfer may only pass
/// exposure along or retire it.
fn validate_open_interest_did_not_grow(
    maps: &AccountMaps,
    market_index: u16,
    open_interest_before: u128,
    what: &str,
) -> Result<()> {
    let open_interest_after = maps
        .perp_market_map
        .get_ref(&market_index)?
        .get_open_interest();

    validate!(
        open_interest_after <= open_interest_before,
        ErrorCode::InvalidTransferPerpPosition,
        "open interest must not increase after {}. oi_before: {}, oi_after: {}",
        what,
        open_interest_before,
        open_interest_after
    )?;

    Ok(())
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_transfer_perp_position<'c: 'info, 'info>(
    ctx: Context<'info, TransferPerpPosition<'info>>,
    market_index: u16,
    amount: Option<i64>,
) -> anchor_lang::Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let mut to_user = load_mut!(ctx.accounts.to_user)?;
    let mut from_user = load_mut!(ctx.accounts.from_user)?;
    let user_stats = load!(ctx.accounts.user_stats)?;

    let parties = &mut TransferParties {
        from_user_key: ctx.accounts.from_user.key(),
        to_user_key: ctx.accounts.to_user.key(),
        from_user: &mut from_user,
        to_user: &mut to_user,
        signer: None,
    };

    admit_perp_position_transfer(parties, &user_stats)?;

    let mut maps = load_one_perp_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        market_index,
        clock.slot,
    )?;

    // No `update_amm` here: settle_funding_payment reads only the market's
    // stored `cumulative_funding_rate_long/short`, not AMM peg or reserves.
    // The funding-rate accumulators are updated by `update_funding_rate`;
    // refreshing the AMM here was cargo-cult.
    settle_funding_payment(
        parties.from_user,
        &parties.from_user_key,
        maps.perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;
    settle_funding_payment(
        parties.to_user,
        &parties.to_user_key,
        maps.perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    let market = read_transfer_market(&mut maps, market_index, now)?;

    let (transfer_amount, direction_to_close) =
        transfer_base_amount(parties.from_user, market_index, amount, market.step_size)?;

    let fill = price_perp_transfer(
        &market,
        market_index,
        transfer_amount,
        direction_to_close,
        &clock,
    )?;

    let existing = apply_perp_transfer(parties, &mut maps, &fill)?;

    validate_transfer_margin(parties, &mut maps)?;

    validate_open_interest_did_not_grow(
        &maps,
        market_index,
        market.open_interest_before,
        "transfer",
    )?;

    parties.from_user.update_last_active_slot(clock.slot);
    parties.to_user.update_last_active_slot(clock.slot);

    let order_ids = emit_transfer_orders(parties, &fill, &existing)?;

    let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;
    emit_transfer_fill(parties, &mut perp_market, &fill, &existing, order_ids)
}

/// Prove the account may hand a position back to the vAMM. Only the vAMM
/// hedger account may, and only while it is solvent and not in liquidation.
fn admit_vamm_hedger(user: &User) -> Result<()> {
    validate!(
        user.special_user_status == SpecialUserStatus::VammHedger as u8,
        ErrorCode::InvalidTransferPerpPosition,
        "user is not a special account user (vamm hedger)"
    )?;

    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt"
    )?;

    validate!(
        !user.is_being_liquidated(),
        ErrorCode::UserIsBeingLiquidated,
        "user is being liquidated"
    )?;

    Ok(())
}

/// Read the market a vAMM transfer prices itself against, and prove it may take
/// one.
fn read_vamm_transfer_market(maps: &mut AccountMaps, market_index: u16) -> Result<TransferMarket> {
    let perp_market = maps.perp_market_map.get_ref(&market_index)?;
    let open_interest_before = perp_market.get_open_interest();

    validate!(
        !perp_market.is_operation_paused(PerpOperation::Fill),
        ErrorCode::InvalidTransferPerpPosition,
        "perp market fills paused"
    )?;

    let (oracle_price_data, oracle_validity) = maps.oracle_map.get_price_data_and_validity(
        MarketType::Perp,
        market_index,
        &perp_market.oracle_id(),
        perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        perp_market.get_max_confidence_interval_multiplier()?,
        perp_market.oracle_slot_delay_override,
        perp_market.oracle_low_risk_slot_delay_override,
        Some(LogMode::Margin),
    )?;

    validate!(
        is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::MarginCalc))?,
        ErrorCode::InvalidTransferPerpPosition,
        "oracle is not valid for action"
    )?;

    Ok(TransferMarket {
        open_interest_before,
        oracle_price: oracle_price_data.price,
        step_size: perp_market.order_step_size,
        tick_size: perp_market.order_tick_size,
    })
}

/// The base a vAMM transfer moves, and the direction that closes the position.
/// The position must face the vAMM's own inventory, and may not exceed it, so
/// the transfer can only retire exposure the vAMM already carries.
fn vamm_transfer_base_amount(
    user: &mut User,
    maps: &AccountMaps,
    market_index: u16,
    amount: Option<i64>,
    step_size: u64,
) -> Result<(i64, PositionDirection)> {
    let position = user.force_get_perp_position_mut(market_index)?;

    validate!(
        position.base_asset_amount != 0,
        ErrorCode::InvalidTransferPerpPosition,
        "user has no position in market"
    )?;

    let market = maps.perp_market_map.get_ref(&market_index)?;

    validate!(
        position.base_asset_amount.signum()
            == market
                .amm
                .base_asset_amount_with_amm
                .cast::<i64>()?
                .signum(),
        ErrorCode::InvalidTransferPerpPosition,
        "user position must be opposite of vamm's inventory"
    )?;

    let direction_to_close = position.get_direction_to_close();

    let transfer_amount = if let Some(amount) = amount {
        validate!(
            amount.signum() == position.base_asset_amount.signum(),
            ErrorCode::InvalidTransferPerpPosition,
            "amount direction must match position direction"
        )?;

        validate!(
            amount.abs() <= position.base_asset_amount.abs(),
            ErrorCode::InvalidTransferPerpPosition,
            "amount exceeds position size"
        )?;

        validate!(
            is_multiple_of_step_size(amount.unsigned_abs(), step_size)?,
            ErrorCode::InvalidTransferPerpPosition,
            "amount is not a multiple of step size"
        )?;

        amount
    } else {
        position.base_asset_amount
    };

    validate!(
        transfer_amount.unsigned_abs()
            <= market
                .amm
                .base_asset_amount_with_amm
                .cast::<i64>()?
                .unsigned_abs(),
        ErrorCode::InvalidTransferPerpPosition,
        "transfer amount exceeds amm exposure"
    )?;

    Ok((transfer_amount, direction_to_close))
}

/// Price the transfer at the market's oracle and size the position change it
/// applies to both sides.
fn price_vamm_transfer(
    market: &TransferMarket,
    transfer_amount: i64,
    direction_to_close: PositionDirection,
) -> Result<PositionDelta> {
    let transfer_price = standardize_price_i64(
        market.oracle_price,
        market.tick_size.cast()?,
        direction_to_close,
    )?;

    let base_asset_value = calculate_base_asset_value_with_oracle_price(
        transfer_amount.cast::<i128>()?,
        transfer_price,
    )?
    .cast::<u64>()?;

    Ok(get_position_delta_for_fill(
        transfer_amount.unsigned_abs(),
        base_asset_value,
        direction_to_close,
    )?)
}

/// Move the base off the user and onto the vAMM as its settlement
/// counterparty.
fn apply_vamm_transfer(
    user: &mut User,
    maps: &mut AccountMaps,
    market_index: u16,
    position_delta: &PositionDelta,
) -> Result<()> {
    let mut market = maps.perp_market_map.get_ref_mut(&market_index)?;
    let position = user.force_get_perp_position_mut(market_index)?;

    update_position_and_market(position, &mut market, position_delta)?;

    <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::apply_settlement_counterparty(
        &mut market.amm,
        position_delta.base_asset_amount.cast()?,
    )?;

    validate!(
        market.amm.net_counterparty_position().unsigned_abs() <= MAX_BASE_ASSET_AMOUNT_WITH_AMM,
        ErrorCode::InvalidAmmDetected,
        "base_asset_amount_with_amm exceeds max"
    )?;

    // Spread reserves are cached on the AMM, refreshed by
    // `crate::vlp::amm::math::spread::update_amm_quote_state` on each crank/fill.

    Ok(())
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_special_transfer_perp_position_to_vamm<'c: 'info, 'info>(
    ctx: Context<'info, SpecialTransferPerpPositionToVamm<'info>>,
    market_index: u16,
    amount: Option<i64>,
) -> Result<()> {
    let user_key = ctx.accounts.user.key();
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let mut user = load_mut!(ctx.accounts.user)?;

    admit_vamm_hedger(&user)?;

    let mut maps = load_one_perp_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        market_index,
        clock.slot,
    )?;

    // No `update_amm` here: settle_funding_payment reads only stored
    // cumulative funding rates. Same rationale as transfer_perp_position.
    settle_funding_payment(
        &mut user,
        &user_key,
        maps.perp_market_map.get_ref_mut(&market_index)?.deref_mut(),
        now,
    )?;

    let market = read_vamm_transfer_market(&mut maps, market_index)?;

    let (transfer_amount, direction_to_close) =
        vamm_transfer_base_amount(&mut user, &maps, market_index, amount, market.step_size)?;

    let position_delta = price_vamm_transfer(&market, transfer_amount, direction_to_close)?;

    apply_vamm_transfer(&mut user, &mut maps, market_index, &position_delta)?;

    let user_margin_calculation =
        calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &mut maps,
            MarginContext::standard(MarginRequirementType::Maintenance),
        )?;

    validate!(
        user_margin_calculation.meets_margin_requirement(),
        ErrorCode::InsufficientCollateral,
        "user margin requirement is greater than total collateral"
    )?;

    validate_open_interest_did_not_grow(
        &maps,
        market_index,
        market.open_interest_before,
        "special transfer",
    )?;

    user.update_last_active_slot(clock.slot);

    msg!(
        "user {:?} transferred {} base to vamm in market {}",
        user.authority,
        transfer_amount,
        market_index
    );

    Ok(())
}

#[derive(Accounts)]
pub struct TransferPerpPosition<'info> {
    #[account(
        mut,
        constraint = can_sign_for_user(&from_user, &authority)? && is_stats_for_user(&from_user, &user_stats)?
    )]
    pub from_user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = can_sign_for_user(&to_user, &authority)? && is_stats_for_user(&to_user, &user_stats)?
    )]
    pub to_user: AccountLoader<'info, User>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct SpecialTransferPerpPositionToVamm<'info> {
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    pub state: AccountLoader<'info, State>,
}
