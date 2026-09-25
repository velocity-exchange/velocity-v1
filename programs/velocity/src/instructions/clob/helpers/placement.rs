//! Place a resting limit order on a registered CLOB.
//!
//! Velocity owns the placement policy. It checks the `User` authority, reserves
//! the order's worst-case open-order aggregates, and gates margin the way a
//! slot placement does. The CLOB trusts its `place_authority`, which is the
//! market's quoter slab PDA. The CLOB enforces only book-level rules: the tick,
//! the step, the minimum order size, the capacity, and the activation delay.

use {
    crate::{
        controller::{self, position::PositionDirection},
        error::ErrorCode,
        instructions::optional_accounts::AccountMaps,
        load, load_mut,
        math::{margin::meets_place_order_margin_requirement, orders::is_order_position_reducing},
        msg,
        state::{
            events::OrderActionExplanation,
            prop_amm::{
                ClobMarket, PlaceOrderArgsV0, QuoterSlabExt, QuoterSlabV0, SideV0, UserRefV0,
            },
            user::{Order, OrderReservation, ReleaseCheck, User},
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

/// Why the book or the owner's account would refuse to hold a rest. Each
/// book variant names one rule `place_order_v0` enforces. Velocity tests them
/// itself because a failed CPI aborts the whole transaction, and a rejection the
/// caller could have predicted would otherwise fail the fill that carried it.
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
    /// The owner is bankrupt.
    OwnerBankrupt,
    /// The position holds as many orders as its `open_orders` counter holds.
    PositionAtOrderLimit,
    /// The rest increases risk, and the account cannot carry it.
    FailsMarginGate,
}

impl RestRefusal {
    /// The error of a placement that must rest, such as a maker quote.
    pub fn error_code(self) -> ErrorCode {
        match self {
            RestRefusal::BookClosed => ErrorCode::ClobQuoterNotActive,
            RestRefusal::SizeBelowMinimum => ErrorCode::InvalidOrderMinOrderSize,
            RestRefusal::SizeOffStep => ErrorCode::InvalidOrderNotStepSizeMultiple,
            RestRefusal::PriceOffTick => ErrorCode::InvalidOrderLimitPrice,
            RestRefusal::ExpiryPassed | RestRefusal::ExpiresBeforeActivation => {
                ErrorCode::InvalidOrderMaxTs
            }
            RestRefusal::DelayAboveMaximum => ErrorCode::InvalidOrder,
            RestRefusal::SideAtCapacity | RestRefusal::PositionAtOrderLimit => {
                ErrorCode::MaxNumberOfOrders
            }
            RestRefusal::OwnerBankrupt => ErrorCode::UserBankrupt,
            RestRefusal::FailsMarginGate => ErrorCode::InsufficientCollateral,
        }
    }

    /// The explanation of the cancel record for a taker remainder that the
    /// book refused.
    pub fn cancel_explanation(self) -> OrderActionExplanation {
        match self {
            RestRefusal::SizeBelowMinimum => OrderActionExplanation::ClobRemainderCulled,
            RestRefusal::FailsMarginGate => OrderActionExplanation::InsufficientFreeCollateral,
            _ => OrderActionExplanation::None,
        }
    }
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
/// it does not hold the side counts or the delay ceiling. `OrderWouldCross` is
/// not tested. Only a post-only maker place can hit it.
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
    rules: &crate::state::prop_amm::OrderRulesV0,
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

/// The accounts one rest on the book reads.
pub struct ClobRestAccounts<'a, 'info> {
    pub user: &'a AccountLoader<'info, User>,
    pub quoter_slab: &'a AccountLoader<'info, QuoterSlabV0>,
    pub clob_market: &'a AccountInfo<'info>,
    pub clob_program: &'a AccountInfo<'info>,
}

/// One order to rest on the book.
#[derive(Clone, Copy, Debug)]
pub struct ClobRestOrder {
    pub market_index: u16,
    pub direction: PositionDirection,
    pub price: u64,
    pub base_asset_amount: u64,
    pub max_ts: i64,
    /// The id of the order this rest came off, so it keeps one identity
    /// through the migration.
    pub client_order_id: u32,
    /// A taker remainder rests taker-origin, so a counterparty crosses it at
    /// the counterparty's price. A maker quote does not.
    pub taker_origin: bool,
    /// A post-only maker refuses to rest crossed. A taker remainder rests
    /// crossed, and the cross crank matches it.
    pub reject_if_crossed: bool,
    /// The book clamps a fill against a reduce-only order to the owner's
    /// `base_cover` cap. A reduce-only `base_asset_amount` must already be
    /// clamped to the position, as `restable_remainder` does.
    pub reduce_only: bool,
    /// `None` takes the book's default speed bump. The caller attests a
    /// below-default value before it reaches here.
    pub activation_delay_slots: Option<u32>,
}

/// An order the book now holds.
#[derive(Clone, Copy, Debug)]
pub struct PlacedRest {
    pub clob_order_id: u64,
    /// The rest price, snapped to the book's tick.
    pub price: u64,
    /// The order holds `open_orders` on the position, so its margin regime
    /// cannot change while it rests.
    pub is_isolated_position: bool,
}

/// What one attempt to rest an order did.
#[derive(Clone, Copy, Debug)]
pub enum RestOutcome {
    Placed(PlacedRest),
    Refused(RestRefusal),
}

/// Rest one order on the book, or report why the book or the account refuses
/// it. A refusal leaves the account as it was. The caller decides whether a
/// refusal is an error, and writes the records.
pub fn rest_on_clob<'info>(
    accounts: &ClobRestAccounts<'_, 'info>,
    maps: &mut AccountMaps,
    order: &ClobRestOrder,
    clock: &Clock,
) -> Result<RestOutcome> {
    if !accounts.quoter_slab.clob_slot(order.market_index)?.quotes() {
        return Ok(RestOutcome::Refused(RestRefusal::BookClosed));
    }

    let clob = ClobMarket::from_slab(
        accounts.quoter_slab,
        order.market_index,
        accounts.clob_market,
        accounts.clob_program,
    )?;

    // A partial fill often leaves a remainder the book refuses, so the rules
    // are tested before the CPI that would revert the fill.
    let price = match clob_admits_rest(
        &clob,
        order.direction,
        order.price,
        order.base_asset_amount,
        order.max_ts,
        order.activation_delay_slots,
        clock.unix_timestamp,
    )? {
        RestAdmission::Admitted { price } => price,
        RestAdmission::Refused(reason) => return Ok(RestOutcome::Refused(reason)),
    };

    let reserved = match reserve_remainder(
        accounts.user,
        maps,
        &OrderReservation::book_order(
            order.market_index,
            order.direction,
            order.base_asset_amount,
            order.reduce_only,
        ),
        order.direction,
        order.base_asset_amount,
        clock.slot,
    )? {
        RemainderReservation::Held(reserved) => reserved,
        RemainderReservation::Refused(reason) => return Ok(RestOutcome::Refused(reason)),
    };

    // A failed CPI aborts the transaction, which unwinds the reservation with
    // it. `clob_admits_rest` caught every rejection it can, which leaves
    // `OrderWouldCross` for a post-only maker.
    let order_ref = clob.place(PlaceOrderArgsV0 {
        side: SideV0::from(order.direction),
        price,
        base_asset_amount: order.base_asset_amount,
        activation_delay_slots: order.activation_delay_slots,
        max_ts: order.max_ts,
        user: reserved.user_ref,
        taker_origin: order.taker_origin,
        client_order_id: order.client_order_id,
        reject_if_crossed: order.reject_if_crossed,
        reduce_only: order.reduce_only,
    })?;

    msg!(
        "placed clob order {} (node {})",
        order_ref.order_id,
        order_ref.node_index
    );

    Ok(RestOutcome::Placed(PlacedRest {
        clob_order_id: order_ref.order_id,
        price,
        is_isolated_position: reserved.is_isolated_position,
    }))
}

/// Rest an order's unfilled remainder on the CLOB and record it. A refusal is
/// not an error, because a taker's fill has already landed. The caller reads
/// the refusal and records the cancel.
pub fn rest_remainder_on_clob<'info>(
    accounts: &ClobRestAccounts<'_, 'info>,
    maps: &mut AccountMaps,
    order: &ClobRestOrder,
    clock: &Clock,
) -> Result<RestOutcome> {
    let outcome = rest_on_clob(accounts, maps, order, clock)?;
    match outcome {
        RestOutcome::Placed(placed) => super::emit_clob_place_record(
            clock.unix_timestamp,
            &accounts.user.key(),
            super::ClobOrderFacts {
                order_id: order.client_order_id,
                market_index: order.market_index,
                direction: order.direction,
                price: placed.price,
                base_asset_amount: order.base_asset_amount,
                base_asset_amount_filled: 0,
                max_ts: order.max_ts,
                slot: clock.slot,
                taker_origin: order.taker_origin,
            },
            placed.is_isolated_position,
        )?,
        RestOutcome::Refused(reason) => {
            msg!("book refuses the remainder ({:?}); stays cancelled", reason);
        }
    }

    Ok(outcome)
}

/// The terms a detached taker's remainder rests on, beyond the order itself.
pub struct DetachedRemainderTerms {
    /// The oracle an `OracleTriggerMarket` offset is relative to.
    pub rest_oracle_price: Option<i64>,
    pub activation_delay_slots: Option<u32>,
}

/// Rest the unfilled part of a detached taker order, or record its cancel.
/// Returns the CLOB order id when the part rests.
///
/// The fill already landed, so no refusal here is an error. Every part that
/// does not rest emits an `OrderActionRecord(Cancel)`, so the order does not
/// leave the order history without a record.
pub fn rest_or_cancel_detached_remainder<'info>(
    accounts: &ClobRestAccounts<'_, 'info>,
    maps: &mut AccountMaps,
    order: &Order,
    terms: &DetachedRemainderTerms,
    clock: &Clock,
) -> Result<Option<u64>> {
    if order.get_base_asset_amount_unfilled(None)? == 0 {
        return Ok(None);
    }

    let explanation = match detached_remainder_rest(accounts, maps, order, terms, clock)? {
        RemainderRest::Rested(clob_order_id) => return Ok(Some(clob_order_id)),
        RemainderRest::Cancelled(explanation) => explanation,
    };

    controller::orders::emit_detached_cancel_record(
        &*load!(accounts.user)?,
        &accounts.user.key(),
        order,
        maps,
        clock.unix_timestamp,
        explanation,
    )?;

    Ok(None)
}

