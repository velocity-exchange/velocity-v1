mod calculate_auction_prices {
    use crate::{
        controller::position::PositionDirection,
        math::{auction::calculate_auction_prices, constants::PRICE_PRECISION_I64},
        state::oracle::OraclePriceData,
    };

    #[test]
    fn no_limit_price_long() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Long;
        let limit_price = 0;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 100500000);
    }

    #[test]
    fn no_limit_price_short() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Short;
        let limit_price = 0;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 99500000);
    }

    #[test]
    fn limit_price_much_better_than_oracle_long() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Long;
        let limit_price = 90000000;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 89550000);
        assert_eq!(auction_end_price, 90000000);
    }

    #[test]
    fn limit_price_slightly_better_than_oracle_long() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Long;
        let limit_price = 99999999;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 99500000);
        assert_eq!(auction_end_price, 99999999);
    }

    #[test]
    fn limit_price_much_worse_than_oracle_long() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Long;
        let limit_price = 110000000;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 100500000);
    }

    #[test]
    fn limit_price_slightly_worse_than_oracle_long() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Long;
        let limit_price = 100400000;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 100400000);
    }

    #[test]
    fn limit_price_much_better_than_oracle_short() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Short;
        let limit_price = 110000000;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 110550000);
        assert_eq!(auction_end_price, 110000000);
    }

    #[test]
    fn limit_price_slightly_better_than_oracle_short() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Short;
        let limit_price = 100000001;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 100500001);
        assert_eq!(auction_end_price, 100000001);
    }

    #[test]
    fn limit_price_much_worse_than_oracle_short() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Short;
        let limit_price = 90000000;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 99500000);
    }

    #[test]
    fn limit_price_slightly_worse_than_oracle_short() {
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let position_direction = PositionDirection::Short;
        let limit_price = 99999999;

        let (auction_start_price, auction_end_price) =
            calculate_auction_prices(&oracle_price_data, position_direction, limit_price).unwrap();

        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 99999999);
    }
}

mod calculate_auction_price {
    use crate::{
        math::{
            auction::calculate_auction_price,
            constants::{PRICE_PRECISION_I64, PRICE_PRECISION_U64},
            time::SlotClock,
        },
        state::user::{Order, OrderType},
        PositionDirection,
    };

    #[test]
    fn long_oracle_order() {
        let tick_size = 1;

        // auction starts $.10 below oracle and ends $.1 above oracle
        let order = Order {
            order_type: OrderType::Oracle,
            auction_duration: 10,
            slot: 0,
            auction_start_price: -PRICE_PRECISION_I64 / 10,
            auction_end_price: PRICE_PRECISION_I64 / 10,
            ..Order::default()
        };
        let oracle_price = Some(PRICE_PRECISION_I64);

        let slot = 0;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 9 * PRICE_PRECISION_U64 / 10);

