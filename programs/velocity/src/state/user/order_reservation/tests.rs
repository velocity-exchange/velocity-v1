use {
    super::{OrderReservation, ReleaseCheck},
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        state::user::{Order, OrderStatus, OrderTriggerCondition, OrderType, User},
    },
};

const MARKET: u16 = 3;

fn reduce_only_ask(base_asset_amount: u64) -> OrderReservation {
    OrderReservation::book_order(MARKET, PositionDirection::Short, base_asset_amount, true)
}

#[test]
fn a_release_gives_back_every_part_the_reserve_took() {
    let mut user = User::default();
    let before = user.perp_positions[0];

    let index = user.reserve_orders(&reduce_only_ask(7)).unwrap();
    let position = user.perp_positions[index];
    assert_eq!(position.open_asks, -7);
    assert_eq!(position.open_orders, 1);
    assert_eq!(position.reduce_only_clob_orders, 1);
    assert_eq!(user.open_orders, 1);
    assert!(user.has_open_order);

    user.release_orders(&reduce_only_ask(7), ReleaseCheck::HeldToReservation)
        .unwrap();
    let position = user.perp_positions[index];
    assert_eq!(position.open_asks, before.open_asks);
    assert_eq!(position.open_orders, 0);
    assert_eq!(position.reduce_only_clob_orders, 0);
    assert_eq!(user.open_orders, 0);
    assert!(!user.has_open_order);
}

#[test]
fn an_armed_trigger_holds_its_count_and_no_base() {
    let order = Order {
        status: OrderStatus::Open,
        order_type: OrderType::TriggerLimit,
        market_index: MARKET,
        direction: PositionDirection::Long,
        base_asset_amount: 9,
        trigger_condition: OrderTriggerCondition::Above,
        ..Order::default()
    };

    assert_eq!(
        OrderReservation::of_order(&order).unwrap(),
        OrderReservation {
            market_index: MARKET,
            open_orders: 1,
            ..OrderReservation::default()
        }
    );
}

/// A release above the reservation fails unless the path is an exit. An exit
/// releases the whole side instead.
#[test]
fn only_an_exit_releases_more_than_was_reserved() {
    let mut user = User::default();
    let index = user.reserve_orders(&reduce_only_ask(7)).unwrap();

    assert_eq!(
        user.release_orders(&reduce_only_ask(8), ReleaseCheck::HeldToReservation),
        Err(ErrorCode::QuoterReportExceedsReservation)
    );

    user.release_orders(&reduce_only_ask(8), ReleaseCheck::ClampedForExit)
        .unwrap();
    assert_eq!(user.perp_positions[index].open_asks, 0);
    assert_eq!(user.perp_positions[index].open_orders, 0);
}

/// A position whose only content is one order keeps its index when that order
/// is replaced. Releasing first would free the position and let the reserve
/// open the market in another slot.
#[test]
fn a_replacement_keeps_the_position_it_replaces_in() {
    let mut user = User::default();
    user.perp_positions[0].market_index = MARKET + 1;
    user.perp_positions[0].base_asset_amount = 1;
    user.perp_positions[2].market_index = MARKET;
    user.perp_positions[2].open_orders = 1;

    let armed = OrderReservation {
        market_index: MARKET,
        open_orders: 1,
        ..OrderReservation::default()
    };
    user.open_orders = 1;

    let index = user
        .replace_reservation(&armed, &reduce_only_ask(5))
        .unwrap();
    assert_eq!(index, 2);
    assert_eq!(user.perp_positions[2].open_asks, -5);
    assert_eq!(user.perp_positions[2].open_orders, 1);
    assert_eq!(user.perp_positions[2].reduce_only_clob_orders, 1);
    assert_eq!(user.open_orders, 1);
}
