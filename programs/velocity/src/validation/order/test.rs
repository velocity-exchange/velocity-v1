use {
    super::validate_order,
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        state::{
            perp_market::PerpMarket,
            user::{Order, OrderStatus, OrderType},
        },
        BASE_PRECISION_U64, PRICE_PRECISION_I64, PRICE_PRECISION_U64,
    },
};

fn test_market() -> PerpMarket {
    PerpMarket {
        order_step_size: 1,
        order_tick_size: 1,
        ..PerpMarket::default()
    }
}

fn limit_order() -> Order {
    Order {
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        direction: PositionDirection::Long,
        base_asset_amount: BASE_PRECISION_U64,
        price: 100 * PRICE_PRECISION_U64,
        ..Order::default()
    }
}

#[test]
fn a_limit_order_with_an_oracle_offset_is_refused() {
    let market = test_market();
    let order = Order {
        price: 0,
        oracle_price_offset: PRICE_PRECISION_I64,
        ..limit_order()
    };

    let result = validate_order(&order, &market, Some(100 * PRICE_PRECISION_I64), 1);

    assert_eq!(result, Err(ErrorCode::InvalidOrderOracleOffset));
}

#[test]
fn an_oracle_offset_is_refused_even_beside_a_fixed_price() {
    let market = test_market();
    let order = Order {
        oracle_price_offset: -PRICE_PRECISION_I64,
        ..limit_order()
    };

    let result = validate_order(&order, &market, Some(100 * PRICE_PRECISION_I64), 1);

    assert_eq!(result, Err(ErrorCode::InvalidOrderOracleOffset));
}

#[test]
fn a_fixed_price_limit_order_validates() {
    let market = test_market();
    let order = limit_order();

    validate_order(&order, &market, Some(100 * PRICE_PRECISION_I64), 1).unwrap();
}
