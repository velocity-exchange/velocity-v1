//! Trigger a resting trigger-limit order onto the market's CLOB.
//!
//! `User.orders` is the conditional store. A trigger-limit rests there, armed,
//! until a keeper cranks this instruction with the trigger condition met.
//! Velocity then places the order on the CLOB, and the slot becomes a shadow
//! that holds the trigger parameters and the CLOB `OrderRef`. The shadow stays
//! untriggered, so every discovery path ignores it the way it ignores an
//! armed order. Only the [`OrderBitFlag::PlacedOnClob`] bit marks it, and the
//! CLOB order carries the slot's open-order count from that point on.
//!
//! The gates are the gates of `trigger_market_order_v1`: oracle validity and
//! TWAP divergence. A risk-increasing, non-reduce-only trigger is cancelled with
//! `InsufficientFreeCollateral` rather than placed when the account fails
//! initial margin, the buffered equity floor, or the authority equity breaker.
//! Such an order is never re-armed, so an underfunded stop cannot repeat
//! forever. The keeper earns nothing for that cancel, so relay cannot land it.
//! The order then stays armed in its own relay slot and holds up no other
//! trigger, because each slot's resolver stages only that slot's order.
//!
//! A reduce-only trigger rests at most the position it can reduce. With no
//! position left to reduce, it is cancelled with
//! `ReduceOnlyOrderIncreasedPosition` and the keeper earns nothing.
//!
//! A fired order the book would refuse, for its size, step, price, expiry or a
//! full side, is cancelled rather than placed, and the keeper earns the flat
//! reward. A refused placement would revert every crank and hold the owner's
//! later triggers behind it.
//!
//! Re-triggering after an eviction runs behind an edge gate, which is
//! [`OrderBitFlag::AwaitingTriggerRecross`]. While the flag is set, a crank
//! that observes the price on the non-trigger side clears it and places
//! nothing. That crank earns the flat reward, because the stop fires again
//! only once the flag is clear. A crank that observes the price still through
//! the trigger fails. This is the on-chain approximation of a price that must
//! cross back through the trigger. An evicted stop-limit sits near the tail by
//! definition, and the gate stops it from re-placing into an immediate second
//! eviction.
//!
//! The placed order rests taker-origin. A fired trigger is an order that came
//! to trade, so it gets what any other taker remainder gets. A cross settles at
//! the counterparty's price rather than its own, and the activation-slot window
//! turns the race to fill it into a race on price. Resting taker-origin is also
//! how a fired trigger reaches a route at all. The taker-origin cross crank
//! carries the market's baseline book, and an order resting as an ordinary
//! maker quote never asks for one.
//!
//! Two consequences follow. The owner pays taker fees when a counterparty
//! crosses the order, which is the price of demanding liquidity. The order also
//! cannot be cancelled before its activation slot, so a trigger commits its
//! owner for that window. A liquidation force-cancel stays exempt, and `max_ts`
//! still bounds the order's life.
//!
//! Stop-markets never come here. `trigger_market_order_v1` fires them, fills
//! them through the router, and rests only the remainder. A fired market order
//! fills first. A fired limit rests whole.

