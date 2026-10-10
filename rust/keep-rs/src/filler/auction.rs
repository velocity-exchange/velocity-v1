//! Auction fills: each order whose auction crosses resting makers or the AMM gets its own fill
//! tx, which posts the pyth-lazer price and, for a trigger order, triggers it first
//!
//! Each cross is planned first: the taker is loaded, a trigger taker is modelled as the trigger
//! instruction leaves it, the program's AMM gate and sizing decide whether the AMM leg is usable,
//! and a `cross_decision` event records the route. Executing the plan sends the fill, or for a
//! skipped trigger cross the bare trigger, since the run loop's trigger pass leaves crossing
//! trigger orders to this path.

use {
    crate::{
        common::{
            grpc::fetch_user_and_stats,
            tx::{build_fill_tx, scale_cu_limit_for_accounts, with_spot_interest_cranks, TxIntent},
        },
        filler::{
            market::{amm_fill_size, AmmGate, MarketView},
            Filler, SlotTick, TARGET,
        },
    },
    velocity_rs::{
        dlob::{CrossesAndTopMakers, L3Order, MakerCrosses, OrderKind},
        program::{
            math::time::{Millis, SlotClock},
            state::user::OrderBitFlag,
        },
        types::{
            accounts::{User, UserStats},
            MarketType, Order, OrderTriggerCondition, PositionDirection,
        },
        Pubkey, TransactionBuilder, VelocityClient,
    },
};

/// `update_trigger_order_params` flags a reduce-only trigger that rested longer than this as safe.
const SAFE_TRIGGER_ORDER_MIN_REST: Millis = Millis::from_secs(60);

/// Fill each auction cross of one market with its own tx.
///
/// A fill tx carries the pyth-lazer post whenever the post changes what the fill reads, and
/// then reads the oracle as projected with it. The immediate AMM leg needs an exchange oracle
/// written in the same slot, and each tx simulates and lands on its own, so one tx's post does
/// not cover another. The post is left out only when the fill would read identical oracle
/// inputs without it, which happens when the MM oracle stays the safe price and the cached
/// exchange price already matches the update. A trigger taker always posts, because its
/// trigger instruction reads the exchange oracle.
pub(super) async fn fill_auction_crosses(
    filler: &Filler,
    tick: &SlotTick,
    view: &MarketView,
    auction_crosses: CrossesAndTopMakers,
) {
    let pass = AuctionPass::new(filler, tick, view, &auction_crosses);
    for (taker_order, crosses) in auction_crosses.crosses {
        log::info!(target: TARGET, "try fill auction order: {taker_order:?}");
        if let Some(fill) = pass.plan(taker_order, crosses) {
            pass.execute(fill).await;
        }
    }
}

/// What every cross of one market's auction pass reads.
struct AuctionPass<'a> {
    filler: &'a Filler,
    tick: &'a SlotTick,
    view: &'a MarketView,
    filler_account: User,
    top_maker_asks: Vec<User>,
    top_maker_bids: Vec<User>,
    /// A fill reads the same oracle inputs with or without the post.
    post_redundant: bool,
}

/// One planned auction fill.
struct CrossFill {
    taker_order: L3Order,
    taker: User,
    taker_stats: UserStats,
    crosses: MakerCrosses,
    makers: Vec<User>,
    route: CrossRoute,
    is_trigger: bool,
    posts_oracle: bool,
    gate: AmmGate,
}

impl<'a> AuctionPass<'a> {
    fn new(
        filler: &'a Filler,
        tick: &'a SlotTick,
        view: &'a MarketView,
        auction_crosses: &CrossesAndTopMakers,
    ) -> Self {
        // a top maker missing from the cache only shortens the fallback maker list
        let cached_users = |makers: &[Pubkey]| -> Vec<User> {
            makers
                .iter()
                .filter_map(|maker| filler.keeper.cached_user(maker))
                .collect()
        };

        let post_redundant =
            view.pyth_update.is_some() && view.posted.oracle.same_fill_inputs(&view.chain.oracle);

        Self {
            filler,
            tick,
            view,
            filler_account: filler.account(),
            top_maker_asks: cached_users(auction_crosses.top_maker_asks.as_slice()),
            top_maker_bids: cached_users(auction_crosses.top_maker_bids.as_slice()),
            post_redundant,
        }
    }

