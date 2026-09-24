//! Placing a signed-message taker order, routing it, and resting what is left.
//!
//! The taker signs a message rather than a transaction, so a keeper builds the
//! transaction and the program holds it to the route the message names. One
//! instruction runs three legs in order. The placement builds the order and its
//! sidecars. The fill routes what the order can take now. The rest leg migrates
//! any remainder onto the market's book.

use super::*;

#[cfg(test)]
mod tests;

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_signed_msg_taker_order<'c: 'info, 'info>(
    ctx: Context<'info, PlaceSignedMsgTakerOrder<'info>>,
    signed_msg_order_params_message_bytes: Vec<u8>,
    is_delegate_signer: bool,
    flow_attestation: Option<crate::validation::sig_verification::FlowAttestationV0>,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    // The taker's own signature is the first 64 bytes of the envelope, and a
    // flow attestation binds to it. Read it before the placement consumes the
    // bytes.
    let taker_order_signature: Option<[u8; 64]> = signed_msg_order_params_message_bytes
        .get(..64)
        .and_then(|sig| <[u8; 64]>::try_from(sig).ok());
    // The market comes off the quoter slab and not off an argument. The
    // crank-conditions seed already derives from the slab, and the two must name
    // the same market. The message is checked against it once decoded.
    let market_index = ctx.accounts.quoter_slab.load()?.market;

    let mut sections = FillSections::load(
        ctx.remaining_accounts,
        &ctx.accounts.user,
        market_index,
        &state,
        clock.slot,
    )?;

    let placed = run_placement_leg(
        &ctx,
        &mut sections,
        signed_msg_order_params_message_bytes,
        is_delegate_signer,
        &state,
        &clock,
    )?;

    // The message was stale, replayed, or past its placement deadline. Those
    // are no-ops rather than failures, so there is nothing to fill.
    let Some(mut placed) = placed else {
        return Ok(());
    };

    validate!(
        placed.market_index == market_index,
        ErrorCode::InvalidSignedMsgOrderParam,
        "signed message names market {} but the passed CLOB entry is for {}",
        placed.market_index,
        market_index
    )?;

    let taker_served_window =
        verify_taker_served_window(&flow_attestation, taker_order_signature, &state, &clock)?;
    crate::instructions::attest_activation_delay(
        &ctx.accounts.quoter_slab,
        market_index,
        placed.activation_delay_slots,
        taker_served_window,
    )?;
    let synchronous_take = crate::instructions::synchronous_take_allowed(
        taker_served_window,
        &ctx.accounts.quoter_slab,
        market_index,
    )?;

    if !synchronous_take {
        validate_unattested_entry(&placed.order)?;
    }

    let _filled = if synchronous_take {
        fill_signed_msg_taker_order(
            &ctx,
            &mut sections,
            &mut placed,
            &state,
            taker_served_window,
            &clock,
        )?
    } else {
        msg!("unattested taker on a bumped book; the order rests whole");
        0
    };

    rest_signed_msg_remainder(&ctx, &placed, &mut sections.maps, &clock)?;

    if let Some(ref mut escrow) = sections.route.escrow {
        let taker = load_mut!(ctx.accounts.user)?;
        escrow.revoke_completed_orders(&taker)?;
    }

    Ok(())
}

/// Build the order and its sidecars.
///
/// This leg borrows the taker's `User` for its own body alone, because the
/// router fill takes the loader instead of a live borrow. The escrow goes out
/// and comes back, because the placement may attach builder rows to it.
fn run_placement_leg<'c: 'info, 'info>(
    ctx: &Context<'info, PlaceSignedMsgTakerOrder<'info>>,
    sections: &mut FillSections<'info>,
    message_bytes: Vec<u8>,
    is_delegate_signer: bool,
    state: &State,
    clock: &Clock,
) -> Result<Option<PlacedSignedMsgOrder>> {
    let mut taker = load_mut!(ctx.accounts.user)?;
    let mut taker_stats = load_mut!(ctx.accounts.user_stats)?;
    let mut signed_msg_taker = ctx.accounts.signed_msg_user_orders.load_mut()?;
    let (escrow, placed) = place_signed_msg_taker_order(
        SignedMsgTaker {
            key: ctx.accounts.user.key(),
            user: &mut taker,
            stats: &mut taker_stats,
            orders: &mut signed_msg_taker,
        },
        message_bytes,
        &mut PlacementEnv {
            maps: &mut sections.maps,
            state,
            clock,
        },
        sections.route.escrow.take(),
        is_delegate_signer,
    )?;

    sections.route.escrow = escrow;
    Ok(placed)
}

