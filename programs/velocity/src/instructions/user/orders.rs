//! Placing, cancelling, and modifying an order.
//!
//! Every handler here delegates to `controller::orders`, which owns the book
//! and the margin gate. What lives here is the account plumbing: the market and
//! oracle maps, the builder escrow, and the batch that defers one margin check
//! to the end.

use super::*;

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_perp_order<'c: 'info, 'info>(
    ctx: Context<'info, PlaceOrder>,
    params: OrderParams,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps = load_no_market_maps(&mut remaining_accounts, &state, clock.slot)?;

    if params.is_immediate_or_cancel() {
        msg!("immediate_or_cancel order must be in place_and_make or place_and_take");
        return Err(print_error!(ErrorCode::InvalidOrderIOC)().into());
    }

    let user_key = ctx.accounts.user.key();
    let mut user = load_mut!(ctx.accounts.user)?;

    let escrow = if state.builder_codes_enabled() {
        get_revenue_share_escrow_account(&mut remaining_accounts, &user.authority)?
    } else {
        None
    };
    let (mut escrow, builder_fee_bps) = validate_and_load_builder(
        escrow,
        &user.authority,
        params.builder_idx,
        params.builder_fee_tenth_bps,
        &state,
    )?;
    let mut builder_order = add_builder_order(
        &mut escrow,
        &user,
        params.builder_idx,
        builder_fee_bps,
        user.next_order_id,
        params.market_index,
    )?;

    controller::orders::place_perp_order(
        &*ctx.accounts.state.load()?,
        &mut user,
        user_key,
        &mut maps,
        clock,
        params,
        PlaceOrderOptions::default(),
        &mut builder_order,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_cancel_order<'c: 'info, 'info>(
    ctx: Context<'info, CancelOrder>,
    order_id: Option<u32>,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_no_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        clock.slot,
    )?;

    let order_id = match order_id {
        Some(order_id) => order_id,
        None => load!(ctx.accounts.user)?.get_last_order_id(),
    };

    controller::orders::cancel_order_by_order_id(order_id, &ctx.accounts.user, &mut maps, clock)?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_cancel_order_by_user_id<'c: 'info, 'info>(
    ctx: Context<'info, CancelOrder>,
    user_order_id: u8,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_no_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        clock.slot,
    )?;

    controller::orders::cancel_order_by_user_order_id(
        user_order_id,
        &ctx.accounts.user,
        &mut maps,
        clock,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_cancel_orders_by_ids<'c: 'info, 'info>(
    ctx: Context<'info, CancelOrder>,
    order_ids: Vec<u32>,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_no_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        clock.slot,
    )?;

    for order_id in order_ids {
        controller::orders::cancel_order_by_order_id(
            order_id,
            &ctx.accounts.user,
            &mut maps,
            clock,
        )?;
    }

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_cancel_orders<'c: 'info, 'info>(
    ctx: Context<'info, CancelOrder<'info>>,
    market_type: Option<MarketType>,
    market_index: Option<u16>,
    direction: Option<PositionDirection>,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_no_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        clock.slot,
    )?;

    let user_key = ctx.accounts.user.key();
    let mut user = load_mut!(ctx.accounts.user)?;

    cancel_orders(
        &mut user,
        &user_key,
        None,
        &mut maps,
        clock.unix_timestamp,
        clock.slot,
        OrderActionExplanation::None,
        market_type,
        market_index,
        direction,
        false,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_modify_order<'c: 'info, 'info>(
    ctx: Context<'info, CancelOrder<'info>>,
    order_id: Option<u32>,
    modify_order_params: ModifyOrderParams,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_no_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        clock.slot,
    )?;

    let order_id = match order_id {
        Some(order_id) => order_id,
        None => load!(ctx.accounts.user)?.get_last_order_id(),
    };

    controller::orders::modify_order(
        ModifyOrderId::OrderId(order_id),
        modify_order_params,
        &ctx.accounts.user,
        &state,
        &mut maps,
        clock,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_modify_order_by_user_order_id<'c: 'info, 'info>(
    ctx: Context<'info, CancelOrder<'info>>,
    user_order_id: u8,
    modify_order_params: ModifyOrderParams,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_no_market_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &state,
        clock.slot,
    )?;

    controller::orders::modify_order(
        ModifyOrderId::UserOrderId(user_order_id),
        modify_order_params,
        &ctx.accounts.user,
        &state,
        &mut maps,
        clock,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_orders<'c: 'info, 'info>(
    ctx: Context<'info, PlaceOrder>,
    params: Vec<OrderParams>,
) -> Result<()> {
    place_orders(&ctx, PlaceOrdersInput::Orders(params))
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_scale_orders<'c: 'info, 'info>(
    ctx: Context<'info, PlaceOrder>,
    params: ScaleOrderParams,
) -> Result<()> {
    place_orders(&ctx, PlaceOrdersInput::ScaleOrders(params))
}

/// Input for place_orders - either direct OrderParams or ScaleOrderParams to expand
enum PlaceOrdersInput {
    Orders(Vec<OrderParams>),
    ScaleOrders(ScaleOrderParams),
}

/// The accounts one batch of orders is placed against.
struct BatchPlacement<'a, 'info> {
    state: &'a State,
    user: &'a mut User,
    user_key: Pubkey,
    maps: &'a mut AccountMaps<'info>,
    escrow: &'a mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
}

