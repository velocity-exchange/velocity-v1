//! The per-market fill passes after the auction pass: standalone triggers, limit uncrosses and
//! resting-order fills against the AMM
//!
//! Each pass plans its txs from the slot's `MarketView` and the DLOB, recording a decision event
//! for each candidate it drops, then sends the planned txs.

use {
    crate::{
        common::{
            grpc::fetch_user_and_stats,
            tx::{
                build_fill_tx, scale_cu_limit_for_accounts, with_spot_interest_cranks,
                OrderSlotLimiter, TxIntent,
            },
        },
        filler::{
            market::{amm_fill_size, AmmGate, MarketView},
            order_dedup_key, Filler, SlotTick, TARGET,
        },
    },
    std::collections::HashSet,
    velocity_rs::{
        dlob::{CrossingRegion, L3Order, OrderKind},
        types::accounts::{User, UserStats},
        Pubkey,
    },
};

/// Trigger the orders whose condition is met but that do not cross any liquidity yet.
///
/// `find_crosses_for_auctions` only surfaces trigger orders whose post-trigger price crosses
/// right away, so without this pass a stop or take-profit limit that rests after triggering
/// would never be triggered. Orders in `crossing_triggers` were already handled by the auction
/// pass, which triggers and fills them in one tx. The send path simulates first, so an order
/// that is not triggerable onchain is dropped there.
pub(super) async fn trigger_resting_orders(
    filler: &Filler,
    tick: &SlotTick,
    view: &MarketView,
    triggerable: &[(Pubkey, u32)],
    crossing_triggers: &HashSet<(Pubkey, u32)>,
    limiter: &mut OrderSlotLimiter<40>,
) {
    if !triggerable.is_empty() {
        log::info!(target: TARGET, "found {} triggerable order(s) (market: {})", triggerable.len(), view.market_index);
    }

    // The limiter keys on the full (user, order_id) identity, the same key the auction pass
    // uses, so a trigger+fill and a standalone trigger of one order share a rate-limit window.
    let planned: Vec<(Pubkey, u32)> = triggerable
        .iter()
        .copied()
        .filter(|order| !crossing_triggers.contains(order))
        .filter(|(user, order_id)| limiter.allow_event(tick.slot, order_dedup_key(user, *order_id)))
        .collect();

    for (taker, order_id) in planned {
        filler
            .send_trigger(
                tick.priority_fee,
                view.market_index,
                taker,
                order_id,
                tick.landing_slot,
                // a standalone trigger is decided on the chain exchange price and posts nothing
                None,
            )
            .await;
    }
}

/// One side of an uncross: the top resting order on one side takes against the crossing
/// resting orders on the other.
struct UncrossLeg<'a> {
    taker_order: &'a L3Order,
    maker_orders: Vec<&'a L3Order>,
    taker: User,
    taker_stats: UserStats,
    makers: Vec<User>,
}

/// Uncross the top of the book: each best order takes against the other side's crossing orders.
pub(super) async fn uncross_limits(
    filler: &Filler,
    tick: &SlotTick,
    view: &MarketView,
    crosses: CrossingRegion,
) {
    let market_index = view.market_index;
    log::info!(target: TARGET, "try uncross book={market_index},slot={}", tick.landing_slot);
    log::debug!(
        target: TARGET,
        "X asks: {:?}, X bids: {:?}",
        crosses.crossing_asks.iter().take(3),
        crosses.crossing_bids.iter().take(3),
    );

    let filler_account = filler.account();
    for leg in plan_uncross(filler, tick, view, &crosses) {
        let tx_builder =
            filler.tx_builder(&filler_account, tick.priority_fee, filler.fill_cu_limit);
        let tx_builder =
            with_spot_interest_cranks(tx_builder, filler.keeper.velocity, &leg.taker, &leg.makers)
                .fill_perp_order(
                    market_index,
                    leg.taker_order.user,
                    &leg.taker,
                    &leg.taker_stats,
                    Some(leg.taker_order.order_id),
                    leg.makers.as_slice(),
                    None,
                );
        let (tx_builder, cu_limit) =
            scale_cu_limit_for_accounts(tx_builder, filler.fill_cu_limit, 40, 25);
        let (tx, simulation_tx) = build_fill_tx(tx_builder, false);

        UncrossAttempt {
            market_index,
            slot: tick.landing_slot,
            action: "sent",
            taker: leg.taker_order,
            maker_candidates: &leg.maker_orders,
            n_maker_accounts: leg.makers.len(),
        }
        .emit();
        filler
            .keeper
            .tx
            .send_fill_tx(
                tx,
                simulation_tx,
                TxIntent::LimitUncross {
                    slot: tick.landing_slot,
                    market_index,
                    taker_order_id: leg.taker_order.order_id,
                    taker_user: leg.taker_order.user,
                    maker_order_id: leg.maker_orders.first().map_or(0, |maker| maker.order_id),
                },
                cu_limit as u64,
            )
            .await;
    }
}