/// Whether the taker's flow served the book's protection window.
///
/// A book with a speed bump gives makers priority, so only attested flow fills synchronously.
/// An unattested signed-message submission instead rests the whole order taker-origin through
/// the activation window, and the cross cranks fill it. The attestation is detached: Swift signs
/// over the taker's own order signature after the hold, so the flow authority never signs a
/// keeper-built transaction, and the fill pays no second signature fee. The placement already
/// validated the envelope, so the signature prefix is present.
fn verify_taker_served_window(
    attestation: &Option<crate::validation::sig_verification::FlowAttestationV0>,
    taker_order_signature: Option<[u8; 64]>,
    state: &State,
    clock: &Clock,
) -> Result<bool> {
    let Some(attestation) = attestation else {
        return Ok(false);
    };

    crate::validation::sig_verification::verify_flow_attestation(
        attestation,
        &state.hot_key(crate::state::state::HotRole::FlowAuthority),
        &taker_order_signature.ok_or(ErrorCode::SigVerificationFailed)?,
        clock.unix_timestamp,
    )?;

    Ok(true)
}

/// Route the freshly placed signed-message order and fill what the route
/// reaches at or better than its worst price.
///
/// The taker did not sign this transaction, so the keeper is a filler and the
/// obligation rules apply. The keeper owes the taker every maker it had room to
/// carry, and it must carry every quoter the message named.
/// `require_signed_route` states the second rule. The route here is the
/// message's own list and not a claim the caller makes, because the message
/// rides this transaction. Only a later fill of the rested remainder works from
/// the digest.
fn fill_signed_msg_taker_order<'c: 'info, 'info>(
    ctx: &Context<'info, PlaceSignedMsgTakerOrder<'info>>,
    sections: &mut FillSections<'info>,
    placed: &mut PlacedSignedMsgOrder,
    state: &State,
    taker_served_window: bool,
    clock: &Clock,
) -> Result<u64> {
    let mode = FillMode::PlaceAndTake;
    let order = RoutedOrder::read(
        &*load!(ctx.accounts.user)?,
        &placed.order,
        &mut sections.maps,
        mode,
    )?;

    if order.unfilled == 0 {
        return Ok(0);
    }

    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let filled = crate::instructions::RouteFill {
        state,
        clock,
        tail: sections.route.quoters,
        scratch: &mut cpi_scratch,
    }
    .run(
        crate::instructions::RouteRequest {
            order,
            taker_served_window,
            include_taker_origin_reservations: false,
            claim: Some(crate::instructions::RouteClaim {
                quoters: &placed.route,
                digest: placed.route_digest,
            }),
            filler: crate::instructions::FillerTerms::keeper(Some(
                &ctx.accounts.ix_sysvar.to_account_info(),
            ))?,
        },
        controller::orders::FillRequest {
            // The taker order is detached. It never reserved, so the fill
            // unwinds no exposure for it.
            order: &mut placed.order,
            reserved: false,
            mode,
            referrer_is_accelerated: sections.route.referrer_is_accelerated,
        },
        controller::orders::PerpFillAccounts {
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            filler: &ctx.accounts.filler,
            filler_stats: &ctx.accounts.filler_stats,
            rev_share_escrow: &mut sections.route.escrow.as_mut(),
        },
        &mut controller::orders::FillParties {
            maps: &mut sections.maps,
            makers_and_referrer: &sections.route.makers_and_referrer,
            makers_and_referrer_stats: &sections.route.makers_and_referrer_stats,
        },
    )?;

    Ok(filled.amounts.base)
}