use {
    super::helpers::placement::{rest_admission, RestAdmission, RestRefusal},
    crate::{
        controller::{
            orders::{cancel_order, pay_keeper_flat_reward_for_perps},
            position::PositionDirection,
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
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
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::OrderActionExplanation,
            margin_calculation::MarginContext,
            oracle_map::OracleMap,
            perp_market_map::{MarketSet, PerpMarketMap},
            prop_amm::{
                ClobMarket, ClobOrderRefV0, OrderRulesV0, PlaceOrderArgsV0, QuoterSlabExt,
                QuoterSlabV0, SideV0, UserRefV0,
            },
            state::State,
            user::{
                MarketType, Order, OrderBitFlag, OrderReservation, OrderTriggerCondition,
                OrderType, User, UserStats,
            },
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct TriggerLimitOrderV1Args {
    pub market_index: u16,
    /// The trigger-limit order to fire, by its `User.orders` id.
    pub order_id: u32,
}

#[derive(Accounts)]
#[instruction(args: TriggerLimitOrderV1Args)]
pub struct TriggerLimitOrderV1<'info> {
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
    /// Read for the authority-wide equity breaker in the margin gate. A cancel
    /// by that gate can trip the breaker.
    #[account(mut, constraint = is_stats_for_user(&user, &user_stats)?)]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// The market's quoter slab. Placement is allowed only on the vetted book
    /// that its `Clob` slot names, as in `place_and_make_perp_order_v1`.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: validated against the book slot's registered response account
    /// in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// Expiry-hint host, same optional contract as `place_and_make_perp_order_v1`.
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
}

/// Fires an armed stop-limit trigger onto the book as a taker-origin order.
///
/// Everything that can decide against placing runs while `user` is borrowed:
/// the gate, the reservation, and the reward. The CPI that places the order
/// runs afterward, with no borrow of `user` held.
#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_trigger_limit_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, TriggerLimitOrderV1<'info>>,
    args: TriggerLimitOrderV1Args,
) -> Result<()> {
    let TriggerLimitOrderV1Args {
        market_index,
        order_id,
    } = args;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let state = ctx.accounts.state.load()?;
    let user_key = ctx.accounts.user.key();

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps: AccountMaps = load_maps(
        &mut remaining_accounts,
        &MarketSet::new(),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let clob = {
        let slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
        validate!(
            slot.quotes(),
            ErrorCode::ClobQuoterNotActive,
            "CLOB quoter is not active and approved"
        )?;

        drop(slot);
        ClobMarket::from_slab(
            &ctx.accounts.quoter_slab,
            market_index,
            &ctx.accounts.clob_market,
            &ctx.accounts.clob_program,
        )?
    };

    // Read before the account is borrowed, so the gate tests the book's rules
    // without holding a borrow across the CPI.
    let rules = clob.reader().order_rules()?;
    let crank = TriggerLimitCrank {
        user: &ctx.accounts.user,
        user_stats: &ctx.accounts.user_stats,
        filler: &ctx.accounts.filler,
        state: &state,
        market_index,
        order_id,
        clock: &clock,
    };

    let TriggerPlacement {
        side,
        price,
        base_asset_amount,
        max_ts,
        reduce_only,
        user_ref,
        is_isolated_position,
        filler_reward,
    } = match crank.decide(&mut maps, &rules)? {
        TriggerLimitStep::NoWork => return Ok(()),
        TriggerLimitStep::Settled { keeper_reward } => {
            return super::helpers::crank_common::finish_trigger_crank(
                &ctx.accounts.state,
                &ctx.accounts.filler,
                &ctx.accounts.authority,
                &ctx.accounts.user,
                &ctx.accounts.trigger_conditions,
                &ctx.accounts.crank_conditions,
                &super::helpers::crank_common::CrankedTrigger {
                    market_index,
                    order_id,
                    keeper_reward,
                    release_slot: true,
                },
            );
        }
        TriggerLimitStep::Rearmed { keeper_reward } => {
            rewatch_rearmed_trigger(
                &ctx.accounts.user,
                &ctx.accounts.trigger_conditions,
                &maps,
                market_index,
                order_id,
            )?;

            return super::helpers::crank_common::finish_trigger_crank(
                &ctx.accounts.state,
                &ctx.accounts.filler,
                &ctx.accounts.authority,
                &ctx.accounts.user,
                &ctx.accounts.trigger_conditions,
                &ctx.accounts.crank_conditions,
                &super::helpers::crank_common::CrankedTrigger {
                    market_index,
                    order_id,
                    keeper_reward,
                    release_slot: false,
                },
            );
        }
        TriggerLimitStep::Place(placement) => placement,
    };

    let order_ref = clob.place(PlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount,
        activation_delay_slots: None,
        max_ts,
        user: user_ref,
        // A fired trigger rests taker-origin, so a live counterparty crosses
        // it at the counterparty's price, and the activation window decides
        // the fill by price rather than by who lands a transaction
        // first. Taker-origin is also what routes it into the cross crank.
        taker_origin: true,
        // The slot the trigger armed keeps its id. To the owner this is the
        // order they placed, now live, and the shadow slot holds the same
        // id.
        client_order_id: order_id,
        // A triggered stop is meant to reach the market. Refusing it for
        // crossing would leave the position unprotected, which is the one
        // thing the trigger exists to prevent.
        reject_if_crossed: false,
        // A reduce-only trigger rests flagged. The book clamps its fills to
        // the owner's base cover.
        reduce_only,
    })?;

    // Mark the slot as the placed shadow.
    mark_slot_placed(&ctx.accounts.user, order_id, &order_ref, slot)?;

    // A trigger that fired is an order that started resting, and it rests
    // under the id it armed under. The slot it came from is now a shadow, so
    // this record is the only statement that the order is live.
    super::emit_clob_place_record(
        now,
        &user_key,
        super::ClobOrderFacts {
            order_id,
            market_index,
            direction: PositionDirection::from(side),
            price,
            base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts,
            slot,
            taker_origin: true,
        },
        is_isolated_position,
    )?;

    super::helpers::crank_common::finish_trigger_crank(
        &ctx.accounts.state,
        &ctx.accounts.filler,
        &ctx.accounts.authority,
        &ctx.accounts.user,
        &ctx.accounts.trigger_conditions,
        &ctx.accounts.crank_conditions,
        &super::helpers::crank_common::CrankedTrigger {
            market_index,
            order_id,
            keeper_reward: filler_reward,
            release_slot: true,
        },
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

/// What one trigger crank reads, fixed for the whole crank.
struct TriggerLimitCrank<'a, 'info> {
    user: &'a AccountLoader<'info, User>,
    user_stats: &'a AccountLoader<'info, UserStats>,
    filler: &'a AccountLoader<'info, User>,
    state: &'a State,
    market_index: u16,
    order_id: u32,
    clock: &'a Clock,
}

/// What the crank does once the owner's account is released.
enum TriggerLimitStep {
    /// Nothing moved, and the keeper earns nothing.
    NoWork,
    /// The crank ended the trigger's work without a placement, and collected
    /// this reward from the owner.
    Settled { keeper_reward: u64 },
    /// The crank observed an evicted order's recross and re-armed it, and
    /// collected this reward from the owner. The relay watch must move back to
    /// the trigger side.
    Rearmed { keeper_reward: u64 },
    /// The order rests on the book at these terms.
    Place(TriggerPlacement),
}

/// The terms the fired order rests on the book at.
struct TriggerPlacement {
    side: SideV0,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    reduce_only: bool,
    user_ref: UserRefV0,
    is_isolated_position: bool,
    filler_reward: u64,
}

impl TriggerLimitCrank<'_, '_> {
    /// Run every gate that can decide against placing, while the owner's
    /// account is borrowed: the trigger condition, the reservation, the
    /// book's rules and the reward.
    fn decide(&self, maps: &mut AccountMaps<'_>, rules: &OrderRulesV0) -> Result<TriggerLimitStep> {
        let now = self.clock.unix_timestamp;
        let user_key = self.user.key();
        let filler_key = self.filler.key();
        let user = &mut load_mut!(self.user)?;
        let user_stats = &mut load_mut!(self.user_stats)?;

        let fired = match self.fire(user, maps)? {
            TriggerFire::Fired(fired) => fired,
            TriggerFire::Done(step) => return Ok(step),
        };

        let Some(reserved) = reserve_and_gate_trigger(
            user,
            user_stats,
            fired.order_index,
            self.market_index,
            fired.prices.oracle_price,
            maps,
            &user_key,
            &filler_key,
            now,
            self.clock.slot,
        )?
        else {
            return Ok(TriggerLimitStep::NoWork);
        };

        let admission = rest_admission(
            rules,
            reserved.direction,
            user.orders[fired.order_index].price,
            reserved.base_asset_amount,
            user.orders[fired.order_index].max_ts,
            None,
            now,
        );

        let filler_reward = pay_trigger_keeper(
            user,
            self.filler,
            &maps.perp_market_map,
            self.market_index,
            self.state.perp_fee_structure.flat_filler_fee,
            &user_key,
            &filler_key,
            self.clock.slot,
        )?;

        let price = match admission {
            RestAdmission::Admitted { price } => price,
            RestAdmission::Refused(reason) => {
                cancel_refused_trigger(
                    user,
                    fired.order_index,
                    &reserved,
                    reason,
                    maps,
                    &TriggerCancel {
                        market_index: self.market_index,
                        user_key: &user_key,
                        filler_key: &filler_key,
                        filler_reward,
                        clock: self.clock,
                    },
                )?;

                return Ok(TriggerLimitStep::Settled {
                    keeper_reward: filler_reward,
                });
            }
        };

        self.placement(user, &fired, &reserved, price, filler_reward)
            .map(TriggerLimitStep::Place)
    }

    /// Find the armed order and hold it to the trigger condition.
    fn fire(&self, user: &mut User, maps: &mut AccountMaps<'_>) -> Result<TriggerFire> {
        let now = self.clock.unix_timestamp;
        let order_index = find_armed_trigger_limit(user, self.order_id, self.market_index)?;

        // An armed trigger past its own `max_ts` is dead. `should_expire_order`
        // exempts it, so the sweep never removes it, and firing would pay the
        // keeper and then revert. This is a no-op rather than a refusal, since
        // reverting would starve every armed trigger behind it on this account.
        let order_max_ts = user.orders[order_index].max_ts;
        if order_max_ts != 0 && now > order_max_ts {
            msg!(
                "Order max_ts {} passed (now {}); nothing to trigger",
                order_max_ts,
                now
            );

            return Ok(TriggerFire::Done(TriggerLimitStep::NoWork));
        }

        validate_user_not_being_liquidated(user, maps, self.state.liquidation_margin_buffer_ratio)?;
        validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

        let prices = read_trigger_prices(
            &maps.perp_market_map,
            &mut maps.oracle_map,
            self.state,
            &user.orders[order_index],
            now,
        )?;

        let fired = observe_trigger_condition(
            user,
            order_index,
            self.order_id,
            prices.trigger_price,
            self.clock.slot,
        )?;

        if fired {
            return Ok(TriggerFire::Fired(FiredTriggerLimit {
                order_index,
                prices,
            }));
        }

        let keeper_reward = pay_trigger_keeper(
            user,
            self.filler,
            &maps.perp_market_map,
            self.market_index,
            self.state.perp_fee_structure.flat_filler_fee,
            &self.user.key(),
            &self.filler.key(),
            self.clock.slot,
        )?;

        Ok(TriggerFire::Done(TriggerLimitStep::Rearmed {
            keeper_reward,
        }))
    }

    /// Record the trigger and state the terms the order rests at.
    fn placement(
        &self,
        user: &User,
        fired: &FiredTriggerLimit,
        reserved: &ReservedTrigger,
        price: u64,
        filler_reward: u64,
    ) -> Result<TriggerPlacement> {
        // The reservation holds `open_orders` on the position, so the order's
        // margin regime is fixed from here on and the records can state it.
        let is_isolated_position = user.get_perp_position(self.market_index)?.is_isolated();
        crate::controller::orders::TriggerRecord {
            fired: fired_view(&user.orders[fired.order_index]),
            user: self.user.key(),
            filler: self.filler.key(),
            filler_reward,
            oracle_price: fired.prices.oracle_price,
            trigger_price: fired.prices.trigger_price,
            is_isolated_position,
        }
        .emit(self.clock.unix_timestamp)?;

        Ok(TriggerPlacement {
            side: SideV0::from(reserved.direction),
            price,
            base_asset_amount: reserved.base_asset_amount,
            max_ts: user.orders[fired.order_index].max_ts,
            reduce_only: reserved.reduce_only,
            user_ref: user.clob_user_ref(),
            is_isolated_position,
            filler_reward,
        })
    }
}

/// What holding the armed order to its trigger condition decided.
enum TriggerFire {
    Fired(FiredTriggerLimit),
    /// The crank ends here. An observed recross is paid work, and an expired
    /// order is none.
    Done(TriggerLimitStep),
}

/// The armed order a crank fired, and the prices it fired at.
struct FiredTriggerLimit {
    order_index: usize,
    prices: TriggerPrices,
}

/// Who cancels a trigger, and what the cancel record states.
struct TriggerCancel<'a> {
    market_index: u16,
    user_key: &'a Pubkey,
    filler_key: &'a Pubkey,
    filler_reward: u64,
    clock: &'a Clock,
}

/// Cancel a fired trigger that the book would refuse to hold.
///
/// The book fails a placement it refuses, and the failure reverts the crank.
/// The trigger then stays armed and due, and the resolver stages it ahead of
/// every later trigger of the owner. A clamped reduce-only order below the
/// book's minimum or off its step is refused on every crank. The book
/// reservation that the gate took moves back to the slot before the cancel
/// releases it.
fn cancel_refused_trigger(
    user: &mut User,
    order_index: usize,
    reserved: &ReservedTrigger,
    reason: RestRefusal,
    maps: &mut AccountMaps<'_>,
    cancel: &TriggerCancel,
) -> Result<()> {
    msg!(
        "book refuses trigger order {} ({:?}); cancelling it",
        user.orders[order_index].order_id,
        reason
    );

    let armed = OrderReservation::of_order(&user.orders[order_index])?;
    let placed = OrderReservation::book_order(
        cancel.market_index,
        reserved.direction,
        reserved.base_asset_amount,
        reserved.reduce_only,
    );

    user.replace_reservation(&placed, &armed)?;

    cancel_order(
        order_index,
        user,
        cancel.user_key,
        maps,
        cancel.clock.unix_timestamp,
        cancel.clock.slot,
        reason.cancel_explanation(),
        Some(cancel.filler_key),
        cancel.filler_reward,
        false,
    )?;

    user.update_last_active_slot(cancel.clock.slot);
    Ok(())
}

/// The armed slot this crank fires, by order id.
///
/// The slot must be an open trigger-limit on this perp market, and it must not
/// already rest on the CLOB. It must also carry a fixed price, because an
/// oracle-offset order has no price the book can hold.
fn find_armed_trigger_limit(user: &User, order_id: u32, market_index: u16) -> Result<usize> {
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
        "only trigger-limit orders place on the CLOB (stop-markets go through \
         trigger_market_order_v1)"
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

    Ok(order_index)
}

/// The prices a fired trigger is judged and reserved against.
struct TriggerPrices {
    /// The live oracle price. The reservation is sized against it.
    oracle_price: i64,
    /// The price the trigger condition reads.
    trigger_price: u64,
}

/// Reads the prices for a trigger, and refuses a market or an oracle that
/// cannot carry one.
///
/// The market must be active, out of settlement, and not fill-paused. A
/// `ReduceOnly` market admits a reduce-only trigger, which rests flagged so
/// the book clamps its fills to the owner's position. The oracle must be
/// valid for a trigger, and it must stay near the five-minute TWAP. A stale
/// feed or a divergent feed can fire a stop that the market never reached.
fn read_trigger_prices(
    perp_market_map: &PerpMarketMap<'_>,
    oracle_map: &mut OracleMap<'_>,
    state: &State,
    armed: &Order,
    now: i64,
) -> Result<TriggerPrices> {
    let perp_market = perp_market_map.get_ref(&armed.market_index)?;
    validate!(
        super::helpers::crank_common::market_status_admits_trigger(perp_market.status, armed),
        ErrorCode::MarketPlaceOrderPaused,
        "market takes no trigger of this order (status {:?}, reduce only {})",
        perp_market.status,
        armed.reduce_only
    )?;

    crate::controller::orders::trigger_market_gates(&perp_market, now)?;

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

    let trigger_price =
        perp_market.get_trigger_price(oracle_price, now, state.use_median_trigger_price())?;
    Ok(TriggerPrices {
        oracle_price,
        trigger_price,
    })
}

/// Whether the trigger fired.
///
/// A `false` answer ends the crank. The order observed the price back on the
/// non-trigger side after an eviction, so it is armed again and nothing is
/// placed. The keeper that observed it is paid, so the stop can fire again.
fn observe_trigger_condition(
    user: &mut User,
    order_index: usize,
    order_id: u32,
    trigger_price: u64,
    slot: u64,
) -> Result<bool> {
    let satisfied = order_satisfies_trigger_condition(&user.orders[order_index], trigger_price)?;

    // The edge gate after an eviction. A crank that observes the price back on
    // the non-trigger side re-arms the trigger. A crank that observes the price
    // still through the trigger must wait for the recross.
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

        return Ok(false);
    }

    validate!(
        satisfied,
        ErrorCode::OrderDidNotSatisfyTriggerCondition,
        "Order did not satisfy trigger condition. trigger_price: {} oracle_price: {} trigger_condition: {:?}",
        trigger_price,
        user.orders[order_index].trigger_price,
        user.orders[order_index].trigger_condition
    )?;

    Ok(true)
}

/// The exposure this crank reserved for the order it is about to place.
struct ReservedTrigger {
    direction: PositionDirection,
    base_asset_amount: u64,
    reduce_only: bool,
}

/// Reserves the resting order on the account, or cancels the trigger.
///
/// `None` means the gate cancelled the order instead of placing it. The order
/// is never re-armed, so an underfunded stop cannot repeat forever.
///
/// The margin gate exempts a reduce-only order. The book is position-blind,
/// but the router sends it an authoritative `base_cover` per user, and the
/// book clamps every reduce-only fill to that cover.
///
/// The subaccount may already sit below its raw floor when a risk cancel
/// succeeds, so that cancel trips the equity breaker inline.
#[allow(clippy::too_many_arguments)]
fn reserve_and_gate_trigger(
    user: &mut User,
    user_stats: &mut UserStats,
    order_index: usize,
    market_index: u16,
    oracle_price: i64,
    maps: &mut AccountMaps<'_>,
    user_key: &Pubkey,
    filler_key: &Pubkey,
    now: i64,
    slot: u64,
) -> Result<Option<ReservedTrigger>> {
    let explanation = match gate_trigger(
        user,
        user_stats,
        order_index,
        market_index,
        oracle_price,
        maps,
    )? {
        TriggerGate::Rest(reserved) => return Ok(Some(reserved)),
        TriggerGate::Cancel(explanation) => explanation,
    };

    cancel_order(
        order_index,
        user,
        user_key,
        maps,
        now,
        slot,
        explanation,
        Some(filler_key),
        0,
        false,
    )?;

    user.update_last_active_slot(slot);

    if explanation == OrderActionExplanation::InsufficientFreeCollateral {
        crate::controller::equity_floor::try_lazy_equity_breaker_trip(user, user_stats, maps)?;
    }

    Ok(None)
}

/// What the gate decided for a fired trigger.
enum TriggerGate {
    /// The book order's reservation is taken, and the slot's is released.
    Rest(ReservedTrigger),
    /// The account still holds the armed slot's reservation, which the cancel
    /// releases.
    Cancel(OrderActionExplanation),
}

/// Moves the fired order's reservation from its slot to the book, then gates
/// exactly like `trigger_market_order_v1`.
///
/// A reduce-only order rests at most the position it can reduce, so the margin
/// exemption it gets is true of its reservation as well as its fills. With no
/// position left to reduce, it is cancelled. A risk-increasing, non-reduce-only
/// order is cancelled on an account that fails initial margin, the buffered
/// equity floor, or the authority equity breaker.
fn gate_trigger(
    user: &mut User,
    user_stats: &UserStats,
    order_index: usize,
    market_index: u16,
    oracle_price: i64,
    maps: &mut AccountMaps<'_>,
) -> Result<TriggerGate> {
    let reduce_only = user.orders[order_index].reduce_only;
    let direction = user.orders[order_index].direction;
    let position_base = user
        .get_perp_position(market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0);
    let base_asset_amount =
        user.orders[order_index].get_base_asset_amount_unfilled(Some(position_base))?;
    if base_asset_amount == 0 {
        return Ok(TriggerGate::Cancel(
            OrderActionExplanation::ReduceOnlyOrderIncreasedPosition,
        ));
    }

    let armed = OrderReservation::of_order(&user.orders[order_index])?;
    let placed =
        OrderReservation::book_order(market_index, direction, base_asset_amount, reduce_only);
    let (_, worst_case_before) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    user.replace_reservation(&armed, &placed)?;

    let (_, worst_case_after) = user
        .get_perp_position(market_index)?
        .worst_case_liability_value(oracle_price)?;
    if worst_case_after > worst_case_before
        && !reduce_only
        && !account_carries_risk_increase(user, user_stats, maps)?
    {
        user.replace_reservation(&placed, &armed)?;
        return Ok(TriggerGate::Cancel(
            OrderActionExplanation::InsufficientFreeCollateral,
        ));
    }

    Ok(TriggerGate::Rest(ReservedTrigger {
        direction,
        base_asset_amount,
        reduce_only,
    }))
}

/// Whether the account may take on more risk: it meets initial margin, clears
/// its buffered equity floor, and its authority's equity breaker is not set.
///
/// A floor that cannot be verified fails the crank rather than cancelling the
/// trigger. A cancel is irreversible, so a brief oracle fault must not destroy
/// a resting order. The keeper retries once the feed recovers.
fn account_carries_risk_increase(
    user: &User,
    user_stats: &UserStats,
    maps: &mut AccountMaps<'_>,
) -> Result<bool> {
    let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
        user,
        maps,
        MarginContext::standard(MarginRequirementType::Initial),
    )?;

    let net_equity = calculate_net_equity_for_floor(user, maps)?;
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

    Ok(margin_calc.meets_margin_requirement()
        && net_equity.is_none_or(|net_equity| net_equity.clears_buffered_floor(user))
        && !user_stats.is_equity_breaker_tripped())
}

