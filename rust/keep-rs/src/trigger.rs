//! The trigger backup, which fires crossed trigger orders that relay left armed.
//!
//! Relay fires a crossed trigger order in about one second. Each tick this mode scans the account
//! map for armed `TriggerMarket` and `TriggerLimit` orders that the market's trigger price makes
//! due. An order that stays due for `--trigger-grace-slots` is fired through
//! `trigger_market_order_v1` or `trigger_limit_order_v1`, with this keeper's `User` as the filler.
//! The grace window keeps the backup from racing relay for the keeper fee. The executor checks
//! every gate again, so the send path simulates first and sends only a fire that passes. Like the
//! relay resolvers, a fire stages no quoter tail. A stop-market then rests whole on the book, and
//! the taker-origin cross crank fills it.

use {
    crate::{
        filler::{TxSender, TxWorker},
        http::Metrics,
        util::TxIntent,
        Config,
    },
    anchor_lang::{InstructionData, ToAccountMetas},
    solana_instruction::Instruction,
    std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    velocity_rs::{
        build_accounts,
        constants::{
            derive_clob_crank_conditions, derive_spot_market_account, state_account, ProgramData,
            PROGRAM_ID,
        },
        market_book,
        priority_fee_subscriber::PriorityFeeSubscriber,
        program::{self, state::user::OrderBitFlag},
        types::{
            accounts::User, MarketId, MarketType, Order, OrderStatus, OrderTriggerCondition,
            OrderType, StateExt,
        },
        ClobFillAccounts, GrpcSubscribeOpts, Pubkey, TransactionBuilder, VelocityClient, Wallet,
    },
};

const TARGET: &str = "trigger";

const TICK: Duration = Duration::from_millis(400);

/// The longest wait between two fires of one order whose fires keep failing.
const MAX_RETRY_SLOTS: u64 = 1_200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OrderKey {
    pub user: Pubkey,
    pub order_id: u32,
}

/// An armed trigger order that the current trigger price makes due.
#[derive(Clone, Copy, Debug)]
struct DueOrder {
    key: OrderKey,
    market_index: u16,
    order_type: OrderType,
}

/// Whether `order` waits in `User.orders` for a keeper to fire it.
fn is_armed_trigger(order: &Order, now: i64) -> bool {
    let expired = order.max_ts != 0 && now > order.max_ts;
    order.status == OrderStatus::Open
        && order.market_type == MarketType::Perp
        && matches!(
            order.order_type,
            OrderType::TriggerMarket | OrderType::TriggerLimit
        )
        && !order.is_placed_on_clob()
        && !expired
}

/// Whether a fire of `order` can land at `trigger_price`. This mirrors the program's
/// `trigger_crank_is_due`. An order re-armed after an eviction is due while its condition is not
/// met, because the crank that clears its recross flag observes the price back across.
pub fn trigger_is_due(order: &Order, trigger_price: u64) -> bool {
    let satisfied = match order.trigger_condition {
        OrderTriggerCondition::Above => trigger_price > order.trigger_price,
        OrderTriggerCondition::Below => trigger_price < order.trigger_price,
        OrderTriggerCondition::TriggeredAbove | OrderTriggerCondition::TriggeredBelow => {
            return false;
        }
    };

    satisfied != order.is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross)
}

#[derive(Clone, Copy, Debug)]
struct Retry {
    next_slot: u64,
    attempts: u32,
}

/// When each due order first became due, and when a failed fire may run again.
pub struct GraceWindow {
    grace_slots: u64,
    first_due_slot: HashMap<OrderKey, u64>,
    retries: HashMap<OrderKey, Retry>,
}

impl GraceWindow {
    pub fn new(grace_slots: u64) -> Self {
        Self {
            grace_slots,
            first_due_slot: HashMap::new(),
            retries: HashMap::new(),
        }
    }