        let slot = 5;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, PRICE_PRECISION_U64);

        let slot = 10;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 11 * PRICE_PRECISION_U64 / 10);

        // auction starts $.20 below oracle and ends $.1 below oracle
        let order = Order {
            order_type: OrderType::Oracle,
            auction_duration: 10,
            slot: 0,
            auction_start_price: -PRICE_PRECISION_I64 / 5,
            auction_end_price: -PRICE_PRECISION_I64 / 10,
            ..Order::default()
        };

        let slot = 0;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 8 * PRICE_PRECISION_U64 / 10);

        let slot = 5;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 85 * PRICE_PRECISION_U64 / 100);

        let slot = 10;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 9 * PRICE_PRECISION_U64 / 10);

        // auction starts $.10 above oracle and ends $.2 above oracle
        let order = Order {
            order_type: OrderType::Oracle,
            auction_duration: 10,
            slot: 0,
            auction_start_price: PRICE_PRECISION_I64 / 10,
            auction_end_price: PRICE_PRECISION_I64 / 5,
            ..Order::default()
        };

        let slot = 0;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 11 * PRICE_PRECISION_U64 / 10);

        let slot = 5;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 115 * PRICE_PRECISION_U64 / 100);

        let slot = 10;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 12 * PRICE_PRECISION_U64 / 10);
    }

    #[test]
    fn short_oracle_order() {
        let tick_size = 1;
        // auction starts $.10 above oracle and ends $.1 below oracle
        let order = Order {
            order_type: OrderType::Oracle,
            auction_duration: 10,
            slot: 0,
            auction_start_price: PRICE_PRECISION_I64 / 10,
            auction_end_price: -PRICE_PRECISION_I64 / 10,
            ..Order::default()
        };
        let oracle_price = Some(PRICE_PRECISION_I64);

        let slot = 0;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 11 * PRICE_PRECISION_U64 / 10);

        let slot = 5;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, PRICE_PRECISION_U64);

        let slot = 10;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 9 * PRICE_PRECISION_U64 / 10);

        // auction starts $.20 above oracle and ends $.1 above oracle
        let order = Order {
            order_type: OrderType::Oracle,
            auction_duration: 10,
            slot: 0,
            auction_start_price: PRICE_PRECISION_I64 / 5,
            auction_end_price: PRICE_PRECISION_I64 / 10,
            ..Order::default()
        };

        let slot = 0;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 12 * PRICE_PRECISION_U64 / 10);

        let slot = 5;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 115 * PRICE_PRECISION_U64 / 100);

        let slot = 10;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 11 * PRICE_PRECISION_U64 / 10);

        // auction starts $.10 below oracle and ends $.2 below oracle
        let order = Order {
            order_type: OrderType::Oracle,
            auction_duration: 10,
            slot: 0,
            auction_start_price: -PRICE_PRECISION_I64 / 10,
            auction_end_price: -PRICE_PRECISION_I64 / 5,
            ..Order::default()
        };

        let slot = 0;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 9 * PRICE_PRECISION_U64 / 10);

        let slot = 5;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 85 * PRICE_PRECISION_U64 / 100);

        let slot = 10;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();

        assert_eq!(price, 8 * PRICE_PRECISION_U64 / 10);
    }

    #[test]
    fn same_auction_start_and_end() {
        let tick_size = 1;
        let mut order = Order {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_duration: 10,
            slot: 0,
            auction_start_price: PRICE_PRECISION_I64,
            auction_end_price: PRICE_PRECISION_I64,
            ..Order::default()
        };

        let slot = 5;
        let price =
            calculate_auction_price(&order, slot, tick_size, None, SlotClock::baseline()).unwrap();
        assert_eq!(price, PRICE_PRECISION_U64);

        order.direction = PositionDirection::Short;
        let price =
            calculate_auction_price(&order, slot, tick_size, None, SlotClock::baseline()).unwrap();
        assert_eq!(price, PRICE_PRECISION_U64);

        let mut order = Order {
            order_type: OrderType::Oracle,
            direction: PositionDirection::Long,
            auction_duration: 10,
            slot: 0,
            auction_start_price: PRICE_PRECISION_I64 / 2,
            auction_end_price: PRICE_PRECISION_I64 / 2,
            ..Order::default()
        };
        let oracle_price = Some(PRICE_PRECISION_I64);
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 3 * PRICE_PRECISION_U64 / 2);

        order.direction = PositionDirection::Short;
        let price =
            calculate_auction_price(&order, slot, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 3 * PRICE_PRECISION_U64 / 2);
    }

    #[test]
    fn long_order_with_auction_and_oracle_price_offset() {
        let tick_size = 1;
        let order = Order {
            order_type: OrderType::Limit,
            direction: PositionDirection::Long,
            auction_duration: 10,
            slot: 0,
            auction_start_price: 100 * PRICE_PRECISION_I64 / 20, // 5% above oracle
            auction_end_price: 100 * PRICE_PRECISION_I64 / 10,   // 10% above oracle
            oracle_price_offset: 100 * PRICE_PRECISION_I64 / 5,  // 20% above oracle
            ..Order::default()
        };

        let oracle_price = Some(100 * PRICE_PRECISION_I64);

        // At start of auction
        let price =
            calculate_auction_price(&order, 0, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 105 * PRICE_PRECISION_U64);

        // Midway through auction
        let price =
            calculate_auction_price(&order, 5, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 107_5 * PRICE_PRECISION_U64 / 10);

        // End of auction
        let price =
            calculate_auction_price(&order, 10, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 110 * PRICE_PRECISION_U64);
    }

    #[test]
    fn short_order_with_auction_and_oracle_price_offset() {
        let tick_size = 1;
        let order = Order {
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            auction_duration: 10,
            slot: 0,
            auction_start_price: -100 * PRICE_PRECISION_I64 / 20, // 5% below oracle
            auction_end_price: -100 * PRICE_PRECISION_I64 / 10,   // 10% below oracle
            oracle_price_offset: -100 * PRICE_PRECISION_I64 / 5,  // 20% below oracle
            ..Order::default()
        };

        let oracle_price = Some(100 * PRICE_PRECISION_I64);

        // At start of auction
        let price =
            calculate_auction_price(&order, 0, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 95 * PRICE_PRECISION_U64);

        // Midway through auction
        let price =
            calculate_auction_price(&order, 5, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 92_5 * PRICE_PRECISION_U64 / 10);

        // End of auction
        let price =
            calculate_auction_price(&order, 10, tick_size, oracle_price, SlotClock::baseline())
                .unwrap();
        assert_eq!(price, 90 * PRICE_PRECISION_U64);
    }
}