/// The uncross legs worth sending: a taker that may take, with makers that can act as makers.
fn plan_uncross<'a>(
    filler: &Filler,
    tick: &SlotTick,
    view: &MarketView,
    crosses: &'a CrossingRegion,
) -> Vec<UncrossLeg<'a>> {
    let (Some(best_bid), Some(best_ask)) =
        (crosses.crossing_bids.first(), crosses.crossing_asks.first())
    else {
        return Vec::new();
    };

    // the program picks the maker orders to match; these decide the accounts attached
    let counterparties = |orders: &'a [L3Order], taker: &L3Order| -> Vec<&'a L3Order> {
        orders
            .iter()
            .take(3)
            .filter(|order| order.user != taker.user)
            .collect()
    };

    let mut legs = Vec::with_capacity(2);
    for (taker_order, maker_orders) in [
        (best_ask, counterparties(&crosses.crossing_bids, best_ask)),
        (best_bid, counterparties(&crosses.crossing_asks, best_bid)),
    ] {
        let makers: Vec<User> = maker_orders
            .iter()
            .filter(|order| is_maker_eligible(order))
            .filter_map(|order| filler.keeper.cached_user(&order.user))
            .collect();

        let skip = if taker_order.is_post_only() {
            Some("skip_taker_post_only")
        } else if makers.is_empty() {
            // distinguish "nothing on the other side" from "counterparties exist but none can
            // act as a maker onchain" (per-maker `eligible` in the event)
            log::debug!(target: TARGET, "no eligible makers to uncross (market={})", view.market_index);
            Some(if maker_orders.is_empty() {
                "skip_no_makers"
            } else {
                "skip_no_eligible_makers"
            })
        } else {
            None
        };
        if let Some(action) = skip {
            UncrossAttempt {
                market_index: view.market_index,
                slot: tick.landing_slot,
                action,
                taker: taker_order,
                maker_candidates: &maker_orders,
                n_maker_accounts: makers.len(),
            }
            .emit();
            continue;
        }

        let Some((taker, taker_stats)) =
            fetch_user_and_stats(filler.keeper.velocity, &taker_order.user, "uncross")
        else {
            continue;
        };
        legs.push(UncrossLeg {
            taker_order,
            maker_orders,
            taker,
            taker_stats,
            makers,
        });
    }
    legs
}

/// Only a resting limit order can act as a maker onchain (`is_maker_for_taker` requires
/// `Order::is_resting_limit_order`): DLOB kinds Limit and FloatingLimit. Market, oracle and
/// untriggered trigger orders never match as makers, so attaching them lands a no-op fill.
fn is_maker_eligible(order: &L3Order) -> bool {
    matches!(order.kind, OrderKind::Limit | OrderKind::FloatingLimit)
}

/// One resting order the AMM's quote crosses, planned for a fill.
struct AmmTakerFill {
    order: L3Order,
    user: User,
    user_stats: UserStats,
    makers: Vec<User>,
}

