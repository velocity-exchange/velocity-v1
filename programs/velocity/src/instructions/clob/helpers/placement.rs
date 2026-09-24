//! Place a resting limit order on a registered CLOB.
//!
//! Velocity owns the placement policy. It checks the `User` authority, reserves
//! the order's worst-case open-order aggregates, and gates margin the way a
//! slot placement does. The CLOB trusts its `place_authority`, which is the
//! market's quoter slab PDA. The CLOB enforces only book-level rules: the tick,
//! the step, the minimum order size, the capacity, and the activation delay.

use {
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        instructions::optional_accounts::AccountMaps,
        load_mut,
        math::{margin::meets_place_order_margin_requirement, orders::is_order_position_reducing},
        msg,
        state::{
            prop_amm::{
                ClobMarket, ClobPlaceOrderArgsV0, QuoterSlabExt, QuoterSlabV0, SideV0, UserRefV0,
            },
            user::{OrderReservation, ReleaseCheck, User},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// The part of an order that can rest on the book.
pub struct RestableRemainder {
    pub direction: PositionDirection,
    pub price: u64,
    pub unfilled: u64,
    pub max_ts: i64,
    pub reduce_only: bool,
}

/// The remainder of `order` that can rest, or `None` when
/// [`restable_remainder_price`] refuses it. Every migrating route uses this one
/// derivation. An `unfilled` of zero is the caller's to skip.
pub fn restable_remainder(
    user: &User,
    order: &crate::state::user::Order,
    market_index: u16,
    // Only a fired trigger-market reads it.
    rest_oracle_price: Option<i64>,
) -> Option<RestableRemainder> {
    let price = restable_remainder_price(order, rest_oracle_price)?;
    let position_base = user
        .get_perp_position(market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0);
    Some(RestableRemainder {
        direction: order.direction,
        price,
        unfilled: order
            .get_base_asset_amount_unfilled(Some(position_base))
            .unwrap_or(0),
        max_ts: order.max_ts,
        reduce_only: order.reduce_only,
    })
}

/// The price a remainder can rest at, or `None` when it cannot rest. A market
/// or fired trigger-market rests at its worst price. That is safe because a
/// taker-origin remainder crosses only at the counterparty's price.
pub fn restable_remainder_price(
    order: &crate::state::user::Order,
    // The oracle an `OracleTriggerMarket` offset is relative to. Other order
    // types ignore it.
    oracle_price: Option<i64>,
) -> Option<u64> {
    use crate::state::user::{OrderBitFlag, OrderStatus, OrderType};
    if order.status != OrderStatus::Open || order.post_only {
        return None;
    }

    let price = match order.order_type {
        OrderType::Limit => order.price,
        // The worst price is the only price a market order has.
        OrderType::Market | OrderType::TriggerMarket => {
            if order.is_bit_flag_set(OrderBitFlag::OracleTriggerMarket) {
                // A short's offset is negative, so read as an absolute price it
                // would clamp to zero and drop the rest.
                oracle_price?
                    .checked_add(order.oracle_price_offset)?
                    .max(0)
                    .unsigned_abs()
            } else {
                order.price
            }
        }
        _ => return None,
    };

    (price != 0).then_some(price)
}

/// Snap a rest price to the book's `tick_size`. A fired trigger-market's
/// price is the oracle plus an arbitrary offset, and the book rejects a
/// non-tick price with `PriceNotTickAligned`. Rounding favors the order's
/// own agreed price: a short rounds up, a long rounds down.
pub fn align_rest_price_to_tick(price: u64, tick_size: u64, direction: PositionDirection) -> u64 {
    if tick_size <= 1 {
        return price;
    }

    match direction {
        PositionDirection::Short => price.div_ceil(tick_size).saturating_mul(tick_size),
        PositionDirection::Long => (price / tick_size).saturating_mul(tick_size),
    }
}

/// Gate a below-default activation delay on the flow authority's
/// attestation. The speed bump is the taker protection that replaced
/// just-in-time matching. Attested flow is a signed-message order that swift
/// held for the hold window and then attested with a detached signature.
pub fn attest_activation_delay(
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    market_index: u16,
    requested: Option<u32>,
    attested: bool,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };

    // The mirror the attach wrote, rather than a CPI. See
    // `QuoterConfigV0::book_tick_size`.
    let default_delay = quoter_slab
        .clob_slot(market_index)?
        .config
        .book_default_activation_delay_slots;
    if requested >= default_delay {
        return Ok(());
    }

    validate!(
        attested,
        ErrorCode::UnattestedFastActivation,
        "activation delay {} is below the default {} and the order carries \
         no flow attestation",
        requested,
        default_delay
    )?;

    Ok(())
}

/// Whether this transaction may fill against the market's book at once. Attested
/// flow may. Unattested flow may only when the book runs no speed bump, and
/// otherwise rests taker-origin for the cross cranks to fill.
/// [`attest_activation_delay`] is the placement-side half of the rule.
pub fn synchronous_take_allowed(
    taker_served_window: bool,
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    market_index: u16,
) -> Result<bool> {
    if taker_served_window {
        return Ok(true);
    }

    // The mirror the attach wrote, rather than a CPI.
    Ok(quoter_slab
        .clob_slot(market_index)?
        .config
        .book_default_activation_delay_slots
        == 0)
}

/// Why the book would refuse to hold a remainder. Each variant names one
/// rule `place_order_v0` enforces. Velocity tests them itself because a
/// failed CPI aborts the whole transaction, and a rejection the caller
/// could have predicted would otherwise fail the fill that carried it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestRefusal {
    /// The market's book slot is inactive, unapproved, or suspended.
    BookClosed,
    /// The size is zero, or below the book's minimum order size.
    SizeBelowMinimum,
    /// The size is not a multiple of the book's step size.
    SizeOffStep,
    /// The rest price is zero, or not a multiple of the book's tick size.
    PriceOffTick,
    /// The expiry is at or before the current time.
    ExpiryPassed,
    /// The requested activation delay is above the book's maximum.
    DelayAboveMaximum,
    /// The expiry falls inside the activation delay, so nothing could match
    /// the order.
    ExpiresBeforeActivation,
    /// The side the remainder would rest on holds every order it can.
    SideAtCapacity,
}

