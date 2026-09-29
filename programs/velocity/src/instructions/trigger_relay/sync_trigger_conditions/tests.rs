//! Tests for the trigger watch threshold.

use {super::earliest_oracle_trigger_price, crate::state::oracle_watch::WatchDirection};

const TRIGGER_PRICE: u64 = 100_000;

/// Every clamp divisor `PerpMarket::trigger_price_clamp_divisor` returns.
const CLAMP_DIVISORS: [u64; 3] = [500, 100, 40];

/// The median trigger price can sit anywhere in `oracle ± oracle / divisor`,
/// so the watch must fire at every oracle price where the band reaches the
/// trigger.
#[test]
fn the_watch_fires_wherever_the_median_can_reach_the_trigger() {
    for divisor in CLAMP_DIVISORS {
        let above =
            earliest_oracle_trigger_price(TRIGGER_PRICE, divisor, WatchDirection::AtOrAbove)
                .unwrap();
        let below =
            earliest_oracle_trigger_price(TRIGGER_PRICE, divisor, WatchDirection::AtOrBelow)
                .unwrap();

        for oracle in 90_000..110_000u64 {
            let band = oracle / divisor;
            if oracle + band >= TRIGGER_PRICE {
                assert!(oracle >= above, "divisor {} oracle {}", divisor, oracle);
            }

            if oracle - band <= TRIGGER_PRICE {
                assert!(oracle <= below, "divisor {} oracle {}", divisor, oracle);
            }
        }
    }
}

/// The widening is at most the clamp band, so the watch stays near the
/// trigger.
#[test]
fn the_watch_moves_the_threshold_by_the_band_at_most() {
    let above = earliest_oracle_trigger_price(TRIGGER_PRICE, 40, WatchDirection::AtOrAbove);
    let below = earliest_oracle_trigger_price(TRIGGER_PRICE, 40, WatchDirection::AtOrBelow);

    assert_eq!(above, Some(97_560));
    assert_eq!(below, Some(102_564));
}

/// `load_maps` reads spot markets only until the first perp market. A call
/// that passes the perp market first must still store the quote spot market
/// where the parser reads it.
#[test]
fn markets_are_stored_spot_first_whatever_the_call_order() {
    use {
        super::collect_trigger_inputs,
        crate::{
            create_anchor_account_info,
            instructions::optional_accounts::load_maps,
            math::time::SlotClock,
            state::{perp_market::PerpMarket, perp_market_map::MarketSet, spot_market::SpotMarket},
        },
        anchor_lang::prelude::Pubkey,
    };

    let (perp_key, spot_key) = (Pubkey::new_unique(), Pubkey::new_unique());
    let mut perp = PerpMarket::default();
    create_anchor_account_info!(perp, &perp_key, PerpMarket, perp_info);
    let mut spot = SpotMarket::default();
    create_anchor_account_info!(spot, &spot_key, SpotMarket, spot_info);

    let passed = [perp_info.clone(), spot_info.clone()];
    let inputs = collect_trigger_inputs(&passed).unwrap();
    let stored: Vec<Pubkey> = inputs
        .market_refs()
        .iter()
        .map(|account| Pubkey::new_from_array(account.address))
        .collect();
    assert_eq!(stored, vec![spot_key, perp_key]);

    let replayed = [spot_info, perp_info];
    let maps = load_maps(
        &mut replayed.iter().peekable(),
        &MarketSet::new(),
        &MarketSet::new(),
        0,
        SlotClock::baseline(),
        None,
    )
    .unwrap();
    assert!(maps.spot_market_map.get_ref(&0).is_ok());
    assert!(maps.perp_market_map.get_ref(&0).is_ok());
}

mod coverage_and_direction {
    use {
        super::super::{
            rewatch_trigger_slot, validate_trigger_coverage, watch_direction, MarketInputs,
        },
        crate::{
            create_anchor_account_info,
            state::{
                oracle::OracleSource,
                oracle_watch::WatchDirection,
                perp_market::PerpMarket,
                pyth_lazer_oracle::PythLazerOracle,
                user::{Order, OrderBitFlag, OrderStatus, OrderTriggerCondition, OrderType, User},
                user_conditions::{TriggerSlotMetaV0, UserConditionsV0, TRIGGER_SLOT_BASE},
            },
            test_utils::get_pyth_price,
        },
        anchor_lang::prelude::Pubkey,
        relay_spec::{ConditionBlock, WakeView},
        std::collections::BTreeMap,
    };