    /// Decide how to fill one cross and record the decision. `None` when there is nothing to do:
    /// the taker is gone from the cache, or a trigger taker's condition is not met.
    fn plan(&self, taker_order: L3Order, crosses: MakerCrosses) -> Option<CrossFill> {
        let velocity = self.filler.keeper.velocity;
        let (taker, taker_stats) =
            fetch_user_and_stats(velocity, &taker_order.user, "auction fill")?;

        // The order may be gone since the DLOB snapshot. A trigger taker is then skipped. Any
        // other taker can still fill against makers, but the AMM leg cannot be validated.
        let order = taker
            .orders
            .iter()
            .find(|order| order.order_id == taker_order.order_id)
            .copied();

        let is_trigger = matches!(
            taker_order.kind,
            OrderKind::TriggerMarket | OrderKind::TriggerLimit
        );
        if is_trigger && !self.trigger_condition_met(&taker_order, order.as_ref()) {
            return None;
        }

        // The trigger ix runs before the fill in the same tx, so the program gates and sizes
        // the triggered order.
        let order = order.map(|order| {
            if is_trigger {
                order_after_trigger(order, self.tick.landing_slot, self.tick.exchange.slot_clock)
            } else {
                order
            }
        });

        let posts_oracle = self.view.pyth_update.is_some() && (is_trigger || !self.post_redundant);
        let view = if posts_oracle {
            &self.view.posted
        } else {
            &self.view.chain
        };

        let makers = crossing_makers(velocity, &taker_order.user, &crosses);
        let can_skip_auction = order.is_some_and(|order| {
            taker
                .can_skip_auction_duration(&taker_stats, order.reduce_only)
                .unwrap_or(false)
        });
        // the gate reads the loaded market, the size the quote projection
        let gate = AmmGate::evaluate(
            &self.view.market,
            &self.tick.exchange,
            Some(&view.oracle),
            order.as_ref(),
            can_skip_auction,
            self.tick.landing_slot,
        );
        let fill_size = order
            .filter(|_| crosses.has_vamm_cross && gate.open)
            .and_then(|order| {
                let existing_base = taker
                    .get_perp_position(self.view.market_index)
                    .map_or(0, |position| position.base_asset_amount);
                amm_fill_size(&view.quote_market, &order, existing_base, None)
            });
        let route = route_cross(
            crosses.has_vamm_cross,
            amm_leg_usable(crosses.has_vamm_cross, gate.open, fill_size),
            !makers.is_empty(),
        );

        CrossDecision {
            market_index: self.view.market_index,
            taker_order: &taker_order,
            crosses: &crosses,
            route,
            gate: &gate,
            amm_fill_size: fill_size,
            n_makers: makers.len(),
            posts_oracle,
        }
        .emit();

        Some(CrossFill {
            taker_order,
            taker,
            taker_stats,
            crosses,
            makers,
            route,
            is_trigger,
            posts_oracle,
            gate,
        })
    }

    async fn execute(&self, fill: CrossFill) {
        match fill.route {
            CrossRoute::Skip => return self.skip(fill).await,
            CrossRoute::FillMakersOnly => {
                log::debug!(target: TARGET, "amm leg gated, filling against makers only: {:?}", fill.crosses);
            }
            CrossRoute::FillWithAmm => {}
        }

        let tx_builder = self.fill_tx(&fill);
        let (tx_builder, cu_limit) =
            scale_cu_limit_for_accounts(tx_builder, self.filler.fill_cu_limit, 20, 20);
        let (tx, simulation_tx) = build_fill_tx(tx_builder, fill.posts_oracle);
        self.filler
            .keeper
            .tx
            .send_fill_tx(
                tx,
                simulation_tx,
                TxIntent::AuctionFill {
                    market_index: self.view.market_index,
                    taker_order_id: fill.taker_order.order_id,
                    taker_user: fill.taker_order.user,
                    maker_crosses: fill.crosses,
                    has_trigger: fill.is_trigger,
                },
                cu_limit as u64,
            )
            .await;
    }

