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
//! worse price than the full router. It rests the whole order instead. The
//! taker-origin cross crank then fills it against a book order that crosses it,
//! or against the vAMM. The relay-staged cross crank carries no quoter, so a
//! quoter's price does not reach the order on that path.
//!
//! A refusal that can clear, such as a full book side, never cancels a fired
//! stop. A fire that filled nothing fails, so the trigger stays armed. A fire
//! that filled part keeps the fill and arms the unfilled part again. A refusal
//! that comes from the order itself cancels the rest.
//!
//! The keeper earns the flat reward in shares. A fire earns the share of the
//! stop's size that it used: what it filled when the stop is armed again, and
//! all that was left when the stop is spent. A fire that fills dust earns
//! dust, and only a fire that spends the stop draws reservoir lamports.
//!
//! The account set is the trigger keeper set plus the market's CLOB accounts
//! the rest needs. `trigger_limit_order_v1` carries the same superset. CLOB
//! trigger-limits keep their own path. They rest their whole order and never
//! take a fill here.

use {
    crate::{
        controller,
        controller::orders::FireOutcome,
        error::ErrorCode,
        instructions::{constraints::*, DetachedRemainder},
        load, load_mut,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            fill_mode::FillMode,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{QuoterSlabExt, QuoterSlabV0},
            spot_market::SpotMarket,
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
    /// CHECK: the user's relay trigger conditions. The handler releases the
    /// fired slot, which silences its level-triggered wake. It is required,
    /// as on `trigger_limit_order_v1`, so a caller cannot leave the slot
    /// waking relay on a freed order. A user created before the block existed
    /// has none, and the `seeds` pin the address.
    #[account(
        mut,
        seeds = [
            crate::state::user_conditions::USER_CONDITIONS_PDA_SEED,
            user.key().as_ref(),
        ],

        bump
    )]
    pub trigger_conditions: UncheckedAccount<'info>,
    /// CHECK: address-locked to the instructions sysvar. It supplies whether
    /// the owner signed the transaction and how many accounts it locks, which
    /// are the filler-obligation facts a fill needs. It is optional and costs
    /// one lock. A fill needs it only when a book withholds
    /// depth for an owner the transaction does not carry, and the owner did not
    /// sign. A trigger crank's owner never signs, so a fill that reaches a
    /// withheld order and passes `None` here is refused.
    #[account(address = ::solana_program::sysvar::instructions::ID)]
    pub ix_sysvar: Option<UncheckedAccount<'info>>,
    /// The SOL spot market, whose TWAP values the reservoir payment in quote.
    /// Program-keeper mode requires it when `State` names a SOL market.
    #[account(
        seeds = [
            b"spot_market",
            state.load()?.sol_spot_market_index.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub sol_spot_market: Option<AccountLoader<'info, SpotMarket>>,
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
    // Shared for `'info`, because the conditions loader borrows its account
    // for that long.
    let accounts: &'info TriggerMarketOrderV1<'info> = ctx.accounts;
    let trigger_conditions =
        super::helpers::crank_common::user_conditions_loader(&accounts.trigger_conditions)?;
    let clock = &Clock::get()?;
    let state = accounts.state.load()?;

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
        &accounts.user,
    )?;

    // Firing is irreversible: it frees the trigger slot, charges the flat
    // reward, and turns the order into a book rest. Refuse before anything
    // moves rather than leave the owner with a paid fee and no order. The
    // trigger stays armed until the book quotes again.
    validate!(
        accounts.quoter_slab.clob_slot(market_index)?.quotes(),
        ErrorCode::ClobRestUnavailable,
        "market {}'s book takes no new orders; the trigger stays armed",
        market_index
    )?;

    let keeper_fee = super::helpers::crank_common::TriggerFeeAccounts {
        state: &accounts.state,
        filler: &accounts.filler,
        crank_conditions: &accounts.crank_conditions,
        trigger_conditions: &trigger_conditions,
        sol_spot_market: &accounts.sol_spot_market,
    }
    .keeper_fee(market_index, order_id)?;

    // Fire the trigger. This validates it, turns a copy of the slot order into
    // a live market order, and frees the slot. `None` means there was no
    // payable work. The order was past its `max_ts`, or it was cancelled unpaid
    // because it had nothing to reduce or the account could not carry it.
    // Either way, skip the fill and the reservoir payout.
    let trigger_accounts = controller::orders::TriggerAccounts {
        user: &accounts.user,
        user_stats: &accounts.user_stats,
        filler: &accounts.filler,
    };
    let Some(fire) = controller::orders::trigger_and_route_order(
        controller::orders::OrderToFire {
            market_index,
            order_id,
        },
        &state,
        &trigger_accounts,
        &mut maps,
        clock,
    )?
    else {
        return Ok(());
    };

    let mut fired = fire.order;
    route_fill_fired_order(
        accounts,
        &mut maps,
        &mut route_accounts,
        &mut fired,
        &signed_route,
        clock,
    )?;

    let outcome = rest_fired_remainder(accounts, &mut maps, &fired, &fire.armed, clock)?;
    let filler_reward = controller::orders::pay_fired_trigger(
        &fire,
        &fired,
        outcome,
        keeper_fee.quote,
        &trigger_accounts,
        &mut maps,
        clock,
    )?;

    // A fire that arms the stop again earns only a share of the quote reward,
    // so it draws no reservoir lamports and keeps the trigger wake slot. Drop
    // the state borrow first, because the reservoir payout loads state itself.
    drop(state);
    super::helpers::crank_common::finish_trigger_crank(
        &accounts.state,
        &accounts.filler,
        &accounts.authority,
        &accounts.user,
        &trigger_conditions,
        &accounts.crank_conditions,
        &super::helpers::crank_common::CrankedTrigger {
            market_index,
            order_id,
            keeper_reward: filler_reward,
            pay_lamports: keeper_fee.pay_lamports && outcome == FireOutcome::Spent,
            release_slot: outcome == FireOutcome::Spent,
        },
    )?;

    Ok(())
}

