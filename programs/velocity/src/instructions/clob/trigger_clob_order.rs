//! Trigger a resting trigger-limit order onto the market's CLOB.
//!
//! `User.orders` is the conditional store: a trigger-limit rests there
//! (`Armed`) until a keeper cranks this instruction with the trigger
//! condition met, at which point velocity places the order on the CLOB and
//! the slot becomes a shadow (`Placed`) keeping the trigger params + the
//! CLOB `OrderRef`. The shadow deliberately stays *untriggered* so every
//! DLOB matching path ignores it exactly like an armed order — the
//! [`OrderBitFlag::PlacedOnClob`] bit alone marks it, and the CLOB order
//! carries the slot's open-order count from here on.
//!
//! Gating mirrors today's `trigger_order`: oracle validity + TWAP
//! divergence, and a risk-increasing, non-reduce-only trigger on an account
//! failing initial margin, the buffered equity floor, or the authority
//! equity breaker is cancelled with `InsufficientFreeCollateral` instead of
//! placed — never re-armed, so an underfunded stop can't livelock. The
//! keeper earns the same flat reward from the user.
//!
//! Re-triggering after an eviction is edge-gated
//! ([`OrderBitFlag::AwaitingTriggerRecross`]): while the flag is set, a
//! crank that observes the price on the non-trigger side clears it (and
//! places nothing); a crank that observes it still through the trigger
//! fails. The on-chain approximation of "price must cross back through the
//! trigger", which is what stops an evicted stop-limit — near the tail by
//! definition — from re-placing into an immediate re-eviction.
//!
//! The placed order rests **taker-origin**. A fired trigger is an order that
//! came to trade, so it gets what any other taker remainder gets: a cross
//! settles at the counterparty's price rather than its own, and the
//! activation-slot window turns the race to fill it into a race on price.
//! That is also how a fired trigger reaches a route at all — the taker-origin
//! cross crank carries the market's baseline book, and an order resting as an
//! ordinary maker quote never asks for one.
//!
//! Two consequences follow. Its owner pays taker fees when a counterparty
//! crosses it, which is the price of demanding liquidity. And it cannot be
//! cancelled before its activation slot, so a trigger commits its owner for
//! that window; liquidation force-cancel stays exempt, and `max_ts` still
//! bounds its life.
//!
//! Stop-markets never come here: a triggered stop-market becomes plain
//! taker flow through `trigger_order` and takes the CLOB speed bump like
//! any unattested taker.