    fn stop_above() -> Order {
        Order {
            order_id: 7,
            status: OrderStatus::Open,
            order_type: OrderType::TriggerMarket,
            market_index: 0,
            trigger_price: 100_000_000,
            trigger_condition: OrderTriggerCondition::Above,
            ..Order::default()
        }
    }

    fn user_with(order: Order) -> User {
        let mut user = User::default();
        user.orders[0] = order;
        user
    }

    fn clob_market(has_slab: bool) -> BTreeMap<u16, MarketInputs> {
        BTreeMap::from([(
            0,
            MarketInputs {
                oracle: Some(Pubkey::new_unique()),
                has_clob: true,
                has_slab,
                keeper_payment_lamports: Some(1),
                ..MarketInputs::default()
            },
        )])
    }

    /// Omitting the slab would skip the order and clear the slot that
    /// watched it, so the call is refused instead.
    #[test]
    fn a_trigger_on_a_clob_market_needs_its_slab() {
        let user = user_with(stop_above());
        assert!(validate_trigger_coverage(&user, &clob_market(false), 0).is_err());
        assert!(validate_trigger_coverage(&user, &clob_market(true), 0).is_ok());
        assert!(validate_trigger_coverage(&user, &BTreeMap::new(), 0).is_err());
    }

    /// A suspended book keeps its market's watches, so a sync during the
    /// suspension does not disarm them.
    #[test]
    fn a_suspended_book_still_arms_its_triggers() {
        use crate::state::prop_amm::{QuoterSlotV0, QuoterType};
        let mut slot = QuoterSlotV0 {
            entry: Pubkey::new_unique(),
            suspended: true,
            ..QuoterSlotV0::default()
        };

        slot.config.quoter_type = QuoterType::Clob;
        slot.config.response_account = Pubkey::new_unique();
        assert!(!slot.quotes());
        assert_eq!(
            super::super::armable_book(&[slot]).map(|(book, _)| book),
            Some(slot.config.response_account)
        );
    }

    /// Only eight triggers are watched. An armed stop-loss takes a slot ahead
    /// of take-profits, entries and placed stop-limits, whatever their slots.
    #[test]
    fn an_armed_stop_loss_takes_the_first_slot() {
        use crate::controller::position::PositionDirection;
        let order = |order_id: u32, reduce_only: bool, trigger_condition| Order {
            order_id,
            status: OrderStatus::Open,
            direction: PositionDirection::Short,
            trigger_condition,
            reduce_only,
            ..Order::default()
        };

        let mut placed = order(1, true, OrderTriggerCondition::Below);
        placed.add_bit_flag(OrderBitFlag::PlacedOnClob);
        let orders = [
            placed,
            order(2, false, OrderTriggerCondition::Below),
            order(3, true, OrderTriggerCondition::Above),
            order(4, true, OrderTriggerCondition::Below),
        ];

        let ids: Vec<u32> = super::super::in_watch_priority(&orders)
            .map(|order| order.order_id)
            .collect();
        assert_eq!(ids, vec![4, 3, 2, 1]);
    }

    /// Placement holds to the slot cap only armed, watched reduce-only
    /// stop-losses.
    #[test]
    fn only_armed_stop_losses_rank_first() {
        use crate::controller::position::PositionDirection;
        let stop_loss = Order {
            order_type: OrderType::TriggerMarket,
            direction: PositionDirection::Short,
            trigger_condition: OrderTriggerCondition::Below,
            reduce_only: true,
            ..stop_above()
        };

        let mut placed = stop_loss;
        placed.add_bit_flag(OrderBitFlag::PlacedOnClob);
        let expired = Order {
            max_ts: 10,
            ..stop_loss
        };
        let take_profit = Order {
            trigger_condition: OrderTriggerCondition::Above,
            ..stop_loss
        };

        let armed: Vec<bool> = [stop_loss, placed, expired, take_profit]
            .iter()
            .map(|order| super::super::is_armed_stop_loss(order, 20))
            .collect();
        assert_eq!(armed, vec![true, false, false, false]);
    }

    /// A market with no CLOB has nowhere to fire, so its order stays skipped.
    #[test]
    fn a_trigger_on_a_market_without_a_clob_is_skipped() {
        let user = user_with(stop_above());
        let mut markets = clob_market(false);
        markets.get_mut(&0).unwrap().has_clob = false;
        assert!(validate_trigger_coverage(&user, &markets, 0).is_ok());
    }