/// What the book would do with a remainder at these terms.
pub enum RestAdmission {
    /// The book holds it, at this price snapped to the book's tick.
    Admitted {
        price: u64,
    },
    Refused(RestRefusal),
}

/// Ask the book for its placement rules, by one read-only `order_rules_v0` CPI,
/// and test the remainder against each. The slab's mirror is not used, because
/// the book's authority can change the rules after the attach. `OrderWouldCross`
/// is not tested. Only a post-only maker place can hit it.
pub fn clob_admits_rest(
    clob: &ClobMarket<'_, '_>,
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    activation_delay_slots: Option<u32>,
    now: i64,
) -> Result<RestAdmission> {
    Ok(rest_admission(
        &clob.reader().order_rules()?,
        direction,
        price,
        base_asset_amount,
        max_ts,
        activation_delay_slots,
        now,
    ))
}

/// [`clob_admits_rest`] after the book answers, as a decision over the rules
/// alone.
pub fn rest_admission(
    rules: &crate::state::prop_amm::ClobOrderRulesV0,
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    activation_delay_slots: Option<u32>,
    now: i64,
) -> RestAdmission {
    if base_asset_amount == 0
        || (rules.min_order_size != 0 && base_asset_amount < rules.min_order_size)
    {
        return RestAdmission::Refused(RestRefusal::SizeBelowMinimum);
    }

    if !base_asset_amount.is_multiple_of(rules.step_size.max(1)) {
        return RestAdmission::Refused(RestRefusal::SizeOffStep);
    }

    let price = align_rest_price_to_tick(price, rules.tick_size, direction);
    if price == 0 || !price.is_multiple_of(rules.tick_size.max(1)) {
        return RestAdmission::Refused(RestRefusal::PriceOffTick);
    }

    // The book takes `max_ts == 0` as good till cancelled and refuses any
    // other value at or before now.
    if max_ts != 0 && max_ts <= now {
        return RestAdmission::Refused(RestRefusal::ExpiryPassed);
    }

    let delay = activation_delay_slots.unwrap_or(rules.default_activation_delay_slots);
    if delay > rules.max_activation_delay_slots {
        return RestAdmission::Refused(RestRefusal::DelayAboveMaximum);
    }

    if clob_wire::expires_before_activation(max_ts, now, delay) {
        return RestAdmission::Refused(RestRefusal::ExpiresBeforeActivation);
    }

    // The arena is shared, so each side holds at most half of it.
    let side = match direction {
        PositionDirection::Long => 0,
        PositionDirection::Short => 1,
    };

    if rules.side_order_counts[side] >= rules.arena_capacity / 2 {
        return RestAdmission::Refused(RestRefusal::SideAtCapacity);
    }

    RestAdmission::Admitted { price }
}