/// Rest what the route could not fill, on the market's book.
///
/// A signed-message order never rests in a `User.orders` slot. Either it is
/// immediate-or-cancel and its residual is cancelled, or the residual migrates
/// to the CLOB as a taker-origin order and competes for price inside its
/// activation window. `restable_remainder_price` is the shared rule for which
/// residuals can rest at all.
///
/// The CLOB order id goes back onto the message's own record. The fill at the
/// activation slot runs in a different transaction, built by somebody else, and
/// that record is how it finds the route this taker signed for.
fn rest_signed_msg_remainder<'c: 'info, 'info>(
    ctx: &Context<'info, PlaceSignedMsgTakerOrder<'info>>,
    placed: &PlacedSignedMsgOrder,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> Result<()> {
    let market_index = placed.market_index;
    let remainder = {
        let user = load!(ctx.accounts.user)?;
        if user.is_being_liquidated() {
            return Ok(());
        }

        // The order lives on `placed`, not `user.orders`. Its filled amounts
        // were updated in place by the fill leg.
        let order = &placed.order;
        // `restable_remainder` is the single decision. It answers `None` for a
        // post-only order, for a type that cannot rest, and for a zero price.
        crate::instructions::restable_remainder(&user, order, market_index, None)
    };

    // Immediate-or-cancel asked for no residual. The order never persisted, so
    // dropping it is enough.
    if placed.is_immediate_or_cancel {
        return Ok(());
    }

    let Some(remainder) = remainder else {
        return Ok(());
    };

    if remainder.unfilled == 0 {
        return Ok(());
    }

    // There is no slot to cancel, because the order never entered
    // `user.orders`. Its remainder migrates straight onto the CLOB.
    let rested = crate::instructions::try_place_remainder_on_clob(
        &ctx.accounts.user,
        &ctx.accounts.quoter_slab,
        &ctx.accounts.clob_market.to_account_info(),
        &ctx.accounts.clob_program.to_account_info(),
        maps,
        market_index,
        remainder.direction,
        remainder.price,
        remainder.unfilled,
        remainder.max_ts,
        placed.order_id,
        true,
        false,
        remainder.reduce_only,
        placed.activation_delay_slots,
        clock,
    )?;

    if let Some(clob_order_id) = rested {
        ctx.accounts
            .signed_msg_user_orders
            .load_mut()?
            .set_resting_route(
                placed.uuid,
                market_index,
                clob_order_id,
                placed.route_digest,
            );
    }

    Ok(())
}

/// The taker's own accounts, borrowed for the placement leg.
pub struct SignedMsgTaker<'a, 'b> {
    pub key: Pubkey,
    pub user: &'a mut User,
    /// Read only when the message carries an isolated-position deposit.
    pub stats: &'a mut UserStats,
    pub orders: &'a mut SignedMsgUserOrdersZeroCopyMut<'b>,
}

/// The market state and clock one placement writes through.
pub struct PlacementEnv<'a, 'info> {
    pub maps: &'a mut AccountMaps<'info>,
    pub state: &'a State,
    pub clock: &'a Clock,
}

/// The builder row every order of one bundle is keyed to.
struct BuilderRows<'a, 'info> {
    escrow: &'a mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
    idx: Option<u8>,
    fee_bps: Option<u16>,
}

