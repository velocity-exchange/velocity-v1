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
