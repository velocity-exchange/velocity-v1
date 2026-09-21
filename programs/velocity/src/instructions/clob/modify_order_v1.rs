//! `modify_order_v1`, which reprices or resizes a resting CLOB order.
//!
//! `modify_order` only reaches `User.orders`, so a maker whose order lives on
//! the book has no modify route through it. That maker must send
//! `cancel_order_v1` and `place_and_make_perp_order_v1` as two instructions,
//! which gives up queue position between them and leaves the maker flat if the
//! second one fails.
//!
//! This handler cancels and then replaces in one instruction. The CLOB has no
//! in-place mutation, and a modify is a new order at the back of its price
//! level either way. The single instruction buys atomicity and one margin gate
//! over the net change. The cancelled size is unwound from the open-order
//! aggregates before the replacement reserves its own, so a same-size reprice
//! never has to pass margin for double the exposure. Place-then-cancel does.
//!
//! The replacement leg follows `place_and_make_perp_order_v1`, with the same
//! margin gate, the same activation-delay attestation rule and the same wake
//! hints. The removal leg follows `cancel_order_v1` and is not gated on the
//! quoter entry's active and approved flags. The replacement leg is gated on
//! them, so on a killed book a modify fails and a cancel is the way out.
//!
//! A `None` field keeps the resting order's value, so a pure reprice does not
//! have to restate the size. The replacement's size is always the new total
//! rather than a delta, and it is measured against what the cancel returned. A
//! partially-filled order therefore modifies against its remaining size, never
//! against its original size.

use {
    crate::{
        controller::{
            self,
            position::{
                add_new_position, decrease_open_bids_and_asks, get_position_index,
                increase_open_bids_and_asks, PositionDirection,
            },
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
        math::{
            liquidation::validate_user_not_being_liquidated,
            margin::meets_place_order_margin_requirement, orders::is_new_order_risk_increasing,
        },
        msg,
        state::{
            market_status::MarketStatus,
            perp_market_map::{MarketSet, PerpMarketMap},
            prop_amm::{
                ClobCancelOrderArgsV0, ClobMarket, ClobOrderRefV0, ClobPlaceOrderArgsV0,
                ClobRemovedOrderV0, QuoterSlabExt, QuoterSlabV0, WireDirectionExt,
            },
            state::State,
            user::{Order, User},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: ModifyOrderV1Params)]
pub struct ModifyOrderV1<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    /// The market's quoter slab. The book's configuration is its `Clob` slot.
    /// The replacement leg also requires that slot to be active and
    /// approved.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == params.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The flow authority, signing this transaction as a named account.
    /// It is required only for an activation delay below the default on the
    /// replacement. The signature is the attestation. The zero key cannot sign,
    /// so an unset flow authority admits nobody.
    #[account(
        constraint = flow_authority.key()
            == state.load()?.hot_key(crate::state::state::HotRole::FlowAuthority)
            @ crate::error::ErrorCode::UnattestedFastActivation
    )]
    pub flow_authority: Option<Signer<'info>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct ModifyOrderV1Params {
    pub market_index: u16,
    /// Handle for the order being modified. The CLOB rejects a stale hint, and
    /// velocity fails the whole call if the removal took another user's
    /// order.
    pub order_ref: ClobOrderRefV0,
    /// `None` keeps the resting price.
    pub price: Option<u64>,
    /// `None` keeps the remaining size of the resting order, not its original
    /// size.
    pub base_asset_amount: Option<u64>,
    /// `None` keeps the resting expiry, which the CLOB's removal response
    /// reports. `Some(0)` makes the replacement good-till-cancelled.
    pub max_ts: Option<i64>,
    /// The rule of `place_and_make_perp_order_v1` applies. `None` takes the
    /// book's default speed bump. A value below it needs the flow-authority
    /// attestation.
    pub activation_delay_slots: Option<u32>,
    /// The rule of `place_and_make_perp_order_v1` applies: refuse rather than
    /// rest crossed. The original is already off the book, so a refused
    /// replacement leaves the maker with no order at all.
    pub reject_if_crossed: bool,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_modify_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, ModifyOrderV1<'info>>,
    params: ModifyOrderV1Params,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        &mut remaining_accounts,
        &MarketSet::new(),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let clob = bind_book_for_replacement(
        &ctx.accounts.quoter_slab,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        &maps.perp_market_map,
        params.market_index,
    )?;

    // The attestation rule belongs to the replacement rather than to the
    // original. A modify that asks for a bump below the default is a new fast
    // placement.
    crate::instructions::attest_activation_delay(
        &ctx.accounts.quoter_slab,
        params.market_index,
        params.activation_delay_slots,
        ctx.accounts.flow_authority.is_some(),
    )?;

    let user_ref = {
        let user = &mut load_mut!(ctx.accounts.user)?;
        validate_replacement_preconditions(&state, user, &mut maps)?;
        // Expired slot orders release their reservations, and that release
        // can be what lets the replacement pass the margin gate below. The
        // ephemeral placement path runs the same sweep before it builds an
        // order.
        controller::orders::expire_orders(
            user,
            &ctx.accounts.user.key(),
            &mut maps,
            clock.unix_timestamp,
            clock.slot,
        )?;

        user.clob_user_ref()
    };

    // Cancel first, so the margin gate below sees the net change.
    let removed = clob.cancel(ClobCancelOrderArgsV0 {
        order_ref: params.order_ref,
        user: user_ref,
        force: false,
    })?;

    validate!(
        removed.user == user_ref,
        ErrorCode::InvalidUserAccount,
        "clob cancelled an order for {}/{} instead of the passed user",
        removed.user.authority,
        removed.user.sub_account_id
    )?;

    let terms = resolve_replacement_terms(&params, &removed)?;

    // The replacement is a placement, so a reduce-only account may only carry
    // a reduce-only order. The replacement takes the removed order's flag, so
    // the check is possible only after the cancel reports it.
    if crate::load!(ctx.accounts.user)?.is_reduce_only() {
        validate!(
            removed.reduce_only,
            ErrorCode::UserReduceOnly,
            "order must be reduce only"
        )?;
    }

    let is_isolated_position = reserve_replacement_margin(
        &ctx.accounts.user,
        &mut maps,
        params.market_index,
        &terms,
        removed.base_asset_amount,
        clock.slot,
    )?;

    // Reduce-only CLOB orders are taker-origin by construction. Every path
    // that fills or removes one relies on that to keep the reduce-only
    // counter balanced, so a modify must preserve it.
    let taker_origin = removed.reduce_only;

    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side: removed.side,
        price: terms.price,
        base_asset_amount: terms.base_asset_amount,
        activation_delay_slots: params.activation_delay_slots,
        max_ts: terms.max_ts,
        user: user_ref,
        taker_origin,
        // The id stays the same, so a reprice reads as one order moved rather
        // than two orders. A placed trigger's shadow slot keeps the id it
        // armed under; a new id here would orphan the shadow.
        client_order_id: removed.client_order_id,
        reject_if_crossed: params.reject_if_crossed,
        // A modify keeps the order's reduce-only status. Otherwise the
        // replacement rests uncapped on a book that clamps only reduce-only
        // fills.
        reduce_only: removed.reduce_only,
    })?;

    restamp_placed_trigger_shadow(
        &ctx.accounts.user,
        params.market_index,
        removed.order_id,
        order_ref,
        &terms,
    )?;

    // One record rather than a cancel and a place. The order kept its id, so a
    // reader sees the same order at new terms.
    super::emit_clob_place_record(
        clock.unix_timestamp,
        &ctx.accounts.user.key(),
        super::ClobOrderFacts {
            order_id: removed.client_order_id,
            market_index: params.market_index,
            direction: removed.side.to_position_direction(),
            price: terms.price,
            base_asset_amount: terms.base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts: terms.max_ts,
            slot: clock.slot,
            taker_origin,
        },
        is_isolated_position,
    )?;

    msg!(
        "modified clob order {} into {} (node {}) for user {}",
        removed.order_id,
        order_ref.order_id,
        order_ref.node_index,
        ctx.accounts.user.key()
    );

    Ok(())
}