/// What became of an unfilled part that the caller tried to rest.
enum RemainderRest {
    Rested(u64),
    Cancelled(OrderActionExplanation),
}

fn detached_remainder_rest<'info>(
    accounts: &ClobRestAccounts<'_, 'info>,
    maps: &mut AccountMaps,
    order: &Order,
    terms: &DetachedRemainderTerms,
    clock: &Clock,
) -> Result<RemainderRest> {
    if order.immediate_or_cancel {
        return Ok(RemainderRest::Cancelled(OrderActionExplanation::None));
    }

    let remainder = {
        let user = load!(accounts.user)?;
        if user.is_being_liquidated() {
            return Ok(RemainderRest::Cancelled(
                OrderActionExplanation::Liquidation,
            ));
        }

        restable_remainder(&user, order, order.market_index, terms.rest_oracle_price)
    };

    let Some(remainder) = remainder else {
        return Ok(RemainderRest::Cancelled(OrderActionExplanation::None));
    };

    // A reduce-only order clamps to the position, so a flat position leaves
    // nothing to rest.
    if remainder.unfilled == 0 {
        return Ok(RemainderRest::Cancelled(
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition,
        ));
    }

    let outcome = rest_remainder_on_clob(
        accounts,
        maps,
        &ClobRestOrder {
            market_index: order.market_index,
            direction: remainder.direction,
            price: remainder.price,
            base_asset_amount: remainder.unfilled,
            max_ts: remainder.max_ts,
            client_order_id: order.order_id,
            taker_origin: true,
            reject_if_crossed: false,
            reduce_only: remainder.reduce_only,
            activation_delay_slots: terms.activation_delay_slots,
        },
        clock,
    )?;

    Ok(match outcome {
        RestOutcome::Placed(placed) => RemainderRest::Rested(placed.clob_order_id),
        RestOutcome::Refused(reason) => RemainderRest::Cancelled(reason.cancel_explanation()),
    })
}