/// Expand a scale order into the ladder it stands for. Direct params pass
/// through unchanged.
fn expand_place_orders_input(
    input: PlaceOrdersInput,
    maps: &AccountMaps,
) -> Result<Vec<OrderParams>> {
    let scale_params = match input {
        PlaceOrdersInput::Orders(params) => return Ok(params),
        PlaceOrdersInput::ScaleOrders(scale_params) => scale_params,
    };

    let order_step_size = match scale_params.market_type {
        MarketType::Perp => {
            let market = maps.perp_market_map.get_ref(&scale_params.market_index)?;
            market.order_step_size
        }
        MarketType::Spot => {
            let market = maps.spot_market_map.get_ref(&scale_params.market_index)?;
            market.order_step_size
        }
    };

    scale_params
        .expand_to_order_params(order_step_size)
        .map_err(|e| {
            msg!("Failed to expand scale order params: {:?}", e);
            ErrorCode::InvalidOrder.into()
        })
}

/// Place one order of a batch. Returns `None` for an entry this instruction
/// does not place.
fn place_batch_order(
    placement: &mut BatchPlacement<'_, '_>,
    params: &OrderParams,
    options: PlaceOrderOptions,
    clock: &Clock,
) -> Result<Option<PlaceOrderResult>> {
    validate!(
        !params.is_immediate_or_cancel(),
        ErrorCode::InvalidOrderIOC,
        "immediate_or_cancel order must be in place_and_make or place_and_take"
    )?;

    validate_spot_dlob_trading_enabled_for_market_type(params.market_type)?;

    if params.market_type != MarketType::Perp {
        return Ok(None);
    }

    let builder_fee_bps = validate_builder_fee(
        placement.escrow.as_mut(),
        &placement.user.authority,
        params.builder_idx,
        params.builder_fee_tenth_bps,
        placement.state,
    )?;
    let next_order_id = placement.user.next_order_id;
    let mut builder_order = add_builder_order(
        placement.escrow,
        placement.user,
        params.builder_idx,
        builder_fee_bps,
        next_order_id,
        params.market_index,
    )?;

    Ok(Some(controller::orders::place_perp_order(
        placement.state,
        placement.user,
        placement.user_key,
        placement.maps,
        clock,
        *params,
        options,
        &mut builder_order,
    )?))
}

/// One post-batch margin check, accumulating risk across the whole batch, so it
/// still runs when the final order was a no-op. It mirrors what placing each
/// order individually would have enforced:
///   - no perp orders placed        -> nothing to check
///   - perp orders, none increasing -> a single maintenance check
///   - some risk-increasing         -> initial margin in each risk scope
fn enforce_batch_margin(
    user: &User,
    maps: &mut AccountMaps,
    results: &[PlaceOrderResult],
) -> Result<()> {
    if results.is_empty() {
        return Ok(());
    }

    // The distinct scopes any order increased risk in: `None` = cross margin,
    // `Some(market_index)` = that isolated market. A `BTreeSet` dedupes them for
    // free, so each scope is checked exactly once.
    let risk_scopes: BTreeSet<Option<u16>> = results
        .iter()
        .filter(|result| result.risk_increasing)
        .map(|result| result.isolated_market_index)
        .collect();

    if risk_scopes.is_empty() {
        meets_place_order_margin_requirement(user, maps, false, None)?;
        return Ok(());
    }

    risk_scopes.iter().try_for_each(|&isolated_market_index| {
        meets_place_order_margin_requirement(user, maps, true, isolated_market_index)
    })?;

    Ok(())
}

/// Internal implementation for placing multiple orders.
/// Used by both handle_place_orders and handle_place_scale_orders.
fn place_orders<'c: 'info, 'info>(
    ctx: &Context<'info, PlaceOrder>,
    input: PlaceOrdersInput,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps = load_no_market_maps(&mut remaining_accounts, &state, clock.slot)?;

    let order_params = expand_place_orders_input(input, &maps)?;

    validate!(
        order_params.len() <= 32,
        ErrorCode::DefaultError,
        "max 32 order params"
    )?;

    let user_key = ctx.accounts.user.key();
    let mut user = load_mut!(ctx.accounts.user)?;

    // Load the RevenueShareEscrow once (it lives after the market/oracle accounts in
    // remaining_accounts) so it can be reused across every order in the batch.
    let mut escrow = if state.builder_codes_enabled() {
        get_revenue_share_escrow_account(&mut remaining_accounts, &user.authority)?
    } else {
        None
    };

    // Place every order, deferring the margin check to a single post-batch pass.
    // Each `place_perp_order` only mutates the book and reports back what risk it
    // introduced; checking per-order — or only on the last order with a fresh
    // `risk_increasing == false` — would let an early risk-increasing order be
    // admitted under a weaker (maintenance) threshold, and a final no-op order
    // (expired / `TryPostOnly` that couldn't post) would skip the check entirely.
    let mut results: Vec<PlaceOrderResult> = Vec::new();
    {
        let placement = &mut BatchPlacement {
            state: &state,
            user: &mut user,
            user_key,
            maps: &mut maps,
            escrow: &mut escrow,
        };

        for (i, params) in order_params.iter().enumerate() {
            let options = PlaceOrderOptions {
                signed_msg_taker_order_slot: None,
                enforce_margin_check: false, // checked once, after the batch
                try_expire_orders: i == 0,   // expire once, on the first order
                risk_increasing: false,
                explanation: OrderActionExplanation::None,
                existing_position_direction_override: None,
                emit_place_record: true,
            };

            if let Some(result) = place_batch_order(placement, params, options, clock)? {
                results.push(result);
            }
        }
    }

    enforce_batch_margin(&user, &mut maps, &results)
}

#[derive(Accounts)]
pub struct PlaceOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct CancelOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
}
