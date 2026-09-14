//! Filling a resting perp order through the router.
//!
//! One quote, split, then an execute sweep across the vAMM ladder, any DLOB
//! makers, and any external quoters. The remainder of a v1 route migrates to
//! the market's book instead of staying in `User.orders`.

use super::*;

/// The router fill: one quote → split → execute sweep across the vAMM
/// ladder (with last look over the rival books), any DLOB makers, and any
/// external quoters.
///
/// `remaining_accounts`, beyond the usual market/oracle/user-map section:
/// the market's `QuoterSlabV0` plus the union of the consulted quoters'
/// registered CPI accounts (including the quoter programs and the velocity
/// signer PDA). A slab slot is consulted when its response account rides the
/// call; each live consulted slot is quoted via CPI into a book, and
/// allocations that land on a book execute through the same slot's
/// `execute_v0`. No slab = vAMM + DLOB routing only —
/// allowed only while the market names no canonical book (`clob_market`).
#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_legacy_fill_perp_order<'c: 'info, 'info>(
    ctx: Context<'info, FillOrder<'info>>,
    order_id: Option<u32>,
    signed_route: Vec<Pubkey>,
) -> Result<()> {
    let (order_id, market_index) = {
        let user = &load!(ctx.accounts.user)?;
        // if there is no order id, use the users last order id
        let order_id = order_id.unwrap_or_else(|| user.get_last_order_id());
        let market_index = match user.get_order(order_id) {
            Some(order) => order.market_index,
            None => {
                msg!("Order does not exist {}", order_id);
                return Ok(());
            }
        };
        (order_id, market_index)
    };

    let obligation = {
        let taker = load!(ctx.accounts.user)?;
        crate::math::router::FillerObligation {
            taker_signed: taker.authority == ctx.accounts.authority.key()
                || (taker.delegate == ctx.accounts.authority.key()
                    && taker.delegate != Pubkey::default()),
            tx_accounts: ctx
                .accounts
                .instructions_sysvar
                .as_ref()
                .map(|sysvar| {
                    crate::instructions::optional_accounts::tx_writable_lock_count(sysvar)
                })
                .transpose()?,
            // Set after the route is assembled: only then is it known which
            // entries the transaction carried.
            unrouted_quoters: 0,
        }
    };
    let user_key = &ctx.accounts.user.key();
    fill_order(
        FillAccounts {
            state: &ctx.accounts.state,
            filler: &ctx.accounts.filler,
            filler_stats: &ctx.accounts.filler_stats,
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
        },
        ctx.remaining_accounts,
        RouterFillRequest {
            order_id,
            market_index,
            signed_route,
            obligation,
            // A keeper fill is never attested flow: the attestation transports
            // are the flow authority signing a swift-built transaction, or a
            // detached attestation bound to a signed-message order — a legacy
            // slot order has neither. On a bumped book the route quotes the
            // book as empty and the restable remainder migrates into the
            // auction.
            taker_served_window: false,
            clob: None,
        },
    )
    .inspect_err(|_e| {
        msg!(
            "Err filling order id {} for user {} for market index {}",
            order_id,
            user_key,
            market_index
        );
    })?;

    Ok(())
}

/// The accounts a fill needs, borrowed so `fill_perp_order` and
/// `fill_legacy_dlob_order` — which have different `#[derive(Accounts)]`
/// shapes — share one body.
pub struct FillAccounts<'a, 'info> {
    pub state: &'a AccountLoader<'info, State>,
    pub filler: &'a AccountLoader<'info, User>,
    pub filler_stats: &'a AccountLoader<'info, UserStats>,
    pub user: &'a AccountLoader<'info, User>,
    pub user_stats: &'a AccountLoader<'info, UserStats>,
}

