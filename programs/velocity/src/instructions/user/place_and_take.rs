//! Placing an order and filling it in one instruction.
//!
//! Two bodies share this file because they share a wire shape and a success
//! condition. The v0 body writes the order into `User.orders` and fills it
//! against the vAMM and the DLOB makers the caller passed. The v1 body keeps
//! the order on the stack, fills it through the router, and rests whatever is
//! left on the market's CLOB.

use super::*;

/// The accounts a place-and-take route acts on. Both bodies take the same set,
/// so a caller that switches routes does not rebuild its account list.
pub struct PlaceAndTakeAccounts<'a, 'info> {
    pub state: &'a AccountLoader<'info, State>,
    pub user: &'a AccountLoader<'info, User>,
    pub user_stats: &'a AccountLoader<'info, UserStats>,
    pub remaining_accounts: &'info [AccountInfo<'info>],
}

/// What the caller asked a v1 take to do.
///
/// `taker_served_window` and `synchronous_take` come from the caller's
/// accounts. Attested flow (the flow authority signed) fills synchronously.
/// Unattested flow on a book with a speed bump rests the order whole instead,
/// which is maker priority.
pub struct PlaceAndTakeRequest {
    pub params: OrderParams,
    /// A `u32` for wire compatibility with v0.
    pub optional_params: Option<u32>,
    pub taker_served_window: bool,
    pub synchronous_take: bool,
}

/// The CLOB accounts a V1 taker route carries, so an unfilled restable
/// remainder can migrate onto the book instead of being cancelled. Built by
/// `instructions::clob::place_and_take_v1` and the keeper's
/// `fill_legacy_dlob_order` route.
pub struct ClobRemainderRoute<'a, 'info> {
    pub quoter_slab: &'a AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    pub clob_market: &'a AccountInfo<'info>,
    pub clob_program: &'a AccountInfo<'info>,
}

/// What the take asks the router for: its direction, the base still unfilled,
/// the taker's book reference, and the price bound the router must respect.
struct TakeShape {
    direction: crate::state::prop_amm::Direction,
    unfilled: u64,
    taker: crate::state::prop_amm::ClobUserRefV0,
    limit_price: u64,
}

/// Everything the router needs to fill one ephemeral taker order.
struct EphemeralTake<'a, 'info> {
    order: &'a mut Order,
    accounts: &'a PlaceAndTakeAccounts<'a, 'info>,
    maps: &'a mut AccountMaps<'info>,
    makers: &'a UserMap<'info>,
    maker_stats: &'a UserStatsMap<'info>,
    escrow: &'a mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
    /// The registry entries the taker named: the tail of `remaining_accounts`
    /// that the sections above did not consume.
    tail: &'info [AccountInfo<'info>],
    market_index: u16,
}

/// What a take filled, and what the caller demanded of it.
struct TakeOutcome {
    base_asset_amount_filled: u64,
    is_immediate_or_cancel: bool,
    success_condition: u8,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_legacy_place_and_take_perp_order<'c: 'info, 'info>(
    ctx: Context<'info, PlaceAndTake<'info>>,
    params: OrderParams,
    optional_params: Option<u32>, // u32 for backwards compatibility
) -> Result<()> {
    place_and_take_perp_order_legacy(
        PlaceAndTakeAccounts {
            state: &ctx.accounts.state,
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            remaining_accounts: ctx.remaining_accounts,
        },
        params,
        optional_params,
    )
}

/// Enforce the caller's success condition against what the take filled. The
/// legacy and v1 bodies share the same wire encoding for the condition, so
/// they share the rule.
fn validate_place_and_take_success_condition(
    success_condition: u8,
    base_asset_amount_filled: u64,
    order_unfilled: bool,
) -> Result<()> {
    if success_condition == PlaceAndTakeOrderSuccessCondition::PartialFill as u8 {
        validate!(
            base_asset_amount_filled > 0,
            ErrorCode::PlaceAndTakeOrderSuccessConditionFailed,
            "no partial fill"
        )?;
    } else if success_condition == PlaceAndTakeOrderSuccessCondition::FullFill as u8 {
        validate!(
            base_asset_amount_filled > 0 && !order_unfilled,
            ErrorCode::PlaceAndTakeOrderSuccessConditionFailed,
            "no full fill"
        )?;
    }
    Ok(())
}

