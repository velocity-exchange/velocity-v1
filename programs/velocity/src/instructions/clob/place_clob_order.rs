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
        math::{margin::meets_place_order_margin_requirement, orders::is_order_position_reducing},
        msg,
        state::{
            prop_amm::{ClobMarket, ClobPlaceOrderArgsV0, ClobSide, QuoterV0},
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

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
/// Everything else stays behind. An oracle-floating price has nothing fixed
/// to rest at, and a trigger-limit has its own placement path. A fired
/// trigger-market does rest: once it fires it is a plain market order, and its
/// remainder belongs on the book. A reduce-only order rests too: the router
/// carries an authoritative `base_cover`, so the book clamps every fill against
/// it to the position it may reduce.
///
/// A `post_only` order never migrates either. It is a maker's own quote, not a
/// taker remainder — migrating it would cancel the maker's resting order, hide
/// it for the activation window, and re-place it as `taker_origin`, so a later
/// cross would charge the maker taker fees on a quote it posted as a maker. A
/// maker order belongs where its owner placed it.
pub fn restable_remainder_price(order: &crate::state::user::Order) -> Option<u64> {
    use crate::state::user::{OrderStatus, OrderType};
    if order.status != OrderStatus::Open || order.has_oracle_price_offset() || order.post_only {
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
            order.auction_end_price.max(0).unsigned_abs()
        }
        _ => return None,
    };
    (price != 0).then_some(price)
}

/// Gate a below-default activation delay on the flow authority's attestation.
///
/// The speed bump is the taker protection that replaced JIT. Skipping it is
/// reserved for attested flow: a transaction the flow authority (swift)
/// co-signed after serving the hold window off-chain. A delay at or above the
/// book's default needs no attestation, and `None` takes the default.
#[allow(clippy::too_many_arguments)]
pub fn attest_activation_delay<'info>(
    state: &crate::state::state::State,
    quoter_loader: &AccountLoader<'info, QuoterV0>,
    clob_market: &AccountInfo<'info>,
    clob_program: &AccountInfo<'info>,
    clob_authority: &AccountInfo<'info>,
    clob_authority_nonce: u8,
    market_index: u16,
    requested: Option<u32>,
    instructions_sysvar: Option<&AccountInfo<'info>>,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let clob = {
        let quoter = quoter_loader.load()?;
        ClobMarket::from_quoter(
            &quoter,
            market_index,
            clob_market,
            clob_program,
            clob_authority,
            clob_authority_nonce,
        )?
    };
    let default_delay = clob.reader().order_rules()?.default_activation_delay_slots;
    if requested >= default_delay {
        return Ok(());
    }
    let flow_authority = state.hot_key(crate::state::state::HotRole::FlowAuthority);
    validate!(
        flow_authority != Pubkey::default(),
        ErrorCode::UnattestedFastActivation,
        "no flow authority is configured; fast activation is disabled"
    )?;
    let sysvar = instructions_sysvar.ok_or_else(|| {
        msg!("fast activation needs the instructions sysvar for attestation");
        ErrorCode::UnattestedFastActivation
    })?;
    validate!(
        crate::instructions::optional_accounts::tx_co_signed_by(sysvar, &flow_authority)?,
        ErrorCode::UnattestedFastActivation,
        "activation delay {} is below the default {} and the transaction is not \
         co-signed by the flow authority",
        requested,
        default_delay
    )?;
    Ok(())
}