/// The account gates every placement passes before its order is built.
///
/// A modify builds no `Order`, so it never reaches the copy of these gates in
/// `create_ephemeral_perp_order`. Without them an account flagged as being
/// liquidated could reprice or upsize a resting book order while a liquidator
/// works on it.
fn validate_replacement_preconditions(
    state: &State,
    user: &mut User,
    maps: &mut AccountMaps,
) -> Result<()> {
    validate_user_not_being_liquidated(user, maps, state.liquidation_margin_buffer_ratio)?;
    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt"
    )?;
    validate!(
        user.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "user pool id ({}) != 0",
        user.pool_id
    )?;

    Ok(())
}

/// Bind the book the market's slab names, and check that the book and the
/// market both take new flow. The replacement leg is a placement, so it must
/// pass the gates a placement passes.
fn bind_book_for_replacement<'a, 'info>(
    quoter_slab: &'a AccountLoader<'info, QuoterSlabV0>,
    clob_market: &'a AccountInfo<'info>,
    clob_program: &'a AccountInfo<'info>,
    perp_market_map: &PerpMarketMap<'_>,
    market_index: u16,
) -> Result<ClobMarket<'a, 'info>> {
    let clob = {
        let slot = quoter_slab.clob_slot(market_index)?;
        // The replacement adds flow to the book, so it answers to the same
        // gate a fresh placement does.
        validate!(
            slot.quotes(),
            ErrorCode::ClobQuoterNotActive,
            "CLOB quoter is not active and approved; cancel the order instead"
        )?;

        drop(slot);
        ClobMarket::from_slab(quoter_slab, market_index, clob_market, clob_program)?
    };

    validate!(
        matches!(
            perp_market_map.get_ref(&market_index)?.status,
            MarketStatus::Active
        ),
        ErrorCode::MarketPlaceOrderPaused,
        "market not active"
    )?;

    Ok(clob)
}