/// A take may not post. A post-only order rests rather than crosses, so it can
/// never be what this instruction is for.
fn validate_take_is_not_post_only(params: &OrderParams) -> Result<()> {
    if params.post_only != PostOnlyParam::None {
        msg!("post_only cant be used in place_and_take");
        return Err(print_error!(ErrorCode::InvalidOrderPostOnly)().into());
    }

    Ok(())
}

/// Load the revenue-share escrow the take reads and price the builder code the
/// order carries.
///
/// The escrow follows the market, oracle, and maker accounts in
/// `remaining_accounts`. It is loaded before the order is placed so the order
/// can be tagged with its builder, and the same escrow is reused for the fill.
/// The borrowing validator is used so the escrow survives even when this order
/// carries no builder code, because the fill still needs it for referral
/// revenue share.
fn load_taker_escrow<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    user: &User,
    params: &OrderParams,
    state: &State,
) -> Result<(Option<RevenueShareEscrowZeroCopyMut<'a>>, Option<u16>, bool)> {
    let mut escrow = if state.builder_codes_enabled() {
        get_revenue_share_escrow_account(account_info_iter, &user.authority)?
    } else {
        None
    };

    let builder_fee_bps = validate_builder_fee(
        escrow.as_mut(),
        &user.authority,
        params.builder_idx,
        params.builder_fee_tenth_bps,
        state,
    )?;

    let referrer_is_accelerated =
        get_referrer_accelerated_status(account_info_iter, escrow.as_ref())?;

    Ok((escrow, builder_fee_bps, referrer_is_accelerated))
}

/// Place the legacy taker order into `User.orders` and return its id, together
/// with the escrow and referral status the fill still needs.
fn place_legacy_take_order<'a>(
    accounts: &PlaceAndTakeAccounts<'_, 'a>,
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    maps: &mut AccountMaps<'a>,
    state: &State,
    clock: &Clock,
    params: OrderParams,
) -> Result<(u32, Option<RevenueShareEscrowZeroCopyMut<'a>>, bool)> {
    let user_key = accounts.user.key();
    let mut user = load_mut!(accounts.user)?;

    let (mut escrow, builder_fee_bps, referrer_is_accelerated) =
        load_taker_escrow(account_info_iter, &user, &params, state)?;

    let next_order_id = user.next_order_id;
    let mut builder_order = add_builder_order(
        &mut escrow,
        &user,
        params.builder_idx,
        builder_fee_bps,
        next_order_id,
        params.market_index,
    )?;

    controller::orders::place_perp_order(
        state,
        &mut user,
        user_key,
        maps,
        clock,
        params,
        PlaceOrderOptions::default(),
        &mut builder_order,
    )?;

    // `builder_order` borrows `escrow`; its borrow ends here at its last use (above), freeing
    // `escrow` to be re-borrowed for the fill below.
    Ok((user.get_last_order_id(), escrow, referrer_is_accelerated))
}