/// What a remainder's owner holds once its reservation is taken.
struct ReservedRemainder {
    user_ref: UserRefV0,
    is_isolated_position: bool,
}

/// Whether the owner's account holds the rest's reservation.
enum RemainderReservation {
    Held(ReservedRemainder),
    Refused(RestRefusal),
}

/// Reserve a remainder on its owner's account before it goes to the book,
/// and gate margin the way a placement does.
///
/// A refusal leaves the account as it was. A reducing remainder skips the
/// margin gate, because refusing it would remove the order that shrinks the
/// position.
fn reserve_remainder(
    user_loader: &AccountLoader<User>,
    maps: &mut AccountMaps,
    reservation: &OrderReservation,
    direction: PositionDirection,
    base_asset_amount: u64,
    slot: u64,
) -> Result<RemainderReservation> {
    let mut user = load_mut!(user_loader)?;
    if user.is_bankrupt() {
        return Ok(RemainderReservation::Refused(RestRefusal::OwnerBankrupt));
    }

    let position = user.get_perp_position(reservation.market_index).ok();
    if position.is_some_and(|position| position.open_orders == u8::MAX) {
        return Ok(RemainderReservation::Refused(
            RestRefusal::PositionAtOrderLimit,
        ));
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
            return Ok(RemainderReservation::Refused(RestRefusal::FailsMarginGate));
        }
    }

    user.update_last_active_slot(slot);
    Ok(RemainderReservation::Held(ReservedRemainder {
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
        crate::{controller::position::PositionDirection, state::prop_amm::OrderRulesV0},
    };

    const NOW: i64 = 1_700_000_000;

    fn rules() -> OrderRulesV0 {
        OrderRulesV0 {
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
            authority: [0; 32],
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
    fn the_books_own_tick_decides_the_rest_price() {
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

#[cfg(test)]
mod detached_remainder_tests {
    use {
        super::*,
        crate::{
            create_anchor_account_info,
            math::constants::BASE_PRECISION_U64,
            state::{
                oracle_map::OracleMap,
                perp_market_map::PerpMarketMap,
                spot_market_map::SpotMarketMap,
                user::{OrderStatus, OrderType, UserStatus},
            },
            test_utils::create_account_info,
        },
        anchor_lang::Discriminator,
    };

    fn open_order() -> Order {
        Order {
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            direction: PositionDirection::Long,
            base_asset_amount: BASE_PRECISION_U64,
            price: 100_000_000,
            ..Order::default()
        }
    }

    /// What the rest leg does with `order` for an owner in `user_status`. Each
    /// case ends before the book, so the book accounts carry no data.
    fn rest(order: Order, user_status: u8) -> RemainderRest {
        let mut user = User {
            status: user_status,
            ..User::default()
        };

        create_anchor_account_info!(user, User, user_info);
        let user_loader = AccountLoader::try_from(&user_info).unwrap();

        let slab_key = Pubkey::new_unique();
        let mut slab_lamports = 0;
        let mut slab_data = vec![0u8; QuoterSlabV0::space(0)];
        slab_data[..8].copy_from_slice(QuoterSlabV0::DISCRIMINATOR);
        let slab_info = create_account_info(
            &slab_key,
            false,
            &mut slab_lamports,
            &mut slab_data,
            &crate::ID,
        );
        let slab_loader = AccountLoader::try_from(&slab_info).unwrap();

        let mut maps = AccountMaps::new(
            PerpMarketMap::empty(),
            SpotMarketMap::empty(),
            OracleMap::empty(),
        );
        let accounts = ClobRestAccounts {
            user: &user_loader,
            quoter_slab: &slab_loader,
            clob_market: &slab_info,
            clob_program: &slab_info,
        };
        let terms = DetachedRemainderTerms {
            rest_oracle_price: None,
            activation_delay_slots: None,
        };

        let Ok(rest) =
            detached_remainder_rest(&accounts, &mut maps, &order, &terms, &Clock::default())
        else {
            panic!("the rest leg failed");
        };

        rest
    }

    fn cancelled_with(rest: RemainderRest) -> Option<OrderActionExplanation> {
        match rest {
            RemainderRest::Cancelled(explanation) => Some(explanation),
            RemainderRest::Rested(_) => None,
        }
    }

    #[test]
    fn an_immediate_or_cancel_remainder_records_a_cancel() {
        let order = Order {
            immediate_or_cancel: true,
            ..open_order()
        };

        assert!(matches!(
            cancelled_with(rest(order, 0)),
            Some(OrderActionExplanation::None)
        ));
    }

    #[test]
    fn the_remainder_of_an_account_under_liquidation_records_a_cancel() {
        let status = UserStatus::BeingLiquidated as u8;

        assert!(matches!(
            cancelled_with(rest(open_order(), status)),
            Some(OrderActionExplanation::Liquidation)
        ));
    }

    #[test]
    fn a_remainder_that_cannot_rest_records_a_cancel() {
        let order = Order {
            order_type: OrderType::Oracle,
            ..open_order()
        };

        assert!(matches!(
            cancelled_with(rest(order, 0)),
            Some(OrderActionExplanation::None)
        ));
    }

    #[test]
    fn a_reduce_only_remainder_with_nothing_to_reduce_records_a_cancel() {
        let order = Order {
            reduce_only: true,
            ..open_order()
        };

        assert!(matches!(
            cancelled_with(rest(order, 0)),
            Some(OrderActionExplanation::ReduceOnlyOrderIncreasedPosition)
        ));
    }
}