/// Rest an order's unfilled remainder on the CLOB, a taker's or a maker's.
/// Returns the CLOB order id, which a signed-message taker records so the fill
/// at the activation slot can find its route. A dead book slot, a failed margin
/// check, or a remainder the book's rules refuse returns `Ok(None)` rather than
/// reverting, because a taker's fill has already landed.
#[allow(clippy::too_many_arguments)]
pub fn try_place_remainder_on_clob<'info>(
    user_loader: &AccountLoader<'info, User>,
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    clob_market: &AccountInfo<'info>,
    clob_program: &AccountInfo<'info>,
    maps: &mut AccountMaps,
    market_index: u16,
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    // The id of the order this remainder came off, so it keeps one identity
    // through the migration.
    client_order_id: u32,
    // A taker remainder rests taker-origin, so a counterparty crosses it at
    // the counterparty's price. A maker remainder does not.
    taker_origin: bool,
    // A post-only maker refuses to rest crossed. A taker remainder rests
    // crossed, and the cross crank matches it.
    reject_if_crossed: bool,
    // The book clamps a fill against a reduce-only order to the owner's
    // `base_cover` cap. A reduce-only `base_asset_amount` must already be
    // clamped to the position, as `restable_remainder` does.
    reduce_only: bool,
    // `None` takes the book's default speed bump. The caller attests a
    // below-default value before it reaches here.
    activation_delay_slots: Option<u32>,
    clock: &Clock,
) -> Result<Option<u64>> {
    if !quoter_slab.clob_slot(market_index)?.quotes() {
        msg!(
            "book refuses the remainder ({:?}); stays cancelled",
            RestRefusal::BookClosed
        );

        return Ok(None);
    }

    let clob = ClobMarket::from_slab(quoter_slab, market_index, clob_market, clob_program)?;

    // A partial fill often leaves a remainder the book refuses, so the rules
    // are tested before the CPI that would revert the fill.
    let price = match clob_admits_rest(
        &clob,
        direction,
        price,
        base_asset_amount,
        max_ts,
        activation_delay_slots,
        clock.unix_timestamp,
    )? {
        RestAdmission::Admitted { price } => price,
        RestAdmission::Refused(reason) => {
            msg!("book refuses the remainder ({:?}); stays cancelled", reason);
            return Ok(None);
        }
    };

    let Some(reserved) = reserve_remainder(
        user_loader,
        maps,
        &OrderReservation::book_order(market_index, direction, base_asset_amount, reduce_only),
        direction,
        base_asset_amount,
        clock.slot,
    )?
    else {
        return Ok(None);
    };

    let side = match direction {
        PositionDirection::Long => SideV0::Bid,
        PositionDirection::Short => SideV0::Ask,
    };

    // A failed CPI aborts the transaction, which unwinds the reservation with
    // it. `clob_admits_rest` caught every rejection it can, which leaves
    // `OrderWouldCross` for a post-only maker.
    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount,
        activation_delay_slots,
        max_ts,
        user: reserved.user_ref,
        taker_origin,
        client_order_id,
        reject_if_crossed,
        reduce_only,
    })?;

    super::emit_clob_place_record(
        clock.unix_timestamp,
        &user_loader.key(),
        super::ClobOrderFacts {
            order_id: client_order_id,
            market_index,
            direction,
            price,
            base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts,
            slot: clock.slot,
            taker_origin,
        },
        reserved.is_isolated_position,
    )?;

    msg!(
        "placed remainder as clob order {} (node {})",
        order_ref.order_id,
        order_ref.node_index
    );

    Ok(Some(order_ref.order_id))
}