/// The armed slot as the trigger record states it: fired through its
/// condition, as a fired stop-market reads. The slot itself stays
/// untriggered.
fn fired_view(armed: &Order) -> Order {
    let mut fired = *armed;
    fired.trigger_condition = match armed.trigger_condition {
        OrderTriggerCondition::Above => OrderTriggerCondition::TriggeredAbove,
        OrderTriggerCondition::Below => OrderTriggerCondition::TriggeredBelow,
        other => other,
    };

    fired
}

/// Pays the crank its flat reward out of the user, and reports the reward.
///
/// A user that cranks its own trigger pays nothing. The account is already
/// borrowed here, and a reward it paid itself would move no value.
#[allow(clippy::too_many_arguments)]
fn pay_trigger_keeper(
    user: &mut User,
    filler: &AccountLoader<'_, User>,
    perp_market_map: &PerpMarketMap<'_>,
    market_index: u16,
    flat_filler_fee: u64,
    user_key: &Pubkey,
    filler_key: &Pubkey,
    slot: u64,
) -> Result<u64> {
    let is_filler_user = user_key == filler_key;
    let mut filler = if !is_filler_user {
        Some(load_mut!(filler)?)
    } else {
        None
    };
    let mut perp_market = perp_market_map.get_ref_mut(&market_index)?;
    Ok(pay_keeper_flat_reward_for_perps(
        user,
        filler.as_deref_mut(),
        &mut perp_market,
        flat_filler_fee,
        slot,
    )?)
}

