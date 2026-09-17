//! Cancelling and modifying an order that a user holds in their own slots.
//!
//! Every handler here delegates to `controller::orders`, which owns the slot
//! and the margin gate. What lives here is the account plumbing: the market and
//! oracle maps.
//!
//! A slot holds an unfired trigger. `place_trigger_orders_v1` arms one, and the
//! handlers here cancel or amend it. A live order rests on the market's book,
//! and `cancel_order_v1` / `modify_order_v1` act on that.

use super::*;

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
