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

/// What the caller asked a v1 take to do. Only a verified signed-message
/// attestation sets `taker_served_window`. Unattested flow on a speed-bumped
/// book rests the order whole instead of filling, which is maker priority.
pub struct PlaceAndTakeRequest {
    pub params: OrderParams,
    pub success_condition: Option<PlaceAndTakeOrderSuccessCondition>,
    pub taker_served_window: bool,
    pub synchronous_take: bool,
}

/// The CLOB accounts a v1 take carries, so an unfilled restable remainder can
/// migrate onto the book instead of being cancelled.
pub struct ClobRemainderRoute<'a, 'info> {
    pub quoter_slab: &'a AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    pub clob_market: &'a AccountInfo<'info>,
    pub clob_program: &'a AccountInfo<'info>,
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
}

/// What a take filled, and what the caller demanded of it.
struct TakeOutcome {
    base_asset_amount_filled: u64,
    is_immediate_or_cancel: bool,
    success_condition: Option<PlaceAndTakeOrderSuccessCondition>,
    activation_delay_slots: Option<u32>,
}

/// Enforce the caller's success condition against what the take filled.
fn validate_place_and_take_success_condition(
    success_condition: Option<PlaceAndTakeOrderSuccessCondition>,
    base_asset_amount_filled: u64,
    order_unfilled: bool,
) -> Result<()> {
    match success_condition {
        Some(PlaceAndTakeOrderSuccessCondition::PartialFill) => validate!(
            base_asset_amount_filled > 0,
            ErrorCode::PlaceAndTakeOrderSuccessConditionFailed,
            "no partial fill"
        )?,
        Some(PlaceAndTakeOrderSuccessCondition::FullFill) => validate!(
            base_asset_amount_filled > 0 && !order_unfilled,
            ErrorCode::PlaceAndTakeOrderSuccessConditionFailed,
            "no full fill"
        )?,
        None => {}
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
/// activation window. A shape that demands a synchronous outcome cannot
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
        request.success_condition.is_none(),
        ErrorCode::UnattestedSynchronousTake,
        "a success condition needs attested flow on a book with a speed bump"
    )?;

    Ok(())
}

/// Maker priority rests the order whole, and the cross cranks fill it through
/// the activation window. An order that cannot rest would do nothing, so
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
    let order = crate::instructions::RoutedOrder::read(
        &*load!(take.accounts.user)?,
        take.order,
        take.maps,
        mode,
    )?;

    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let filled = crate::instructions::RouteFill {
        state,
        clock,
        tail: take.tail,
        scratch: &mut cpi_scratch,
    }
    .run(
        crate::instructions::RouteRequest {
            order,
            taker_served_window,
            include_taker_origin_reservations: false,
            claim: None,
            filler: crate::instructions::FillerTerms::TAKER_SIGNED,
        },
        controller::orders::FillRequest {
            // Detached taker: it never reserved, so the fill unwinds
            // nothing.
            order: take.order,
            reserved: false,
            mode,
            referrer_is_accelerated,
        },
        controller::orders::PerpFillAccounts {
            user: take.accounts.user,
            user_stats: take.accounts.user_stats,
            filler: take.accounts.user,
            filler_stats: take.accounts.user_stats,
            rev_share_escrow: &mut take.escrow.as_mut(),
        },
        &mut controller::orders::FillParties {
            maps: take.maps,
            makers_and_referrer: take.makers,
            makers_and_referrer_stats: take.maker_stats,
        },
    )?;

    Ok(filled.amounts.base)
}

/// What becomes of the part of a take that did not fill.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TakeRemainder {
    /// The order filled whole.
    Filled,
    /// A reduce-only order has less than one step of position left to reduce.
    /// It counts as filled, and the rest is cancelled.
    ReduceOnlySpent,
    /// An immediate-or-cancel order cancels what it did not fill.
    ImmediateOrCancel,
    /// The rest goes to the book.
    Rest,
}

impl TakeRemainder {
    fn of(
        order: &Order,
        position_base: i64,
        step_size: u64,
        is_ioc: bool,
    ) -> crate::error::VelocityResult<Self> {
        if order.get_base_asset_amount_unfilled(None)? == 0 {
            return Ok(TakeRemainder::Filled);
        }

        if order.reduce_only
            && order.get_base_asset_amount_unfilled(Some(position_base))? < step_size
        {
            return Ok(TakeRemainder::ReduceOnlySpent);
        }

        Ok(if is_ioc {
            TakeRemainder::ImmediateOrCancel
        } else {
            TakeRemainder::Rest
        })
    }

    /// Whether the success condition reads the order as not filled.
    fn order_unfilled(self) -> bool {
        matches!(self, TakeRemainder::ImmediateOrCancel | TakeRemainder::Rest)
    }
}