/// Point the relay watch of a re-armed order back at its trigger side.
///
/// The slot watched the non-trigger side while the order waited for its
/// recross. Without this, relay never wakes the crank that fires the order.
fn rewatch_rearmed_trigger(
    user_loader: &AccountLoader<'_, User>,
    trigger_conditions: &Option<AccountLoader<'_, crate::state::user_conditions::UserConditionsV0>>,
    maps: &AccountMaps<'_>,
    market_index: u16,
    order_id: u32,
) -> Result<()> {
    let Some(conditions) = trigger_conditions else {
        return Ok(());
    };

    let user = crate::load!(user_loader)?;
    let order_index = find_armed_trigger_limit(&user, order_id, market_index)?;
    let market = maps.perp_market_map.get_ref(&market_index)?;
    let oracle = maps.oracle_map.get_account_info(&market.oracle)?;
    let mut conditions = load_mut!(conditions)?;
    validate!(
        conditions.user == user_loader.key(),
        ErrorCode::InvalidUserAccount,
        "trigger conditions are for user {}, crank is for {}",
        conditions.user,
        user_loader.key()
    )?;

    crate::instructions::trigger_relay::sync_trigger_conditions::rewatch_trigger_slot(
        &mut conditions,
        &user.orders[order_index],
        &market,
        &oracle,
    )
}