    /// The orders in `due` that may fire at `slot`. An order missing from `due` is forgotten, so
    /// it starts a new grace window when it becomes due again.
    pub fn ready(&mut self, due: &[OrderKey], slot: u64) -> Vec<OrderKey> {
        let due_set: HashSet<OrderKey> = due.iter().copied().collect();
        self.first_due_slot.retain(|key, _| due_set.contains(key));
        self.retries.retain(|key, _| due_set.contains(key));

        due.iter()
            .copied()
            .filter(|key| {
                let first_due = *self.first_due_slot.entry(*key).or_insert(slot);
                let retry_open = self
                    .retries
                    .get(key)
                    .is_none_or(|retry| slot >= retry.next_slot);

                slot.saturating_sub(first_due) >= self.grace_slots && retry_open
            })
            .collect()
    }

    /// The wait before the next fire starts at `grace_slots` and doubles up to `MAX_RETRY_SLOTS`.
    /// A fire that the executor refuses keeps failing until the market moves.
    pub fn record_fire(&mut self, key: OrderKey, slot: u64) {
        let attempts = self.retries.get(&key).map_or(0, |retry| retry.attempts);
        let wait = self
            .grace_slots
            .max(1)
            .saturating_mul(1u64 << attempts.min(16))
            .min(MAX_RETRY_SLOTS);

        self.retries.insert(
            key,
            Retry {
                next_slot: slot.saturating_add(wait),
                attempts: attempts.saturating_add(1),
            },
        );
    }
}

/// The keeper that signs a fire and collects its reward.
#[derive(Clone, Copy, Debug)]
pub struct Keeper {
    pub authority: Pubkey,
    pub filler: Pubkey,
}

/// One trigger order to fire, and the accounts its executor reads beyond the keeper's.
pub struct TriggerFire<'a> {
    pub user: Pubkey,
    pub owner: &'a User,
    pub order_id: u32,
    pub market_index: u16,
    pub order_type: OrderType,
    pub book: ClobFillAccounts,
    pub sol_spot_market: Option<Pubkey>,
}

/// The markets of the owner's margin map. The executor writes them, as it does on the list that
/// `sync_trigger_conditions` stores for relay.
fn margin_map_markets(owner: &User, market_index: u16) -> Vec<MarketId> {
    let spot = owner
        .spot_positions
        .iter()
        .filter(|position| !position.is_available())
        .map(|position| MarketId::spot(position.market_index));
    let perp = owner
        .perp_positions
        .iter()
        .filter(|position| !position.is_available())
        .map(|position| MarketId::perp(position.market_index));

    spot.chain(perp)
        .chain([MarketId::QUOTE_SPOT, MarketId::perp(market_index)])
        .collect()
}

/// The executor's named accounts and its instruction data.
struct ExecutorCall {
    accounts: Vec<solana_instruction::AccountMeta>,
    data: Vec<u8>,
}

fn executor_call(keeper: &Keeper, fire: &TriggerFire) -> ExecutorCall {
    let filler_stats = Wallet::derive_stats_account(&keeper.authority);
    let user_stats = Wallet::derive_stats_account(&fire.owner.authority);
    let trigger_conditions = Pubkey::find_program_address(
        &[
            program::state::user_conditions::USER_CONDITIONS_PDA_SEED,
            fire.user.as_ref(),
        ],
        &PROGRAM_ID,
    )
    .0;
    let crank_conditions = Some(derive_clob_crank_conditions(fire.market_index));

    match fire.order_type {
        OrderType::TriggerLimit => ExecutorCall {
            accounts: program::accounts::TriggerLimitOrderV1 {
                state: *state_account(),
                authority: keeper.authority,
                filler: keeper.filler,
                filler_stats,
                user: fire.user,
                user_stats,
                quoter_slab: fire.book.quoter_slab,
                clob_market: fire.book.clob_market,
                clob_program: fire.book.clob_program,
                crank_conditions,
                trigger_conditions,
                sol_spot_market: fire.sol_spot_market,
            }
            .to_account_metas(None),
            data: program::instruction::TriggerLimitOrderV1 {
                args: program::instructions::TriggerLimitOrderV1Args {
                    market_index: fire.market_index,
                    order_id: fire.order_id,
                },
            }
            .data(),
        },
        _ => ExecutorCall {
            accounts: program::accounts::TriggerMarketOrderV1 {
                state: *state_account(),
                authority: keeper.authority,
                filler: keeper.filler,
                filler_stats,
                user: fire.user,
                user_stats,
                quoter_slab: fire.book.quoter_slab,
                clob_market: fire.book.clob_market,
                clob_program: fire.book.clob_program,
                crank_conditions,
                trigger_conditions,
                ix_sysvar: None,
                sol_spot_market: fire.sol_spot_market,
            }
            .to_account_metas(None),
            data: program::instruction::TriggerMarketOrderV1 {
                args: program::instructions::TriggerMarketOrderV1Args {
                    market_index: fire.market_index,
                    order_id: fire.order_id,
                    signed_route: Vec::new(),
                },
            }
            .data(),
        },
    }
}