/// What a remainder's owner holds once its reservation is taken.
struct ReservedRemainder {
    user_ref: UserRefV0,
    /// The order holds `open_orders` on the position, so its margin regime
    /// cannot change while it rests. The place record states it.
    is_isolated_position: bool,
}

/// Reserve a remainder on its owner's account before it goes to the book,
/// and gate margin the way a placement does.
///
/// `None` leaves the account as it was. That happens to a bankrupt owner, to a
/// position at its order limit, and to a risk-increasing remainder the account
/// cannot carry. A reducing remainder skips the margin gate, because refusing
/// it would remove the order that shrinks the position.
fn reserve_remainder(
    user_loader: &AccountLoader<User>,
    maps: &mut AccountMaps,
    reservation: &OrderReservation,
    direction: PositionDirection,
    base_asset_amount: u64,
    slot: u64,
) -> Result<Option<ReservedRemainder>> {
    let mut user = load_mut!(user_loader)?;
    let position = user.get_perp_position(reservation.market_index).ok();
    if user.is_bankrupt() || position.is_some_and(|position| position.open_orders == u8::MAX) {
        return Ok(None);
    }

    let risk_increasing = !is_order_position_reducing(
        &direction,
        base_asset_amount,
        position.map_or(0, |position| position.base_asset_amount),
    )?;

    let position_index = user.reserve_orders(reservation)?;
    let is_isolated_position = user.perp_positions[position_index].is_isolated();

    if risk_increasing {
        let isolated_market_index = is_isolated_position.then_some(reservation.market_index);
        if meets_place_order_margin_requirement(&user, maps, true, isolated_market_index).is_err() {
            user.release_orders(reservation, ReleaseCheck::HeldToReservation)?;
            msg!("remainder fails the placement margin gate; stays cancelled");
            return Ok(None);
        }
    }

    user.update_last_active_slot(slot);
    Ok(Some(ReservedRemainder {
        user_ref: user.clob_user_ref(),
        is_isolated_position,
    }))
}

#[cfg(test)]
mod align_rest_price_to_tick_tests {
    use {super::align_rest_price_to_tick, crate::controller::position::PositionDirection};

    #[test]
    fn short_rounds_the_ask_up_to_the_next_tick() {
        // A short rests as an ask. Rounding up keeps the ask on a tick without
        // ever resting below the floor it agreed to sell at.
        assert_eq!(
            align_rest_price_to_tick(104_629_001, 1_000, PositionDirection::Short),
            104_630_000
        );
    }

    #[test]
    fn long_rounds_the_bid_down_to_the_prev_tick() {
        // A long rests as a bid. Rounding down keeps the bid on a tick without
        // ever resting above the ceiling it agreed to pay.
        assert_eq!(
            align_rest_price_to_tick(107_371_999, 1_000, PositionDirection::Long),
            107_371_000
        );
    }

    #[test]
    fn an_already_aligned_price_is_unchanged() {
        assert_eq!(
            align_rest_price_to_tick(104_630_000, 1_000, PositionDirection::Short),
            104_630_000
        );
        assert_eq!(
            align_rest_price_to_tick(104_630_000, 1_000, PositionDirection::Long),
            104_630_000
        );
    }