/// Rest an unfilled taker remainder on the CLOB: if it can rest and be
/// matched, it lives on the book, not in `User.orders`.
///
/// Returns the CLOB order id it now rests as. A caller that has to find the
/// order again later needs that id — a signed-message taker records it on its
/// own message entry, which is how the fill at the activation slot knows which
/// route the taker chose.
///
/// Degrades gracefully — a dead quoter entry or a failed margin re-reserve
/// returns `Ok(None)` (the remainder stays cancelled, the fill stands) instead
/// of reverting the whole call. A book that cannot hold the remainder — a full
/// side, or a maker remainder that would rest crossed — degrades the same way.
/// The fill already happened and already cancelled the order, so a remainder
/// that cannot rest never reverts it.
///
/// Reached from the routes that hold CLOB accounts: `place_and_take_perp_order_v1`,
/// `place_and_make_perp_order_v1`, `fill_perp_order_v1`, and
/// `place_signed_msg_taker_order`.
#[allow(clippy::too_many_arguments)]
pub fn try_place_remainder_on_clob<'info>(
    user_loader: &AccountLoader<'info, User>,
    quoter_loader: &AccountLoader<'info, QuoterV0>,
    clob_market: &AccountInfo<'info>,
    clob_program: &AccountInfo<'info>,
    clob_authority: &AccountInfo<'info>,
    clob_authority_nonce: u8,
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
    let clob = {
        let quoter = quoter_loader.load()?;
        let clob = ClobMarket::from_quoter(
            &quoter,
            market_index,
            clob_market,
            clob_program,
            clob_authority,
            clob_authority_nonce,
        )?;
        if !(quoter.is_active && quoter.is_approved) {
            msg!("clob quoter inactive; remainder stays cancelled");
            return Ok(None);
        }
        clob
    };

    // A remainder below the book's minimum cannot rest — the book rejects it,
    // and that rejection would revert the whole fill that already landed. A
    // partial fill routinely leaves a sub-min remainder, so drop it to the
    // plain cancel here instead of failing the fill. Off-tick / off-step
    // remainders cannot arise: the attach pins the book's tick and step to the
    // market's, so a remainder aligned to the market is aligned to the book.
    let min_order_size = clob.reader().order_rules()?.min_order_size;
    if min_order_size != 0 && base_asset_amount < min_order_size {
        msg!(
            "remainder {} is below the book minimum {}; stays cancelled",
            base_asset_amount,
            min_order_size
        );
        return Ok(None);
    }

    // Reserve the worst-case aggregates and re-run the placement margin
    // gate BEFORE the CPI, so a failure can skip resting (remainder stays
    // cancelled) rather than unwind external state.
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
        increase_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &direction,
            base_asset_amount,
            true,
        )?;
        user.perp_positions[position_index].open_orders += 1;
        user.increment_open_orders(false);

        let isolated_market_index = (risk_increasing
            && user.perp_positions[position_index].is_isolated())
        .then_some(market_index);
        if meets_place_order_margin_requirement(
            &user,
            perp_market_map,
            spot_market_map,
            oracle_map,
            risk_increasing,
            isolated_market_index,
        )
        .is_err()
        {
            crate::controller::position::decrease_open_bids_and_asks(
                &mut user.perp_positions[position_index],
                &direction,
                base_asset_amount,
                true,
            )?;
            user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
                .open_orders
                .saturating_sub(1);
            user.decrement_open_orders(false);
            msg!("remainder fails the placement margin gate; stays cancelled");
            return Ok(None);
        }
        user.update_last_active_slot(clock.slot);
        crate::state::prop_amm::ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id.into(),
        }
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
            // stands, so unwind the reserved aggregates and leave the remainder
            // cancelled rather than revert the fill. A full side would otherwise
            // let anyone stall every place-and-take whose remainder must rest.
            let mut user = load_mut!(user_loader)?;
            let position_index = get_position_index(&user.perp_positions, market_index)?;
            crate::controller::position::decrease_open_bids_and_asks(
                &mut user.perp_positions[position_index],
                &direction,
                base_asset_amount,
                true,
            )?;
            user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
                .open_orders
                .saturating_sub(1);
            user.decrement_open_orders(false);
            msg!("book cannot hold the remainder; stays cancelled");
            return Ok(None);
        }
    };

    // The remainder now rests. A reduce-only rest arms the position's counter,
    // so the router caps its fills to the position it may reduce until it
    // leaves the book.
    if reduce_only {
        let mut user = load_mut!(user_loader)?;
        let position_index = get_position_index(&user.perp_positions, market_index)?;
        user.perp_positions[position_index].arm_reduce_only_clob();
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