    /// An evicted order is due on the non-trigger side until it recrosses.
    #[test]
    fn an_order_awaiting_its_recross_watches_the_other_side() {
        let mut order = stop_above();
        assert_eq!(watch_direction(&order), Some(WatchDirection::AtOrAbove));

        order.add_bit_flag(OrderBitFlag::AwaitingTriggerRecross);
        assert_eq!(watch_direction(&order), Some(WatchDirection::AtOrBelow));

        order.trigger_condition = OrderTriggerCondition::Below;
        assert_eq!(watch_direction(&order), Some(WatchDirection::AtOrAbove));
    }

    /// Once the recross clears the flag, the slot must watch the trigger side
    /// again, or the order never fires.
    #[test]
    fn a_recrossed_order_watches_the_trigger_side_again() {
        let oracle_key = Pubkey::new_unique();
        let mut oracle: PythLazerOracle = get_pyth_price(100, 6);
        create_anchor_account_info!(oracle, &oracle_key, PythLazerOracle, oracle_info);
        let market = PerpMarket {
            oracle: oracle_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };

        let mut conditions = Box::new(UserConditionsV0::default());
        conditions.init_block().unwrap();
        conditions.trigger_slots[0] = TriggerSlotMetaV0 {
            order_id: 7,
            ..TriggerSlotMetaV0::default()
        };

        let mut evicted = stop_above();
        evicted.add_bit_flag(OrderBitFlag::AwaitingTriggerRecross);
        let wake_cmp = |conditions: &UserConditionsV0| match ConditionBlock::read_condition(
            &conditions.relay,
            TRIGGER_SLOT_BASE,
        )
        .unwrap()
        .wake()
        .unwrap()
        {
            WakeView::OnValueCross { cmp, .. } => cmp,
            other => panic!("expected OnValueCross, got {:?}", other),
        };

        rewatch_trigger_slot(&mut conditions, &evicted, &market, &oracle_info).unwrap();
        assert_eq!(wake_cmp(&conditions), WatchDirection::AtOrBelow.cmp());

        rewatch_trigger_slot(&mut conditions, &stop_above(), &market, &oracle_info).unwrap();
        assert_eq!(wake_cmp(&conditions), WatchDirection::AtOrAbove.cmp());
    }

    /// Placing a stop-limit parks its slot on the recross side, quiet. An
    /// eviction wakes the slot, and the re-armed order is then due where the
    /// slot points, so relay observes its recross without a new sync.
    #[test]
    fn a_placed_stop_limit_keeps_a_parked_slot_that_an_eviction_wakes() {
        use {
            super::super::park_trigger_slot,
            relay_spec::{ConditionV0, CrankSpecV0, ResolverListV0, WatchValue, WatchedRegion},
        };

        let oracle_key = Pubkey::new_unique();
        let mut oracle: PythLazerOracle = get_pyth_price(100, 6);
        create_anchor_account_info!(oracle, &oracle_key, PythLazerOracle, oracle_info);
        let market = PerpMarket {
            oracle: oracle_key,
            oracle_source: OracleSource::PythLazer,
            ..PerpMarket::default()
        };

        let mut conditions = Box::new(UserConditionsV0::default());
        conditions.init_block().unwrap();
        let armed = ConditionV0::on_value_cross(
            WatchedRegion::new(oracle_key.to_bytes(), 0, 8),
            WatchValue::Signed(0),
            WatchDirection::AtOrAbove.cmp(),
            CrankSpecV0 {
                resolver_program: [1; 32],
                resolver_disc: [2; 8],
                min_payment: 5_000,
            },
            ResolverListV0::new(0, 5),
        );

        conditions.set_condition(TRIGGER_SLOT_BASE, &armed).unwrap();
        conditions.trigger_slots[0] = TriggerSlotMetaV0 {
            order_id: 7,
            ..TriggerSlotMetaV0::default()
        };

        let mut placed = stop_above();
        placed.order_type = OrderType::TriggerLimit;
        placed.add_bit_flag(OrderBitFlag::PlacedOnClob);
        park_trigger_slot(&mut conditions, &placed, &market, &oracle_info).unwrap();

        let slot = |conditions: &UserConditionsV0| {
            ConditionBlock::read_condition(&conditions.relay, TRIGGER_SLOT_BASE).unwrap()
        };

        let parked = slot(&conditions);
        assert!(!parked.is_active());
        assert_eq!(conditions.trigger_slot_index(0, 7), Some(0));

        conditions.set_trigger_slot_active(0, true).unwrap();
        let woken = slot(&conditions);
        assert!(woken.is_active());
        assert_eq!(woken.crank_spec().min_payment, 5_000);
        assert_eq!(woken.wake().unwrap(), parked.wake().unwrap());
        match woken.wake().unwrap() {
            WakeView::OnValueCross { cmp, .. } => {
                assert_eq!(cmp, WatchDirection::AtOrBelow.cmp())
            }
            other => panic!("expected OnValueCross, got {:?}", other),
        }

        let mut evicted = stop_above();
        evicted.add_bit_flag(OrderBitFlag::AwaitingTriggerRecross);
        assert_eq!(watch_direction(&evicted), Some(WatchDirection::AtOrBelow));
    }
}