    /// No counterparty can fill the cross. A trigger taker still gets its trigger: the run
    /// loop's trigger pass leaves crossing trigger orders to this path.
    async fn skip(&self, fill: CrossFill) {
        let market_index = self.view.market_index;
        if fill.gate.safe_stale_for_amm && fill.crosses.has_vamm_cross {
            log::info!(target: TARGET, "skip AMM fill: oracle stale for AMM (market={market_index})");
        } else {
            log::debug!(target: TARGET, "skip cross (amm gated, no makers): {:?}", fill.crosses);
        }

        if !fill.is_trigger {
            return;
        }

        log::info!(
            target: TARGET,
            "cross skipped but taker trigger condition met; sending standalone trigger: market={market_index}, order={}/{}, slot={}",
            fill.taker_order.order_id,
            fill.taker_order.user,
            fill.crosses.slot,
        );
        self.filler
            .send_trigger(
                self.tick.priority_fee,
                market_index,
                fill.taker_order.user,
                fill.taker_order.order_id,
                fill.crosses.slot + 1,
                // the trigger condition was checked on the posted exchange price
                self.view.pyth_update.as_ref(),
            )
            .await;
    }

    /// The fill tx in program order: priority fee, lazer post, trigger, cranks, then the fill.
    fn fill_tx<'b>(&'b self, fill: &'b CrossFill) -> TransactionBuilder<'b> {
        let market_index = self.view.market_index;
        let mut tx_builder = self.filler.tx_builder(
            &self.filler_account,
            self.tick.priority_fee,
            self.filler.fill_cu_limit,
        );

        if let Some(update) = self.view.pyth_update.as_ref().filter(|_| fill.posts_oracle) {
            tx_builder =
                tx_builder.post_pyth_lazer_oracle_update(&[update.feed_id], &update.message);
        }

        if fill.is_trigger {
            tx_builder = tx_builder.trigger_order(
                fill.taker_order.user,
                &fill.taker,
                fill.taker_order.order_id,
                (market_index, MarketType::Perp),
            );
        }

        // pad a short maker list with the top makers on the opposite side
        let makers = if fill.makers.len() < 3 {
            match fill.crosses.taker_direction {
                PositionDirection::Long => self.top_maker_asks.as_slice(),
                PositionDirection::Short => self.top_maker_bids.as_slice(),
            }
        } else {
            fill.makers.as_slice()
        };

        with_spot_interest_cranks(tx_builder, self.filler.keeper.velocity, &fill.taker, makers)
            .fill_perp_order(
                market_index,
                fill.taker_order.user,
                &fill.taker,
                &fill.taker_stats,
                Some(fill.taker_order.order_id),
                makers,
                None,
            )
    }

    fn trigger_condition_met(&self, taker_order: &L3Order, order: Option<&Order>) -> bool {
        let Some(order) = order else {
            log::debug!(target: TARGET, "trigger order {} gone before fill, skipping", taker_order.order_id);
            return false;
        };

        // the trigger ix rides in the fill tx, so it reads the posted exchange oracle when the
        // tx posts one; a trigger taker always posts when an update applies
        let trigger_price = self.view.posted.trigger_price;
        let condition_met = match order.trigger_condition {
            OrderTriggerCondition::Above | OrderTriggerCondition::TriggeredAbove => {
                trigger_price > order.trigger_price
            }
            _ => trigger_price < order.trigger_price,
        };

        if condition_met {
            log::info!(
                target: TARGET,
                "attempting trigger and fill: trigger_price={trigger_price}, order_price={}, {:?}/{:?}",
                order.trigger_price,
                taker_order.order_id,
                taker_order.user
            );
        }

        condition_met
    }
}