mod calculate_auction_params_for_trigger_order {
    use crate::{
        math::auction::calculate_auction_params_for_trigger_order,
        state::{
            oracle::OraclePriceData,
            user::{Order, OrderType},
        },
        PositionDirection, PRICE_PRECISION_I64, PRICE_PRECISION_U64,
    };

    #[test]
    fn trigger_limit() {
        let mut order = Order {
            order_type: OrderType::TriggerLimit,
            direction: PositionDirection::Long,
            trigger_price: 100 * PRICE_PRECISION_U64,
            price: 90 * PRICE_PRECISION_U64,
            ..Order::default()
        };
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let min_auction_duration = 10;

        order.direction = PositionDirection::Long;
        order.price = 110 * PRICE_PRECISION_U64;

        let (auction_duration, auction_start_price, auction_end_price) =
            calculate_auction_params_for_trigger_order(
                &order,
                &oracle_price_data,
                min_auction_duration,
                None,
            )
            .unwrap();
        assert_eq!(auction_duration, 10);
        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 100500000);

        order.direction = PositionDirection::Short;
        order.price = 90 * PRICE_PRECISION_U64;

        let (auction_duration, auction_start_price, auction_end_price) =
            calculate_auction_params_for_trigger_order(
                &order,
                &oracle_price_data,
                min_auction_duration,
                None,
            )
            .unwrap();

        assert_eq!(auction_duration, 10);
        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 99500000);
    }

    #[test]
    fn trigger_market() {
        let mut order = Order {
            order_type: OrderType::TriggerMarket,
            direction: PositionDirection::Long,
            trigger_price: 100 * PRICE_PRECISION_U64,
            ..Order::default()
        };
        let oracle_price_data = OraclePriceData {
            price: 100 * PRICE_PRECISION_I64,
            ..OraclePriceData::default()
        };
        let min_auction_duration = 10;

        let (auction_duration, auction_start_price, auction_end_price) =
            calculate_auction_params_for_trigger_order(
                &order,
                &oracle_price_data,
                min_auction_duration,
                None,
            )
            .unwrap();

        assert_eq!(auction_duration, 10);
        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 100500000);

        order.direction = PositionDirection::Short;

        let (auction_duration, auction_start_price, auction_end_price) =
            calculate_auction_params_for_trigger_order(
                &order,
                &oracle_price_data,
                min_auction_duration,
                None,
            )
            .unwrap();

        assert_eq!(auction_duration, 10);
        assert_eq!(auction_start_price, 100000000);
        assert_eq!(auction_end_price, 99500000);
    }
}