pub fn place_signed_msg_taker_order<'c: 'info, 'info>(
    mut taker: SignedMsgTaker<'_, '_>,
    taker_order_params_message_bytes: Vec<u8>,
    env: &mut PlacementEnv<'_, '_>,
    escrow: Option<RevenueShareEscrowZeroCopyMut<'info>>,
    is_delegate_signer: bool,
) -> Result<(
    Option<RevenueShareEscrowZeroCopyMut<'info>>,
    Option<PlacedSignedMsgOrder>,
)> {
    let mut message = verify_signed_msg(
        &taker,
        &taker_order_params_message_bytes,
        is_delegate_signer,
    )?;

    let (mut escrow_zc, builder_fee_bps) = validate_and_load_builder(
        escrow,
        &taker.user.authority,
        message.builder_idx,
        message.builder_fee_tenth_bps,
        env.state,
    )?;

    let Some(mut order_id) = signed_msg_order_slot(&mut taker, &message, env)? else {
        return Ok((escrow_zc, None));
    };

    apply_message_position_settings(&mut taker, &message, env)?;

    // Place nothing when the main taker order would soft-skip on an already-expired `max_ts`. The
    // reduce-only sidecars below are trigger orders, so `max_ts` expiry does not apply to them, and
    // they would otherwise install as standalone triggers with no main entry, breaking the bundle's
    // atomicity. The check runs before the main order is placed, so the sidecars keep their ids,
    // and the main order keeps the trailing id that clients and `SignedMsgOrderRecord` rely on.
    if let Some(max_ts) = message.signed_msg_order_params.max_ts {
        if max_ts != 0 && max_ts < env.clock.unix_timestamp {
            msg!(
                "signed msg main order max_ts {} expired (< now {}); skipping bundle",
                max_ts,
                env.clock.unix_timestamp
            );

            return Ok((escrow_zc, None));
        }
    }

    let mut builder = BuilderRows {
        escrow: &mut escrow_zc,
        idx: message.builder_idx,
        fee_bps: builder_fee_bps,
    };

    // The sidecars go first. Each builder row is keyed to
    // `taker.next_order_id`, the id the placement assigns, so the main
    // order takes the trailing id.
    place_bracket_orders(&mut taker, &message, &mut builder, env)?;
    let placed = place_entry_order(&mut taker, &mut message, &mut order_id, &mut builder, env)?;

    // This function does not run `revoke_completed_orders`. The fill leg follows
    // in the same instruction and completes orders of its own, so the caller
    // revokes once, after it.
    Ok((escrow_zc, placed))
}

/// Authenticate the message and bind it to the taker account it names.
///
/// This function verifies the taker's signature inside the program, over the
/// message the argument carries. No preceding ed25519 precompile instruction is
/// required. A delegate signs for a named taker pubkey. An authority signs for a
/// subaccount, and the PDA of that subaccount must be the account passed.
fn verify_signed_msg(
    taker: &SignedMsgTaker<'_, '_>,
    message_bytes: &[u8],
    is_delegate_signer: bool,
) -> Result<VerifiedMessage> {
    let signer = message_signer(taker.user, is_delegate_signer)?;
    let message =
        verify_and_decode_signed_msg(message_bytes, &signer.to_bytes(), is_delegate_signer)?;

    if is_delegate_signer {
        validate!(
            message.delegate_signed_taker_pubkey == Some(taker.key),
            ErrorCode::SignedMsgUserContextUserMismatch,
            "Delegate signed msg for taker pubkey different than supplied pubkey"
        )?;
    } else {
        let taker_pda = Pubkey::find_program_address(
            &[
                "user".as_bytes(),
                &taker.user.authority.to_bytes(),
                &message.sub_account_id.unwrap().to_le_bytes(),
            ],
            &ID,
        );

        validate!(
            taker_pda.0 == taker.key,
            ErrorCode::SignedMsgUserContextUserMismatch,
            "Taker key does not match pda"
        )?;
    }

    Ok(message)
}

/// The key that must have signed the message.
///
/// A user with no delegate holds the all-zero key. That key is a small-order
/// point, so it cannot sign for anybody.
fn message_signer(user: &User, is_delegate_signer: bool) -> Result<Pubkey> {
    if !is_delegate_signer {
        return Ok(user.authority);
    }

    validate!(
        user.delegate != Pubkey::default(),
        ErrorCode::SigVerificationFailed,
        "a delegate-signed message names a user with no delegate"
    )?;

    Ok(user.delegate)
}