/// Marks the armed slot as the shadow of the order that now rests on the book.
///
/// The slot keeps the trigger parameters and takes the CLOB handle. It stays
/// untriggered, so every discovery path ignores it.
fn mark_slot_placed(
    user_loader: &AccountLoader<'_, User>,
    order_id: u32,
    order_ref: &ClobOrderRefV0,
    slot: u64,
) -> Result<()> {
    let mut user = load_mut!(user_loader)?;
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
    Ok(())
}

/// The relay resolver for `trigger_limit_order_v1`. It is simulation-only and
/// is staged from the user's synced trigger conditions.
#[derive(Accounts)]
pub struct ResolveTriggerLimitOrderV1<'info> {
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

pub fn handle_resolve_trigger_limit_order_v1(
    ctx: Context<ResolveTriggerLimitOrderV1>,
    fired: super::FiredConditionArgV0,
) -> Result<()> {
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        let fired = {
            let conditions = ctx.accounts.trigger_conditions.load()?;
            let user = crate::load!(ctx.accounts.user)?;
            let market = ctx.accounts.perp_market.load()?;
            super::helpers::crank_common::find_fired_trigger(
                super::helpers::crank_common::fired_trigger_slot(
                    &conditions,
                    &ctx.accounts.trigger_conditions.key(),
                    &fired,
                )?,
                &user,
                &market,
                &ctx.accounts.oracle,
                &clock,
                super::helpers::crank_common::TriggerResolverKind::ClobRest,
            )?
        };
        let Some(meta) = fired else {
            return Ok(None);
        };

        let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
        let user_stats =
            crate::state::pdas::user_stats(&crate::load!(ctx.accounts.user)?.authority);
        Ok(Some(
            crate::staged_call!(TriggerLimitOrderV1 {
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
            })
            .refs(ctx.accounts.trigger_conditions.load()?.read_sync_accounts())
            .arg(TriggerLimitOrderV1Args {
                market_index: meta.market_index,
                order_id: meta.order_id,
            })?,
        ))
    })
}

