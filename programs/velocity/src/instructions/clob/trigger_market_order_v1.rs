//! `trigger_market_order_v1` — fire a DLOB trigger straight to the book.
//!
//! A stop-market on the DLOB rests as an armed trigger in `User.orders`. The v0
//! `trigger_order` crank flips it live and leaves it there for a later
//! `fill_legacy_dlob_order` crank to fill. This crank fires it and rests it on the
//! book in one instruction: it validates the trigger, transforms the slot's
//! order into a live market order, frees the slot, and rests the order as a
//! taker-origin order. Nothing lingers live in `User.orders`.
//!
//! Whether the order fills here first depends on the account tail. A caller
//! that staged a quoter tail — a keeper that read the book — routes the fill
//! and rests only the remainder. The relay resolver stages no tail: it sees
//! only the book, not the propAMMs, so a fill it staged would take a worse
//! price than the full router. It rests the whole order instead, and the cross
//! crank fills it across every source at the best price.
//!
//! The account set is the v0 trigger keeper set plus the market's CLOB accounts
//! the rest needs, the same superset `trigger_limit_order_v1` and
//! `fill_legacy_dlob_order` carry. CLOB trigger-limits keep their own path: they
//! rest their whole order and never take a fill here.

use {
    crate::{
        controller::{self, position::PositionDirection},
        error::ErrorCode,
        instructions::constraints::*,
        load,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            fill_mode::FillMode,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{ClobUserRefV0, Direction, QuoterSlabV0},
            state::State,
            user::{Order, User, UserStats},
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct TriggerMarketOrderV1Args {
    pub market_index: u16,
    /// The trigger-market order to fire, by its `User.orders` id.
    pub order_id: u32,
    /// The taker's signed route, when the fill claims one. Empty claims the
    /// market baseline.
    pub signed_route: Vec<Pubkey>,
}

#[derive(Accounts)]
#[instruction(args: TriggerMarketOrderV1Args)]
pub struct TriggerMarketOrderV1<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler`; in
    /// program-keeper mode (protocol `User` as filler, relay turners) it is
    /// only the lamport payout target and no signature is required.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = can_crank_for_filler(&filler, &authority, &state)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The owner of the armed trigger order.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// The market's quoter slab — the remainder only ever rests on the
    /// vetted book its `Clob` slot names.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: validated against the book slot's registered response account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// Wake-hint host for the rested remainder, optional as on every CLOB
    /// placement path.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    /// The user's relay trigger conditions: the fired slot is released so its
    /// level-triggered wake goes quiet. Optional, like everything relay-side.
    #[account(
        mut,
        seeds = [
            crate::state::user_conditions::USER_CONDITIONS_PDA_SEED,
            user.key().as_ref(),
        ],
        bump
    )]
    pub trigger_conditions:
        Option<AccountLoader<'info, crate::state::user_conditions::UserConditionsV0>>,
    /// CHECK: address-locked to the instructions sysvar. Read for whether the
    /// owner signed the transaction and how many accounts it locks — the same
    /// filler-obligation facts `fill_legacy_dlob_order` needs. Optional, and it
    /// costs one lock: a fill needs it only when a book withholds depth for an
    /// owner the transaction does not carry, and the owner did not sign. A
    /// trigger crank's owner never signs, so a fill that reaches a withheld
    /// order and passes `None` here is refused.
    #[account(address = ::solana_program::sysvar::instructions::ID)]
    pub ix_sysvar: Option<UncheckedAccount<'info>>,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_trigger_market_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, TriggerMarketOrderV1<'info>>,
    args: TriggerMarketOrderV1Args,
) -> Result<()> {
    let TriggerMarketOrderV1Args {
        market_index,
        order_id,
        signed_route,
    } = args;
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts = ctx.remaining_accounts;
    let remaining_accounts_iter = &mut remaining_accounts.iter().peekable();
    let mut maps = crate::instructions::optional_accounts::load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;
    let mut tail = read_route_tail(
        remaining_accounts,
        remaining_accounts_iter,
        &state,
        &ctx.accounts.user,
    )?;

    // Fire the trigger: validate, transform a copy of the slot order into a
    // live market order, free the slot, pay the flat reward. `None` means no
    // payable work — already triggered, or a risk-increasing trigger on a
    // failing account cancelled — so skip the fill and the reservoir payout.
    let Some(mut fired) = controller::orders::trigger_and_route_order(
        order_id,
        &state,
        &controller::orders::TriggerAccounts {
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            filler: &ctx.accounts.filler,
        },
        &mut maps,
        clock,
    )?
    else {
        return Ok(());
    };

    let route_inputs = read_fired_route_inputs(
        &ctx.accounts.user,
        &fired,
        market_index,
        &maps.perp_market_map,
        &state,
        clock,
    )?;

    route_fill_fired_order(
        &ctx,
        &state,
        &mut maps,
        &mut tail,
        &route_inputs,
        &mut fired,
        &signed_route,
        market_index,
        clock,
    )?;

    rest_fired_remainder(&ctx, &mut maps, &fired, market_index, order_id, clock)?;

    // ---- Pay the reservoir and release the trigger wake slot. ----
    // Drop the state borrow first: the reservoir payout loads state itself.
    drop(state);
    super::helpers::crank_common::finish_trigger_crank(
        &ctx.accounts.state,
        &ctx.accounts.filler,
        &ctx.accounts.authority,
        &ctx.accounts.user,
        &ctx.accounts.trigger_conditions,
        &ctx.accounts.crank_conditions,
        market_index,
        order_id,
    )?;

    Ok(())
}

/// What the caller staged after the market maps.
///
/// A trigger crank carries the tail a fill carries: the maker accounts, the
/// taker's builder escrow, and the quoter accounts a route reads. The quoter
/// accounts decide whether the fired order fills here at all.
struct RouteTail<'info> {
    makers_and_referrer: crate::state::user_map::UserMap<'info>,
    makers_and_referrer_stats: crate::state::user_map::UserStatsMap<'info>,
    /// The quoter section: everything the maps, the makers and the escrow
    /// left behind.
    accounts: &'info [AccountInfo<'info>],
    escrow: Option<crate::state::revenue_share::RevenueShareEscrowZeroCopyMut<'info>>,
    referrer_is_accelerated: bool,
}

/// Reads the tail out of the remaining accounts, in the order a fill reads it.
fn read_route_tail<'info>(
    remaining_accounts: &'info [AccountInfo<'info>],
    remaining_accounts_iter: &mut std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
    state: &State,
    user: &AccountLoader<'info, User>,
) -> Result<RouteTail<'info>> {
    let (makers_and_referrer, makers_and_referrer_stats) =
        crate::state::user_map::load_user_maps(remaining_accounts_iter, true)?;

    let escrow = if state.builder_codes_enabled() {
        crate::instructions::optional_accounts::get_revenue_share_escrow_account(
            remaining_accounts_iter,
            &load!(user)?.authority,
        )?
    } else {
        None
    };
    let referrer_is_accelerated =
        crate::instructions::optional_accounts::get_referrer_accelerated_status(
            remaining_accounts_iter,
            escrow.as_ref(),
        )?;

    let tail_from = remaining_accounts.len() - remaining_accounts_iter.len();
    Ok(RouteTail {
        makers_and_referrer,
        makers_and_referrer_stats,
        accounts: &remaining_accounts[tail_from..],
        escrow,
        referrer_is_accelerated,
    })
}

/// What the fired order asks a route for.
struct FiredRouteInputs {
    direction: Direction,
    /// The base still to fill, held to the position a reduce-only order may
    /// reduce.
    unfilled: u64,
    taker: ClobUserRefV0,
    /// The worst price the fill accepts, from the order's auction.
    limit_price: u64,
}

/// Reads the fired order against the market, as the route needs it.
fn read_fired_route_inputs(
    user: &AccountLoader<'_, User>,
    fired: &Order,
    market_index: u16,
    perp_market_map: &crate::state::perp_market_map::PerpMarketMap<'_>,
    state: &State,
    clock: &Clock,
) -> Result<FiredRouteInputs> {
    let user = load!(user)?;
    let position_base = user
        .get_perp_position(market_index)
        .map(|position| position.base_asset_amount)
        .ok();
    Ok(FiredRouteInputs {
        direction: match fired.direction {
            PositionDirection::Long => Direction::Long,
            PositionDirection::Short => Direction::Short,
        },
        unfilled: fired.get_base_asset_amount_unfilled(position_base)?,
        taker: user.clob_user_ref(),
        limit_price: FillMode::Fill.quote_limit_price(
            fired,
            clock.slot,
            perp_market_map.get_ref(&market_index)?.order_tick_size,
            state.slot_clock(),
        ),
    })
}

/// Routes the fired order against the book, and fills what the route reaches.
///
/// A caller routes the fill only by staging a quoter tail. A keeper that read
/// the book stages one and fills here; the relay resolver stages none and skips
/// straight to the rest. Relay cannot route: it sees only the book, not the
/// propAMMs, so a fill it stages would take a worse price than the full router.
/// The whole order rests taker-origin instead, and the cross crank fills it
/// across every source at the best price.
///
/// Maker priority gates the staged fill the same way it gates every taker
/// route: on a book with a speed bump, only attested flow fills
/// synchronously. An unattested keeper's tail is ignored and the fired
/// order rests whole, exactly as the relay path does.
/// A trigger crank is keeper-built and carries no attestation transport,
/// so its flow never counts as protected.
#[allow(clippy::too_many_arguments)]
fn route_fill_fired_order<'info>(
    ctx: &Context<'info, TriggerMarketOrderV1<'info>>,
    state: &State,
    maps: &mut crate::instructions::optional_accounts::AccountMaps<'info>,
    tail: &mut RouteTail<'info>,
    route_inputs: &FiredRouteInputs,
    fired: &mut Order,
    signed_route: &[Pubkey],
    market_index: u16,
    clock: &Clock,
) -> Result<()> {
    let taker_served_window = false;
    let synchronous_take = crate::instructions::synchronous_take_allowed(
        taker_served_window,
        &ctx.accounts.quoter_slab,
        market_index,
    )?;
    if route_inputs.unfilled == 0 || tail.accounts.is_empty() || !synchronous_take {
        return Ok(());
    }

    let (route_reference_price, route_margin_ratio_initial) = {
        let market = maps.perp_market_map.get_ref(&market_index)?;
        let oracle_id = market.oracle_id();
        let margin_ratio_initial = market.margin_ratio_initial;
        drop(market);
        (
            maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        )
    };
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let users = crate::state::prop_amm::quoter_wire_users(
        tail.makers_and_referrer.user_ref_index()?.into_keys().map(
            |(authority, sub_account_id)| ClobUserRefV0 {
                authority,
                sub_account_id,
            },
        ),
    )?;
    let quoted = crate::instructions::quote_route(
        tail.accounts,
        crate::instructions::QuoteInputs {
            market_index,
            direction: route_inputs.direction,
            size: route_inputs.unfilled,
            users: &users,
            reference_price: route_reference_price,
            taker: route_inputs.taker,
            limit_price: route_inputs.limit_price,
            taker_served_window,
            consume_reservation: false,
            margin_ratio_initial: route_margin_ratio_initial,
        },
        &mut crate::instructions::CapInputs {
            taker_key: &ctx.accounts.user.key(),
            makers_and_referrer: &tail.makers_and_referrer,
            makers_and_referrer_stats: &tail.makers_and_referrer_stats,
            maps,
            slot: clock.slot,
            now: clock.unix_timestamp,
        },
        &mut cpi_scratch,
    )?;
    let route = quoted.route;
    let sized = quoted.sized;
    route.require_baseline(maps.perp_market_map.get_ref(&market_index)?.clob_market)?;
    let route_digest = crate::state::order_params::NO_ROUTE_DIGEST;
    route.require_signed_route(signed_route, route_digest)?;

    let obligation = crate::math::router::FillerObligation {
        // A trigger crank is not a signed transaction: the owner does not
        // sign, so the keeper answers for what its account list left out.
        taker_signed: false,
        tx_accounts: match &ctx.accounts.ix_sysvar {
            Some(sysvar) => Some(
                crate::instructions::optional_accounts::tx_writable_lock_count(
                    &sysvar.to_account_info(),
                )?,
            ),
            None => None,
        },
        unrouted_quoters: route.unrouted_quoters(signed_route, route_digest)?,
    };

    let mut book_storage =
        [crate::math::router::QuoterBook::default(); crate::state::prop_amm::MAX_ROUTE_QUOTERS];
    let books = route.books(&mut book_storage)?;
    let mut executor = route.executor(&sized, clock.slot, clock.unix_timestamp, &mut cpi_scratch);
    let mut router_inputs = crate::math::router::RouterFillInputs {
        books,
        executor: &mut executor,
        protocol_authority: state.signer,
        taker_exposure_closed_by_caller: false,
        obligation,
        worst_fill_price: None,
    };

    controller::orders::fill_perp_order(
        controller::orders::FillRequest {
            // The fired order is ephemeral: it never reserved, so the fill
            // unwinds no exposure for it.
            target: controller::orders::FillTarget::Detached {
                order: fired,
                reserved: false,
            },
            mode: FillMode::Fill,
            referrer_is_accelerated: tail.referrer_is_accelerated,
        },
        state,
        clock,
        controller::orders::PerpFillAccounts {
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            filler: &ctx.accounts.filler,
            filler_stats: &ctx.accounts.filler_stats,
            rev_share_escrow: &mut tail.escrow.as_mut(),
        },
        &mut controller::orders::FillParties {
            maps,
            makers_and_referrer: &tail.makers_and_referrer,
            makers_and_referrer_stats: &tail.makers_and_referrer_stats,
        },
        &mut router_inputs,
    )?;
    Ok(())
}

/// Rests the fired order's unfilled base on the book, taker-origin.
///
/// A fired market order rests its unfilled amount at its slippage bound. Safe
/// only because a migrated remainder is taker-origin: a cross settles at the
/// counterparty's price, so a maker arriving in the activation window
/// competes on price rather than on transaction landing.
/// A fired trigger-market's auction bound is stored relative to the oracle,
/// so the rest price is read against the live oracle.
fn rest_fired_remainder<'info>(
    ctx: &Context<'info, TriggerMarketOrderV1<'info>>,
    maps: &mut crate::instructions::optional_accounts::AccountMaps<'info>,
    fired: &Order,
    market_index: u16,
    order_id: u32,
    clock: &Clock,
) -> Result<()> {
    let rest_oracle_price = {
        let oracle_id = maps.perp_market_map.get_ref(&market_index)?.oracle_id();
        maps.oracle_map.get_price_data(&oracle_id)?.price
    };
    let remainder = {
        let user = load!(ctx.accounts.user)?;
        if user.is_being_liquidated() {
            None
        } else {
            crate::instructions::restable_remainder(
                &user,
                fired,
                market_index,
                Some(rest_oracle_price),
            )
        }
    };
    if let Some(remainder) = remainder {
        if remainder.unfilled > 0 {
            crate::instructions::try_place_remainder_on_clob(
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
                order_id,
                true,
                false,
                remainder.reduce_only,
                None,
                clock,
            )?;
        }
    }
    Ok(())
}

/// The relay resolver for `trigger_market_order_v1` (`Resolve<EndpointName>`):
/// simulation-only, staged from the user's synced trigger conditions. It fires
/// a DLOB stop-market to the book.
///
/// The staged executor carries no quoter tail, so it does not fill: the whole
/// fired order rests taker-origin and the cross crank fills it across every
/// source at the best price. A resolver sees only the book, not the propAMMs,
/// so a fill it staged would take a worse price than the full router. The
/// executor still names the market's CLOB accounts, which the rest places
/// behind. A DLOB trigger carries no signed route, so the staged call claims
/// none.
#[derive(Accounts)]
pub struct ResolveTriggerMarketOrderV1<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account, not into
    /// the block they read.
    #[account(constraint = trigger_conditions.load()?.user == user.key())]
    pub trigger_conditions: AccountLoader<'info, crate::state::user_conditions::UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
    /// CHECK: the perp market's `has_one` binds it.
    pub oracle: UncheckedAccount<'info>,
    #[account(has_one = oracle)]
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
}

pub fn handle_resolve_trigger_market_order_v1(
    ctx: Context<ResolveTriggerMarketOrderV1>,
) -> Result<()> {
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        let (fired, quote_spot_market_index) = {
            let conditions = ctx.accounts.trigger_conditions.load()?;
            let user = load!(ctx.accounts.user)?;
            let market = ctx.accounts.perp_market.load()?;
            (
                super::helpers::crank_common::find_fired_trigger(
                    &conditions,
                    &user,
                    &market,
                    &ctx.accounts.oracle,
                    clock.slot,
                    super::helpers::crank_common::TriggerResolverKind::ClobFill,
                )?,
                market.quote_spot_market_index,
            )
        };
        let Some(meta) = fired else {
            return Ok(None);
        };

        let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
        let user_stats = crate::state::pdas::user_stats(&load!(ctx.accounts.user)?.authority);
        Ok(Some(
            crate::staged_call!(TriggerMarketOrderV1 {
                state: crate::state::pdas::state(),
                authority: crate::state::pdas::keeper_placeholder(),
                filler: protocol_user,
                filler_stats: protocol_user_stats,
                user: ctx.accounts.user.key(),
                user_stats,
                quoter_slab: meta.quoter_slab,
                clob_market: meta.clob_market,
                clob_program: meta.clob_program,
                crank_conditions: Some(crate::state::pdas::clob_crank_conditions(
                    meta.market_index,
                )),
                trigger_conditions: Some(ctx.accounts.trigger_conditions.key()),
                // No fill, so no filler-obligation read of the sysvar.
                ix_sysvar: None,
            })
            // The rest's margin map: the fired market and the quote spot
            // market. No quoter tail follows, so the executor rests the whole
            // order rather than routing it.
            .map_section(
                ctx.accounts.oracle.key(),
                quote_spot_market_index,
                meta.market_index,
            )
            .arg(TriggerMarketOrderV1Args {
                market_index: meta.market_index,
                order_id: meta.order_id,
                signed_route: Vec::new(),
            })?,
        ))
    })
}