/// The v0 `place_and_take` body, ABI-frozen with the DLOB it fills against.
/// The order is placed into `User.orders`, filled against the vAMM and the
/// passed DLOB makers, and an unfilled IOC is cancelled from the slot it
/// occupies. This path is deleted with the DLOB.
pub fn place_and_take_perp_order_legacy<'info>(
    accounts: PlaceAndTakeAccounts<'_, 'info>,
    params: OrderParams,
    optional_params: Option<u32>, // u32 for backwards compatibility
) -> Result<()> {
    let clock = Clock::get()?;
    let state = accounts.state.load()?;

    let remaining_accounts_iter = &mut accounts.remaining_accounts.iter().peekable();
    let mut maps = load_one_perp_market_maps(
        remaining_accounts_iter,
        &state,
        params.market_index,
        clock.slot,
    )?;

    validate_take_is_not_post_only(&params)?;

    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;

    let is_immediate_or_cancel = params.is_immediate_or_cancel();

    // No `update_amm` here: `fill_perp_order_without_external_books` (called
    // below) snaps the AMM
    // and refreshes PerpMarket-level oracle stats internally before
    // reading peg / reserves.

    let (success_condition, auction_duration_percentage) = parse_optional_params(optional_params);

    let (order_id, mut escrow, referrer_is_accelerated) = place_legacy_take_order(
        &accounts,
        remaining_accounts_iter,
        &mut maps,
        &state,
        &clock,
        params,
    )?;

    let fill_mode = FillMode::PlaceAndTake(
        is_immediate_or_cancel || optional_params.is_some(),
        auction_duration_percentage,
    );
    let filled = controller::orders::fill_perp_order_without_external_books(
        order_id,
        &state,
        accounts.user,
        accounts.user_stats,
        &mut maps,
        &accounts.user.clone(),
        &accounts.user_stats.clone(),
        &makers_and_referrer,
        &makers_and_referrer_stats,
        &Clock::get()?,
        fill_mode,
        &mut escrow.as_mut(),
        referrer_is_accelerated,
    )?;

    let order_unfilled = load!(accounts.user)?
        .orders
        .iter()
        .any(|order| order.order_id == order_id && order.status == OrderStatus::Open);

    if is_immediate_or_cancel && order_unfilled {
        controller::orders::cancel_order_by_order_id(
            order_id,
            accounts.user,
            &mut maps,
            &Clock::get()?,
        )?;
    }

    validate_place_and_take_success_condition(success_condition, filled.base, order_unfilled)
}

/// An unattested taker on a bumped book rests whole and fills through the
/// activation-slot auction. A shape that demands a synchronous outcome cannot
/// have one, so it is refused rather than silently rested: an IOC has nothing
/// to rest, and a success condition measures a fill this transaction will not
/// perform.
fn validate_unattested_take(request: &PlaceAndTakeRequest) -> Result<()> {
    if request.synchronous_take {
        return Ok(());
    }

    validate!(
        !request.params.is_immediate_or_cancel(),
        ErrorCode::UnattestedSynchronousTake,
        "an IOC take needs attested flow on a book with a speed bump"
    )?;

    validate!(
        parse_optional_params(request.optional_params).0 == 0,
        ErrorCode::UnattestedSynchronousTake,
        "a success condition needs attested flow on a book with a speed bump"
    )?;

    Ok(())
}

/// Maker priority: the order rests whole and the cross cranks fill it through
/// the activation-slot auction. An order that cannot rest would silently do
/// nothing, so it is refused instead. The shape that trips this is an
/// `OrderType::Oracle` taker: its bound floats with the oracle, so it has no
/// fixed price to rest at.
fn validate_order_can_rest(order: &Order) -> Result<()> {
    validate!(
        crate::instructions::restable_remainder_price(order, None).is_some(),
        ErrorCode::UnattestedSynchronousTake,
        "the order cannot rest on the book and unattested flow cannot fill synchronously"
    )?;

    Ok(())
}

/// Build the ephemeral taker order, and return the escrow and referral status
/// the fill still needs. Returns `None` for the order when nothing was built:
/// an order whose `max_ts` already passed builds nothing.
fn create_ephemeral_take<'a>(
    accounts: &PlaceAndTakeAccounts<'_, 'a>,
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    maps: &mut AccountMaps<'a>,
    state: &State,
    clock: &Clock,
    params: OrderParams,
) -> Result<(
    Option<Order>,
    Option<RevenueShareEscrowZeroCopyMut<'a>>,
    bool,
)> {
    let user_key = accounts.user.key();
    let mut user = load_mut!(accounts.user)?;

    let (mut escrow, builder_fee_bps, referrer_is_accelerated) =
        load_taker_escrow(account_info_iter, &user, &params, state)?;

    let next_order_id = user.next_order_id;
    let mut builder_order = add_builder_order(
        &mut escrow,
        &user,
        params.builder_idx,
        builder_fee_bps,
        next_order_id,
        params.market_index,
    )?;

    // Sweep expired slot orders first: their reservations release, which
    // can be what lets the new order pass the margin gate. The create
    // never touches `user.orders`, so the sweep is the caller's.
    controller::orders::expire_orders(
        &mut user,
        &user_key,
        maps,
        clock.unix_timestamp,
        clock.slot,
    )?;

    let order = controller::orders::create_ephemeral_perp_order(
        state,
        &mut user,
        user_key,
        maps,
        clock,
        params,
        PlaceOrderOptions::default(),
        &mut builder_order,
    )?;

    // `builder_order` borrows `escrow`; its borrow ends here at its last use (above), freeing
    // `escrow` to be re-borrowed for the fill below.
    Ok((order, escrow, referrer_is_accelerated))
}