use {
    crate::{
        controller::{
            orders::{cancel_order, pay_keeper_flat_reward_for_perps},
            position::{
                decrease_open_bids_and_asks, get_position_index, increase_open_bids_and_asks,
                PositionDirection,
            },
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load, load_mut,
        math::{
            casting::Cast,
            liquidation::validate_user_not_being_liquidated,
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, MarginRequirementType,
            },
            oracle::{is_oracle_valid_for_action, VelocityAction},
            orders::{is_oracle_too_divergent_with_twap_5min, order_satisfies_trigger_condition},
        },
        msg,
        signer::CLOB_AUTHORITY_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::OrderActionExplanation,
            margin_calculation::MarginContext,
            market_status::MarketStatus,
            perp_market_map::MarketSet,
            prop_amm::{ClobMarket, ClobPlaceOrderArgsV0, ClobSide, QuoterV0, WireDirectionExt},
            state::State,
            user::{MarketType, OrderBitFlag, OrderType, User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct TriggerClobOrder<'info> {
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
    /// Read for the authority-wide equity breaker in the margin gate.
    #[account(constraint = is_stats_for_user(&user, &user_stats)?)]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// The market's CLOB registry entry — placement is only allowed on a
    /// vetted book, same as a direct `place_clob_order`.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the CLOB place authority PDA — what a book's `place_authority`
    /// is set to. Its own key, distinct from the per-entry signer a
    /// third-party quoter is handed: signer privilege is inherited by a
    /// callee, and this one may place and cancel on any book, for any user.
    #[account(seeds = [CLOB_AUTHORITY_SEED], bump)]
    pub clob_authority: UncheckedAccount<'info>,
    /// Expiry-hint host, same optional contract as `place_clob_order`.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    /// The user's relay trigger conditions: the fired slot is released so
    /// its level-triggered wake goes quiet. Optional, like everything else
    /// on the relay side.
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
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_trigger_clob_order<'c: 'info, 'info>(
    ctx: Context<'info, TriggerClobOrder<'info>>,
    market_index: u16,
    order_id: u32,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let state = ctx.accounts.state.load()?;
    let user_key = ctx.accounts.user.key();
    let filler_key = ctx.accounts.filler.key();

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        &mut remaining_accounts,
        &MarketSet::new(),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let clob = {
        let quoter = ctx.accounts.quoter.load()?;
        validate!(
            quoter.is_active && quoter.is_approved,
            ErrorCode::DefaultError,
            "CLOB quoter is not active and approved"
        )?;
        ClobMarket::from_quoter(
            &quoter,
            market_index,
            &ctx.accounts.clob_market,
            &ctx.accounts.clob_program,
            &ctx.accounts.clob_authority,
            ctx.bumps.clob_authority,
        )?
    };

    // ---- Gate, reserve, reward — everything that can decide NOT to place,
    // while the user is borrowed. ----
    let (side, price, base_asset_amount, max_ts, user_ref) = {
        let user = &mut load_mut!(ctx.accounts.user)?;
        let user_stats = load!(ctx.accounts.user_stats)?;

        let order_index = user
            .orders
            .iter()
            .position(|order| {
                order.order_id == order_id && order.status == crate::state::user::OrderStatus::Open
            })
            .ok_or(ErrorCode::OrderDoesNotExist)?;

        validate!(
            user.orders[order_index].order_type == OrderType::TriggerLimit,
            ErrorCode::OrderNotTriggerable,
            "only trigger-limit orders place on the CLOB (stop-markets go through trigger_order)"
        )?;
        validate!(
            !user.orders[order_index].is_placed_on_clob(),
            ErrorCode::OrderPlacedOnClob,
            "order already rests on the CLOB"
        )?;
        validate!(
            user.orders[order_index].market_type == MarketType::Perp
                && user.orders[order_index].market_index == market_index,
            ErrorCode::InvalidOrderMarketType,
            "order is not a perp order on market {}",
            market_index
        )?;
        validate!(
            !user.orders[order_index].has_oracle_price_offset(),
            ErrorCode::InvalidOrderOracleOffset,
            "oracle-offset trigger orders cannot rest at a fixed CLOB price"
        )?;

        validate_user_not_being_liquidated(
            user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            state.liquidation_margin_buffer_ratio,
        )?;
        validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

        let (oracle_price, trigger_price) = {
            let perp_market = perp_market_map.get_ref(&market_index)?;
            validate!(
                matches!(perp_market.status, MarketStatus::Active),
                ErrorCode::MarketPlaceOrderPaused,
                "market not active"
            )?;
            validate!(
                !perp_market.is_in_settlement(now),
                ErrorCode::MarketPlaceOrderPaused,
                "Market is in settlement mode",
            )?;

            let (oracle_price_data, oracle_validity) = oracle_map.get_price_data_and_validity(
                MarketType::Perp,
                perp_market.market_index,
                &perp_market.oracle_id(),
                perp_market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap,
                perp_market.get_max_confidence_interval_multiplier()?,
                perp_market.oracle_slot_delay_override,
                perp_market.oracle_low_risk_slot_delay_override,
                None,
            )?;
            let is_oracle_valid =
                is_oracle_valid_for_action(oracle_validity, Some(VelocityAction::TriggerOrder))?;
            validate!(is_oracle_valid, ErrorCode::InvalidOracle)?;

            let oracle_price = oracle_price_data.price;
            let oracle_too_divergent = is_oracle_too_divergent_with_twap_5min(
                oracle_price,
                perp_market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
                state
                    .oracle_guard_rails
                    .max_oracle_twap_5min_percent_divergence()
                    .cast()?,
            )?;
            validate!(
                !oracle_too_divergent,
                ErrorCode::OrderBreachesOraclePriceLimits,
                "oracle price vs twap too divergent"
            )?;

            let trigger_price = perp_market.get_trigger_price(
                oracle_price,
                now,
                state.use_median_trigger_price(),
            )?;
            (oracle_price, trigger_price)
        };

        let satisfied =
            order_satisfies_trigger_condition(&user.orders[order_index], trigger_price)?;

        // Edge gate after an eviction: a crank observing the price back on
        // the non-trigger side re-arms the trigger for real; one observing
        // it still through the trigger must wait for the recross.
        if user.orders[order_index].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross) {
            validate!(
                !satisfied,
                ErrorCode::OrderAwaitingTriggerRecross,
                "trigger price never crossed back after eviction"
            )?;
            user.orders[order_index].remove_bit_flag(OrderBitFlag::AwaitingTriggerRecross);
            user.update_last_active_slot(slot);
            msg!(
                "trigger order {} observed the recross and is armed again",
                order_id
            );
            return Ok(());
        }

        validate!(
            satisfied,
            ErrorCode::OrderDidNotSatisfyTriggerCondition,
            "Order did not satisfy trigger condition. trigger_price: {} oracle_price: {} trigger_condition: {:?}",
            trigger_price,
            user.orders[order_index].trigger_price,
            user.orders[order_index].trigger_condition
        )?;

        // Reduce-only has no meaning on the book. The CLOB cannot clamp a fill
        // to the maker's position, so a reduce-only order that rests here fills
        // its full size, and a position that shrank elsewhere after placement
        // makes that fill risk-increasing. The trigger-time gate below exempts
        // reduce-only orders the way trigger_order does, which on the CLOB
        // would let the full size rest ungated. So a reduce-only trigger stays
        // on the speed-bumped taker flow (trigger_order), where reduce-only is
        // enforced at fill. Mirrors restable_remainder_price.
        validate!(
            !user.orders[order_index].reduce_only,
            ErrorCode::ReduceOnlyOrderCannotRestOnClob,
            "reduce-only trigger order {} cannot rest on the CLOB",
            order_id
        )?;

        // Reserve worst-case aggregates for the resting order, then gate
        // exactly like trigger_order: a risk-increasing trigger on a failing
        // account cancels instead of placing.
        let direction = user.orders[order_index].direction;
        let base_asset_amount = user.orders[order_index].get_base_asset_amount_unfilled(None)?;
        let (_, worst_case_before) = user
            .get_perp_position(market_index)?
            .worst_case_liability_value(oracle_price)?;
        {
            let user_position = user.get_perp_position_mut(market_index)?;
            increase_open_bids_and_asks(user_position, &direction, base_asset_amount, true)?;
        }
        let (_, worst_case_after) = user
            .get_perp_position(market_index)?
            .worst_case_liability_value(oracle_price)?;
        let is_risk_increasing = worst_case_after > worst_case_before;

        if is_risk_increasing && !user.orders[order_index].reduce_only {
            let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
                user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::standard(MarginRequirementType::Initial),
            )?;
            let net_equity = calculate_net_equity_for_floor(
                user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
            )?;

            // An unverifiable floor rejects the trigger instead of cancelling:
            // a cancel is irreversible, so an oracle blip must not destroy a
            // resting order the account may legitimately carry. The keeper
            // retries once the feed recovers and the gate resolves either way.
            if let Some(net_equity) = net_equity {
                validate!(
                    net_equity.all_oracles_valid,
                    ErrorCode::InvalidOracle,
                    "cannot verify equity floor {} + buffer {} with an invalid oracle (authority {} subaccount {})",
                    user.equity_floor,
                    user.equity_floor_buffer,
                    user.authority,
                    user.sub_account_id
                )?;
            }

            if !margin_calc.meets_margin_requirement()
                || net_equity.is_some_and(|net_equity| !net_equity.clears_buffered_floor(user))
                || user_stats.is_equity_breaker_tripped()
            {
                // The slot reads as untriggered, so cancel_order won't unwind
                // the aggregates we just reserved — take them back first.
                let position_index = get_position_index(&user.perp_positions, market_index)?;
                decrease_open_bids_and_asks(
                    &mut user.perp_positions[position_index],
                    &direction,
                    base_asset_amount,
                    true,
                )?;
                cancel_order(
                    order_index,
                    user,
                    &user_key,
                    &perp_market_map,
                    &spot_market_map,
                    &mut oracle_map,
                    now,
                    slot,
                    OrderActionExplanation::InsufficientFreeCollateral,
                    Some(&filler_key),
                    0,
                    false,
                )?;
                user.update_last_active_slot(slot);
                return Ok(());
            }
        }

        // Trigger accepted: pay the keeper the flat reward from the user.
        let is_filler_user = user_key == filler_key;
        let mut filler = if !is_filler_user {
            Some(load_mut!(ctx.accounts.filler)?)
        } else {
            None
        };
        let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;
        pay_keeper_flat_reward_for_perps(
            user,
            filler.as_deref_mut(),
            &mut perp_market,
            state.perp_fee_structure.flat_filler_fee,
            slot,
        )?;

        let side = match direction {
            PositionDirection::Long => ClobSide::Bid,
            PositionDirection::Short => ClobSide::Ask,
        };
        (
            side,
            user.orders[order_index].price,
            base_asset_amount,
            user.orders[order_index].max_ts,
            crate::state::prop_amm::ClobUserRefV0 {
                authority: user.authority,
                sub_account_id: user.sub_account_id.into(),
            },
        )
    };

    // ---- CPI the placement while no user borrows are held. ----
    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount,
        activation_delay_slots: None,
        max_ts,
        user: user_ref,
        // A fired trigger came to trade. It rests taker-origin so a live
        // counterparty crosses it at the counterparty's price instead of
        // picking it off at its own, and so the activation-slot auction
        // decides who fills it on price rather than on who lands a
        // transaction first. It is also what routes it: the taker-origin
        // cross crank carries the market's baseline book, which an order
        // resting as an ordinary maker quote never asks for.
        taker_origin: true,
        // The slot the trigger armed keeps its id: to its owner this is the
        // order they placed, now live, and the shadow slot holds the same id.
        client_order_id: order_id,
        // A triggered stop is meant to reach the market. Refusing it for
        // crossing would leave the position unprotected, which is the one
        // thing the trigger exists to prevent.
        reject_if_crossed: false,
    })?;

    // ---- Mark the slot as the placed shadow. ----
    {
        let mut user = load_mut!(ctx.accounts.user)?;
        let order_index = user
            .orders
            .iter()
            .position(|order| {
                order.order_id == order_id && order.status == crate::state::user::OrderStatus::Open
            })
            .ok_or(ErrorCode::OrderDoesNotExist)?;
        user.orders[order_index].set_clob_order_ref(order_ref.node_index, order_ref.order_id);
        user.orders[order_index].add_bit_flag(OrderBitFlag::PlacedOnClob);
        user.update_last_active_slot(slot);
    }

    // A trigger that fired is an order that started resting, and it rests
    // under the id it armed under. The slot it came from is now a shadow, so
    // this record is the only statement that the order is live.
    super::emit_clob_place_record(
        now,
        &user_key,
        super::ClobOrderFacts {
            order_id,
            market_index,
            direction: side.to_position_direction(),
            price,
            base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts,
            slot,
            taker_origin: true,
        },
    )?;

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

    msg!(
        "triggered order {} onto the clob as order {} (node {}) for user {}",
        order_id,
        order_ref.order_id,
        order_ref.node_index,
        user_key
    );
    Ok(())
}