#[cfg(test)]
mod crank_tests {
    use {
        super::{
            cancel_refused_trigger, gate_trigger, rest_admission, RestAdmission, RestRefusal,
            TriggerCancel, TriggerFire, TriggerGate, TriggerLimitCrank, TriggerLimitStep,
        },
        crate::{
            controller::position::PositionDirection,
            create_anchor_account_info,
            instructions::optional_accounts::AccountMaps,
            math::{
                constants::{AMM_RESERVE_PRECISION, BASE_PRECISION_I64, PEG_PRECISION},
                time::SlotClock,
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                prop_amm::OrderRulesV0,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market_map::SpotMarketMap,
                state::State,
                user::{
                    MarketType, Order, OrderBitFlag, OrderReservation, OrderStatus,
                    OrderTriggerCondition, OrderType, PerpPosition, User, UserStats,
                },
            },
            test_utils::{get_positions, get_pyth_price},
        },
        anchor_lang::prelude::{AccountLoader, Clock, Pubkey},
        std::str::FromStr,
    };

    fn rules() -> OrderRulesV0 {
        OrderRulesV0 {
            min_order_size: BASE_PRECISION_I64 as u64,
            blocking_min_size: 0,
            default_activation_delay_slots: 0,
            max_activation_delay_slots: 10,
            place_authority: [0; 32],
            tick_size: 1,
            step_size: 1,
            side_order_counts: [0, 0],
            arena_capacity: 512,
            evict_threshold_per_side: 200,
            authority: [0; 32],
        }
    }