/// Read what the take asks the router for.
fn read_take_shape(
    order: &Order,
    user_loader: &AccountLoader<'_, User>,
    maps: &AccountMaps,
    mode: FillMode,
    clock: &Clock,
    state: &State,
) -> Result<TakeShape> {
    let user = load!(user_loader)?;
    let position_base = user
        .get_perp_position(order.market_index)
        .map(|position| position.base_asset_amount)
        .ok();

    Ok(TakeShape {
        direction: match order.direction {
            PositionDirection::Long => crate::state::prop_amm::Direction::Long,
            PositionDirection::Short => crate::state::prop_amm::Direction::Short,
        },
        unfilled: order.get_base_asset_amount_unfilled(position_base)?,
        taker: user.clob_user_ref(),
        limit_price: mode.quote_limit_price(
            order,
            clock.slot,
            maps.perp_market_map
                .get_ref(&order.market_index)?
                .order_tick_size,
            state.slot_clock(),
        ),
    })
}

/// Describe the take to the quoters — what it wants, at what bound, and
/// which loaded users they may fill it against — then size and quote it.
fn quote_take_route<'a, 'info>(
    take: &mut EphemeralTake<'_, 'info>,
    users: &'a [crate::state::prop_amm::ClobUserRefV0],
    shape: &TakeShape,
    mark: &RouteMark,
    taker_served_window: bool,
    clock: &Clock,
    scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<crate::instructions::RouteQuote<'a, 'info>> {
    let taker_key = take.accounts.user.key();
    let inputs = crate::instructions::QuoteInputs {
        market_index: take.market_index,
        direction: shape.direction,
        size: shape.unfilled,
        users,
        reference_price: mark.reference_price,
        margin_ratio_initial: mark.margin_ratio_initial,
        taker: shape.taker,
        limit_price: shape.limit_price,
        taker_served_window,
        consume_reservation: false,
    };

    crate::instructions::quote_route(
        take.tail,
        inputs,
        // The taker signed this transaction, so they picked the account list
        // themselves and no route binds the filler.
        None,
        &mut crate::instructions::CapInputs {
            taker_key: &taker_key,
            makers_and_referrer: take.makers,
            makers_and_referrer_stats: take.maker_stats,
            maps: take.maps,
            slot: clock.slot,
            now: clock.unix_timestamp,
        },
        scratch,
    )
}

/// The market facts a route is priced against: the mark a capped maker's
/// loss is measured from, and the band a quoter's levels default to.
struct RouteMark {
    reference_price: i64,
    margin_ratio_initial: u32,
}

/// Fill the ephemeral order against the route the router just priced.
fn fill_against_route(
    take: &mut EphemeralTake<'_, '_>,
    router: &mut crate::math::router::RouterLeg<'_, '_, '_>,
    state: &State,
    mode: FillMode,
    referrer_is_accelerated: bool,
) -> Result<u64> {
    let filled = controller::orders::fill_perp_order(
        controller::orders::FillRequest {
            // Ephemeral taker: it never reserved, so the fill unwinds
            // nothing.
            target: controller::orders::FillTarget::Detached {
                order: take.order,
                reserved: false,
            },
            mode,
            referrer_is_accelerated,
        },
        state,
        &Clock::get()?,
        controller::orders::PerpFillAccounts {
            user: take.accounts.user,
            user_stats: take.accounts.user_stats,
            filler: &take.accounts.user.clone(),
            filler_stats: &take.accounts.user_stats.clone(),
            rev_share_escrow: &mut take.escrow.as_mut(),
        },
        &mut controller::orders::FillParties {
            maps: take.maps,
            makers_and_referrer: take.makers,
            makers_and_referrer_stats: take.maker_stats,
        },
        router,
    )?;

    Ok(filled.base)
}

/// Quote the route the taker named and fill the ephemeral order against it.
/// Returns the base filled.
///
/// The taker signed a transaction naming the registry entries it wants
/// consulted, so the accounts it passed *are* its route. There is no third
/// party whose choice needs constraining; that is the keeper path's problem,
/// and the signed route's.
fn fill_ephemeral_take(
    take: &mut EphemeralTake<'_, '_>,
    state: &State,
    clock: &Clock,
    mode: FillMode,
    taker_served_window: bool,
    referrer_is_accelerated: bool,
) -> Result<u64> {
    let market_index = take.market_index;
    let shape = read_take_shape(
        take.order,
        take.accounts.user,
        take.maps,
        mode,
        clock,
        state,
    )?;

    let mark = {
        let market = take.maps.perp_market_map.get_ref(&market_index)?;
        let oracle_id = market.oracle_id();
        let margin_ratio_initial = market.margin_ratio_initial;
        drop(market);
        RouteMark {
            reference_price: take.maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        }
    };

    let users =
        crate::state::prop_amm::quoter_wire_users(take.makers.user_ref_index()?.into_keys().map(
            |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                authority,
                sub_account_id,
            },
        ))?;

    // One set of CPI buffers for the fill: the quote legs below and the
    // execute legs the router runs later all refill the same allocation,
    // because velocity's heap never gives a freed one back.
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let quoted = quote_take_route(
        take,
        &users,
        &shape,
        &mark,
        taker_served_window,
        clock,
        &mut cpi_scratch,
    )?;
    let mut books = quoted.books(clock, &mut cpi_scratch)?;
    let mut router = books.for_fill(crate::instructions::FillerStanding {
        protocol_authority: state.signer,
        taker_exposure_closed_by_caller: false,
        // The taker signs a place-and-take, so the taker chose the account
        // list and no filler obligation applies.
        obligation: crate::math::router::FillerObligation {
            taker_signed: true,
            tx_accounts: None,
            unrouted_quoters: 0,
        },
    });
    fill_against_route(take, &mut router, state, mode, referrer_is_accelerated)
}