/// The AMM leg is usable when the program would let the AMM fill and the fill is not empty. A
/// size the keeper cannot compute does not close it.
fn amm_leg_usable(has_vamm_cross: bool, gate_open: bool, fill_size: Option<u64>) -> bool {
    has_vamm_cross && gate_open && fill_size.is_none_or(|size| size > 0)
}

/// The cross's makers that are in the cache, without the taker.
fn crossing_makers(velocity: &VelocityClient, taker: &Pubkey, crosses: &MakerCrosses) -> Vec<User> {
    // a maker missing from the cache shrinks the cross and is not an error
    crosses
        .orders
        .iter()
        .filter(|(maker_order, _fill_size)| maker_order.user != *taker)
        .filter_map(|(maker_order, _fill_size)| {
            velocity.try_get_account::<User>(&maker_order.user).ok()
        })
        .collect()
}

/// The order as a trigger ix at `trigger_slot` leaves it, as `update_trigger_order_params`
/// does. A triggered order restarts its auction, so its age counts from the trigger.
fn order_after_trigger(order: Order, trigger_slot: u64, slot_clock: SlotClock) -> Order {
    let mut triggered = order;
    triggered.trigger_condition = match order.trigger_condition {
        OrderTriggerCondition::Above => OrderTriggerCondition::TriggeredAbove,
        OrderTriggerCondition::Below => OrderTriggerCondition::TriggeredBelow,
        _ => return order,
    };

    if order.reduce_only
        && slot_clock.elapsed(order.slot, trigger_slot) > SAFE_TRIGGER_ORDER_MIN_REST
    {
        triggered.add_bit_flag(OrderBitFlag::SafeTriggerOrder);
    }

    triggered.slot = trigger_slot;
    triggered
}

/// How to handle one auction cross given the AMM's usability and available DLOB makers.
///
/// A cross is NOT categorically one or the other: `MakerCrosses` sets `has_vamm_cross`
/// independently of the maker orders it collected, so both legs routinely coexist. A gated
/// AMM must therefore degrade the fill to makers-only, never drop it (the program happily
/// executes the Match steps with `amm_is_available = false`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CrossRoute {
    /// send the fill relying on the AMM (any makers ride along)
    FillWithAmm,
    /// AMM leg gated but DLOB makers can still fill
    FillMakersOnly,
    /// no fillable counterparty at all
    Skip,
}

impl CrossRoute {
    /// stable label for wide-event logging
    fn label(&self) -> &'static str {
        match self {
            CrossRoute::FillWithAmm => "fill_with_amm",
            CrossRoute::FillMakersOnly => "makers_only",
            CrossRoute::Skip => "skip",
        }
    }
}

fn route_cross(has_vamm_cross: bool, amm_usable: bool, has_makers: bool) -> CrossRoute {
    if has_vamm_cross && amm_usable {
        CrossRoute::FillWithAmm
    } else if has_makers {
        CrossRoute::FillMakersOnly
    } else {
        CrossRoute::Skip
    }
}

/// One auction cross's routing decision and each AMM gate input behind it. The tx event only
/// covers crosses that send, so this event explains the skipped and makers-only ones. It joins
/// the tx event on (market, order_id).
struct CrossDecision<'a> {
    market_index: u16,
    taker_order: &'a L3Order,
    crosses: &'a MakerCrosses,
    route: CrossRoute,
    gate: &'a AmmGate,
    amm_fill_size: Option<u64>,
    n_makers: usize,
    posts_oracle: bool,
}

impl CrossDecision<'_> {
    /// Writes one JSON line to the log target `tx_event`.
    fn emit(&self) {
        let gate = self.gate;
        let event = serde_json::json!({
            "event": "cross_decision",
            "market": self.market_index,
            "taker": self.taker_order.user.to_string(),
            "order_id": self.taker_order.order_id,
            "slot": self.crosses.slot,
            "action": self.route.label(),
            "has_amm_cross": self.crosses.has_vamm_cross,
            "amm_open": gate.open,
            "safe_stale_for_amm": gate.safe_stale_for_amm,
            "safe_oracle_delay": gate.safe_oracle_delay,
            "drawdown": gate.drawdown,
            "order_low_risk": gate.order_low_risk,
            "can_skip_auction": gate.can_skip_auction,
            "amm_wants_to_jit_make": gate.wants_jit,
            "safe_stale_immediate": gate.safe_stale_immediate,
            "amm_fill_size": self.amm_fill_size,
            "n_makers": self.n_makers,
            "posts_oracle": self.posts_oracle,
        });

        log::info!(target: "tx_event", "{event}");
    }
}

