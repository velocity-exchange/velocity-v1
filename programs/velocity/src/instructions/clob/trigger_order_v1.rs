//! `trigger_order_v1` — fire a DLOB trigger straight to the book.
//!
//! A stop-market on the DLOB rests as an armed trigger in `User.orders`. The v0
//! `trigger_order` crank flips it live and leaves it there for a later
//! `fill_perp_order_v1` crank to fill. This crank fires it and rests it on the
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
//! the rest needs, the same superset `trigger_clob_order` and
//! `fill_perp_order_v1` carry. CLOB trigger-limits keep their own path: they
//! rest their whole order and never take a fill here.

use {
    crate::{
        controller::{self, position::PositionDirection},
        error::ErrorCode,
        instructions::constraints::*,
        load,
        signer::CLOB_AUTHORITY_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            fill_mode::FillMode,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{ClobUserRefV0, Direction, QuoterV0},
            state::State,
            user::{User, UserStats},
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(market_index: u16, order_id: u32, signed_route: Vec<Pubkey>)]
pub struct TriggerOrderV1<'info> {
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
    /// The market's CLOB registry entry — the remainder only ever rests on a
    /// vetted book.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered accounts.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the CLOB place authority PDA — a book's `place_authority`, which
    /// may place and cancel on any book for any user. Distinct from the
    /// per-entry signer a third-party quoter is handed.
    #[account(seeds = [CLOB_AUTHORITY_SEED], bump)]
    pub clob_authority: UncheckedAccount<'info>,
    /// Wake-hint host for the rested remainder, optional as on every CLOB
    /// placement path.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
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
    /// filler-obligation facts `fill_perp_order_v1` needs. Optional, and it
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
pub fn handle_trigger_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, TriggerOrderV1<'info>>,
    market_index: u16,
    order_id: u32,
    signed_route: Vec<Pubkey>,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts = ctx.remaining_accounts;
    let remaining_accounts_iter = &mut remaining_accounts.iter().peekable();
    let crate::instructions::optional_accounts::AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = crate::instructions::optional_accounts::load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let (makers_and_referrer, makers_and_referrer_stats) =
        crate::state::user_map::load_user_maps(remaining_accounts_iter, true)?;

    let mut escrow = if state.builder_codes_enabled() {
        crate::instructions::optional_accounts::get_revenue_share_escrow_account(
            remaining_accounts_iter,
            &load!(ctx.accounts.user)?.authority,
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
    let tail = &remaining_accounts[tail_from..];

    // Fire the trigger: validate, transform a copy of the slot order into a
    // live market order, free the slot, pay the flat reward. `None` means no
    // payable work — already triggered, or a risk-increasing trigger on a
    // failing account cancelled — so skip the fill and the reservoir payout.
    let Some(mut fired) = controller::orders::trigger_and_route_order(
        order_id,
        &state,
        &ctx.accounts.user,
        &ctx.accounts.user_stats,
        &spot_market_map,
        &perp_market_map,
        &mut oracle_map,
        &ctx.accounts.filler,
        clock,
    )?
    else {
        return Ok(());
    };

    // ---- Route the fired order against the book. ----
    let (direction, unfilled, taker_ref, quote_limit_price) = {
        let user = load!(ctx.accounts.user)?;
        let position_base = user
            .get_perp_position(market_index)
            .map(|position| position.base_asset_amount)
            .ok();
        (
            match fired.direction {
                PositionDirection::Long => Direction::Long,
                PositionDirection::Short => Direction::Short,
            },
            fired.get_base_asset_amount_unfilled(position_base)?,
            ClobUserRefV0 {
                authority: user.authority,
                sub_account_id: user.sub_account_id.into(),
            },
            FillMode::Fill.quote_limit_price(
                &fired,
                clock.slot,
                perp_market_map.get_ref(&market_index)?.order_tick_size,
                state.slot_clock(),
            ),
        )
    };

    // A caller routes the fill only by staging a quoter tail. A keeper that
    // read the book stages one and fills here; the relay resolver stages none
    // and skips straight to the rest. Relay cannot route: it sees only the
    // book, not the propAMMs, so a fill it stages would take a worse price than
    // the full router. The whole order rests taker-origin instead, and the
    // cross crank fills it across every source at the best price.
    if unfilled > 0 && !tail.is_empty() {
        let (clob_authority, clob_authority_nonce) = crate::signer::find_clob_authority();
        let route_reference_price = {
            let oracle_id = perp_market_map.get_ref(&market_index)?.oracle_id();
            oracle_map.get_price_data(&oracle_id)?.price
        };
        let inputs = crate::instructions::QuoteInputs {
            caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
            market_index,
            direction,
            size: unfilled,
            users: &crate::state::prop_amm::quoter_wire_users(
                makers_and_referrer.user_ref_index()?.into_keys().map(
                    |(authority, sub_account_id)| ClobUserRefV0 {
                        authority,
                        sub_account_id: sub_account_id.into(),
                    },
                ),
            )?,
            reference_price: route_reference_price,
            taker: taker_ref,
            limit_price: quote_limit_price,
            clob_authority,
            clob_authority_nonce,
        };
        let inputs = crate::instructions::QuoteInputs {
            caps: crate::instructions::build_user_caps(
                tail,
                &inputs,
                &mut crate::instructions::CapInputs {
                    makers_and_referrer: &makers_and_referrer,
                    makers_and_referrer_stats: &makers_and_referrer_stats,
                    perp_market_map: &perp_market_map,
                    spot_market_map: &spot_market_map,
                    oracle_map: &mut oracle_map,
                    slot: clock.slot,
                    now: clock.unix_timestamp,
                },
            )?,
            ..inputs
        };

        let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
        let route = crate::instructions::QuotedRoute::assemble(tail, &inputs, &mut cpi_scratch)?;
        route.require_baseline(perp_market_map.get_ref(&market_index)?.clob_quoter)?;
        let route_digest = crate::state::order_params::NO_ROUTE_DIGEST;
        route.require_signed_route(&signed_route, route_digest)?;

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
            unrouted_quoters: route.unrouted_quoters(&signed_route, route_digest),
        };

        let mut book_storage =
            [crate::math::router::QuoterBook::default(); crate::instructions::MAX_ROUTE_QUOTERS];
        let books = route.books(&mut book_storage);
        let mut executor =
            route.executor(&inputs, clock.slot, clock.unix_timestamp, &mut cpi_scratch);
        let mut router_inputs = crate::math::router::RouterFillInputs {
            books,
            executor: &mut executor,
            protocol_authority: state.signer,
            obligation,
        };

        controller::orders::fill_perp_order_with_router(
            // The fired order is ephemeral: it never reserved, so the fill
            // unwinds no exposure for it.
            controller::orders::FillTarget::Detached {
                order: &mut fired,
                reserved: false,
            },
            &state,
            &ctx.accounts.user,
            &ctx.accounts.user_stats,
            &spot_market_map,
            &perp_market_map,
            &mut oracle_map,
            &ctx.accounts.filler,
            &ctx.accounts.filler_stats,
            &makers_and_referrer,
            &makers_and_referrer_stats,
            clock,
            FillMode::Fill,
            &mut router_inputs,
            &mut escrow.as_mut(),
            referrer_is_accelerated,
        )?;
    }

    // ---- Rest the remainder taker-origin on the book. ----
    // A fired market order rests its unfilled amount at its slippage bound. Safe
    // only because a migrated remainder is taker-origin: a cross settles at the
    // counterparty's price, so a maker arriving in the activation window
    // competes on price rather than on transaction landing.
    let remainder = {
        let user = load!(ctx.accounts.user)?;
        if user.is_being_liquidated() {
            None
        } else {
            let position_base = user
                .get_perp_position(market_index)
                .map(|position| position.base_asset_amount)
                .unwrap_or(0);
            let unfilled = fired
                .get_base_asset_amount_unfilled(Some(position_base))
                .unwrap_or(0);
            crate::instructions::restable_remainder_price(&fired)
                .map(|price| (fired.direction, price, unfilled, fired.max_ts))
        }
    };
    if let Some((direction, price, unfilled, max_ts)) = remainder {
        if unfilled > 0 {
            let (_, clob_authority_nonce) = crate::signer::find_clob_authority();
            crate::instructions::try_place_remainder_on_clob(
                &ctx.accounts.user,
                &ctx.accounts.quoter,
                &ctx.accounts.clob_market.to_account_info(),
                &ctx.accounts.clob_program.to_account_info(),
                &ctx.accounts.clob_authority.to_account_info(),
                clob_authority_nonce,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                market_index,
                direction,
                price,
                unfilled,
                max_ts,
                order_id,
                true,
                false,
                fired.reduce_only,
                None,
                clock,
            )?;
        }
    }

    // ---- Pay the reservoir and release the trigger wake slot. ----
    // Drop the state borrow first: the reservoir payout loads state itself.
    drop(state);
    super::crank_common::finish_trigger_crank(
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

/// The relay resolver for `trigger_order_v1` (`Resolve<EndpointName>`):
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
pub struct ResolveTriggerOrderV1<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account, not into
    /// the block they read.
    #[account(constraint = trigger_conditions.load()?.user == user.key())]
    pub trigger_conditions: AccountLoader<'info, crate::state::user_conditions::UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
    /// CHECK: validated against the market's oracle in the handler.
    pub oracle: UncheckedAccount<'info>,
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
}

pub fn handle_resolve_trigger_order_v1(ctx: Context<ResolveTriggerOrderV1>) -> Result<()> {
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        let (fired, quote_spot_market_index) = {
            let conditions = ctx.accounts.trigger_conditions.load()?;
            let user = load!(ctx.accounts.user)?;
            let market = ctx.accounts.perp_market.load()?;
            (
                super::crank_common::find_fired_trigger(
                    &conditions,
                    &user,
                    &market,
                    &ctx.accounts.oracle,
                    clock.slot,
                    super::crank_common::TriggerResolverKind::ClobFill,
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
            crate::staged_call!(TriggerOrderV1 {
                state: crate::state::pdas::state(),
                authority: crate::state::pdas::keeper_placeholder(),
                filler: protocol_user,
                filler_stats: protocol_user_stats,
                user: ctx.accounts.user.key(),
                user_stats,
                quoter: meta.quoter,
                clob_market: meta.clob_market,
                clob_program: meta.clob_program,
                clob_authority: crate::state::pdas::clob_authority(),
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
            .arg(meta.market_index)?
            .arg(meta.order_id)?
            .arg(Vec::<Pubkey>::new())?,
        ))
    })
}