/// Rest what the take did not fill, then hold the caller's success condition
/// against the result.
///
/// An unfilled IOC needs no cancel: the order is ephemeral, it never persisted,
/// so dropping it is enough. A restable remainder lives on the book, not in
/// `User.orders`, so it is migrated instead of dropped.
/// `restable_remainder_price` is the whole rule, shared with the keeper fill
/// route. A remainder that rests on one route and not the other is a remainder
/// whose fate depends on which one reached it, and once the DLOB is gone the
/// one that does not rest is an order nothing will fill.
///
/// Any can't-rest outcome downgrades to a cancel rather than reverting the fill
/// that already landed. The CLOB's `OrderRef` is left as the transaction's
/// return data for the client to persist as its cancel hint.
fn settle_take_remainder<'info>(
    accounts: &PlaceAndTakeAccounts<'_, 'info>,
    clob: &ClobRemainderRoute<'_, 'info>,
    maps: &mut AccountMaps,
    order: &Order,
    outcome: &TakeOutcome,
) -> Result<()> {
    let order_unfilled = {
        let user = load!(accounts.user)?;
        let position_base = user
            .get_perp_position(order.market_index)
            .map(|position| position.base_asset_amount)
            .unwrap_or(0);
        order
            .get_base_asset_amount_unfilled(Some(position_base))
            .unwrap_or(0)
            > 0
    };

    if !outcome.is_immediate_or_cancel && order_unfilled {
        let remainder = load!(accounts.user).ok().and_then(|user| {
            crate::instructions::restable_remainder(&user, order, order.market_index, None)
        });
        if let Some(remainder) = remainder {
            if remainder.unfilled > 0 {
                crate::instructions::try_place_remainder_on_clob(
                    accounts.user,
                    clob.quoter_slab,
                    clob.clob_market,
                    clob.clob_program,
                    maps,
                    order.market_index,
                    remainder.direction,
                    remainder.price,
                    remainder.unfilled,
                    remainder.max_ts,
                    order.order_id,
                    true,
                    false,
                    remainder.reduce_only,
                    None,
                    &Clock::get()?,
                )?;
            }
        }
    }

    validate_place_and_take_success_condition(
        outcome.success_condition,
        outcome.base_asset_amount_filled,
        order_unfilled,
    )
}