/// Every book shares one program. A sync over two slabs stores it once, and a
/// list stored with one copy per slab still replays.
#[test]
fn two_book_markets_store_the_book_program_once() {
    use {
        super::collect_trigger_inputs,
        crate::{
            instructions::refuse_duplicate_accounts_except,
            state::prop_amm::{QuoterSlabV0, QuoterSlotV0, QuoterType},
        },
        anchor_lang::{prelude::AccountInfo, Discriminator},
        solana_program::pubkey::Pubkey,
    };

    let program_key = Pubkey::new_unique();
    let other_owner = Pubkey::new_unique();
    let owner = crate::ID;
    let make_slab = |market: u16, book: Pubkey| {
        let mut slot: QuoterSlotV0 = bytemuck::Zeroable::zeroed();
        slot.entry = Pubkey::new_unique();
        slot.config.quoter_type = QuoterType::Clob;
        slot.config.response_account = book;
        slot.config.program_id = program_key;
        let header = QuoterSlabV0 {
            capacity: 1,
            market,
            ..QuoterSlabV0::default()
        };
        let mut data = QuoterSlabV0::DISCRIMINATOR.to_vec();
        data.extend_from_slice(bytemuck::bytes_of(&header));
        data.extend_from_slice(bytemuck::bytes_of(&slot));
        data
    };

    let (book0, book1) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (slab0_key, slab1_key) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (mut d0, mut d1) = (make_slab(0, book0), make_slab(1, book1));
    let (mut db0, mut db1, mut dp0, mut dp1) = (vec![], vec![], vec![], vec![]);
    let mut lamports = [0u64; 6];
    let [l0, l1, l2, l3, l4, l5] = &mut lamports;
    let slab0 = AccountInfo::new(&slab0_key, false, false, l0, &mut d0, &owner, false);
    let slab1 = AccountInfo::new(&slab1_key, false, false, l1, &mut d1, &owner, false);
    let b0 = AccountInfo::new(&book0, false, true, l2, &mut db0, &other_owner, false);
    let b1 = AccountInfo::new(&book1, false, true, l3, &mut db1, &other_owner, false);
    let p0 = AccountInfo::new(&program_key, false, false, l4, &mut dp0, &other_owner, true);
    let p1 = AccountInfo::new(&program_key, false, false, l5, &mut dp1, &other_owner, true);
    fn stored_keys<'a>(accounts: &'a [AccountInfo<'a>]) -> Vec<Pubkey> {
        collect_trigger_inputs(accounts)
            .unwrap()
            .tail_refs
            .iter()
            .map(|r| Pubkey::new_from_array(r.address))
            .collect()
    }

    let synced = [slab0.clone(), slab1.clone()];
    let stored = stored_keys(&synced);
    assert_eq!(
        stored,
        vec![slab0_key, book0, program_key, slab1_key, book1]
    );

    let legacy = [slab0.clone(), b0.clone(), p0, slab1.clone(), b1.clone(), p1];
    let legacy_inputs = collect_trigger_inputs(&legacy).unwrap();
    assert!(refuse_duplicate_accounts_except(&legacy, &legacy_inputs.book_programs).is_ok());
    assert!(legacy_inputs.oracle_infos.is_empty());
    assert_eq!(stored_keys(&legacy), stored);

    let twice = [slab0.clone(), b0, slab0, slab1, b1];
    let twice_inputs = collect_trigger_inputs(&twice).unwrap();
    assert!(refuse_duplicate_accounts_except(&twice, &twice_inputs.book_programs).is_err());
}