#[cfg(test)]
mod tests {
    use super::{amm_leg_usable, order_after_trigger, route_cross, CrossRoute};

    #[test]
    fn amm_gated_cross_degrades_to_makers_instead_of_skipping() {
        // Regression: a cross carrying both an AMM leg and DLOB maker orders used to be
        // skipped entirely when the AMM was gated (drawdown/staleness/timing), dropping
        // perfectly fillable maker matches. It must degrade to a makers-only fill.
        assert_eq!(route_cross(true, false, true), CrossRoute::FillMakersOnly);
        // AMM gated and no makers: nothing to fill against
        assert_eq!(route_cross(true, false, false), CrossRoute::Skip);
        // AMM usable: send the fill relying on it, with or without makers
        assert_eq!(route_cross(true, true, false), CrossRoute::FillWithAmm);
        assert_eq!(route_cross(true, true, true), CrossRoute::FillWithAmm);
        // no AMM cross at all: plain maker fill or skip
        assert_eq!(route_cross(false, false, true), CrossRoute::FillMakersOnly);
        assert_eq!(route_cross(false, false, false), CrossRoute::Skip);
    }

    #[test]
    fn amm_leg_needs_an_open_gate_and_a_nonzero_fill() {
        assert!(amm_leg_usable(true, true, Some(1)));
        // the program's sizing rounds to the step size, so 0 means the AMM fills nothing
        assert!(!amm_leg_usable(true, true, Some(0)));
        assert!(!amm_leg_usable(true, false, Some(1)));
        assert!(!amm_leg_usable(false, true, Some(1)));
        // a size the keeper cannot compute does not close the leg
        assert!(amm_leg_usable(true, true, None));
    }

    #[test]
    fn order_after_trigger_restarts_age_and_flags_a_rested_reduce_only() {
        use velocity_rs::{
            program::{math::time::SlotClock, state::user::OrderBitFlag},
            types::{Order, OrderTriggerCondition},
        };

        let clock = SlotClock::baseline();
        let placed = Order {
            slot: 1_000,
            trigger_condition: OrderTriggerCondition::Below,
            reduce_only: true,
            ..Order::default()
        };

        // 151 slots at 400ms is past the 60s rest
        let triggered = order_after_trigger(placed, 1_151, clock);
        assert_eq!(triggered.slot, 1_151);
        assert_eq!(
            triggered.trigger_condition,
            OrderTriggerCondition::TriggeredBelow
        );
        assert!(triggered.triggered());
        assert!(triggered.is_bit_flag_set(OrderBitFlag::SafeTriggerOrder));
        assert!(triggered
            .is_low_risk_for_amm(i64::MAX / 2, 1_151, false, true)
            .unwrap());

        let early = order_after_trigger(placed, 1_100, clock);
        assert_eq!(early.slot, 1_100);
        assert!(!early.is_bit_flag_set(OrderBitFlag::SafeTriggerOrder));
        // the trigger restamps the slot, so the old placement no longer counts as low risk
        assert!(!early.is_low_risk_for_amm(10, 1_100, false, true).unwrap());

        let not_reduce_only = order_after_trigger(
            Order {
                reduce_only: false,
                ..placed
            },
            1_151,
            clock,
        );

        assert!(!not_reduce_only.is_bit_flag_set(OrderBitFlag::SafeTriggerOrder));

        let already_triggered = Order {
            trigger_condition: OrderTriggerCondition::TriggeredBelow,
            ..placed
        };

        assert_eq!(
            order_after_trigger(already_triggered, 1_151, clock),
            already_triggered
        );
    }
}