/// The v1 `place_and_take` body. The taker order is ephemeral: it is built on
/// the stack, margin-checked, filled through the router, and never written into
/// `User.orders`. A restable remainder rests on the market's CLOB.
pub fn place_and_take_perp_order_v1<'info>(
    accounts: PlaceAndTakeAccounts<'_, 'info>,
    request: PlaceAndTakeRequest,
    clob: ClobRemainderRoute<'_, 'info>,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = accounts.state.load()?;
    let params = request.params;

    let remaining_accounts_iter = &mut accounts.remaining_accounts.iter().peekable();
    let mut maps = load_one_perp_market_maps(
        remaining_accounts_iter,
        &state,
        params.market_index,
        clock.slot,
    )?;

    validate_take_is_not_post_only(&params)?;
    validate_unattested_take(&request)?;

    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;

    let is_immediate_or_cancel = params.is_immediate_or_cancel();

    // No `update_amm` here: the router fill snaps the AMM and refreshes
    // PerpMarket-level oracle stats internally before reading peg /
    // reserves.

    let (success_condition, auction_duration_percentage) =
        parse_optional_params(request.optional_params);

    let (order, mut escrow, referrer_is_accelerated) = create_ephemeral_take(
        &accounts,
        remaining_accounts_iter,
        &mut maps,
        &state,
        &clock,
        params,
    )?;

    let Some(mut ephemeral_order) = order else {
        // The one soft-skip reachable on this route: an order whose `max_ts`
        // already passed builds nothing (the other skip, a failed
        // try-post-only, cannot reach here — place_and_take refuses every
        // post-only above). Nothing was placed or filled, so enforce the
        // success condition against an empty take and stop.
        return validate_place_and_take_success_condition(success_condition, 0, false);
    };

    let mode = FillMode::PlaceAndTake(
        is_immediate_or_cancel || request.optional_params.is_some(),
        auction_duration_percentage,
    );

    let base_asset_amount_filled = if !request.synchronous_take {
        validate_order_can_rest(&ephemeral_order)?;
        0u64
    } else {
        // The tail as a subslice rather than a collected list: what the sections
        // above consumed is the difference in the iterator's remaining length, and
        // borrowing from there costs nothing where cloning every account did.
        let tail_from = accounts.remaining_accounts.len() - remaining_accounts_iter.len();
        fill_ephemeral_take(
            &mut EphemeralTake {
                order: &mut ephemeral_order,
                accounts: &accounts,
                maps: &mut maps,
                makers: &makers_and_referrer,
                maker_stats: &makers_and_referrer_stats,
                escrow: &mut escrow,
                tail: &accounts.remaining_accounts[tail_from..],
                market_index: params.market_index,
            },
            &state,
            &clock,
            mode,
            request.taker_served_window,
            referrer_is_accelerated,
        )?
    };

    settle_take_remainder(
        &accounts,
        &clob,
        &mut maps,
        &ephemeral_order,
        &TakeOutcome {
            base_asset_amount_filled,
            is_immediate_or_cancel,
            success_condition,
        },
    )
}

#[derive(Accounts)]
pub struct PlaceAndTake<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct PlaceAndMatchRFQOrders<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    /// CHECK: The address check is needed because otherwise
    /// the supplied Sysvar could be anything else.
    /// The Instruction Sysvar has not been implemented
    /// in the Anchor framework yet, so this is the safe approach.
    #[account(address = IX_ID)]
    pub ix_sysvar: UncheckedAccount<'info>,
}
