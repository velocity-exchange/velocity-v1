use crate::{
    state::{
        fill_mode::FillMode,
        user::{Order, OrderType},
    },
    PositionDirection, PRICE_PRECISION_I64, PRICE_PRECISION_U64,
};

/// Every mode reads the order's own worst price. The modes differed only while
/// an order auctioned, where place-and-take priced at a fraction of the ramp.
#[test]
fn every_mode_reads_the_orders_own_worst_price() {
    let market_order = Order {
        order_type: OrderType::Market,
        direction: PositionDirection::Long,
        price: 120 * PRICE_PRECISION_U64,
        ..Order::default()
    };

    let oracle_price = Some(100 * PRICE_PRECISION_I64);
    let tick_size = 1;

    for mode in [
        FillMode::Fill,
        FillMode::PlaceAndTake,
        FillMode::Liquidation,
    ] {
        let limit_price = mode
            .get_limit_price(&market_order, oracle_price, tick_size)
            .unwrap();

        assert_eq!(limit_price, Some(120 * PRICE_PRECISION_U64));
    }
}

/// An oracle-relative order resolves its offset against the oracle.
#[test]
fn an_oracle_relative_order_resolves_against_the_oracle() {
    let order = Order {
        order_type: OrderType::Oracle,
        direction: PositionDirection::Long,
        oracle_price_offset: PRICE_PRECISION_I64,
        ..Order::default()
    };

    let limit_price = FillMode::Fill
        .get_limit_price(&order, Some(100 * PRICE_PRECISION_I64), 1)
        .unwrap();

    assert_eq!(limit_price, Some(101 * PRICE_PRECISION_U64));
}

/// `quote_limit_price` resolves without an oracle, so an oracle-relative order
/// reports no bound rather than guessing one.
#[test]
fn an_oracle_relative_order_quotes_no_bound() {
    let order = Order {
        order_type: OrderType::Oracle,
        direction: PositionDirection::Long,
        oracle_price_offset: PRICE_PRECISION_I64,
        ..Order::default()
    };

    assert_eq!(FillMode::Fill.quote_limit_price(&order, 1), 0);
}