/// The `trigger_market_order_v1` or `trigger_limit_order_v1` instruction for `fire`. The account
/// list is the one the relay resolvers stage, with the keeper in place of the protocol `User`.
pub fn trigger_ix(program_data: &ProgramData, keeper: &Keeper, fire: &TriggerFire) -> Instruction {
    let call = executor_call(keeper, fire);
    let markets = margin_map_markets(fire.owner, fire.market_index);
    let mut accounts = build_accounts(
        program_data,
        call.accounts,
        std::iter::empty(),
        std::iter::empty(),
        markets.iter(),
    );

    // The authority is an unchecked account, so its generated meta does not sign.
    for meta in accounts.iter_mut().filter(|m| m.pubkey == keeper.authority) {
        meta.is_signer = true;
    }

    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: call.data,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Feed the client's account map when no other mode does. The liquidator's source already feeds
/// it, so a combined run skips this.
pub async fn subscribe_accounts(config: &Config, velocity: &VelocityClient) {
    match config.account_source {
        crate::AccountSource::Websocket => {
            let mut events = crate::ws_source::setup_websocket(velocity.clone()).await;
            tokio::spawn(async move { while events.recv().await.is_some() {} });
        }
        crate::AccountSource::Grpc => {
            let _ = crate::filler::sync_user_accounts(velocity).await;
            velocity
                .grpc_subscribe(
                    std::env::var("GRPC_ENDPOINT")
                        .unwrap_or_else(|_| "https://api.rpcpool.com".to_string()),
                    std::env::var("GRPC_X_TOKEN").expect("GRPC_X_TOKEN set"),
                    GrpcSubscribeOpts::default()
                        .commitment(solana_commitment_config::CommitmentLevel::Processed)
                        .usermap_on(),
                    true,
                )
                .await
                .expect("grpc subscribed");
        }
    }

    log::info!(target: TARGET, "subscribed {:?}", config.account_source);
}

pub struct TriggerBot {
    velocity: VelocityClient,
    keeper: Keeper,
    tx_sender: TxSender,
    priority_fees: Arc<PriorityFeeSubscriber>,
    cu_limit: u32,
    grace: GraceWindow,
}

impl TriggerBot {
    pub async fn new(config: &Config, velocity: VelocityClient, metrics: Arc<Metrics>) -> Self {
        let keeper = Keeper {
            authority: *velocity.wallet().authority(),
            filler: velocity.wallet().sub_account(config.sub_account_id),
        };

        let market_pubkeys: Vec<Pubkey> = velocity
            .program_data()
            .perp_market_configs()
            .iter()
            .map(|market| market.pubkey)
            .collect();
        let priority_fees =
            PriorityFeeSubscriber::new(velocity.rpc().url(), &market_pubkeys).subscribe();

        velocity.subscribe_blockhashes().await.expect("subscribed");
        let tx_sender = TxWorker::new(
            velocity.clone(),
            metrics,
            config.dry,
            None,
            None,
            None,
            None,
        )
        .run(tokio::runtime::Handle::current());

        log::info!(
            target: TARGET,
            "trigger backup started: filler={} grace_slots={}",
            keeper.filler,
            config.trigger_grace_slots
        );

        Self {
            velocity,
            keeper,
            tx_sender,
            priority_fees,
            cu_limit: config.trigger_cu_limit,
            grace: GraceWindow::new(config.trigger_grace_slots),
        }
    }

    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(TICK);
        loop {
            interval.tick().await;
            let Some(slot) = self.velocity.get_slot().await else {
                continue;
            };

            let due = self.due_orders(unix_now());
            let keys: Vec<OrderKey> = due.iter().map(|order| order.key).collect();
            let ready: HashSet<OrderKey> = self.grace.ready(&keys, slot).into_iter().collect();

            for order in due.iter().filter(|order| ready.contains(&order.key)) {
                self.fire(order, slot).await;
                self.grace.record_fire(order.key, slot);
            }
        }
    }

    /// The trigger price of every perp market, as the executor computes it. A market with no
    /// cached account or oracle price is missing.
    fn trigger_prices(&self, now: i64) -> HashMap<u16, u64> {
        let use_median = self
            .velocity
            .state_account()
            .is_ok_and(|state| state.has_median_trigger_price_feature());

        self.velocity
            .program_data()
            .perp_market_configs()
            .iter()
            .filter_map(|config| {
                let index = config.market_index;
                let market = self.velocity.try_get_perp_market_account(index).ok()?;
                let oracle = self
                    .velocity
                    .try_get_oracle_price_data_and_slot(MarketId::perp(index))?;
                let price = market
                    .get_trigger_price(oracle.data.price, now, use_median)
                    .ok()?;

                Some((index, price))
            })
            .collect()
    }

    fn due_orders(&self, now: i64) -> Vec<DueOrder> {
        let prices = self.trigger_prices(now);
        let mut due = Vec::new();
        self.velocity
            .backend()
            .account_map()
            .iter_accounts_with::<User>(|pubkey, user, _slot| {
                let orders = user.orders.iter().filter(|order| {
                    is_armed_trigger(order, now)
                        && prices
                            .get(&order.market_index)
                            .is_some_and(|price| trigger_is_due(order, *price))
                });

                due.extend(orders.map(|order| DueOrder {
                    key: OrderKey {
                        user: *pubkey,
                        order_id: order.order_id,
                    },
                    market_index: order.market_index,
                    order_type: order.order_type,
                }));
            });

        due
    }

    async fn filler_account(&self) -> Option<User> {
        match self.velocity.try_get_account::<User>(&self.keeper.filler) {
            Ok(user) => Some(user),
            Err(_) => self
                .velocity
                .get_user_account(&self.keeper.filler)
                .await
                .ok(),
        }
    }

    async fn fire(&self, order: &DueOrder, slot: u64) {
        let DueOrder {
            key,
            market_index,
            order_type,
        } = *order;
        let skip = |reason: &str| {
            log::warn!(
                target: TARGET,
                "trigger fire skipped: user={} order_id={} market={market_index} reason={reason}",
                key.user,
                key.order_id
            );
        };

        let Ok(owner) = self.velocity.try_get_account::<User>(&key.user) else {
            return skip("owner_not_cached");
        };

        let Some(filler) = self.filler_account().await else {
            return skip("filler_account_missing");
        };

        let Some(book) = market_book(&self.velocity, market_index).await else {
            return skip("market_has_no_book");
        };

        let sol_spot_market = self
            .velocity
            .state_account()
            .ok()
            .filter(|state| state.sol_spot_market_index != 0)
            .map(|state| derive_spot_market_account(state.sol_spot_market_index));

        let fire = TriggerFire {
            user: key.user,
            owner: &owner,
            order_id: key.order_id,
            market_index,
            order_type,
            book: book.accounts,
            sol_spot_market,
        };
        let ix = trigger_ix(self.velocity.program_data(), &self.keeper, &fire);
        let tx = TransactionBuilder::new(
            self.velocity.program_data(),
            self.keeper.filler,
            Cow::Owned(filler),
            false,
        )
        .with_priority_fee(
            self.priority_fees.priority_fee_nth(0.6),
            Some(self.cu_limit),
        )
        .add_ix(ix)
        .build_v1();

        let intent = TxIntent::Trigger {
            market_index,
            order_id: key.order_id,
            taker_user: key.user,
            slot,
        };
        match self
            .tx_sender
            .send_tx(tx, intent, u64::from(self.cu_limit))
            .await
        {
            Some(sig) => log::info!(
                target: TARGET,
                "trigger fire queued after grace: user={} order_id={} market={market_index} type={order_type:?} sig={sig}",
                key.user,
                key.order_id
            ),
            None => skip("sign_or_queue_failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        anchor_lang::Discriminator,
        velocity_rs::types::{
            accounts::{PerpMarket, SpotMarket},
            PerpPosition,
        },
    };

    fn order(condition: OrderTriggerCondition, trigger_price: u64) -> Order {
        Order {
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            market_type: MarketType::Perp,
            trigger_condition: condition,
            trigger_price,
            order_id: 7,
            ..Default::default()
        }
    }

    fn key(order_id: u32) -> OrderKey {
        OrderKey {
            user: Pubkey::new_from_array([1; 32]),
            order_id,
        }
    }

    #[test]
    fn an_order_is_due_once_the_price_crosses_its_trigger() {
        let above = order(OrderTriggerCondition::Above, 100);
        assert!(!trigger_is_due(&above, 100));
        assert!(trigger_is_due(&above, 101));

        let below = order(OrderTriggerCondition::Below, 100);
        assert!(!trigger_is_due(&below, 100));
        assert!(trigger_is_due(&below, 99));
    }

    #[test]
    fn a_rearmed_order_is_due_on_the_recross() {
        let mut above = order(OrderTriggerCondition::Above, 100);
        above.add_bit_flag(OrderBitFlag::AwaitingTriggerRecross);
        assert!(trigger_is_due(&above, 100));
        assert!(!trigger_is_due(&above, 101));
    }

    #[test]
    fn a_triggered_order_is_never_due() {
        let triggered = order(OrderTriggerCondition::TriggeredAbove, 100);
        assert!(!trigger_is_due(&triggered, 200));
    }

    #[test]
    fn only_open_unexpired_perp_triggers_are_armed() {
        let armed = order(OrderTriggerCondition::Above, 100);
        assert!(is_armed_trigger(&armed, 10));

        let limit = Order {
            order_type: OrderType::Limit,
            ..armed
        };
        assert!(!is_armed_trigger(&limit, 10));

        let expired = Order { max_ts: 5, ..armed };
        assert!(!is_armed_trigger(&expired, 10));

        let mut placed = armed;
        placed.add_bit_flag(OrderBitFlag::PlacedOnClob);
        assert!(!is_armed_trigger(&placed, 10));
    }

    #[test]
    fn an_order_waits_out_the_grace_window() {
        let mut grace = GraceWindow::new(20);
        assert!(grace.ready(&[key(1)], 100).is_empty());
        assert!(grace.ready(&[key(1)], 119).is_empty());
        assert_eq!(grace.ready(&[key(1)], 120), vec![key(1)]);
    }

    #[test]
    fn an_order_that_stops_being_due_restarts_its_window() {
        let mut grace = GraceWindow::new(20);
        grace.ready(&[key(1)], 100);
        grace.ready(&[], 110);
        assert!(grace.ready(&[key(1)], 125).is_empty());
        assert_eq!(grace.ready(&[key(1)], 145), vec![key(1)]);
    }

    #[test]
    fn a_failed_fire_backs_off() {
        let mut grace = GraceWindow::new(20);
        grace.ready(&[key(1)], 100);
        assert_eq!(grace.ready(&[key(1)], 120), vec![key(1)]);

        grace.record_fire(key(1), 120);
        assert!(grace.ready(&[key(1)], 139).is_empty());
        assert_eq!(grace.ready(&[key(1)], 140), vec![key(1)]);

        grace.record_fire(key(1), 140);
        assert!(grace.ready(&[key(1)], 179).is_empty());
        assert_eq!(grace.ready(&[key(1)], 180), vec![key(1)]);
    }

    fn program_data() -> ProgramData {
        let spot = (0..2)
            .map(|index| SpotMarket {
                market_index: index,
                pubkey: Pubkey::new_from_array([10 + index as u8; 32]),
                oracle: Pubkey::new_from_array([20 + index as u8; 32]),
                ..Default::default()
            })
            .collect();
        let perp = (0..3)
            .map(|index| PerpMarket {
                market_index: index,
                pubkey: Pubkey::new_from_array([30 + index as u8; 32]),
                oracle: Pubkey::new_from_array([40 + index as u8; 32]),
                ..Default::default()
            })
            .collect();

        ProgramData::new(spot, perp, vec![], Default::default())
    }

    fn fire_accounts(order_type: OrderType) -> Instruction {
        let keeper = Keeper {
            authority: Pubkey::new_from_array([2; 32]),
            filler: Pubkey::new_from_array([3; 32]),
        };
        let mut owner = User::default();
        owner.perp_positions[0] = PerpPosition {
            market_index: 1,
            base_asset_amount: 5,
            ..Default::default()
        };
        let fire = TriggerFire {
            user: Pubkey::new_from_array([4; 32]),
            owner: &owner,
            order_id: 9,
            market_index: 2,
            order_type,
            book: ClobFillAccounts {
                market_index: 2,
                quoter_slab: Pubkey::new_from_array([5; 32]),
                clob_market: Pubkey::new_from_array([6; 32]),
                clob_program: Pubkey::new_from_array([7; 32]),
                crank_conditions: None,
            },
            sol_spot_market: None,
        };

        trigger_ix(&program_data(), &keeper, &fire)
    }

    #[test]
    fn a_fire_carries_the_owner_margin_map_after_the_named_accounts() {
        let ix = fire_accounts(OrderType::TriggerMarket);
        let named = program::accounts::TriggerMarketOrderV1 {
            state: Pubkey::default(),
            authority: Pubkey::default(),
            filler: Pubkey::default(),
            filler_stats: Pubkey::default(),
            user: Pubkey::default(),
            user_stats: Pubkey::default(),
            quoter_slab: Pubkey::default(),
            clob_market: Pubkey::default(),
            clob_program: Pubkey::default(),
            crank_conditions: None,
            trigger_conditions: Pubkey::default(),
            ix_sysvar: None,
            sol_spot_market: None,
        }
        .to_account_metas(None)
        .len();
        let map = &ix.accounts[named..];

        let oracles: Vec<Pubkey> = map
            .iter()
            .take_while(|m| !m.is_writable)
            .map(|m| m.pubkey)
            .collect();
        let markets: Vec<Pubkey> = map[oracles.len()..].iter().map(|m| m.pubkey).collect();

        assert_eq!(oracles.len(), 3);
        assert!(map[oracles.len()..].iter().all(|m| m.is_writable));
        assert_eq!(
            markets,
            vec![
                Pubkey::new_from_array([10; 32]),
                Pubkey::new_from_array([31; 32]),
                Pubkey::new_from_array([32; 32]),
            ]
        );
    }

    #[test]
    fn the_keeper_signs_and_the_order_type_picks_the_executor() {
        let market = fire_accounts(OrderType::TriggerMarket);
        let limit = fire_accounts(OrderType::TriggerLimit);

        assert!(market.accounts[1].is_signer);
        assert!(limit.accounts[1].is_signer);
        assert_eq!(
            &market.data[..8],
            program::instruction::TriggerMarketOrderV1::DISCRIMINATOR
        );
        assert_eq!(
            &limit.data[..8],
            program::instruction::TriggerLimitOrderV1::DISCRIMINATOR
        );
        assert_eq!(market.accounts.len(), limit.accounts.len() + 1);
    }
}