/// Decide whether the message may still be placed, and reserve its record.
///
/// `None` means the message is too old, already placed, or past its landing
/// deadline, [`crate::state::signed_msg_user::signed_msg_max_slot`]. Those are
/// no-ops rather than failures. The returned id carries that deadline as
/// `max_slot`. It bounds placement, not the order's life, which `max_ts`
/// bounds. The entry's own order id and route digest are written onto it
/// later, once the sidecars have taken their ids.
///
/// Immediate-or-cancel is allowed. This instruction routes and fills in the same
/// transaction, and it cancels the residual instead of storing it. Nothing of an
/// immediate-or-cancel order survives the call. A stored order would be the
/// hazard, because a limit order defaults `max_ts` to 0 and residual
/// cancellation exists only in the take and make fill modes.
fn signed_msg_order_slot(
    taker: &mut SignedMsgTaker<'_, '_>,
    message: &VerifiedMessage,
    env: &PlacementEnv<'_, '_>,
) -> Result<Option<SignedMsgOrderId>> {
    let params = &message.signed_msg_order_params;
    if params.market_type != MarketType::Perp {
        msg!("First order must be a perp taker order");
        return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
    }

    validate_entry_order_type(params)?;

    // A limit order rests from placement, and its message slot is its placement deadline. A client
    // stamps it ahead by its signing budget of about 14 seconds, so the lead is bounded.
    let is_resting_limit = params.order_type == OrderType::Limit;
    let max_resting_limit_lead = Millis::from_secs(30);
    // About 200 seconds of wall-clock age, integrated per slot duration regime.
    let max_order_age = Millis::from_secs(200);

    let order_slot = message.slot;
    if order_slot > env.clock.slot {
        if !is_resting_limit {
            msg!(
                "SignedMsg order slot {} is ahead of current slot {}",
                order_slot,
                env.clock.slot
            );

            return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
        }
        if env.state.slot_clock().elapsed(env.clock.slot, order_slot) > max_resting_limit_lead {
            msg!(
                "SignedMsg resting limit order slot {} is too far ahead: must be within {}ms of current slot {}",
                order_slot,
                max_resting_limit_lead.as_ms(),
                env.clock.slot
            );

            return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
        }
    }
    if env.state.slot_clock().elapsed(order_slot, env.clock.slot) > max_order_age {
        msg!(
            "SignedMsg order slot {} is too old: must be within {}ms of current slot {}",
            order_slot,
            max_order_age.as_ms(),
            env.clock.slot
        );

        return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
    }

    let max_slot = crate::state::signed_msg_user::signed_msg_max_slot(
        env.state.slot_clock(),
        order_slot,
        is_resting_limit,
    );

    if max_slot < env.clock.slot {
        msg!(
            "SignedMsg order max_slot {} < current slot {}",
            max_slot,
            env.clock.slot
        );

        return Ok(None);
    }

    let signed_msg_order_id = SignedMsgOrderId::new(message.uuid, max_slot, 0);
    if taker
        .orders
        .check_exists_and_prune_stale_signed_msg_order_ids(
            signed_msg_order_id,
            env.clock.slot,
            env.state.slot_clock(),
        )
    {
        msg!("SignedMsg order already exists for taker {:?}", taker.key);
        return Ok(None);
    }

    Ok(Some(signed_msg_order_id))
}

/// The entry must be able to take now or rest on the book. A post-only order
/// cannot take, and a trigger order needs a `User.orders` slot that the entry
/// never gets.
fn validate_entry_order_type(params: &OrderParams) -> Result<()> {
    if params.post_only != crate::state::order_params::PostOnlyParam::None {
        msg!("a signed-message entry cannot be post-only");
        return Err(print_error!(ErrorCode::InvalidOrderPostOnly)().into());
    }

    if params.is_trigger_order() {
        msg!("a signed-message entry cannot be a trigger order");
        return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
    }

    Ok(())
}

/// An unattested entry on a book with a speed bump rests whole, so it must be
/// able to rest. An immediate-or-cancel entry has nothing to rest.
fn validate_unattested_entry(order: &Order) -> Result<()> {
    validate!(
        !order.immediate_or_cancel,
        ErrorCode::UnattestedSynchronousTake,
        "an IOC signed-message entry needs attested flow on a book with a speed bump"
    )?;

    validate!(
        crate::instructions::restable_remainder_price(order, None).is_some(),
        ErrorCode::UnattestedSynchronousTake,
        "the entry cannot rest on the book and unattested flow cannot fill synchronously"
    )?;

    Ok(())
}

