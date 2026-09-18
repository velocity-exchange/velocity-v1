//! `place_trigger_orders_v1`, which arms trigger orders in the user's own slots.
//!
//! A `User.orders` slot holds one unfired conditional. The order is dormant. It
//! matches nothing and reserves no depth until the market reaches its trigger
//! price. A keeper then fires it through `trigger_market_order_v1` or
//! `trigger_limit_order_v1`, and the fired order reaches the market's book.
//!
//! A live order rests on the book instead of in a slot, so this endpoint
//! refuses every order type but `TriggerMarket` and `TriggerLimit`.
//!
//! The batch defers one margin check to the end. A stop loss and a take profit
//! arrive together, and a check after each one alone would admit the first
//! under a weaker threshold than the pair needs.

use super::*;

#[derive(Accounts)]
pub struct PlaceTriggerOrdersV1<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct PlaceTriggerOrdersV1Args {
    /// The triggers to arm. Each entry must be a `TriggerMarket` or a
    /// `TriggerLimit` on a perp market.
    pub params: Vec<OrderParams>,
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_trigger_orders_v1<'c: 'info, 'info>(
    ctx: Context<'info, PlaceTriggerOrdersV1>,
    args: PlaceTriggerOrdersV1Args,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    validate!(
        args.params.len() <= MAX_TRIGGERS_PER_CALL,
        ErrorCode::InvalidOrder,
        "a call arms at most {} triggers",
        MAX_TRIGGERS_PER_CALL
    )?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps = load_no_market_maps(&mut remaining_accounts, &state, clock.slot)?;

    let user_key = ctx.accounts.user.key();
    let mut user = load_mut!(ctx.accounts.user)?;

    // The escrow sits after the market and oracle accounts, so it is read once
    // and reused across the batch.
    let mut escrow = if state.builder_codes_enabled() {
        get_revenue_share_escrow_account(&mut remaining_accounts, &user.authority)?
    } else {
        None
    };

    let results = {
        let placement = &mut BatchPlacement {
            state: &state,
            user: &mut user,
            user_key,
            maps: &mut maps,
            escrow: &mut escrow,
        };

        arm_triggers(placement, &args.params, clock)?
    };

    enforce_batch_margin(&user, &mut maps, &results)
}

/// One `User.orders` slot per armed trigger, and the account holds 32 of them.
const MAX_TRIGGERS_PER_CALL: usize = 32;

/// The accounts one batch of triggers is armed against.
pub(super) struct BatchPlacement<'a, 'info> {
    pub state: &'a State,
    pub user: &'a mut User,
    pub user_key: Pubkey,
    pub maps: &'a mut AccountMaps<'info>,
    pub escrow: &'a mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
}

/// Arm every trigger of the batch, and report what risk each one introduced.
fn arm_triggers(
    placement: &mut BatchPlacement<'_, '_>,
    params: &[OrderParams],
    clock: &Clock,
) -> Result<Vec<PlaceOrderResult>> {
    params
        .iter()
        .enumerate()
        .map(|(index, params)| arm_trigger(placement, params, index == 0, clock))
        .collect()
}

/// Arm one trigger.
///
/// The order type is checked first. A live order rests on the book, so nothing
/// but a conditional ever reaches a slot, and the refusal names that rule
/// rather than the margin gate the order would have failed later.
fn arm_trigger(
    placement: &mut BatchPlacement<'_, '_>,
    params: &OrderParams,
    try_expire_orders: bool,
    clock: &Clock,
) -> Result<PlaceOrderResult> {
    validate!(
        params.is_trigger_order(),
        ErrorCode::OrderTypeNotConditional,
        "a live order rests on the market's book, not in a user order slot"
    )?;

    validate!(
        params.market_type == MarketType::Perp,
        ErrorCode::InvalidOrder,
        "only a perp market arms a trigger"
    )?;

    validate!(
        !params.is_immediate_or_cancel(),
        ErrorCode::InvalidOrderIOC,
        "a trigger order cannot be immediate or cancel"
    )?;

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

    // The margin check runs once over the whole batch. `try_expire_orders`
    // fires on the first entry alone, because one sweep clears every expired
    // order the account holds.
    let options = PlaceOrderOptions {
        signed_msg_taker_order_slot: None,
        enforce_margin_check: false,
        try_expire_orders,
        risk_increasing: false,
        explanation: OrderActionExplanation::None,
        existing_position_direction_override: None,
        emit_place_record: true,
    };

    Ok(controller::orders::place_perp_order(
        placement.state,
        placement.user,
        placement.user_key,
        placement.maps,
        clock,
        *params,
        options,
        &mut builder_order,
    )?)
}

/// One post-batch margin check, accumulating risk across the whole batch, so it
/// still runs when the final entry was a no-op. It mirrors what arming each
/// trigger on its own would have enforced:
///   - nothing armed               -> nothing to check
///   - armed, none increasing risk -> a single maintenance check
///   - some risk-increasing        -> initial margin in each risk scope
pub(super) fn enforce_batch_margin(
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

    risk_scopes
        .iter()
        .try_for_each(|&isolated_market_index| {
            meets_place_order_margin_requirement(user, maps, true, isolated_market_index)
        })
        .map_err(Into::into)
}