/// Fill the resting limit orders that the AMM quote crosses (`find_crosses_for_auctions`'
/// `vamm_taker_bid`/`vamm_taker_ask`).
///
/// The fill posts nothing, so every check reads the chain view. The program sources the fill
/// from the AMM and dispatches on `order.post_only` (`math/fulfillment.rs`): a non-post-only
/// order takes against the AMM quote, and a post-only order is crossed by the AMM at its own
/// price. Both need `amm_is_available`, so both go through the program's AMM gate, and the size
/// is the program's own sizing capped at the order's price. Expired crossing orders are sent
/// too: the program turns the fill into an expiry cancel, which clears them off the book.
///
/// `top_makers` are (asks, bids). Each fill attaches the opposite side, so the program can
/// route to a better-priced user maker than the AMM.
pub(super) async fn fill_amm_takers(
    filler: &Filler,
    tick: &SlotTick,
    view: &MarketView,
    candidates: [Option<L3Order>; 2],
    top_makers: (Vec<Pubkey>, Vec<Pubkey>),
    limiter: &mut OrderSlotLimiter<40>,
) {
    let planned = plan_amm_takers(filler, tick, view, candidates, &top_makers, limiter);
    if planned.is_empty() {
        return;
    }

    let market_index = view.market_index;
    let filler_account = filler.account();
    for fill in planned {
        let tx_builder =
            filler.tx_builder(&filler_account, tick.priority_fee, filler.fill_cu_limit);
        let tx_builder =
            with_spot_interest_cranks(tx_builder, filler.keeper.velocity, &fill.user, &fill.makers)
                .fill_perp_order(
                    market_index,
                    fill.order.user,
                    &fill.user,
                    &fill.user_stats,
                    Some(fill.order.order_id),
                    fill.makers.as_slice(),
                    None,
                );
        let (tx_builder, cu_limit) =
            scale_cu_limit_for_accounts(tx_builder, filler.fill_cu_limit, 20, 20);
        let (tx, simulation_tx) = build_fill_tx(tx_builder, false);

        filler
            .keeper
            .tx
            .send_fill_tx(
                tx,
                simulation_tx,
                TxIntent::AmmTakerFill {
                    slot: tick.slot,
                    market_index,
                    maker_order_id: fill.order.order_id,
                    taker_user: fill.order.user,
                },
                cu_limit as u64,
            )
            .await;
    }
}

fn plan_amm_takers(
    filler: &Filler,
    tick: &SlotTick,
    view: &MarketView,
    candidates: [Option<L3Order>; 2],
    top_makers: &(Vec<Pubkey>, Vec<Pubkey>),
    limiter: &mut OrderSlotLimiter<40>,
) -> Vec<AmmTakerFill> {
    let velocity = filler.keeper.velocity;
    let mut planned = Vec::with_capacity(2);

    for l3_order in candidates.into_iter().flatten() {
        let Some((user, user_stats)) =
            fetch_user_and_stats(velocity, &l3_order.user, "amm-taker fill")
        else {
            continue;
        };
        let Some(order) = user
            .orders
            .iter()
            .find(|order| order.order_id == l3_order.order_id)
            .copied()
        else {
            continue;
        };

        let decision = AmmTakerDecision {
            market_index: view.market_index,
            user: &l3_order.user,
            order_id: l3_order.order_id,
            slot: tick.slot,
            post_only: order.post_only,
            order_slot: order.slot,
            oracle_stale_for_amm: view.oracle_stale_for_amm,
            oracle_delay: view.chain.oracle.safe.delay,
            limit_price: l3_order.price,
            fillable: None,
            size_threshold: Some(view.market.order_step_size),
        };

        let can_skip_auction = user
            .can_skip_auction_duration(&user_stats, order.reduce_only)
            .unwrap_or(false);
        // the fill posts nothing, so it reads the chain view; the gate reads the loaded market
        let gate = AmmGate::evaluate(
            &view.market,
            &tick.exchange,
            Some(&view.chain.oracle),
            Some(&order),
            can_skip_auction,
            tick.landing_slot,
        );
        if !gate.open {
            decision.emit("skip_amm_gated");
            continue;
        }

        let existing_base = user
            .get_perp_position(view.market_index)
            .map_or(0, |position| position.base_asset_amount);
        let Some(fillable) = amm_fill_size(
            &view.chain.quote_market,
            &order,
            existing_base,
            Some(l3_order.price),
        ) else {
            continue;
        };
        let decision = AmmTakerDecision {
            fillable: Some(fillable),
            ..decision
        };
        if fillable == 0 {
            decision.emit("skip_too_small");
            continue;
        }

        // rate-limited re-attempts are not wide-logged (see `AmmTakerDecision`)
        if !limiter.allow_event(
            tick.slot,
            order_dedup_key(&l3_order.user, l3_order.order_id),
        ) {
            continue;
        }

        log::info!(
            target: TARGET,
            "try amm-taker fill: market={} user={} order={} limit={} fillable={fillable} stale_for_amm={}",
            view.market_index,
            l3_order.user,
            l3_order.order_id,
            l3_order.price,
            view.oracle_stale_for_amm,
        );
        decision.emit("sent");

        let (top_maker_asks, top_maker_bids) = top_makers;
        let makers: Vec<User> = if l3_order.is_long() {
            top_maker_asks
        } else {
            top_maker_bids
        }
        .iter()
        .filter(|maker| **maker != l3_order.user) // can't fill itself
        .filter_map(|maker| filler.keeper.cached_user(maker))
        .collect();

        planned.push(AmmTakerFill {
            order: l3_order,
            user,
            user_stats,
            makers,
        });
    }
    planned
}