/// Apply the per-position settings the message carries. Those are the margin
/// ratio cap and the isolated-position deposit.
fn apply_message_position_settings(
    taker: &mut SignedMsgTaker<'_, '_>,
    message: &VerifiedMessage,
    env: &mut PlacementEnv<'_, '_>,
) -> Result<()> {
    let market_index = message.signed_msg_order_params.market_index;

    if let Some(max_margin_ratio) = message.max_margin_ratio {
        taker
            .user
            .update_perp_position_max_margin_ratio(market_index, max_margin_ratio)?;
    }

    #[cfg(feature = "isolated-position")]
    if let Some(isolated_position_deposit) = message.isolated_position_deposit {
        env.maps.spot_market_map.update_writable_spot_market(0)?;
        transfer_isolated_perp_position_deposit(
            taker.user,
            Some(taker.stats),
            env.maps,
            env.clock.slot,
            env.clock.unix_timestamp,
            0,
            market_index,
            isolated_position_deposit.cast::<i64>()?,
            env.state.funding_paused()?,
        )?;
    }
    #[cfg(not(feature = "isolated-position"))]
    {
        let _ = (&taker.stats, &env);
        validate!(
            message.isolated_position_deposit.is_none(),
            ErrorCode::IsolatedPositionDisabled,
            "signed msg isolated position deposit not enabled in this build"
        )?;
    }

    Ok(())
}

/// Install the reduce-only stop-loss and take-profit sidecars of a bundle.
///
/// Both sit opposite the entry. A long entry stops below and takes profit above.
/// A short entry is the mirror. The entry is not placed yet, so
/// `existing_position_direction_override` tells the margin walk which side it
/// will open.
fn place_bracket_orders(
    taker: &mut SignedMsgTaker<'_, '_>,
    message: &VerifiedMessage,
    builder: &mut BuilderRows<'_, '_>,
    env: &mut PlacementEnv<'_, '_>,
) -> Result<()> {
    let entry = message.signed_msg_order_params;
    let sidecars = [
        (
            message.stop_loss_order_params.as_ref(),
            OrderTriggerCondition::Below,
            OrderTriggerCondition::Above,
        ),
        (
            message.take_profit_order_params.as_ref(),
            OrderTriggerCondition::Above,
            OrderTriggerCondition::Below,
        ),
    ];

    for (params, on_long_entry, on_short_entry) in sidecars {
        let Some(params) = params else {
            continue;
        };
        let sidecar = OrderParams {
            order_type: OrderType::TriggerMarket,
            direction: entry.direction.opposite(),
            trigger_price: Some(params.trigger_price),
            base_asset_amount: params.base_asset_amount,
            trigger_condition: if entry.direction == PositionDirection::Long {
                on_long_entry
            } else {
                on_short_entry
            },

            market_index: entry.market_index,
            market_type: MarketType::Perp,
            reduce_only: true,
            ..OrderParams::default()
        };

        let mut builder_order = add_builder_order(
            builder.escrow,
            taker.user,
            builder.idx,
            builder.fee_bps,
            taker.user.next_order_id,
            entry.market_index,
        )?;

        controller::orders::place_perp_trigger_order(
            env.state,
            taker.user,
            taker.key,
            env.maps,
            env.clock,
            sidecar,
            PlaceOrderOptions {
                enforce_margin_check: false,
                existing_position_direction_override: Some(entry.direction),
                ..PlaceOrderOptions::default()
            },
            &mut builder_order,
        )?;
    }

    Ok(())
}