    #[test]
    fn a_unit_tick_aligns_every_price() {
        assert_eq!(
            align_rest_price_to_tick(104_629_001, 1, PositionDirection::Short),
            104_629_001
        );
        assert_eq!(
            align_rest_price_to_tick(104_629_001, 0, PositionDirection::Long),
            104_629_001
        );
    }
}

#[cfg(test)]
mod restable_remainder_price_tests {
    use {
        super::restable_remainder_price,
        crate::{
            controller::position::PositionDirection,
            state::user::{Order, OrderBitFlag, OrderStatus, OrderType},
        },
    };

    fn fired_trigger(direction: PositionDirection, offset: i64) -> Order {
        Order {
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            direction,
            bit_flags: OrderBitFlag::OracleTriggerMarket as u8,
            oracle_price_offset: offset,
            ..Order::default()
        }
    }

    #[test]
    fn short_fired_trigger_rests_at_oracle_plus_offset() {
        // A short's worst price is a negative offset. Read as an absolute
        // price it clamps to zero and the rest is dropped. Adding the oracle
        // recovers the price the fill settles at.
        let order = fired_trigger(PositionDirection::Short, -1_371_000);
        assert_eq!(
            restable_remainder_price(&order, Some(106_000_000)),
            Some(104_629_000)
        );

        // The offset cannot be resolved without the oracle.
        assert_eq!(restable_remainder_price(&order, None), None);
    }

    #[test]
    fn long_fired_trigger_rests_at_oracle_plus_offset() {
        let order = fired_trigger(PositionDirection::Long, 1_371_000);
        assert_eq!(
            restable_remainder_price(&order, Some(106_000_000)),
            Some(107_371_000)
        );
    }

    #[test]
    fn plain_market_uses_its_absolute_worst_price() {
        // Without the OracleTriggerMarket flag the worst price is already
        // absolute, so no oracle is needed.
        let order = Order {
            status: OrderStatus::Open,
            order_type: OrderType::Market,
            price: 104_000_000,
            ..Order::default()
        };

        assert_eq!(restable_remainder_price(&order, None), Some(104_000_000));
    }
}

#[cfg(test)]
mod rest_admission_tests {
    use {
        super::{rest_admission, RestAdmission, RestRefusal},
        crate::{controller::position::PositionDirection, state::prop_amm::ClobOrderRulesV0},
    };

    const NOW: i64 = 1_700_000_000;

    fn rules() -> ClobOrderRulesV0 {
        ClobOrderRulesV0 {
            min_order_size: 100,
            blocking_min_size: 0,
            default_activation_delay_slots: 2,
            max_activation_delay_slots: 10,
            place_authority: [0; 32],
            tick_size: 1_000,
            step_size: 100,
            // Room on both sides, so a case that does not name capacity is
            // measuring one of the other rules.
            side_order_counts: [0, 0],
            arena_capacity: 512,
            evict_threshold_per_side: 200,
        }
    }

    fn refusal(admission: RestAdmission) -> Option<RestRefusal> {
        match admission {
            RestAdmission::Admitted { .. } => None,
            RestAdmission::Refused(reason) => Some(reason),
        }
    }

    #[test]
    fn a_remainder_that_meets_every_rule_rests_at_its_snapped_price() {
        let admission = rest_admission(
            &rules(),
            PositionDirection::Long,
            104_629_001,
            1_000,
            0,
            None,
            NOW,
        );

        assert!(matches!(
            admission,
            RestAdmission::Admitted { price: 104_629_000 }
        ));
    }

    #[test]
    fn a_remainder_below_the_minimum_is_refused() {
        let admission = rest_admission(
            &rules(),
            PositionDirection::Long,
            104_629_000,
            99,
            0,
            None,
            NOW,
        );

        assert_eq!(refusal(admission), Some(RestRefusal::SizeBelowMinimum));
    }