/// The relay resolver for `trigger_clob_order` (`Resolve<EndpointName>`):
/// simulation-only, staged from the user's synced trigger conditions.
#[derive(Accounts)]
pub struct ResolveTriggerClobOrder<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account, not
    /// into the block they read.
    #[account(constraint = trigger_conditions.load()?.user == user.key())]
    pub trigger_conditions: AccountLoader<'info, crate::state::user_conditions::UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
    /// CHECK: validated against the market's oracle in the handler.
    pub oracle: UncheckedAccount<'info>,
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
}

pub fn handle_resolve_trigger_clob_order(ctx: Context<ResolveTriggerClobOrder>) -> Result<()> {
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        let fired = {
            let conditions = ctx.accounts.trigger_conditions.load()?;
            let user = crate::load!(ctx.accounts.user)?;
            let market = ctx.accounts.perp_market.load()?;
            super::crank_common::find_fired_trigger(
                &conditions,
                &user,
                &market,
                &ctx.accounts.oracle,
                clock.slot,
                true,
            )?
        };
        let Some(meta) = fired else {
            return Ok(None);
        };

        let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
        let user_stats =
            crate::state::pdas::user_stats(&crate::load!(ctx.accounts.user)?.authority);
        Ok(Some(
            crate::staged_call!(TriggerClobOrder {
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
            })
            .refs(ctx.accounts.trigger_conditions.load()?.read_sync_accounts())
            .arg(meta.market_index)?
            .arg(meta.order_id)?,
        ))
    })
}