/// The terms the replacement order rests with. Each field is either the
/// parameter the caller sent or the value the removed order carried.
struct ReplacementTerms {
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    /// Carried from the cancelled order. The replacement cannot change it, and
    /// a reduce-only order never increases risk.
    reduce_only: bool,
}

/// Read the replacement's terms from the parameters and the removed order. A
/// `None` parameter keeps the removed order's value.
fn resolve_replacement_terms(
    params: &ModifyOrderV1Params,
    removed: &ClobRemovedOrderV0,
) -> Result<ReplacementTerms> {
    // The side is not modifiable. Turning a bid into an ask is a different
    // order and a different risk decision, so it goes through a cancel and a
    // place. Carrying the removed order's side also stops a stale hint from
    // putting the replacement on the wrong book side.
    let direction = removed.side.to_position_direction();
    let price = params.price.unwrap_or(removed.price);
    let base_asset_amount = params
        .base_asset_amount
        .unwrap_or(removed.base_asset_amount);
    // `None` keeps the expiry the order rested with. The removal response
    // reports it, which is the last moment it is knowable.
    let max_ts = params.max_ts.unwrap_or(removed.max_ts);
    validate!(
        base_asset_amount > 0 && price > 0,
        ErrorCode::InvalidOrder,
        "modify must leave a live order: price {} size {}",
        price,
        base_asset_amount
    )?;

    Ok(ReplacementTerms {
        direction,
        price,
        base_asset_amount,
        max_ts,
        reduce_only: removed.reduce_only,
    })
}

/// Re-reserve the aggregates net of the cancel, then gate margin the way a
/// placement does. Both legs run inside this transaction, so a failure unwinds
/// the cancel with it and the maker is never left flat.
///
/// Reports whether the position is isolated, for the place record. The
/// reservation keeps `open_orders` on the position across the whole modify, so
/// the replacement rests under the same margin regime the cancelled order
/// held.
#[allow(clippy::too_many_arguments)]
fn reserve_replacement_margin<'info>(
    user_loader: &AccountLoader<'info, User>,
    maps: &mut AccountMaps,
    market_index: u16,
    terms: &ReplacementTerms,
    cancelled_base_asset_amount: u64,
    slot: u64,
) -> Result<bool> {
    let mut user = load_mut!(user_loader)?;
    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt"
    )?;

    let position_index = get_position_index(&user.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
    decrease_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &terms.direction,
        cancelled_base_asset_amount,
        true,
    )?;

    // The same predicate every placement uses, read at the same point: the
    // position net of the cancel, before the replacement reserves. Counting
    // existing reservations lets an order that fits the bare position still
    // read as risk-increasing, which the margin type and equity floor rely on.
    let prospective = Order {
        direction: terms.direction,
        base_asset_amount: terms.base_asset_amount,
        reduce_only: terms.reduce_only,
        ..Order::default()
    };
    let position = &user.perp_positions[position_index];
    let risk_increasing = is_new_order_risk_increasing(
        &prospective,
        position.base_asset_amount,
        position.open_bids,
        position.open_asks,
    )?;

    increase_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &terms.direction,
        terms.base_asset_amount,
        true,
    )?;

    // One order leaves and one arrives, so the order count does not change.
    // Neither the position counter nor `User.open_orders` moves. A placed
    // trigger's shadow slot keeps its parameters; only its CLOB ref changes,
    // written by the re-stamp below.
    let isolated_market_index = (risk_increasing
        && user.perp_positions[position_index].is_isolated())
    .then_some(market_index);
    meets_place_order_margin_requirement(&user, maps, risk_increasing, isolated_market_index)?;
    user.update_last_active_slot(slot);
    Ok(user.perp_positions[position_index].is_isolated())
}

/// A placed trigger's shadow follows its live order to the new handle. Without
/// the re-stamp the shadow points at a dead node, and every later removal path
/// fails to find it. The size is restated as the replacement's total, with
/// `base_asset_amount_filled` cleared. That keeps the shadow's unfilled amount
/// equal to the live order's size, which is what an eviction re-arms on.
fn restamp_placed_trigger_shadow<'info>(
    user_loader: &AccountLoader<'info, User>,
    market_index: u16,
    cancelled_order_id: u64,
    order_ref: ClobOrderRefV0,
    terms: &ReplacementTerms,
) -> Result<()> {
    let mut user = load_mut!(user_loader)?;
    if let Some(index) = user.find_placed_trigger_slot(market_index, cancelled_order_id) {
        let order = &mut user.orders[index];
        order.set_clob_order_ref(order_ref.node_index, order_ref.order_id);
        order.base_asset_amount = terms.base_asset_amount;
        order.base_asset_amount_filled = 0;
        order.price = terms.price;
        order.max_ts = terms.max_ts;
    }

    Ok(())
}