    /// A market at $100 whose maps carry what a cancel record reads.
    fn market_maps<'a>(
        oracle_info: &'a anchor_lang::prelude::AccountInfo<'a>,
        market_info: &'a anchor_lang::prelude::AccountInfo<'a>,
    ) -> AccountMaps<'a> {
        let oracle_map = OracleMap::load_one(oracle_info, 0, SlotClock::baseline(), None).unwrap();
        let market_map = PerpMarketMap::load_one(market_info, true).unwrap();
        AccountMaps::new(market_map, SpotMarketMap::empty(), oracle_map)
    }

    fn market(oracle_key: Pubkey, oracle_price: i64) -> PerpMarket {
        PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Active,
            oracle: oracle_key,
            oracle_source: OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: oracle_price,
                    last_oracle_price_twap: oracle_price,
                    last_oracle_price_twap_5min: oracle_price,
                    ..HistoricalOracleData::default()
                },
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        }
    }

    /// A reduce-only stop-loss for 1 base, armed on a long of 0.5.
    fn armed_on_half_a_long() -> (Order, User) {
        let armed = Order {
            order_id: 7,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerLimit,
            market_type: MarketType::Perp,
            direction: PositionDirection::Short,
            base_asset_amount: BASE_PRECISION_I64 as u64,
            price: 90_000_000,
            trigger_price: 95_000_000,
            trigger_condition: OrderTriggerCondition::Below,
            reduce_only: true,
            ..Order::default()
        };
        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64 / 2,
                ..PerpPosition::default()
            }),
            ..User::default()
        };

        user.orders[0] = armed;
        user.reserve_orders(&OrderReservation::of_order(&armed).unwrap())
            .unwrap();
        (armed, user)
    }

    /// The gate clamps the stop to 0.5, which is under the book's minimum
    /// of 1.
    #[test]
    fn a_trigger_the_book_refuses_is_cancelled_and_its_reservation_released() {
        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_key = Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
        let mut market = market(oracle_key, oracle_price.price);
        create_anchor_account_info!(market, PerpMarket, market_info);
        let mut maps = market_maps(&oracle_info, &market_info);
        let (armed, mut user) = armed_on_half_a_long();

        let TriggerGate::Rest(reserved) = gate_trigger(
            &mut user,
            &UserStats::default(),
            0,
            0,
            100_000_000,
            &mut maps,
        )
        .unwrap() else {
            panic!("a reduce-only trigger with a position to reduce rests");
        };

        assert_eq!(reserved.base_asset_amount, BASE_PRECISION_I64 as u64 / 2);

        let RestAdmission::Refused(reason) = rest_admission(
            &rules(),
            reserved.direction,
            armed.price,
            reserved.base_asset_amount,
            armed.max_ts,
            None,
            0,
        ) else {
            panic!("the book refuses a rest under its minimum");
        };

        assert_eq!(reason, RestRefusal::SizeBelowMinimum);

        cancel_refused_trigger(
            &mut user,
            0,
            &reserved,
            reason,
            &mut maps,
            &TriggerCancel {
                market_index: 0,
                user_key: &Pubkey::new_unique(),
                filler_key: &Pubkey::new_unique(),
                filler_reward: 0,
                clock: &Clock::default(),
            },
        )
        .unwrap();

        assert_eq!(user.orders[0].status, OrderStatus::Canceled);
        let position = user.get_perp_position(0).unwrap();
        assert_eq!(position.open_asks, 0);
        assert_eq!(position.open_orders, 0);
        assert_eq!(user.open_orders, 0);
    }

    const FLAT_FILLER_FEE: u64 = 10_000;

    /// An evicted stop-loss waits for the price to cross back. The crank that
    /// observes the recross clears the gate and earns the flat reward, so a
    /// keeper and relay have a reason to send it.
    #[test]
    fn the_crank_that_observes_the_recross_is_paid() {
        let mut oracle_price = get_pyth_price(100, 6);
        let oracle_key = Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
        create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
        let mut market = market(oracle_key, oracle_price.price);
        create_anchor_account_info!(market, PerpMarket, market_info);
        let mut maps = market_maps(&oracle_info, &market_info);

        let (_, mut user) = armed_on_half_a_long();
        user.orders[0].add_bit_flag(OrderBitFlag::AwaitingTriggerRecross);
        let (user_key, filler_key) = (Pubkey::new_unique(), Pubkey::new_unique());
        create_anchor_account_info!(user, &user_key, User, user_info);
        create_anchor_account_info!(User::default(), &filler_key, User, filler_info);
        create_anchor_account_info!(UserStats::default(), UserStats, user_stats_info);

        let mut state = State::default();
        state.perp_fee_structure.flat_filler_fee = FLAT_FILLER_FEE;
        let user_loader = AccountLoader::<User>::try_from(&user_info).unwrap();
        let filler_loader = AccountLoader::<User>::try_from(&filler_info).unwrap();
        let crank = TriggerLimitCrank {
            user: &user_loader,
            user_stats: &AccountLoader::try_from(&user_stats_info).unwrap(),
            filler: &filler_loader,
            state: &state,
            market_index: 0,
            order_id: 7,
            clock: &Clock::default(),
        };

        let fire = crank
            .fire(&mut user_loader.load_mut().unwrap(), &mut maps)
            .unwrap();
        assert!(matches!(
            fire,
            TriggerFire::Done(TriggerLimitStep::Rearmed {
                keeper_reward: FLAT_FILLER_FEE
            })
        ));

        let user = user_loader.load().unwrap();
        assert!(!user.orders[0].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross));
        assert_eq!(user.orders[0].status, OrderStatus::Open);
        assert_eq!(
            filler_loader.load().unwrap().perp_positions[0].quote_asset_amount,
            FLAT_FILLER_FEE as i64
        );
    }
}
