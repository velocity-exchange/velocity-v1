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
//! over the net change. The replacement's reservation replaces the cancelled
//! order's before the gate runs, so a same-size reprice never has to pass
//! margin for double the exposure. Place-then-cancel does.
//!
//! The replacement leg follows `place_and_make_perp_order_v1`, with the same
//! margin gate, the same activation-delay rule and the same wake
//! hints. It also passes the order rules of a fresh placement: the market's
//! tick, step and minimum order size, the vAMM post-only check when it asks to
//! refuse a cross, and the open-interest cap when it increases risk. The removal leg follows `cancel_order_v1` and is not gated on the
//! quoter entry's active and approved flags. The replacement leg is gated on
//! them, so on a killed book a modify fails and a cancel is the way out.
//!
//! A `None` field keeps the resting order's value, so a pure reprice does not
//! have to restate the size. The replacement's size is always the new total
//! rather than a delta, and it is measured against what the cancel returned. A
//! partially-filled order therefore modifies against its remaining size, never
//! against its original size.
//!
//! The replacement takes a new book id. The owner's writable
//! `SignedMsgUserOrders` record may ride after the margin maps. A modified
//! signed-message remainder then keeps its route under the new id. Without the
//! record the replacement fills as unrouted.

use {
    crate::{
        controller::{
            self, orders::validate_open_interest_after_order, position::PositionDirection,
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
        math::{
            casting::Cast,
            liquidation::validate_user_not_being_liquidated,
            margin::meets_place_order_margin_requirement,
            orders::{is_new_order_risk_increasing, reduce_only_cover},
            safe_math::SafeMath,
        },
        msg,
        state::{
            market_status::MarketStatus,
            perp_market::PerpMarket,
            perp_market_map::{MarketSet, PerpMarketMap},
            prop_amm::{
                CancelOrderArgsV0, ClobMarket, ClobOrderRefV0, PlaceOrderArgsV0, QuoterSlabExt,
                QuoterSlabV0, RemovedOrderV0,
            },
            signed_msg_user::carried_signed_msg_record,
            state::State,
            user::{MarketType, Order, OrderReservation, OrderStatus, OrderType, User},
        },
        validate,
        validation::order::validate_order,
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
    /// book's default speed bump. A value below it is refused.
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
        clock.unix_timestamp,
    )?;

    // The replacement is a new placement with no flow attestation, so it
    // cannot ask for an activation delay below the book's default.
    crate::instructions::attest_activation_delay(
        &ctx.accounts.quoter_slab,
        params.market_index,
        params.activation_delay_slots,
        false,
    )?;

    let user_ref = {
        let user = &mut load_mut!(ctx.accounts.user)?;
        validate_replacement_preconditions(&state, user, &mut maps)?;
        // Expired slot orders release their reservations, and that release
        // can be what lets the replacement pass the margin gate below. The
        // detached placement path runs the same sweep before it builds an
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
    let removed = clob.cancel(CancelOrderArgsV0 {
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

    let position_base = crate::load!(ctx.accounts.user)?
        .get_perp_position(params.market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0);
    let terms = resolve_replacement_terms(&params, &removed, position_base)?;
    validate_replacement_order(&mut maps, &replacement_order(&params, &terms), clock.slot)?;

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
        &replacement_order(&params, &terms),
        removed.base_asset_amount,
        clock.slot,
    )?;

    let order_ref = clob.place(PlaceOrderArgsV0 {
        side: removed.side,
        price: terms.price,
        base_asset_amount: terms.base_asset_amount,
        activation_delay_slots: params.activation_delay_slots,
        max_ts: terms.max_ts,
        user: user_ref,
        taker_origin: terms.taker_origin,
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

    if terms.taker_origin {
        if let Some(mut record) =
            carried_signed_msg_record(remaining_accounts.next(), &user_ref.authority)
        {
            record.move_resting_route(params.market_index, removed.order_id, order_ref.order_id);
        }
    }

    // One record rather than a cancel and a place. The order kept its id, so a
    // reader sees the same order at new terms.
    super::emit_clob_place_record(
        clock.unix_timestamp,
        &ctx.accounts.user.key(),
        super::ClobOrderFacts {
            order_id: removed.client_order_id,
            market_index: params.market_index,
            direction: PositionDirection::from(removed.side),
            price: terms.price,
            base_asset_amount: terms.base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts: terms.max_ts,
            slot: clock.slot,
            taker_origin: terms.taker_origin,
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
/// `create_detached_perp_order`. Without them an account flagged as being
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
    now: i64,
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

    let market = perp_market_map.get_ref(&market_index)?;
    validate_market_takes_replacement(&market, now)?;
    Ok(clob)
}

/// A replacement adds flow, so the market must be active and not past its
/// expiry.
fn validate_market_takes_replacement(market: &PerpMarket, now: i64) -> Result<()> {
    validate!(
        matches!(market.status, MarketStatus::Active) && !market.is_in_settlement(now),
        ErrorCode::MarketPlaceOrderPaused,
        "market not active"
    )?;

    Ok(())
}

/// The terms the replacement order rests with. Each field is either the
/// parameter the caller sent or the value the removed order carried.
struct ReplacementTerms {
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    /// Carried from the cancelled order. The replacement cannot change it.
    reduce_only: bool,
    /// Carried from the cancelled order. The replacement cannot change it.
    taker_origin: bool,
}

/// Read the replacement's terms from the parameters and the removed order. A
/// `None` parameter keeps the removed order's value.
///
/// A reduce-only replacement rests at most the position it can reduce. The
/// margin gate exempts it, and that is sound only while its reservation fits
/// the position its fills are capped to.
fn resolve_replacement_terms(
    params: &ModifyOrderV1Params,
    removed: &RemovedOrderV0,
    position_base: i64,
) -> Result<ReplacementTerms> {
    // The side is not modifiable. Turning a bid into an ask is a different
    // order and a different risk decision, so it goes through a cancel and a
    // place. Carrying the removed order's side also stops a stale hint from
    // putting the replacement on the wrong book side.
    let direction = PositionDirection::from(removed.side);
    let price = params.price.unwrap_or(removed.price);
    let requested_base_asset_amount = params
        .base_asset_amount
        .unwrap_or(removed.base_asset_amount);
    let base_asset_amount = if removed.reduce_only {
        requested_base_asset_amount.min(reduce_only_cover(position_base, direction))
    } else {
        requested_base_asset_amount
    };

    // `None` keeps the expiry the order rested with. The removal response
    // reports it, which is the last moment it is knowable.
    let max_ts = params.max_ts.unwrap_or(removed.max_ts);
    validate!(
        base_asset_amount > 0 && price > 0,
        ErrorCode::InvalidOrder,
        "modify must leave a live order: price {} size {} position {}",
        price,
        base_asset_amount,
        position_base
    )?;

    Ok(ReplacementTerms {
        direction,
        price,
        base_asset_amount,
        max_ts,
        reduce_only: removed.reduce_only,
        taker_origin: removed.taker_origin,
    })
}

/// The replacement as the `Order` a fresh placement would build. A post-only
/// replacement is one that asks the book to refuse a cross. A taker remainder
/// is never post-only.
fn replacement_order(params: &ModifyOrderV1Params, terms: &ReplacementTerms) -> Order {
    Order {
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        market_index: params.market_index,
        direction: terms.direction,
        price: terms.price,
        base_asset_amount: terms.base_asset_amount,
        reduce_only: terms.reduce_only,
        post_only: params.reject_if_crossed && !terms.taker_origin,
        max_ts: terms.max_ts,
        ..Order::default()
    }
}

/// Hold the replacement to the order rules of a fresh placement. The book
/// checks only its own grid, and the market's grid can change after attach.
fn validate_replacement_order(maps: &mut AccountMaps, order: &Order, slot: u64) -> Result<()> {
    let market = maps.perp_market_map.get_ref(&order.market_index)?;
    let oracle_price = maps.oracle_map.get_price_data(&market.oracle_id())?.price;
    validate_replacement_against_market(&market, order, oracle_price, slot)
}

/// [`validate_replacement_order`] once the market and its oracle price are
/// read.
fn validate_replacement_against_market(
    market: &PerpMarket,
    order: &Order,
    oracle_price: i64,
    slot: u64,
) -> Result<()> {
    validate!(
        order.price.is_multiple_of(market.order_tick_size.max(1)),
        ErrorCode::InvalidOrderLimitPrice,
        "price {} is not a multiple of the market tick {}",
        order.price,
        market.order_tick_size
    )?;

    validate_order(order, market, Some(oracle_price), slot)?;
    Ok(())
}

/// Replace the cancelled order's reservation with the replacement's, then gate
/// margin and open interest the way a placement does. Both legs run inside
/// this transaction, so a failure unwinds the cancel with it and the maker is
/// never left flat.
///
/// Reports whether the position is isolated, for the place record. The
/// reservation keeps `open_orders` on the position across the whole modify, so
/// the replacement rests under the same margin regime the cancelled order
/// held.
fn reserve_replacement_margin<'info>(
    user_loader: &AccountLoader<'info, User>,
    maps: &mut AccountMaps,
    replacement_order: &Order,
    cancelled_base_asset_amount: u64,
    slot: u64,
) -> Result<bool> {
    let mut user = load_mut!(user_loader)?;
    let market_index = replacement_order.market_index;
    let removed = OrderReservation::book_order(
        market_index,
        replacement_order.direction,
        cancelled_base_asset_amount,
        replacement_order.reduce_only,
    );
    let replacement = OrderReservation::book_order(
        market_index,
        replacement_order.direction,
        replacement_order.base_asset_amount,
        replacement_order.reduce_only,
    );

    // The same predicate every placement uses, read at the same point: the
    // position net of the cancel, before the replacement reserves. Counting
    // existing reservations lets an order that fits the bare position still
    // read as risk-increasing, which the margin type and equity floor rely on.
    let position = user.get_perp_position(market_index)?;
    let risk_increasing = is_new_order_risk_increasing(
        replacement_order,
        position.base_asset_amount,
        position.open_bids.safe_sub(removed.open_bids.cast()?)?,
        position.open_asks.safe_add(removed.open_asks.cast()?)?,
    )?;

    // A placed trigger's shadow slot keeps its parameters. Only its CLOB ref
    // changes, written by the re-stamp below.
    let position_index = user.replace_reservation(&removed, &replacement)?;
    let isolated_market_index = (risk_increasing
        && user.perp_positions[position_index].is_isolated())
    .then_some(market_index);
    meets_place_order_margin_requirement(&user, maps, risk_increasing, isolated_market_index)?;
    let market = maps.perp_market_map.get_ref(&market_index)?;
    validate_open_interest_after_order(&market, replacement_order, risk_increasing)?;

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

#[cfg(test)]
mod resolve_replacement_terms_tests {
    use {
        super::{resolve_replacement_terms, ModifyOrderV1Params},
        crate::{
            controller::position::PositionDirection,
            state::prop_amm::{ClobOrderRefV0, RemovedOrderV0},
        },
        quoter_spec::{SideV0, UserRefV0},
    };

    fn removed(taker_origin: bool, reduce_only: bool) -> RemovedOrderV0 {
        RemovedOrderV0 {
            user: UserRefV0 {
                authority: Default::default(),
                sub_account_id: 0,
            },
            order_id: 1,
            client_order_id: 1,
            price: 100_000,
            base_asset_amount: 1_000,
            side: SideV0::Bid,
            taker_origin,
            reduce_only,
            max_ts: 0,
        }
    }

    fn params() -> ModifyOrderV1Params {
        ModifyOrderV1Params {
            market_index: 0,
            order_ref: ClobOrderRefV0 {
                node_index: 0,
                order_id: 1,
            },
            price: None,
            base_asset_amount: None,
            max_ts: None,
            activation_delay_slots: None,
            reject_if_crossed: false,
        }
    }

    #[test]
    fn a_maker_order_replaces_as_a_maker_order() {
        let terms = resolve_replacement_terms(&params(), &removed(false, false), 0).unwrap();
        assert!(!terms.taker_origin);
        assert!(!terms.reduce_only);
    }

    #[test]
    fn a_taker_remainder_replaces_as_a_taker_remainder() {
        let terms = resolve_replacement_terms(&params(), &removed(true, false), 0).unwrap();
        assert!(terms.taker_origin);
        assert!(!terms.reduce_only);
    }

    #[test]
    fn a_reduce_only_taker_remainder_replaces_as_a_reduce_only_taker_remainder() {
        // The removed order is a bid, so a short position is what it has left
        // to reduce.
        let terms = resolve_replacement_terms(&params(), &removed(true, true), -1_000).unwrap();
        assert!(terms.taker_origin);
        assert!(terms.reduce_only);
    }

    #[test]
    fn side_still_carries_the_removed_orders_direction() {
        let terms = resolve_replacement_terms(&params(), &removed(true, false), 0).unwrap();
        assert_eq!(terms.direction, PositionDirection::from(SideV0::Bid));
    }
}

#[cfg(test)]
mod replacement_rules_tests {
    use {
        super::{
            replacement_order, validate_market_takes_replacement,
            validate_replacement_against_market, ModifyOrderV1Params, ReplacementTerms,
        },
        crate::{
            controller::{orders::validate_open_interest_after_order, position::PositionDirection},
            error::ErrorCode,
            state::{
                market_status::MarketStatus,
                perp_market::{MarketStats, PerpMarket},
                prop_amm::ClobOrderRefV0,
            },
        },
    };

    fn market() -> PerpMarket {
        PerpMarket {
            status: MarketStatus::Active,
            order_step_size: 10,
            order_tick_size: 100,
            market_stats: MarketStats {
                min_order_size: 50,
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        }
    }

    fn params(reject_if_crossed: bool) -> ModifyOrderV1Params {
        ModifyOrderV1Params {
            market_index: 0,
            order_ref: ClobOrderRefV0 {
                node_index: 0,
                order_id: 1,
            },
            price: None,
            base_asset_amount: None,
            max_ts: None,
            activation_delay_slots: None,
            reject_if_crossed,
        }
    }

    fn terms(base_asset_amount: u64, price: u64, reduce_only: bool) -> ReplacementTerms {
        ReplacementTerms {
            direction: PositionDirection::Long,
            price,
            base_asset_amount,
            max_ts: 0,
            reduce_only,
            taker_origin: false,
        }
    }

    fn validate(base_asset_amount: u64, price: u64, reduce_only: bool) -> anchor_lang::Result<()> {
        let order = replacement_order(
            &params(false),
            &terms(base_asset_amount, price, reduce_only),
        );

        validate_replacement_against_market(&market(), &order, 1_000, 1)
    }

    #[test]
    fn a_replacement_on_the_market_grid_passes() {
        assert!(validate(50, 1_000, false).is_ok());
    }

    #[test]
    fn a_replacement_below_the_market_minimum_is_refused() {
        assert_eq!(
            validate(40, 1_000, false),
            Err(ErrorCode::InvalidOrderMinOrderSize.into())
        );
    }

    #[test]
    fn a_reduce_only_replacement_may_rest_below_the_market_minimum() {
        assert!(validate(40, 1_000, true).is_ok());
    }

    #[test]
    fn a_replacement_off_the_market_step_is_refused() {
        assert_eq!(
            validate(55, 1_000, false),
            Err(ErrorCode::InvalidOrderNotStepSizeMultiple.into())
        );
    }

    #[test]
    fn a_replacement_off_the_market_tick_is_refused() {
        assert_eq!(
            validate(50, 1_050, false),
            Err(ErrorCode::InvalidOrderLimitPrice.into())
        );
    }

    #[test]
    fn only_a_maker_that_refuses_a_cross_is_post_only() {
        assert!(replacement_order(&params(true), &terms(50, 1_000, false)).post_only);
        assert!(!replacement_order(&params(false), &terms(50, 1_000, false)).post_only);

        let taker_remainder = ReplacementTerms {
            taker_origin: true,
            ..terms(50, 1_000, false)
        };

        assert!(!replacement_order(&params(true), &taker_remainder).post_only);
    }

    #[test]
    fn a_risk_increasing_replacement_is_held_to_open_interest() {
        let market = PerpMarket {
            max_open_interest: 1_000,
            base_asset_amount_long: 995,
            ..market()
        };
        let order = replacement_order(&params(false), &terms(500, 1_000, false));

        assert_eq!(
            validate_open_interest_after_order(&market, &order, true),
            Err(ErrorCode::MaxOpenInterest)
        );
        assert_eq!(
            validate_open_interest_after_order(&market, &order, false),
            Ok(())
        );
    }

    #[test]
    fn an_expired_market_refuses_a_replacement() {
        let market = PerpMarket {
            expiry_ts: 100,
            ..market()
        };

        assert!(validate_market_takes_replacement(&market, 99).is_ok());
        assert_eq!(
            validate_market_takes_replacement(&market, 100),
            Err(ErrorCode::MarketPlaceOrderPaused.into())
        );
    }
}