mod auction_wall_clock_across_gates {
    use crate::{
        controller::position::PositionDirection,
        math::{
            auction::{calculate_auction_price, is_auction_complete},
            constants::{PRICE_PRECISION_I64, PRICE_PRECISION_U64},
            time::SlotClock,
        },
        state::user::{Order, OrderType},
    };

    // a 10 unit (4s) auction on a fully-200ms chain: the wall clock ramp is
    // unchanged, so it now spans 20 actual slots instead of 10
    fn order() -> Order {
        Order {
            order_type: OrderType::Market,
            direction: PositionDirection::Long,
            auction_duration: 10,
            slot: 1_000,
            auction_start_price: 100 * PRICE_PRECISION_I64,
            auction_end_price: 110 * PRICE_PRECISION_I64,
            ..Order::default()
        }
    }

    fn clock_200() -> SlotClock {
        SlotClock::from_state_fields([1, 1, 1, 1], 0, 0, 0)
    }

    #[test]
    fn interpolation_holds_its_wall_clock_shape_at_200ms() {
        let order = order();
        let clock = clock_200();
        // start
        assert_eq!(
            calculate_auction_price(&order, 1_000, 1, None, clock).unwrap(),
            100 * PRICE_PRECISION_U64
        );
        // 10 slots at 200ms = 2s = halfway through the 4s ramp
        assert_eq!(
            calculate_auction_price(&order, 1_010, 1, None, clock).unwrap(),
            105 * PRICE_PRECISION_U64
        );
        // 20 slots = 4s = the end; further slots stay clamped at the end price
        assert_eq!(
            calculate_auction_price(&order, 1_020, 1, None, clock).unwrap(),
            110 * PRICE_PRECISION_U64
        );
        assert_eq!(
            calculate_auction_price(&order, 1_100, 1, None, clock).unwrap(),
            110 * PRICE_PRECISION_U64
        );
    }

    #[test]
    fn completion_holds_its_wall_clock_length_at_200ms() {
        let order = order();
        let clock = clock_200();
        assert!(!is_auction_complete(order.slot, 10, 1_020, clock).unwrap());
        assert!(is_auction_complete(order.slot, 10, 1_021, clock).unwrap());
        // baseline sanity: 10 slots at 400ms is exactly the 4s length
        let baseline = SlotClock::baseline();
        assert!(!is_auction_complete(order.slot, 10, 1_010, baseline).unwrap());
        assert!(is_auction_complete(order.slot, 10, 1_011, baseline).unwrap());
    }

    #[test]
    fn interpolation_integrates_across_a_transition() {
        // 350ms regime starts at slot 1_005: 5 slots at 400ms (2s) reach the
        // 4s ramp's halfway point, then ~5.72 more 350ms slots finish it
        let order = order();
        let clock = SlotClock::from_state_fields([1_005, 0, 0, 0], 0, 0, 0);
        assert_eq!(
            calculate_auction_price(&order, 1_005, 1, None, clock).unwrap(),
            105 * PRICE_PRECISION_U64
        );
        // 2000ms + 6 * 350ms = 4100ms > 4000ms: clamped at the end price
        assert_eq!(
            calculate_auction_price(&order, 1_011, 1, None, clock).unwrap(),
            110 * PRICE_PRECISION_U64
        );
        assert!(is_auction_complete(order.slot, 10, 1_011, clock).unwrap());
        // 2000ms + 5 * 350ms = 3750ms: still inside the ramp
        assert!(!is_auction_complete(order.slot, 10, 1_010, clock).unwrap());
    }
}
