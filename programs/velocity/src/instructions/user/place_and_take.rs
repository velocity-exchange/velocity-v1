//! Placing an order and filling it in one instruction.
//!
//! The order stays on the stack. The router fills it against every liquidity
//! source the market has, and whatever is left rests on the market's CLOB.
//! The caller passes no counterparties: the book names the makers a fill
//! settles against, and the transaction carries those user accounts so the
//! settlement can reach them.

use super::*;

/// The accounts a place-and-take route acts on.
pub struct PlaceAndTakeAccounts<'a, 'info> {
    pub state: &'a AccountLoader<'info, State>,
    pub user: &'a AccountLoader<'info, User>,
    pub user_stats: &'a AccountLoader<'info, UserStats>,
    pub remaining_accounts: &'info [AccountInfo<'info>],
}

/// What the caller asked a v1 take to do.
/// `taker_served_window` and `synchronous_take` come from the caller's accounts.
/// Attested flow fills synchronously. Unattested flow on a speed-bumped book
/// rests the order whole instead, which is maker priority.
pub struct PlaceAndTakeRequest {
    pub params: OrderParams,
    pub optional_params: Option<u32>,
    pub taker_served_window: bool,
    pub synchronous_take: bool,
}

/// The CLOB accounts a V1 taker route carries, so an unfilled restable
/// remainder can migrate onto the book instead of being cancelled. Built by
/// `instructions::clob::place_and_take_v1` and the keeper's
/// keeper route.
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

/// Everything the router needs to fill one detached taker order.
struct DetachedTake<'a, 'info> {
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

/// An unattested taker on a bumped book rests whole and fills through the
/// activation-slot auction. A shape that demands a synchronous outcome cannot
/// have one, so it is refused rather than rested. An IOC has nothing to rest.
/// A success condition measures a fill this transaction does not perform.
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

/// Maker priority rests the order whole, and the cross cranks fill it through
/// the activation-slot auction. An order that cannot rest would do nothing, so
/// it is refused instead. An `OrderType::Oracle` taker trips this. Its bound
/// floats with the oracle, so it has no fixed price to rest at.
fn validate_order_can_rest(order: &Order) -> Result<()> {
    validate!(
        crate::instructions::restable_remainder_price(order, None).is_some(),
        ErrorCode::UnattestedSynchronousTake,
        "the order cannot rest on the book and unattested flow cannot fill synchronously"
    )?;

    Ok(())
}

/// Build the detached taker order, and return the escrow and referral status
/// the fill still needs. Returns `None` for the order when nothing was built:
/// an order whose `max_ts` already passed builds nothing.
fn create_detached_take<'a>(
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

    let order = controller::orders::create_detached_perp_order(
        state,
        &mut user,
        user_key,
        maps,
        clock,
        params,
        PlaceOrderOptions::default(),
        &mut builder_order,
    )?;

    // `builder_order` borrows `escrow`. That borrow ends at its last use above, so
    // `escrow` is free to borrow again for the fill below.
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

/// Describe the take to the quoters, then size and quote it. The description
/// carries what the take wants, at what bound, and which loaded users the
/// quoters may fill it against.
fn quote_take_route<'a, 'info>(
    take: &mut DetachedTake<'_, 'info>,
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
        include_taker_origin_reservations: false,
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

/// Fill the detached order against the route the router just priced.
fn fill_against_route(
    take: &mut DetachedTake<'_, '_>,
    router: &mut crate::math::router::RouterLeg<'_, '_, '_>,
    state: &State,
    mode: FillMode,
    referrer_is_accelerated: bool,
) -> Result<u64> {
    let filled = controller::orders::fill_perp_order(
        controller::orders::FillRequest {
            // Detached taker: it never reserved, so the fill unwinds
            // nothing.
            order: take.order,
            reserved: false,
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

/// Quote the route the taker named and fill the detached order against it.
/// Returns the base filled.
///
/// The taker signed a transaction naming the registry entries it wants
/// consulted, so the accounts it passed are its route. No third party chose
/// that route, so nothing here needs to constrain one. The keeper path and the
/// signed route carry that problem.
fn fill_detached_take(
    take: &mut DetachedTake<'_, '_>,
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
/// An unfilled IOC needs no cancel. The order is detached and never
/// persisted, so dropping it is enough. A restable remainder lives on the book
/// and not in `User.orders`, so it migrates instead. `restable_remainder_price`
/// is the whole rule, shared with every keeper route. Otherwise a remainder's
/// fate depends on which route reached it, and a remainder left in a slot is
/// an order nothing fills.
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

/// The v1 `place_and_take` body. The taker order is detached: it is built on
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

    let (order, mut escrow, referrer_is_accelerated) = create_detached_take(
        &accounts,
        remaining_accounts_iter,
        &mut maps,
        &state,
        &clock,
        params,
    )?;

    let Some(mut detached_order) = order else {
        // An order whose `max_ts` already passed builds nothing. It is the one
        // soft skip reachable here. The other skip, a failed try-post-only, is
        // refused above. Nothing was placed or filled, so enforce the success
        // condition against an empty take and stop.
        return validate_place_and_take_success_condition(success_condition, 0, false);
    };

    let mode = FillMode::PlaceAndTake(
        is_immediate_or_cancel || request.optional_params.is_some(),
        auction_duration_percentage,
    );

    let base_asset_amount_filled = if !request.synchronous_take {
        validate_order_can_rest(&detached_order)?;
        0u64
    } else {
        // The tail is a subslice, not a collected list. What the sections above
        // consumed is the difference in the iterator's remaining length. A
        // collected list clones every account, and the subslice clones none.
        let tail_from = accounts.remaining_accounts.len() - remaining_accounts_iter.len();
        fill_detached_take(
            &mut DetachedTake {
                order: &mut detached_order,
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
        &detached_order,
        &TakeOutcome {
            base_asset_amount_filled,
            is_immediate_or_cancel,
            success_condition,
        },
    )
}
