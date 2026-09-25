//! `trigger_market_order_v1`, which fires an armed trigger straight to the
//! book.
//!
//! A stop-market rests as an armed trigger in `User.orders`. This crank fires
//! it and rests it on the book in one instruction. It validates the trigger,
//! turns the slot's order into a live market order, frees the slot, and rests
//! the order taker-origin. Nothing stays live in `User.orders`.
//!
//! The account tail decides whether the order fills here first. A caller that
//! staged a quoter tail, such as a keeper that read the book, routes the fill
//! and rests only the remainder. The relay resolver stages no tail. A resolver
//! sees only the book and not the propAMMs, so a fill it staged would take a
//! worse price than the full router. It rests the whole order instead, and the
//! cross crank fills it across every source at the best price.
//!
//! The account set is the trigger keeper set plus the market's CLOB accounts
//! the rest needs. `trigger_limit_order_v1` carries the same superset. CLOB
//! trigger-limits keep their own path. They rest their whole order and never
//! take a fill here.

use {
    crate::{
        controller,
        error::ErrorCode,
        instructions::constraints::*,
        load,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            fill_mode::FillMode,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{QuoterSlabExt, QuoterSlabV0},
            state::State,
            user::{Order, User, UserStats},
        },
        validate,
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
    /// CHECK: in signed-keeper mode this must sign for `filler`. In
    /// program-keeper mode, where the protocol `User` is the filler and relay
    /// turners call, it is only the lamport payout target and needs no
    /// signature.
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
    /// The market's quoter slab. The remainder only ever rests on the vetted
    /// book that its `Clob` slot names.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: validated against the book slot's registered response account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
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
    /// The user's relay trigger conditions. The handler releases the fired
    /// slot, which silences its level-triggered wake. It is optional, like
    /// every relay-side account.
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
    /// CHECK: address-locked to the instructions sysvar. It supplies whether
    /// the owner signed the transaction and how many accounts it locks, which
    /// are the filler-obligation facts a fill needs. It is optional and costs
    /// one lock. A fill needs it only when a book withholds
    /// depth for an owner the transaction does not carry, and the owner did not
    /// sign. A trigger crank's owner never signs, so a fill that reaches a
    /// withheld order and passes `None` here is refused.
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
    let mut route_accounts = crate::instructions::RouteFillAccounts::read(
        remaining_accounts,
        remaining_accounts_iter,
        &state,
        &ctx.accounts.user,
    )?;

    // Firing is irreversible: it frees the trigger slot, charges the flat
    // reward, and turns the order into a book rest. Refuse before anything
    // moves rather than leave the owner with a paid fee and no order. The
    // trigger stays armed until the book quotes again.
    validate!(
        ctx.accounts.quoter_slab.clob_slot(market_index)?.quotes(),
        ErrorCode::ClobRestUnavailable,
        "market {}'s book takes no new orders; the trigger stays armed",
        market_index
    )?;

    // Fire the trigger. This validates it, turns a copy of the slot order into
    // a live market order, frees the slot, and pays the flat reward. `None`
    // means there was no payable work. The order was past its `max_ts`, or a
    // risk-increasing trigger on a failing account was cancelled. Either way,
    // skip the fill and the reservoir payout.
    let Some(mut fired) = controller::orders::trigger_and_route_order(
        controller::orders::OrderToFire {
            market_index,
            order_id,
        },
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

    route_fill_fired_order(
        &ctx,
        &state,
        &mut maps,
        &mut route_accounts,
        &mut fired,
        &signed_route,
        market_index,
        clock,
    )?;

    rest_fired_remainder(&ctx, &mut maps, &fired, clock)?;

    // Pay the reservoir and release the trigger wake slot. Drop the state
    // borrow first, because the reservoir payout loads state itself.
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

/// Routes the fired order against the book, and fills what the route reaches.
///
/// A caller routes the fill only by staging a quoter tail. A keeper that read
/// the book stages one and fills here. The relay resolver stages none and goes
/// straight to the rest. Relay cannot route, because it sees only the book and
/// not the propAMMs, so a fill it stages would take a worse price than the full
/// router. The whole order rests taker-origin instead, and the cross crank
/// fills it across every source at the best price.
///
/// Maker priority gates the staged fill the way it gates every taker route. On
/// a book with a speed bump, only attested flow fills synchronously. An
/// unattested keeper's tail is ignored, and the fired order rests whole, as it
/// does on the relay path. A trigger crank is keeper-built and carries no
/// attestation transport, so its flow never counts as protected.
#[allow(clippy::too_many_arguments)]
fn route_fill_fired_order<'info>(
    ctx: &Context<'info, TriggerMarketOrderV1<'info>>,
    state: &State,
    maps: &mut crate::instructions::optional_accounts::AccountMaps<'info>,
    route_accounts: &mut crate::instructions::RouteFillAccounts<'info>,
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

    if route_accounts.quoters.is_empty() || !synchronous_take {
        return Ok(());
    }

    let order = crate::instructions::RoutedOrder::read(
        &*load!(ctx.accounts.user)?,
        fired,
        maps,
        FillMode::Fill,
    )?;
    if order.unfilled == 0 {
        return Ok(());
    }

    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    crate::instructions::RouteFill {
        state,
        clock,
        tail: route_accounts.quoters,
        scratch: &mut cpi_scratch,
    }
    .run(
        crate::instructions::RouteRequest {
            order,
            taker_served_window,
            include_taker_origin_reservations: false,
            claim: Some(crate::instructions::RouteClaim {
                quoters: signed_route,
                digest: crate::state::order_params::NO_ROUTE_DIGEST,
            }),
            // A trigger crank is not a signed transaction. The owner does not
            // sign, so the keeper answers for what its account list left out.
            filler: crate::instructions::FillerTerms::keeper(
                ctx.accounts
                    .ix_sysvar
                    .as_ref()
                    .map(|sysvar| sysvar.as_ref()),
            )?,
        },
        controller::orders::FillRequest {
            // The fired order is detached. It reserved nothing, so the fill
            // unwinds no exposure for it.
            order: fired,
            reserved: false,
            mode: FillMode::Fill,
            referrer_is_accelerated: route_accounts.referrer_is_accelerated,
        },
        controller::orders::PerpFillAccounts {
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            filler: &ctx.accounts.filler,
            filler_stats: &ctx.accounts.filler_stats,
            rev_share_escrow: &mut route_accounts.escrow.as_mut(),
        },
        &mut controller::orders::FillParties {
            maps,
            makers_and_referrer: &route_accounts.makers_and_referrer,
            makers_and_referrer_stats: &route_accounts.makers_and_referrer_stats,
        },
    )?;

    Ok(())
}

/// Rests the fired order's unfilled base on the book, taker-origin.
///
/// A fired market order rests its unfilled amount at its slippage bound. That
/// is safe only because a migrated remainder is taker-origin. A cross settles
/// at the counterparty's price, so a maker that arrives in the activation
/// window competes on price rather than on transaction landing. A fired
/// trigger-market's worst price is stored relative to the oracle, so the rest
/// price is read against the live oracle.
///
/// A remainder that cannot rest is lost. The trigger slot was freed and the
/// flat reward charged before the fill ran, and the fill already moved the
/// position, so nothing can be restored. The lost remainder emits a cancel
/// record instead.
fn rest_fired_remainder<'info>(
    ctx: &Context<'info, TriggerMarketOrderV1<'info>>,
    maps: &mut crate::instructions::optional_accounts::AccountMaps<'info>,
    fired: &Order,
    clock: &Clock,
) -> Result<()> {
    let rest_oracle_price = {
        let oracle_id = maps
            .perp_market_map
            .get_ref(&fired.market_index)?
            .oracle_id();
        maps.oracle_map.get_price_data(&oracle_id)?.price
    };

    crate::instructions::rest_or_cancel_detached_remainder(
        &crate::instructions::ClobRestAccounts {
            user: &ctx.accounts.user,
            quoter_slab: &ctx.accounts.quoter_slab,
            clob_market: &ctx.accounts.clob_market.to_account_info(),
            clob_program: &ctx.accounts.clob_program.to_account_info(),
        },
        maps,
        fired,
        &crate::instructions::DetachedRemainderTerms {
            rest_oracle_price: Some(rest_oracle_price),
            activation_delay_slots: None,
        },
        clock,
    )?;

    Ok(())
}

/// The relay resolver for `trigger_market_order_v1`. It is simulation-only and
/// is staged from the user's synced trigger conditions. It fires an armed
/// stop-market to the book.
///
/// The staged executor carries no quoter tail, so it does not fill. The whole
/// fired order rests taker-origin, and the cross crank fills it across every
/// source at the best price. A resolver sees only the book and not the
/// propAMMs, so a fill it staged would take a worse price than the full router.
/// The executor still names the market's CLOB accounts, which the rest places
/// behind. An armed trigger carries no signed route, so the staged call claims
/// none.
#[derive(Accounts)]
pub struct ResolveTriggerMarketOrderV1<'info> {
    /// The shared staging account, at index 0 by convention. A resolver's
    /// response pointer is read against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only. A resolver stages into the shared scratch account rather
    /// than into the block it reads.
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
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;
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
                    clock.unix_timestamp,
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
            // The margin map for the rest holds the fired market and the
            // quote spot market. No quoter tail follows, so the executor rests
            // the whole order rather than routing it.
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
