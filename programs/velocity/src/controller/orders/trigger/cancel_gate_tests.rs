//! Tests for the gate that cancels a fired stop-market instead of firing it.

use {
    super::{fired_order_must_cancel, trigger_must_cancel},
    crate::{
        controller::position::PositionDirection,
        create_anchor_account_info,
        instructions::optional_accounts::AccountMaps,
        math::{
            constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_U64, PEG_PRECISION, SPOT_BALANCE_PRECISION,
                SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                SPOT_WEIGHT_PRECISION,
            },
            time::SlotClock,
        },
        state::{
            market_status::MarketStatus,
            oracle::{HistoricalOracleData, OracleSource},
            oracle_map::OracleMap,
            perp_market::{MarketStats, PerpMarket, AMM},
            perp_market_map::PerpMarketMap,
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            user::{
                MarketType, Order, OrderReservation, OrderStatus, OrderTriggerCondition, OrderType,
                SpotPosition, User, UserStats,
            },
        },
        test_utils::{get_pyth_price, get_spot_positions},
    },
    anchor_lang::prelude::{AccountLoader, Pubkey},
    std::str::FromStr,
};

fn stop_buy(trigger_condition: OrderTriggerCondition) -> Order {
    Order {
        order_id: 1,
        status: OrderStatus::Open,
        order_type: OrderType::TriggerMarket,
        market_type: MarketType::Perp,
        direction: PositionDirection::Long,
        base_asset_amount: BASE_PRECISION_U64,
        trigger_condition,
        ..Order::default()
    }
}

fn five_dollar_user_holding(armed: &Order) -> User {
    let mut user = User {
        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 5 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        ..User::default()
    };

    user.orders[0] = *armed;
    user.reserve_orders(&OrderReservation::of_order(armed).unwrap())
        .unwrap();

    user
}

/// A 1 SOL stop-buy at $100 needs $10 of initial margin once it fires. The
/// account holds $5, so it meets initial margin only while the fired order is
/// left out of the measure.
#[test]
fn initial_margin_counts_the_fired_order() {
    let mut oracle_price = get_pyth_price(100, 6);
    let oracle_key = Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
    create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
    let oracle_map = OracleMap::load_one(&oracle_info, 0, SlotClock::baseline(), None).unwrap();

    let mut market = PerpMarket {
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
                last_oracle_price_twap: oracle_price.price,
                last_oracle_price_twap_5min: oracle_price.price,
                last_oracle_price: oracle_price.price,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };

    create_anchor_account_info!(market, PerpMarket, market_info);
    let market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut usdc = SpotMarket {
        market_index: 0,
        oracle_source: OracleSource::QuoteAsset,
        deposit_balance: 5 * SPOT_BALANCE_PRECISION,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        decimals: 6,
        initial_asset_weight: SPOT_WEIGHT_PRECISION,
        maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
        ..SpotMarket::default()
    };

    create_anchor_account_info!(usdc, SpotMarket, usdc_info);
    let spot_market_map = SpotMarketMap::load_one(&usdc_info, true).unwrap();
    let mut maps = AccountMaps::new(market_map, spot_market_map, oracle_map);

    let armed = stop_buy(OrderTriggerCondition::Above);
    let fired = stop_buy(OrderTriggerCondition::TriggeredAbove);
    let mut user = five_dollar_user_holding(&armed);

    create_anchor_account_info!(UserStats::default(), UserStats, user_stats_info);
    let user_stats = AccountLoader::<UserStats>::try_from(&user_stats_info).unwrap();

    let must_cancel = fired_order_must_cancel(
        &mut user,
        &armed,
        &fired,
        oracle_price.price,
        &user_stats,
        &mut maps,
    )
    .unwrap();

    assert!(must_cancel);
    assert!(!trigger_must_cancel(&user, &user_stats, &mut maps).unwrap());

    let position = user.get_perp_position(0).unwrap();
    assert_eq!(position.open_bids, 0);
    assert_eq!(position.open_orders, 1);
}