/// What a router fill is asked to do, beyond the accounts it is handed.
pub struct RouterFillRequest<'a, 'info> {
    /// The slot in `User.orders` to fill.
    pub order_id: u32,
    pub market_index: u16,
    /// The custom quoters the caller claims to carry.
    pub signed_route: Vec<Pubkey>,
    /// What the fill knows about the party that built the transaction.
    pub obligation: crate::math::router::FillerObligation,
    /// Whether the transaction is attested taker flow. A book with a speed
    /// bump quotes no depth to an unattested taker, so an unattested keeper
    /// fill reaches the vAMM and the DLOB makers only, and the remainder
    /// migrates to the book to wait its window.
    pub taker_served_window: bool,
    /// The book a restable remainder migrates onto. `None` leaves the
    /// remainder where it is.
    pub clob: Option<crate::instructions::ClobRemainderRoute<'a, 'info>>,
}

/// `fill_legacy_dlob_order`'s way in: `fill_order` is private, and this names
/// why it is being called with a CLOB route rather than exposing the whole
/// body.
pub fn fill_legacy_dlob_order_entry<'c: 'info, 'info>(
    accounts: FillAccounts<'_, 'info>,
    remaining_accounts: &'c [AccountInfo<'info>],
    request: RouterFillRequest<'_, 'info>,
) -> Result<()> {
    fill_order(accounts, remaining_accounts, request)
}

fn fill_order<'c: 'info, 'info>(
    accounts: FillAccounts<'_, 'info>,
    remaining_accounts: &'c [AccountInfo<'info>],
    mut request: RouterFillRequest<'_, 'info>,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = accounts.state.load()?;

    // No `update_amm` here: `fill_perp_order` snaps the AMM and refreshes
    // PerpMarket-level oracle stats internally before quoting.
    let mut sections = FillSections::load(
        remaining_accounts,
        accounts.user,
        request.market_index,
        &state,
        clock.slot,
    )?;

    let filled = route_and_fill(&accounts, &mut sections, &mut request, &state, clock)?;
    rest_slot_remainder(accounts.user, &request, &mut sections.maps, filled, clock)
}

/// Quote the external quoters from the leftover accounts, assemble the route,
/// and fill the order against it. Reports the base the fill moved.
fn route_and_fill<'info>(
    accounts: &FillAccounts<'_, 'info>,
    sections: &mut FillSections<'info>,
    request: &mut RouterFillRequest<'_, 'info>,
    state: &State,
    clock: &Clock,
) -> Result<u64> {
    let market_index = request.market_index;
    let order = {
        let user = load!(accounts.user)?;
        let order = user
            .get_order(request.order_id)
            .ok_or(ErrorCode::OrderDoesNotExist)?;
        RouteContext {
            market_index,
            maps: &mut sections.maps,
            state,
            clock,
        }
        .routed_order(&user, order, FillMode::Fill)?
    };

    let users = sections.wire_users()?;
    let inputs = order.quote_inputs(market_index, &users, request.taker_served_window);
    let inputs = sections.with_counterparty_room(inputs, &accounts.user.key(), clock)?;

    // One set of CPI buffers for the fill: the quote legs below and the
    // execute legs the router runs later all refill the same allocation,
    // because velocity's heap never gives a freed one back.
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let route =
        crate::instructions::QuotedRoute::assemble(sections.tail, &inputs, &mut cpi_scratch)?;
    route.require_baseline(
        sections
            .maps
            .perp_market_map
            .get_ref(&market_index)?
            .clob_market,
    )?;
    // A DLOB order carries no route. Only a signed message names one, and such
    // an order routes at placement and rests any remainder on the market's
    // CLOB, so what a route binds is the fill of that remainder.
    // `crank_taker_origin_cross` reads it from the taker's signed-message
    // record.
    let digest = crate::state::order_params::NO_ROUTE_DIGEST;
    route.require_signed_route(&request.signed_route, digest)?;
    // Countable only now: the route is what says which entries arrived, and
    // the obligation is only consulted if a book later withholds.
    request.obligation.unrouted_quoters = route.unrouted_quoters(&request.signed_route, digest)?;

    let mut book_storage =
        [crate::math::router::QuoterBook::default(); crate::state::prop_amm::MAX_ROUTE_QUOTERS];
    let books = route.books(&mut book_storage)?;
    let mut executor = route.executor(&inputs, clock.slot, clock.unix_timestamp, &mut cpi_scratch);
    let mut router_inputs = RouterFillInputs {
        books,
        executor: &mut executor,
        protocol_authority: state.signer,
        taker_exposure_closed_by_caller: false,
        obligation: request.obligation,
        worst_fill_price: None,
    };

    sections.run_fill(
        accounts,
        controller::orders::FillRequest {
            target: controller::orders::FillTarget::Slot(request.order_id),
            mode: FillMode::Fill,
            referrer_is_accelerated: sections.referrer_is_accelerated,
        },
        &mut router_inputs,
        clock,
    )
}

