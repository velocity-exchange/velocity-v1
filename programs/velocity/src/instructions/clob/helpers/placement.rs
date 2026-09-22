//! Place a resting limit order on a registered CLOB.
//!
//! Velocity owns the placement policy. It checks the `User` authority, reserves
//! the order's worst-case open-order aggregates, and gates margin the way a
//! a slot placement does. The CLOB trusts its `place_authority`, which is the
//! CLOB place authority PDA. The CLOB enforces only book-level rules: the tick,
//! the step, the minimum order size, the capacity, and the activation delay.

use {
    crate::{
        controller::position::{
            add_new_position, get_position_index, increase_open_bids_and_asks, PositionDirection,
        },
        error::ErrorCode,
        instructions::optional_accounts::AccountMaps,
        load_mut,
        math::orders::is_order_position_reducing,
        msg,
        state::{
            prop_amm::{ClobMarket, ClobPlaceOrderArgsV0, ClobSide, QuoterSlabExt, QuoterSlabV0},
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// A remainder of `order` that can rest, or `None` when
/// [`restable_remainder_price`] refuses it. An `unfilled` of zero is the
/// caller's to skip. Some routes cancel first anyway. Every migrating route
/// uses this one derivation, so a remainder rests the same on every route.
pub struct RestableRemainder {
    pub direction: crate::controller::position::PositionDirection,
    pub price: u64,
    pub unfilled: u64,
    pub max_ts: i64,
    pub reduce_only: bool,
}

pub fn restable_remainder(
    user: &User,
    order: &crate::state::user::Order,
    market_index: u16,
    // Only a fired trigger-market needs this. See
    // [`restable_remainder_price`].
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

/// The price a remainder can rest at, or `None` when it cannot rest. A
/// market or fired trigger-market rests at its auction bound, its worst
/// agreed price. Resting there is safe. A taker-origin remainder can be
/// crossed only at the counterparty's price, never taken as a free option.
pub fn restable_remainder_price(
    order: &crate::state::user::Order,
    // The oracle price a fired trigger-market's auction is relative to. An
    // `OracleTriggerMarket` stores its auction bound as an offset from the
    // oracle rather than as an absolute price, and
    // `calculate_auction_price_with_progress` reads it that way. Its rest price
    // therefore cannot be recovered without the oracle. Every other order type
    // ignores this argument, so a caller that never rests a fired trigger
    // passes `None`.
    oracle_price: Option<i64>,
) -> Option<u64> {
    use crate::state::user::{OrderBitFlag, OrderStatus, OrderType};
    if order.status != OrderStatus::Open || order.post_only {
        return None;
    }

    let price = match order.order_type {
        OrderType::Limit => order.price,
        // A market order and a fired trigger-market both rest at their
        // auction bound. That bound is the worst fill they already agreed to,
        // and it is the only price a market order has. A fired trigger-market
        // is a market order that started as a conditional. Once it fires, its
        // remainder belongs on the book like any other remainder.
        OrderType::Market | OrderType::TriggerMarket => {
            if order.is_bit_flag_set(OrderBitFlag::OracleTriggerMarket) {
                // The bound is an offset from the oracle. A short's offset is
                // negative, so reading it as an absolute price clamps it to
                // zero and drops the rest. The absolute price the fill settles
                // at is the oracle plus the offset.
                oracle_price?
                    .checked_add(order.auction_end_price)?
                    .max(0)
                    .unsigned_abs()
            } else {
                order.auction_end_price.max(0).unsigned_abs()
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
/// just-in-time matching. Attested flow is a transaction the flow
/// authority, swift, signed after serving the hold window off-chain.
pub fn attest_activation_delay(
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    market_index: u16,
    requested: Option<u32>,
    // Whether the transaction is attested flow. The flow authority signs
    // swift-built transactions as a named account, so the caller reads that
    // signer rather than the instructions sysvar.
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
        "activation delay {} is below the default {} and the transaction is \
         not signed by the flow authority",
        requested,
        default_delay
    )?;

    Ok(())
}

/// Whether this transaction may fill against the market's book in the same
/// transaction. Attested flow may. Unattested flow may only when the book runs
/// no speed bump. With a nonzero default activation delay, an unattested taker
/// rests taker-origin through the activation window and the cross cranks fill
/// it. A maker can then always reprice ahead of aggression it never agreed to
/// fill at once. This is the take-side half of the activation window.
/// [`attest_activation_delay`] is the placement-side half.
#[allow(clippy::too_many_arguments)]
pub fn synchronous_take_allowed(
    taker_served_window: bool,
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    market_index: u16,
) -> Result<bool> {
    if taker_served_window {
        return Ok(true);
    }

    // The mirror the attach wrote, rather than a CPI. The slab is already
    // loaded on every path that asks.
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

/// Ask the book for its placement rules and hold the remainder to every
/// rule velocity can test before it commits. This is one read-only CPI,
/// `order_rules_v0`, replacing the slab's mirrored copy rather than
/// adding to it. The book's authority can move its tick, step, minimum,
/// and maximum delay after the attach wrote the mirror, so a
/// stale-mirror remainder reverts the fill that carried it.
/// `OrderWouldCross` stays untested: only a post-only maker place asks
/// for it, and velocity's wire never calls `next_cross_v0` to check it.
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

    // A fired trigger-market rests at the oracle plus its auction offset,
    // which is an arbitrary value. Snap it toward the price the order already
    // agreed to, then hold the result to the book's rule.
    let price = align_rest_price_to_tick(price, rules.tick_size, direction);
    if price == 0 || !price.is_multiple_of(rules.tick_size.max(1)) {
        return RestAdmission::Refused(RestRefusal::PriceOffTick);
    }

    // The book takes `max_ts == 0` as good till cancelled and refuses any
    // other value at or before now.
    if max_ts != 0 && max_ts <= now {
        return RestAdmission::Refused(RestRefusal::ExpiryPassed);
    }
    if activation_delay_slots.is_some_and(|delay| delay > rules.max_activation_delay_slots) {
        return RestAdmission::Refused(RestRefusal::DelayAboveMaximum);
    }

    // The arena is shared, so each side holds at most half of it. A full
    // side refuses every placement, including a better priced one, until a
    // crank takes a tail. Nothing else writes the book between this answer
    // and the placement in the same instruction, so it still holds.
    let side = match direction {
        PositionDirection::Long => 0,
        PositionDirection::Short => 1,
    };

    if rules.side_order_counts[side] >= rules.arena_capacity / 2 {
        return RestAdmission::Refused(RestRefusal::SideAtCapacity);
    }

    RestAdmission::Admitted { price }
}

/// Rest an unfilled taker remainder on the CLOB. Returns the CLOB order id,
/// which a signed-message taker records so the fill at the activation slot
/// can find its route. A dead book slot, a failed margin re-reserve, or a
/// remainder the book's rules refuse all return `Ok(None)` instead of
/// reverting: the fill already landed and canceled the order, so a
/// remainder that cannot rest must never undo it.
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
    // The id of the order this remainder came off. It carries across so the
    // order keeps one identity through the migration. The same id names it in
    // the records before and in the records after.
    client_order_id: u32,
    // Whether this remainder rests as a taker-origin order. A taker's own
    // remainder rests `true`, so a live counterparty crosses it at the
    // counterparty's price rather than at its own. A maker's remainder from
    // `place_and_make` rests `false`, to avoid taker fees on a later cross.
    taker_origin: bool,
    // Refuse to rest when the order would cross the opposite best price,
    // rather than resting it crossed. A post-only maker asks for this. A
    // taker remainder passes `false`: it must rest even when crossed, and
    // the cross crank matches it at the counterparty's price.
    reject_if_crossed: bool,
    // Whether this order only reduces its owner's position. The book clamps a
    // fill against it to the owner's `base_cover` cap, so a reduce-only order
    // can rest on a position-blind book without a fill ever increasing the
    // position it should shrink.
    reduce_only: bool,
    // The book speed bump the order rests behind. `None` takes the book's
    // default. A taker remainder always passes `None`. Only a maker place sets
    // it, and the caller attests a below-default value before it reaches
    // here.
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

    // Every book rule velocity can test, tested before the CPI that would
    // enforce it. A partial fill routinely leaves a remainder the book refuses,
    // and the fill that carried it has already landed.
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

    // Can the user carry this order? Nothing is committed here. The margin
    // engine prices the user with the prospective exposure, so the check models
    // the reservation and then reverses it. `create_detached_perp_order` runs
    // the same reserve, check and reverse. The user claims the order only after
    // the book holds it, in the commit below, so a refused placement has
    // nothing to unwind.
    let user_ref = {
        let mut user = load_mut!(user_loader)?;
        if user.is_bankrupt() {
            return Ok(None);
        }

        let position_index = get_position_index(&user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
        if user.perp_positions[position_index].open_orders == u8::MAX {
            return Ok(None);
        }

        let risk_increasing = !is_order_position_reducing(
            &direction,
            base_asset_amount,
            user.perp_positions[position_index].base_asset_amount,
        )?;
        let isolated_market_index = (risk_increasing
            && user.perp_positions[position_index].is_isolated())
        .then_some(market_index);
        // A reducing or reduce-only remainder skips the gate, as it does on
        // the trigger path. Refusing one can only leave the account more
        // exposed, because it removes the order that shrinks the position. A
        // stop-loss on an account that has slipped under maintenance is the
        // order the account most needs to keep. The exposure the remainder
        // reserves is on the side that already offsets the position, so the
        // requirement does not rise with it.
        let gate_margin = risk_increasing && !reduce_only;
        if gate_margin
            && crate::controller::orders::check_prospective_order_margin(
                &mut user,
                position_index,
                &crate::controller::orders::ProspectiveReservation {
                    direction,
                    base_asset_amount,
                    update_open_bids_and_asks: true,
                },
                risk_increasing,
                isolated_market_index,
                maps,
            )
            .is_err()
        {
            msg!("remainder fails the placement margin gate; stays cancelled");
            return Ok(None);
        }

        user.clob_user_ref()
    };

    // Identity travels in the args, so the placement CPI does not lend the
    // user account. The borrow is dropped anyway, to match the main placement
    // path.
    let side = match direction {
        PositionDirection::Long => ClobSide::Bid,
        PositionDirection::Short => ClobSide::Ask,
    };

    // A failed CPI aborts the transaction, so this call has no error arm to
    // handle. `clob_admits_rest` above catches every rejection it can. What is
    // left here is `OrderWouldCross` for a post-only maker, which fails the
    // whole call.
    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount,
        activation_delay_slots,
        max_ts,
        user: user_ref,
        // A taker remainder rests taker-origin, so a cross settles at the
        // counterparty's price rather than at the remainder's own price. A
        // maker remainder rests as an ordinary maker quote.
        taker_origin,
        client_order_id,
        // A taker remainder must rest even when crossed. Refusing would leave
        // the taker that came to trade with nothing. A maker remainder refuses
        // to rest crossed, which is what post-only asked for.
        reject_if_crossed,
        reduce_only,
    })?;

    // The book holds the order, so the user now claims it. That is the
    // aggregate reservation, the order counters, and the reduce-only arm. The
    // check above reversed its model, so a position it had just added reads as
    // available again and `get_position_index` skips it. Adding it again finds
    // or revives the same slot.
    let is_isolated_position = {
        let mut user = load_mut!(user_loader)?;
        let position_index = get_position_index(&user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
        increase_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &direction,
            base_asset_amount,
            true,
        )?;

        user.perp_positions[position_index].open_orders += 1;
        user.increment_open_orders(false);
        // A reduce-only rest arms the position's counter. The router then caps
        // its fills to the position it may reduce, until the order leaves the
        // book.
        if reduce_only {
            user.perp_positions[position_index].arm_reduce_only_clob();
        }

        user.update_last_active_slot(clock.slot);
        // The order now holds `open_orders` on this position, so its margin
        // regime cannot change while it rests. The record states it.
        user.perp_positions[position_index].is_isolated()
    };

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
        is_isolated_position,
    )?;

    msg!(
        "placed remainder as clob order {} (node {})",
        order_ref.order_id,
        order_ref.node_index
    );

    Ok(Some(order_ref.order_id))
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
            auction_end_price: offset,
            ..Order::default()
        }
    }

    #[test]
    fn short_fired_trigger_rests_at_oracle_plus_offset() {
        // A short's auction bound is a negative offset. Read as an absolute
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
    fn plain_market_uses_the_absolute_auction_bound() {
        // Without the OracleTriggerMarket flag the auction bound is already
        // absolute, so no oracle is needed.
        let order = Order {
            status: OrderStatus::Open,
            order_type: OrderType::Market,
            auction_end_price: 104_000_000,
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