    #[test]
    fn a_zero_size_remainder_is_refused() {
        let mut rules = rules();
        rules.min_order_size = 0;
        let admission = rest_admission(
            &rules,
            PositionDirection::Long,
            104_629_000,
            0,
            0,
            None,
            NOW,
        );

        assert_eq!(refusal(admission), Some(RestRefusal::SizeBelowMinimum));
    }

    #[test]
    fn an_off_step_remainder_is_refused() {
        let admission = rest_admission(
            &rules(),
            PositionDirection::Long,
            104_629_000,
            1_050,
            0,
            None,
            NOW,
        );

        assert_eq!(refusal(admission), Some(RestRefusal::SizeOffStep));
    }

    #[test]
    fn a_price_that_snaps_to_zero_is_refused() {
        // A long bid rounds down, so a price under one tick has no tick to
        // rest on.
        let admission = rest_admission(&rules(), PositionDirection::Long, 999, 1_000, 0, None, NOW);
        assert_eq!(refusal(admission), Some(RestRefusal::PriceOffTick));
    }

    #[test]
    fn an_expiry_at_or_before_now_is_refused() {
        let admission = rest_admission(
            &rules(),
            PositionDirection::Long,
            104_629_000,
            1_000,
            NOW,
            None,
            NOW,
        );

        assert_eq!(refusal(admission), Some(RestRefusal::ExpiryPassed));
        // A zero expiry means good till cancelled, which the book accepts.
        let admission = rest_admission(
            &rules(),
            PositionDirection::Long,
            104_629_000,
            1_000,
            0,
            None,
            NOW,
        );

        assert_eq!(refusal(admission), None);
    }

    #[test]
    fn an_expiry_inside_the_default_delay_is_refused() {
        let mut rules = rules();
        rules.default_activation_delay_slots = 10;
        let admission = rest_admission(
            &rules,
            PositionDirection::Long,
            104_629_000,
            1_000,
            102,
            None,
            100,
        );

        assert_eq!(
            refusal(admission),
            Some(RestRefusal::ExpiresBeforeActivation)
        );
    }

    #[test]
    fn a_delay_above_the_books_maximum_is_refused() {
        let admission = rest_admission(
            &rules(),
            PositionDirection::Long,
            104_629_000,
            1_000,
            0,
            Some(11),
            NOW,
        );

        assert_eq!(refusal(admission), Some(RestRefusal::DelayAboveMaximum));
        let admission = rest_admission(
            &rules(),
            PositionDirection::Long,
            104_629_000,
            1_000,
            0,
            Some(10),
            NOW,
        );

        assert_eq!(refusal(admission), None);
    }

    #[test]
    fn a_stale_mirror_cannot_admit_an_off_tick_price() {
        // The book's authority may raise its tick after the attach mirrored
        // it. The rules come from the book, so the raised tick is what the
        // remainder answers to.
        let mut rules = rules();
        rules.tick_size = 10_000;
        let admission = rest_admission(
            &rules,
            PositionDirection::Long,
            104_629_000,
            1_000,
            0,
            None,
            NOW,
        );

        assert!(matches!(
            admission,
            RestAdmission::Admitted { price: 104_620_000 }
        ));
    }

    #[test]
    fn a_full_side_refuses_the_remainder_before_the_book_does() {
        let mut rules = rules();
        // Each side holds half the arena, so this bid side is full.
        rules.side_order_counts = [256, 0];
        let admission = rest_admission(
            &rules,
            PositionDirection::Long,
            104_000_000,
            1_000,
            0,
            None,
            NOW,
        );

        assert_eq!(refusal(admission), Some(RestRefusal::SideAtCapacity));
    }

    #[test]
    fn a_full_opposite_side_does_not_refuse_the_remainder() {
        let mut rules = rules();
        // The ask side is full, and a bid still rests.
        rules.side_order_counts = [0, 256];
        let admission = rest_admission(
            &rules,
            PositionDirection::Long,
            104_000_000,
            1_000,
            0,
            None,
            NOW,
        );

        assert!(matches!(admission, RestAdmission::Admitted { .. }));
    }
}