/// Rest what the take did not fill, then hold the caller's success condition
/// against the result.
///
/// A part that does not rest emits a cancel record. That covers an unfilled
/// IOC, a spent reduce-only order, and a remainder the book refuses, which is
/// dropped rather than reverting the fill that already landed. A remainder
/// that can never rest, such as an `OrderType::Oracle` one, fails the take.
fn settle_take_remainder<'info>(
    accounts: &PlaceAndTakeAccounts<'_, 'info>,
    clob: &ClobRemainderRoute<'_, 'info>,
    maps: &mut AccountMaps,
    order: &Order,
    outcome: &TakeOutcome,
) -> Result<()> {
    let remainder = {
        let position_base = load!(accounts.user)?
            .get_perp_position(order.market_index)
            .map_or(0, |position| position.base_asset_amount);
        let step_size = maps
            .perp_market_map
            .get_ref(&order.market_index)?
            .order_step_size;
        TakeRemainder::of(
            order,
            position_base,
            step_size,
            outcome.is_immediate_or_cancel,
        )?
    };

    let cancel_explanation = match remainder {
        TakeRemainder::Filled => None,
        TakeRemainder::ReduceOnlySpent => {
            Some(OrderActionExplanation::ReduceOnlyOrderIncreasedPosition)
        }
        TakeRemainder::ImmediateOrCancel => Some(OrderActionExplanation::None),
        TakeRemainder::Rest => rest_take_remainder(accounts, clob, maps, order, outcome)?,
    };

    if let Some(explanation) = cancel_explanation {
        controller::orders::emit_detached_cancel_record(
            &*load!(accounts.user)?,
            &accounts.user.key(),
            order,
            maps,
            Clock::get()?.unix_timestamp,
            explanation,
        )?;
    }

    validate_place_and_take_success_condition(
        outcome.success_condition,
        outcome.base_asset_amount_filled,
        remainder.order_unfilled(),
    )
}

/// Rest the remainder on the book. Returns the explanation of its cancel
/// record when the book refuses it, and `None` when it rests.
fn rest_take_remainder<'info>(
    accounts: &PlaceAndTakeAccounts<'_, 'info>,
    clob: &ClobRemainderRoute<'_, 'info>,
    maps: &mut AccountMaps,
    order: &Order,
    outcome: &TakeOutcome,
) -> Result<Option<OrderActionExplanation>> {
    let remainder = crate::instructions::restable_remainder(
        &*load!(accounts.user)?,
        order,
        order.market_index,
        None,
    );
    let Some(remainder) = remainder else {
        msg!("the unfilled part of the order cannot rest on the book");
        return Err(print_error!(ErrorCode::InvalidOrder)().into());
    };

    let rest = crate::instructions::rest_remainder_on_clob(
        &crate::instructions::ClobRestAccounts {
            user: accounts.user,
            quoter_slab: clob.quoter_slab,
            clob_market: clob.clob_market,
            clob_program: clob.clob_program,
        },
        maps,
        &crate::instructions::ClobRestOrder {
            market_index: order.market_index,
            direction: remainder.direction,
            price: remainder.price,
            base_asset_amount: remainder.unfilled,
            max_ts: remainder.max_ts,
            client_order_id: order.order_id,
            taker_origin: true,
            reject_if_crossed: false,
            reduce_only: remainder.reduce_only,
            activation_delay_slots: outcome.activation_delay_slots,
        },
        &Clock::get()?,
    )?;

    Ok(match rest {
        crate::instructions::RestOutcome::Placed(_) => None,
        crate::instructions::RestOutcome::Refused(reason) => Some(reason.cancel_explanation()),
    })
}

/// The v1 `place_and_take` body. The taker order is detached: it is built on
/// the stack, margin-checked, filled through the router, and never written into
/// `User.orders`. A restable remainder rests on the market's CLOB. The router
/// fill snaps the AMM itself, so the body calls no `update_amm`.
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
    crate::instructions::attest_activation_delay(
        clob.quoter_slab,
        params.market_index,
        params.activation_delay_slots,
        request.taker_served_window,
    )?;

    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;

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
        // soft skip reachable here, because a post-only take is refused above.
        return validate_place_and_take_success_condition(request.success_condition, 0, false);
    };

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
            },
            &state,
            &clock,
            FillMode::Fill,
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
            is_immediate_or_cancel: params.is_immediate_or_cancel(),
            success_condition: request.success_condition,
            activation_delay_slots: params.activation_delay_slots,
        },
    )
}

#[cfg(test)]
mod take_remainder_tests {
    use {
        super::TakeRemainder,
        crate::{
            controller::position::PositionDirection,
            state::user::{Order, OrderType},
        },
    };

    fn sell(base_asset_amount: u64, filled: u64, reduce_only: bool) -> Order {
        Order {
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount,
            base_asset_amount_filled: filled,
            reduce_only,
            ..Order::default()
        }
    }

    #[test]
    fn a_whole_fill_leaves_nothing() {
        let remainder = TakeRemainder::of(&sell(10, 10, false), 0, 1, false).unwrap();
        assert_eq!(remainder, TakeRemainder::Filled);
        assert!(!remainder.order_unfilled());
    }

    #[test]
    fn a_reduce_only_rest_below_one_step_counts_as_filled() {
        let remainder = TakeRemainder::of(&sell(10, 6, true), 3, 5, false).unwrap();
        assert_eq!(remainder, TakeRemainder::ReduceOnlySpent);
        assert!(!remainder.order_unfilled());
    }

    #[test]
    fn a_reduce_only_rest_of_one_step_rests() {
        let remainder = TakeRemainder::of(&sell(10, 5, true), 5, 5, false).unwrap();
        assert_eq!(remainder, TakeRemainder::Rest);
        assert!(remainder.order_unfilled());
    }

    #[test]
    fn an_unfilled_ioc_is_cancelled() {
        let remainder = TakeRemainder::of(&sell(10, 4, false), 0, 1, true).unwrap();
        assert_eq!(remainder, TakeRemainder::ImmediateOrCancel);
        assert!(remainder.order_unfilled());
    }
}