/// Routes the fired order against the book, and fills what the route reaches.
///
/// A caller routes the fill only by staging a quoter tail. A keeper that read
/// the book stages one and fills here. The relay resolver stages none and goes
/// straight to the rest. Relay cannot route, because it sees only the book and
/// not the propAMMs, so a fill it stages would take a worse price than the full
/// router. The whole order rests taker-origin instead, and the taker-origin
/// cross crank fills it later. See the module doc.
///
/// Maker priority gates the staged fill the way it gates every taker route. On
/// a book with a speed bump, only attested flow fills synchronously. An
/// unattested keeper's tail is ignored, and the fired order rests whole, as it
/// does on the relay path. A trigger crank is keeper-built and carries no
/// attestation transport, so its flow never counts as protected.
fn route_fill_fired_order<'info>(
    accounts: &TriggerMarketOrderV1<'info>,
    maps: &mut crate::instructions::optional_accounts::AccountMaps<'info>,
    route_accounts: &mut crate::instructions::RouteFillAccounts<'info>,
    fired: &mut Order,
    signed_route: &[Pubkey],
    clock: &Clock,
) -> Result<()> {
    let market_index = fired.market_index;
    let state = accounts.state.load()?;
    let taker_served_window = false;
    let synchronous_take = crate::instructions::synchronous_take_allowed(
        taker_served_window,
        &accounts.quoter_slab,
        market_index,
    )?;

    if route_accounts.quoters.is_empty() || !synchronous_take {
        return Ok(());
    }

    let order = crate::instructions::RoutedOrder::read(
        &*load!(accounts.user)?,
        fired,
        maps,
        FillMode::Fill,
    )?;
    if order.unfilled == 0 {
        return Ok(());
    }

    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    crate::instructions::RouteFill {
        state: &state,
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
                accounts.ix_sysvar.as_ref().map(|sysvar| sysvar.as_ref()),
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
            user: &accounts.user,
            user_stats: &accounts.user_stats,
            filler: &accounts.filler,
            filler_stats: &accounts.filler_stats,
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
/// A refusal that comes from the order itself cancels the remainder. Any other
/// refusal can clear, so the stop must not lose the remainder to it. A fire that filled nothing fails the crank,
/// so the trigger stays armed and nothing is paid. A fire that filled part
/// cannot be undone, so the unfilled part is armed again in its slot.
fn rest_fired_remainder<'info>(
    accounts: &TriggerMarketOrderV1<'info>,
    maps: &mut crate::instructions::optional_accounts::AccountMaps<'info>,
    fired: &Order,
    armed: &controller::orders::ArmedSlot,
    clock: &Clock,
) -> Result<FireOutcome> {
    let rest_oracle_price = {
        let oracle_id = maps
            .perp_market_map
            .get_ref(&fired.market_index)?
            .oracle_id();
        maps.oracle_map.get_price_data(&oracle_id)?.price
    };

    let rest = crate::instructions::rest_detached_remainder(
        &crate::instructions::ClobRestAccounts {
            state: &accounts.state,
            user: &accounts.user,
            quoter_slab: &accounts.quoter_slab,
            clob_market: &accounts.clob_market.to_account_info(),
            clob_program: &accounts.clob_program.to_account_info(),
        },
        maps,
        fired,
        &crate::instructions::DetachedRemainderTerms {
            rest_oracle_price: Some(rest_oracle_price),
            activation_delay_slots: None,
        },
        clock,
    )?;

    let reason = match rest {
        DetachedRemainder::Rested(_) | DetachedRemainder::Ended => return Ok(FireOutcome::Spent),
        DetachedRemainder::Refused(reason) => reason,
    };

    if fired.base_asset_amount_filled == armed.order.base_asset_amount_filled {
        return Err(reason.error_code().into());
    }

    let unfilled = fired.get_base_asset_amount_unfilled(None)?;
    msg!(
        "book refuses the remainder of trigger order {} ({:?}); {} base is armed again",
        fired.order_id,
        reason,
        unfilled
    );
    controller::orders::re_arm_fired_trigger(&mut *load_mut!(accounts.user)?, armed, fired)?;
    Ok(FireOutcome::ReArmed)
}

/// The relay resolver for `trigger_market_order_v1`. It is simulation-only and
/// is staged from the user's synced trigger conditions. It fires an armed
/// stop-market to the book.
///
/// The staged executor carries no quoter tail, so it does not fill. The whole
/// fired order rests taker-origin, and the taker-origin cross crank fills it
/// later. See the module doc. A resolver sees only the book and not the
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
    /// The exchange pause, the median-price flag and the oracle guard rails
    /// the executor judges the trigger with.
    pub state: AccountLoader<'info, State>,
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == perp_market.load()?.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it. The resolver asks it for the
    /// room left on the side the order rests on.
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: address-locked to velocity's CLOB.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}

pub fn handle_resolve_trigger_market_order_v1(
    ctx: Context<ResolveTriggerMarketOrderV1>,
    fired: super::FiredConditionArgV0,
) -> Result<()> {
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let accounts = &ctx.accounts;
        let due = super::helpers::crank_common::resolve_due_trigger(
            &super::helpers::crank_common::TriggerResolverAccounts {
                trigger_conditions: &accounts.trigger_conditions,
                user: &accounts.user,
                oracle: &accounts.oracle,
                perp_market: &accounts.perp_market,
                state: &accounts.state,
                quoter_slab: &accounts.quoter_slab,
                clob_market: &accounts.clob_market,
                clob_program: &accounts.clob_program,
            },
            &fired,
            super::helpers::crank_common::TriggerResolverKind::ClobFill,
        )?;
        let Some(meta) = due else {
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

                trigger_conditions: ctx.accounts.trigger_conditions.key(),
                // No fill, so no filler-obligation read of the sysvar.
                ix_sysvar: None,
                sol_spot_market: super::helpers::crank_common::sol_spot_market_ref(
                    &*ctx.accounts.state.load()?,
                ),
            })
            // The margin gate of a risk-increasing fire loads every market the
            // user holds, so the stored map rides whole. Its crank tail does
            // not, so the executor rests the whole order rather than routing it.
            .refs(
                ctx.accounts
                    .trigger_conditions
                    .load()?
                    .read_margin_map_accounts(),
            )
            .arg(TriggerMarketOrderV1Args {
                market_index: meta.market_index,
                order_id: meta.order_id,
                signed_route: Vec::new(),
            })?,
        ))
    })
}

#[cfg(test)]
mod side_room_tests {
    use crate::{
        controller::position::PositionDirection,
        instructions::clob::helpers::crank_common::side_has_room, state::prop_amm::OrderRulesV0,
    };

    fn rules() -> OrderRulesV0 {
        OrderRulesV0 {
            min_order_size: 0,
            blocking_min_size: 0,
            default_activation_delay_slots: 0,
            max_activation_delay_slots: 0,
            place_authority: [0; 32],
            tick_size: 1,
            step_size: 1,
            side_order_counts: [255, 256],
            arena_capacity: 512,
            evict_threshold_per_side: 200,
            authority: [0; 32],
        }
    }

    /// The trigger resolver stages no fire onto a full side, so the stop
    /// waits for an eviction to free room.
    #[test]
    fn a_full_side_has_no_room_and_the_other_side_does() {
        assert!(side_has_room(&rules(), PositionDirection::Long));
        assert!(!side_has_room(&rules(), PositionDirection::Short));
    }
}