/// Build the entry order and record the route the taker signed.
///
/// The entry never enters `user.orders`. It is built, margin-checked, routed
/// straight to the book, and only its remainder rests on the CLOB. The route
/// goes onto the message's own record, because a keeper builds the fill
/// transaction and the route would otherwise be a suggestion it can ignore.
/// The record outlives the message, so a fill in a later transaction is still
/// held to it.
///
/// `None` means the order soft-skipped its build, so there is nothing to fill
/// or rest.
fn place_entry_order(
    taker: &mut SignedMsgTaker<'_, '_>,
    message: &mut VerifiedMessage,
    order_id: &mut SignedMsgOrderId,
    builder: &mut BuilderRows<'_, '_>,
    env: &mut PlacementEnv<'_, '_>,
) -> Result<Option<PlacedSignedMsgOrder>> {
    let entry = message.signed_msg_order_params;

    order_id.order_id = taker.user.next_order_id;
    order_id.route_digest = message
        .route
        .as_deref()
        .map(crate::state::order_params::route_digest)
        .unwrap_or(crate::state::order_params::NO_ROUTE_DIGEST);
    taker
        .orders
        .add_signed_msg_order_id(*order_id, env.clock.slot, env.state.slot_clock())?;

    let mut builder_order = add_builder_order(
        builder.escrow,
        taker.user,
        builder.idx,
        builder.fee_bps,
        taker.user.next_order_id,
        entry.market_index,
    )?;

    // Sweep expired slot orders first. Their reservations release, and that
    // release can be what lets the new order pass the margin gate. The create
    // never touches `user.orders`, so the caller owns the sweep.
    controller::orders::expire_orders(
        taker.user,
        &taker.key,
        env.maps,
        env.clock.unix_timestamp,
        env.clock.slot,
    )?;

    let Some(order) = controller::orders::create_detached_perp_order(
        env.state,
        taker.user,
        taker.key,
        env.maps,
        env.clock,
        entry,
        PlaceOrderOptions {
            enforce_margin_check: true,
            signed_msg_taker_order_slot: Some(message.slot),
            ..PlaceOrderOptions::default()
        },
        &mut builder_order,
    )?
    else {
        return Ok(None);
    };

    // `signature` is `[u8; 64]`, and borsh serializes it as its raw bytes. Hash
    // the array directly instead of allocating an identical copy.
    let order_params_hash = base64::encode(solana_program::hash::hash(&message.signature).as_ref());

    emit!(SignedMsgOrderRecord {
        user: taker.key,
        signed_msg_order_max_slot: order_id.max_slot,
        signed_msg_order_uuid: order_id.uuid,
        user_order_id: order_id.order_id,
        matching_order_params: entry,
        hash: order_params_hash,
        ts: env.clock.unix_timestamp,
    });

    Ok(Some(PlacedSignedMsgOrder {
        order_id: order_id.order_id,
        order,
        uuid: order_id.uuid,
        market_index: entry.market_index,
        route_digest: order_id.route_digest,
        route: message.route.take().unwrap_or_default(),
        is_immediate_or_cancel: entry.is_immediate_or_cancel(),
        activation_delay_slots: entry.activation_delay_slots,
    }))
}

/// What the placement leg hands the fill leg.
///
/// The placement borrows the taker's `User` for its whole body, and the router fill takes the
/// loader instead, so the two cannot share one borrow. This carries what crosses it.
pub struct PlacedSignedMsgOrder {
    pub order_id: u32,
    /// The detached taker order. It never enters `user.orders`. The fill leg
    /// routes it detached and mutates its filled amounts here. The rest leg
    /// reads its remainder from here to migrate onto the CLOB.
    pub order: crate::state::user::Order,
    pub uuid: [u8; 8],
    pub market_index: u16,
    /// The custom quoters the taker's message named. The fill must carry every
    /// one of them. The CLOB and the vAMM are the baseline, so the list omits
    /// them.
    pub route: Vec<Pubkey>,
    /// The digest of `route`. The placement computes it once. The fill leg and
    /// the rest leg both hold the quoters they carry against it, so it is cached
    /// and not re-hashed at each one.
    pub route_digest: crate::state::order_params::RouteDigest,
    /// The taker asked for no remainder to rest.
    pub is_immediate_or_cancel: bool,
    /// The speed bump the remainder rests behind. `None` takes the book's
    /// default.
    pub activation_delay_slots: Option<u32>,
}

#[derive(Accounts)]
pub struct PlaceSignedMsgTakerOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(
        mut,
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), user.load()?.authority.as_ref()],
        bump,
    )]
    /// CHECK: checked in SignedMsgUserOrdersZeroCopy checks
    pub signed_msg_user_orders: UncheckedAccount<'info>,
    pub authority: Signer<'info>,
    /// CHECK: The address check is needed because otherwise
    /// the supplied Sysvar could be anything else.
    /// The Instruction Sysvar has not been implemented
    /// in the Anchor framework yet, so this is the safe approach.
    #[account(address = IX_ID)]
    pub ix_sysvar: UncheckedAccount<'info>,
    /// The keeper's own `User`, credited for the fill it lands. The taker did
    /// not sign this transaction, so the keeper is a filler and owes the taker
    /// every maker it had room to carry.
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The market's quoter slab. The remainder only ever rests on the vetted
    /// book its `Clob` slot names, and that book is the mandatory baseline of
    /// a router fill.
    #[account(has_one = clob_market)]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: registration pins a Clob slot's program to velocity's CLOB. The
    /// handler re-checks it through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}
