//! Place a resting limit order on a registered CLOB. Velocity owns placement
//! policy: it verifies the `User` authority, reserves the order's worst-case
//! open-order aggregates, and gates margin exactly like a DLOB placement —
//! the CLOB trusts its `place_authority` (the CLOB place authority PDA) and only
//! enforces book-level rules (tick/step/min, capacity, activation delay).

use {
    crate::{
        controller::position::{
            add_new_position, get_position_index, increase_open_bids_and_asks, PositionDirection,
        },
        error::ErrorCode,
        load_mut,
        math::orders::is_order_position_reducing,
        msg,
        state::{
            prop_amm::{
                quoter_slab_clob, ClobMarket, ClobPlaceOrderArgsV0, ClobSide, QuoterSlabV0,
            },
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// The restable remainder of `order` for its owner: the direction, the rest
/// price, the base still unfilled, and the facts the rest carries. `None`
/// when the order's own rules keep it off the book
/// ([`restable_remainder_price`]) — an `unfilled` of zero is the caller's to
/// skip, since some routes cancel first either way.
///
/// One derivation for every route that migrates a remainder, so a remainder
/// that rests on one route rests on all of them.
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
    // See [`restable_remainder_price`]: only a fired trigger-market needs it.
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

/// The price an unfilled remainder can rest at on the book, or `None` when it
/// cannot rest at all.
///
/// One rule for both fill routes, because a remainder that rests on one and
/// not the other is a remainder whose fate depends on which keeper reached it.
/// Once the DLOB is gone, "kept DLOB behaviour" is not a fallback — it is an
/// order nothing will ever fill again.
///
/// A `Market` order's own `price` is zero; its bound lives in
/// `auction_end_price`, the worst fill it already agreed to. That is the only
/// price it can rest at, and resting there is safe *because* a migrated
/// remainder is taker-origin: it cannot be taken while a live counterparty
/// crosses it, and a cross settles at the counterparty's price, so a maker
/// that lines up during the activation window competes on price rather than
/// on transaction landing. Without that protection this would be a free
/// option written at the taker's own worst price.
///
/// Everything else stays behind. An `OrderType::Oracle` order prices its
/// bound relative to the oracle for its whole life, so it has nothing fixed
/// to rest at — the type match refuses it. (It is the one order type that
/// still carries `oracle_price_offset`: validation refuses the field on
/// every other type, so no separate offset check is needed here.) A
/// trigger-limit has its own placement path. A fired trigger-market does
/// rest: once it fires it is a plain market order, and its remainder
/// belongs on the book. A reduce-only order rests too: the router carries
/// an authoritative `base_cover`, so the book clamps every fill against it
/// to the position it may reduce.
///
/// A `post_only` order never migrates either. It is a maker's own quote, not a
/// taker remainder — migrating it would cancel the maker's resting order, hide
/// it for the activation window, and re-place it as `taker_origin`, so a later
/// cross would charge the maker taker fees on a quote it posted as a maker. A
/// maker order belongs where its owner placed it.
pub fn restable_remainder_price(
    order: &crate::state::user::Order,
    // The oracle price a fired trigger-market's auction is relative to. An
    // `OracleTriggerMarket` stores its auction bound as an offset from the
    // oracle, not an absolute price (`calculate_auction_price_with_progress`
    // reads it that way), so its rest price cannot be recovered without the
    // oracle. Every other order type ignores this, so callers that never rest
    // a fired trigger pass `None`.
    oracle_price: Option<i64>,
) -> Option<u64> {
    use crate::state::user::{OrderBitFlag, OrderStatus, OrderType};
    if order.status != OrderStatus::Open || order.post_only {
        return None;
    }
    let price = match order.order_type {
        OrderType::Limit => order.price,
        // A market order and a fired trigger-market both rest at their auction
        // bound: the worst fill they already agreed to, and the only price a
        // market order has. A fired trigger-market is a market order that
        // started as a conditional; once it fires, its remainder belongs on the
        // book like any other.
        OrderType::Market | OrderType::TriggerMarket => {
            if order.is_bit_flag_set(OrderBitFlag::OracleTriggerMarket) {
                // The bound is an offset from the oracle. A short's is negative,
                // so reading it as absolute would clamp it to zero and drop the
                // rest; the absolute the fill settles at is oracle plus offset.
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

/// Snap a rest price to the book's `tick_size`, within the order's own auction
/// bound. The book rejects a price that is not a multiple of the tick
/// (PriceNotTickAligned), and a fired trigger-market rests at the oracle plus an
/// arbitrary offset. Round toward the price the order already agreed to, never
/// past it: a short's ask rounds up, so the rest never sits below the floor it
/// agreed to sell at; a long's bid rounds down, so the rest never sits above
/// the ceiling it agreed to pay. A `tick_size` of zero or one aligns every
/// price, so the round is a no-op.
pub fn align_rest_price_to_tick(price: u64, tick_size: u64, direction: PositionDirection) -> u64 {
    if tick_size <= 1 {
        return price;
    }
    match direction {
        PositionDirection::Short => price.div_ceil(tick_size).saturating_mul(tick_size),
        PositionDirection::Long => (price / tick_size).saturating_mul(tick_size),
    }
}

/// Gate a below-default activation delay on the flow authority's attestation.
///
/// The speed bump is the taker protection that replaced JIT. Skipping it is
/// reserved for attested flow: a transaction the flow authority (swift)
/// signed as a named account after serving the hold window off-chain. A
/// delay at or above the book's default needs no attestation, and `None`
/// takes the default.
pub fn attest_activation_delay(
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    market_index: u16,
    requested: Option<u32>,
    // Whether the transaction is attested flow: the flow authority signs
    // swift-built transactions as a named account, so the caller reads the
    // presence of that signer rather than introspecting the sysvar.
    attested: bool,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    // The attach-written mirror, not a CPI (see
    // `QuoterConfigV0::book_tick_size`).
    let default_delay = quoter_slab_clob(quoter_slab, market_index)?
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
/// transaction. Attested flow may. Unattested flow may only when the book
/// runs no speed bump: with a nonzero default activation delay, an
/// unattested taker rests taker-origin through the activation window and
/// the cross cranks fill it — a maker can always reprice ahead of
/// aggression it never agreed to fill instantly. This is the take-side
/// half of the activation window; [`attest_activation_delay`] is the
/// placement-side half.
#[allow(clippy::too_many_arguments)]
pub fn synchronous_take_allowed(
    taker_served_window: bool,
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    market_index: u16,
) -> Result<bool> {
    if taker_served_window {
        return Ok(true);
    }
    // The attach-written mirror, not a CPI: the slab is already loaded on
    // every path that asks.
    Ok(quoter_slab_clob(quoter_slab, market_index)?
        .config
        .book_default_activation_delay_slots
        == 0)
}

/// Rest an unfilled taker remainder on the CLOB: if it can rest and be
/// matched, it lives on the book, not in `User.orders`.
///
/// Returns the CLOB order id it now rests as. A caller that has to find the
/// order again later needs that id — a signed-message taker records it on its
/// own message entry, which is how the fill at the activation slot knows which
/// route the taker chose.
///
/// Degrades gracefully — a dead book slot or a failed margin re-reserve
/// returns `Ok(None)` (the remainder stays cancelled, the fill stands) instead
/// of reverting the whole call. A book that cannot hold the remainder — a full
/// side, or a maker remainder that would rest crossed — degrades the same way.
/// The fill already happened and already cancelled the order, so a remainder
/// that cannot rest never reverts it.
///
/// Reached from the routes that hold CLOB accounts: `place_and_take_perp_order_v1`,
/// `place_and_make_perp_order_v1`, `fill_legacy_dlob_order`, and
/// `place_signed_msg_taker_order`.
#[allow(clippy::too_many_arguments)]
pub fn try_place_remainder_on_clob<'info>(
    user_loader: &AccountLoader<'info, User>,
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    clob_market: &AccountInfo<'info>,
    clob_program: &AccountInfo<'info>,
    perp_market_map: &crate::state::perp_market_map::PerpMarketMap,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
    market_index: u16,
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    // The id of the order this remainder came off. It carries across so the
    // order keeps one identity through the migration: the same id names it in
    // the records before and the records after.
    client_order_id: u32,
    // Whether this remainder rests as a taker-origin order. A taker's own
    // remainder (`place_and_take`, a keeper fill) rests `true`, so a live
    // counterparty crosses it at the counterparty's price rather than picking
    // it off. A maker's own remainder (`place_and_make`) rests `false` — it is
    // a maker quote, and resting it taker-origin would charge its owner taker
    // fees when a later order crossed it. A maker rest also refuses to rest
    // crossed, which is what post-only asked for.
    taker_origin: bool,
    // Refuse to rest when the order would cross the opposite best price, rather
    // than resting it crossed. What a post-only maker asks for. A taker
    // remainder passes `false` — it came to trade and must rest even crossed,
    // where the cross crank matches it at the counterparty's price.
    reject_if_crossed: bool,
    // Whether this order only reduces its owner's position. The book clamps a
    // fill against it to the owner's `base_cover` cap, so a reduce-only order
    // can rest on a position-blind book without a fill ever increasing the
    // position it should shrink.
    reduce_only: bool,
    // The book speed bump the order rests behind. `None` takes the book's
    // default. A taker remainder always passes `None`; only a maker place sets
    // it, and the caller attests a below-default value before it reaches here.
    activation_delay_slots: Option<u32>,
    clock: &Clock,
) -> Result<Option<u64>> {
    if !quoter_slab_clob(quoter_slab, market_index)?.quotes() {
        msg!("clob quoter inactive; remainder stays cancelled");
        return Ok(None);
    }
    let clob = ClobMarket::from_slab(quoter_slab, market_index, clob_market, clob_program)?;

    // A remainder below the book's minimum cannot rest — the book rejects it,
    // and that rejection would revert the whole fill that already landed. A
    // partial fill routinely leaves a sub-min remainder, so drop it to the
    // plain cancel here instead of failing the fill. Off-tick / off-step
    // remainders cannot arise: the attach pins the book's tick and step to the
    // market's, so a remainder aligned to the market is aligned to the book.
    let (min_order_size, order_tick_size) = {
        let slot = quoter_slab_clob(quoter_slab, market_index)?;
        (slot.config.book_min_order_size, slot.config.book_tick_size)
    };
    if min_order_size != 0 && base_asset_amount < min_order_size {
        msg!(
            "remainder {} is below the book minimum {}; stays cancelled",
            base_asset_amount,
            min_order_size
        );
        return Ok(None);
    }

    // Snap the rest price to the book's tick. A remainder migrated from a DLOB
    // order is already tick-aligned, so this is a no-op for it. A fired
    // trigger-market is not: it rests at the oracle plus its auction offset, an
    // arbitrary value the book rejects as off-tick (PriceNotTickAligned).
    let price = align_rest_price_to_tick(price, order_tick_size, direction);
    if price == 0 {
        msg!("remainder rounds to a zero rest price; stays cancelled");
        return Ok(None);
    }

    // ---- Validate: can the user carry this order? Nothing is committed
    // here. The margin engine prices the user with the prospective
    // exposure, so the check models the reservation and reverses it — the
    // same reserve/check/reverse `create_ephemeral_perp_order` runs. The
    // user claims the order only after the book holds it (the commit
    // below), so a refused placement has nothing to unwind.
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
        if crate::controller::orders::check_prospective_order_margin(
            &mut user,
            position_index,
            &direction,
            base_asset_amount,
            true,
            risk_increasing,
            isolated_market_index,
            perp_market_map,
            spot_market_map,
            oracle_map,
        )
        .is_err()
        {
            msg!("remainder fails the placement margin gate; stays cancelled");
            return Ok(None);
        }
        user.clob_user_ref()
    };

    // CPI the placement (identity travels in the args; the user account is
    // not lent, so holding no borrow is not even required — kept dropped
    // for symmetry with the main placement path).
    let side = match direction {
        PositionDirection::Long => ClobSide::Bid,
        PositionDirection::Short => ClobSide::Ask,
    };
    let placement = clob.place(ClobPlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount,
        activation_delay_slots,
        max_ts,
        user: user_ref,
        // A taker remainder rests taker-origin so a cross settles at the
        // counterparty's price rather than picking it off; a maker remainder
        // rests as an ordinary maker quote.
        taker_origin,
        client_order_id,
        // A taker remainder must rest even if crossed — refusing would strand
        // the taker that came to trade. A maker remainder refuses to rest
        // crossed, which is what post-only asked for.
        reject_if_crossed,
        reduce_only,
    });
    let order_ref = match placement {
        Ok(order_ref) => order_ref,
        Err(_) => {
            // The book cannot hold the remainder: the side is full, or a maker
            // remainder would rest crossed. The fill that carried it already
            // stands, so leave the remainder cancelled rather than revert the
            // fill — nothing was committed, so there is nothing to unwind. A
            // full side would otherwise let anyone stall every place-and-take
            // whose remainder must rest.
            msg!("book cannot hold the remainder; stays cancelled");
            return Ok(None);
        }
    };

    // ---- Commit: the book holds the order, so the user now claims it —
    // the aggregate reservation, the order counters, and the reduce-only
    // arm. The validate above reversed its model, so a position it freshly
    // added reads as available again and `get_position_index` skips it;
    // re-adding finds or revives the same slot.
    {
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
        // A reduce-only rest arms the position's counter, so the router caps
        // its fills to the position it may reduce until it leaves the book.
        if reduce_only {
            user.perp_positions[position_index].arm_reduce_only_clob();
        }
        user.update_last_active_slot(clock.slot);
    }

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
        // price it clamps to zero and the rest is dropped; the fix adds the
        // oracle to recover the price the fill settles at.
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
        // No OracleTriggerMarket flag: the auction bound is already absolute,
        // and no oracle is needed.
        let order = Order {
            status: OrderStatus::Open,
            order_type: OrderType::Market,
            auction_end_price: 104_000_000,
            ..Order::default()
        };
        assert_eq!(restable_remainder_price(&order, None), Some(104_000_000));
    }
}