/// Migrate what the route could not fill onto the market's book.
///
/// v1 route only: a restable remainder belongs on the book, not in
/// `User.orders`. Without this a signed-message taker order's leftover rests
/// on the DLOB forever, because such an order cannot be IOC, and the DLOB is
/// not where a restable order lives any more.
///
/// Restable means the same thing it means on the place-and-take route: a
/// fixed price, no oracle offset, not reduce-only, since the CLOB has neither
/// oracle-floating nor reduce-only semantics. A market order rests at its
/// `auction_end_price`. `restable_remainder_price` is the whole rule, shared
/// with the place-and-take route so a remainder's fate does not depend on
/// which one reached it.
fn rest_slot_remainder<'info>(
    user_loader: &AccountLoader<'info, User>,
    request: &RouterFillRequest<'_, 'info>,
    maps: &mut AccountMaps<'info>,
    base_asset_amount_filled: u64,
    clock: &Clock,
) -> Result<()> {
    let Some(clob) = &request.clob else {
        return Ok(());
    };
    let remainder = {
        let user = load!(user_loader)?;
        // Only the taker's own remainder migrates. A keeper fills any user's
        // order, so without this a keeper could cancel a resting order that
        // did not cross and re-place it on the book as taker_origin. Two
        // gates bound it to a genuine taker remainder: the fill must have
        // made progress or the order must be a taker-class order (one with
        // an auction — a market or auction-limit taker), and the owner must
        // not be under liquidation. `restable_remainder_price` (post-only,
        // reduce-only, oracle-offset) carries the rest.
        if user.is_being_liquidated() {
            return Ok(());
        }
        let Ok(order_index) = user.get_order_index(request.order_id) else {
            return Ok(());
        };
        let order = &user.orders[order_index];
        if base_asset_amount_filled == 0 && !order.has_auction() {
            return Ok(());
        }
        crate::instructions::restable_remainder(&user, order, request.market_index, None)
    };
    let Some(remainder) = remainder else {
        return Ok(());
    };
    if remainder.unfilled == 0 {
        return Ok(());
    }

    controller::orders::cancel_order_by_order_id(request.order_id, user_loader, maps, clock)?;
    crate::instructions::try_place_remainder_on_clob(
        user_loader,
        clob.quoter_slab,
        clob.clob_market,
        clob.clob_program,
        maps,
        request.market_index,
        remainder.direction,
        remainder.price,
        remainder.unfilled,
        remainder.max_ts,
        request.order_id,
        true,
        false,
        remainder.reduce_only,
        None,
        clock,
    )?;
    Ok(())
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_revert_fill<'info>(ctx: Context<RevertFill>) -> Result<()> {
    let filler = load_mut!(ctx.accounts.filler)?;
    let clock = Clock::get()?;

    validate!(
        filler.last_active_slot == clock.slot,
        ErrorCode::RevertFill,
        "filler last active slot ({}) != current slot ({})",
        filler.last_active_slot,
        clock.slot
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct FillOrder<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
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
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// CHECK: address-locked to the instructions sysvar. See `FillLegacyDlobOrder` for
    /// what it is read for and why it is optional.
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct RevertFill<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
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
}