/// A wide structured event (one JSON line, log target `tx_event`) for each resting-order-vs-AMM
/// fill candidate that reaches a terminal decision: "sent", "skip_amm_gated" or
/// "skip_too_small".
///
/// Rate-limited re-attempts are deliberately NOT emitted (one per slot for the whole limiter
/// window would drown the signal); the send attempt they throttle already produced a "sent"
/// decision plus a terminal `event: "tx"` (intent `amm_taker`). Correlate on
/// (market, order_id).
#[derive(Clone, Copy)]
struct AmmTakerDecision<'a> {
    market_index: u16,
    user: &'a Pubkey,
    order_id: u32,
    slot: u64,
    post_only: bool,
    order_slot: u64,
    oracle_stale_for_amm: bool,
    oracle_delay: i64,
    limit_price: u64,
    fillable: Option<u64>,
    size_threshold: Option<u64>,
}

impl AmmTakerDecision<'_> {
    fn emit(&self, action: &str) {
        let event = serde_json::json!({
            "event": "amm_taker_decision",
            "market": self.market_index,
            "user": self.user.to_string(),
            "order_id": self.order_id,
            "slot": self.slot,
            "action": action,
            "post_only": self.post_only,
            "order_slot": self.order_slot,
            "oracle_stale_for_amm": self.oracle_stale_for_amm,
            "oracle_delay": self.oracle_delay,
            "limit_price": self.limit_price,
            "fillable": self.fillable,
            "size_threshold": self.size_threshold,
        });
        log::info!(target: "tx_event", "{event}");
    }
}

/// A wide structured event (one JSON line, log target `tx_event`) for each uncross leg that
/// reaches a decision: "sent", "skip_taker_post_only", "skip_no_makers" or
/// "skip_no_eligible_makers".
///
/// Carries the taker order and the crossing counterparty orders considered as makers, with
/// post-only and kind decoded, so a landed-but-`no_fills` `limit_uncross` tx (correlate on
/// (market, taker_order_id, slot=sent_slot)) can be analyzed without replaying the book.
/// `n_maker_accounts` is how many maker user accounts were attached to the tx (candidates
/// missing from the account cache are dropped).
struct UncrossAttempt<'a> {
    market_index: u16,
    slot: u64,
    action: &'a str,
    taker: &'a L3Order,
    maker_candidates: &'a [&'a L3Order],
    n_maker_accounts: usize,
}

impl UncrossAttempt<'_> {
    fn emit(&self) {
        let event = serde_json::json!({
            "event": "uncross_attempt",
            "market": self.market_index,
            "slot": self.slot,
            "action": self.action,
            "taker": self.taker.user.to_string(),
            "taker_order_id": self.taker.order_id,
            "taker_kind": format!("{:?}", self.taker.kind),
            "taker_price": self.taker.price,
            "taker_size": self.taker.size,
            "taker_post_only": self.taker.is_post_only(),
            "taker_is_long": self.taker.is_long(),
            "makers": self.maker_candidates
                .iter()
                .map(|maker| {
                    serde_json::json!({
                        "user": maker.user.to_string(),
                        "order_id": maker.order_id,
                        "kind": format!("{:?}", maker.kind),
                        "price": maker.price,
                        "size": maker.size,
                        "post_only": maker.is_post_only(),
                        "eligible": is_maker_eligible(maker),
                    })
                })
                .collect::<Vec<_>>(),
            "n_maker_accounts": self.n_maker_accounts,
        });
        log::info!(target: "tx_event", "{event}");
    }
}
