use {
    super::{cancel_book_orders, BookCancel, BookCancelScope, BookOrderSweep},
    crate::{
        controller::{liquidation::liquidate_perp, position::PositionDirection},
        create_anchor_account_info,
        error::{ErrorCode, VelocityResult},
        instructions::optional_accounts::AccountMaps,
        math::{
            constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BASE_PRECISION_I64, BASE_PRECISION_U64,
                LIQUIDATION_FEE_PRECISION, LIQUIDATION_PCT_PRECISION, PEG_PRECISION,
                PRICE_PRECISION_I64, QUOTE_PRECISION_I128, QUOTE_PRECISION_I64,
                SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                SPOT_WEIGHT_PRECISION,
            },
            time::{legacy_slot_duration_u8, SlotClock},
        },
        state::{
            market_status::MarketStatus,
            oracle::{HistoricalOracleData, OracleSource},
            oracle_map::OracleMap,
            perp_market::{MarketStats, PerpMarket, AMM},
            perp_market_map::PerpMarketMap,
            prop_amm::{CancelAllOutcomeV0, UserRefV0},
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            state::State,
            user::{
                MarketType, Order, OrderBitFlag, OrderStatus, PerpPosition, PositionFlag,
                SpotPosition, User, UserStats,
            },
        },
        test_utils::{get_positions, get_pyth_price, get_spot_positions},
    },
    anchor_lang::prelude::Pubkey,
    std::{collections::BTreeMap, str::FromStr},
};

/// Books that answer from a fixed table, and record every market asked.
#[derive(Default)]
struct FakeBooks {
    outcomes: BTreeMap<u16, CancelAllOutcomeV0>,
    asked: Vec<u16>,
}

impl FakeBooks {
    fn with(market_index: u16, outcome: CancelAllOutcomeV0) -> Self {
        Self {
            outcomes: BTreeMap::from([(market_index, outcome)]),
            asked: Vec::new(),
        }
    }
}

impl BookOrderSweep for FakeBooks {
    fn cancel_all(
        &mut self,
        market_index: u16,
        user: UserRefV0,
    ) -> VelocityResult<Option<CancelAllOutcomeV0>> {
        self.asked.push(market_index);
        Ok(self
            .outcomes
            .get(&market_index)
            .map(|outcome| CancelAllOutcomeV0 { user, ..*outcome }))
    }
}

const MARKET: u16 = 3;

/// A long that rests a bid and a reduce-only ask on the book. A fired trigger
/// order shadows the bid.
fn user_with_book_orders() -> User {
    let mut user = User {
        perp_positions: get_positions(PerpPosition {
            market_index: MARKET,
            base_asset_amount: BASE_PRECISION_I64,
            open_orders: 2,
            open_bids: 2 * BASE_PRECISION_I64,
            open_asks: -BASE_PRECISION_I64,
            reduce_only_clob_orders: 1,
            ..PerpPosition::default()
        }),
        open_orders: 2,
        has_open_order: true,
        ..User::default()
    };

    user.orders[0] = Order {
        status: OrderStatus::Open,
        market_type: MarketType::Perp,
        market_index: MARKET,
        direction: PositionDirection::Long,
        ..Order::default()
    };
    user.orders[0].add_bit_flag(OrderBitFlag::PlacedOnClob);
    user
}

fn both_sides_swept(exhaustive: bool) -> CancelAllOutcomeV0 {
    CancelAllOutcomeV0 {
        bid_base_asset_amount: 2 * BASE_PRECISION_U64,
        ask_base_asset_amount: BASE_PRECISION_U64,
        bid_orders: 1,
        ask_orders: 1,
        ask_reduce_only_orders: 1,
        exhaustive,
        ..CancelAllOutcomeV0::default()
    }
}

#[test]
fn a_sweep_releases_the_whole_reservation() {
    let mut user = user_with_book_orders();
    let mut books = FakeBooks::with(MARKET, both_sides_swept(true));

    let cancel = cancel_book_orders(&mut user, BookCancelScope::Cross, &mut books).unwrap();

    assert_eq!(
        cancel,
        BookCancel {
            orders: 2,
            orders_remain: false
        }
    );

    let position = &user.perp_positions[0];
    assert_eq!(position.open_bids, 0);
    assert_eq!(position.open_asks, 0);
    assert_eq!(position.open_orders, 0);
    assert_eq!(position.reduce_only_clob_orders, 0);
    assert_eq!(user.open_orders, 0);
    assert_eq!(user.orders[0].status, OrderStatus::Canceled);
    assert_eq!(user.clob_resident_open_orders(MARKET), 0);
}

#[test]
fn a_capped_sweep_leaves_the_shadow_and_reports_orders_remain() {
    let mut user = user_with_book_orders();
    let mut books = FakeBooks::with(MARKET, both_sides_swept(false));

    let cancel = cancel_book_orders(&mut user, BookCancelScope::Cross, &mut books).unwrap();

    assert!(cancel.orders_remain);
    assert_eq!(user.orders[0].status, OrderStatus::Open);
}

#[test]
fn a_missing_book_reports_orders_remain() {
    let mut user = user_with_book_orders();
    let mut books = FakeBooks::default();

    let cancel = cancel_book_orders(&mut user, BookCancelScope::Cross, &mut books).unwrap();

    assert_eq!(books.asked, vec![MARKET]);
    assert!(cancel.orders_remain);
    assert_eq!(user.perp_positions[0].open_orders, 2);
}

#[test]
fn a_market_out_of_scope_is_not_swept() {
    let mut user = user_with_book_orders();
    user.perp_positions[0].position_flag = PositionFlag::IsolatedPosition as u8;
    let mut books = FakeBooks::with(MARKET, both_sides_swept(true));

    let cancel = cancel_book_orders(&mut user, BookCancelScope::Cross, &mut books).unwrap();

    assert!(books.asked.is_empty());
    assert_eq!(cancel, BookCancel::default());
    assert_eq!(user.perp_positions[0].open_orders, 2);

    let cancel =
        cancel_book_orders(&mut user, BookCancelScope::Isolated(MARKET), &mut books).unwrap();

    assert_eq!(cancel.orders, 2);
}

#[test]
fn a_sweep_for_another_user_is_refused() {
    struct WrongUser;
    impl BookOrderSweep for WrongUser {
        fn cancel_all(
            &mut self,
            _market_index: u16,
            _user: UserRefV0,
        ) -> VelocityResult<Option<CancelAllOutcomeV0>> {
            Ok(Some(CancelAllOutcomeV0 {
                user: UserRefV0 {
                    authority: Pubkey::new_unique(),
                    sub_account_id: 0,
                },
                ..both_sides_swept(true)
            }))
        }
    }

    let mut user = user_with_book_orders();

    assert_eq!(
        cancel_book_orders(&mut user, BookCancelScope::Cross, &mut WrongUser),
        Err(ErrorCode::InvalidUserAccount)
    );
}

/// An underwater long that rests a reduce-only ask on the book. A keeper
/// force-cancel keeps that order, because it reduces risk. The liquidation
/// sweeps it and liquidates.
#[test]
fn a_reducing_book_order_no_longer_blocks_liquidation() {
    let now = 0_i64;
    let slot = 0_u64;
    let mut oracle_price = get_pyth_price(100, 6);
    let oracle_price_key =
        Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
    create_anchor_account_info!(
        oracle_price,
        &oracle_price_key,
        PythLazerOracle,
        oracle_account_info
    );
    let oracle_map =
        OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();
    let mut market = test_perp_market(0, oracle_price_key, oracle_price.price);
    create_anchor_account_info!(market, PerpMarket, market_account_info);
    let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
    let mut spot_market = test_quote_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
    let mut maps = AccountMaps::new(perp_market_map, spot_market_map, oracle_map);

    let mut user = User {
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -150 * QUOTE_PRECISION_I64,
            quote_entry_amount: -150 * QUOTE_PRECISION_I64,
            quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
            open_orders: 1,
            open_asks: -BASE_PRECISION_I64,
            reduce_only_clob_orders: 1,
            ..PerpPosition::default()
        }),
        open_orders: 1,
        has_open_order: true,
        ..User::default()
    };
    let mut liquidator = funded_liquidator();
    let mut books = FakeBooks::with(
        0,
        CancelAllOutcomeV0 {
            ask_base_asset_amount: BASE_PRECISION_U64,
            ask_orders: 1,
            ask_reduce_only_orders: 1,
            exhaustive: true,
            ..CancelAllOutcomeV0::default()
        },
    );

    liquidate_perp(
        0,
        BASE_PRECISION_U64,
        None,
        &mut user,
        &Pubkey::default(),
        &mut UserStats::default(),
        &mut liquidator,
        &Pubkey::default(),
        &mut UserStats::default(),
        &mut maps,
        slot,
        now,
        &test_state(),
        &mut books,
    )
    .unwrap();

    assert_eq!(books.asked, vec![0]);
    assert_eq!(user.perp_positions[0].base_asset_amount, 0);
    assert_eq!(user.perp_positions[0].open_asks, 0);
    assert_eq!(user.perp_positions[0].open_orders, 0);
    assert_eq!(user.perp_positions[0].reduce_only_clob_orders, 0);
    assert_eq!(
        liquidator.perp_positions[0].base_asset_amount,
        BASE_PRECISION_I64
    );
}

/// A capped sweep stops the liquidation after the cancel. The account stays
/// latched, and no position moves.
#[test]
fn orders_left_on_the_book_stop_the_liquidation_after_the_cancel() {
    let now = 0_i64;
    let slot = 0_u64;
    let mut oracle_price = get_pyth_price(100, 6);
    let oracle_price_key =
        Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
    create_anchor_account_info!(
        oracle_price,
        &oracle_price_key,
        PythLazerOracle,
        oracle_account_info
    );
    let oracle_map =
        OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();
    let mut market = test_perp_market(0, oracle_price_key, oracle_price.price);
    create_anchor_account_info!(market, PerpMarket, market_account_info);
    let perp_market_map = PerpMarketMap::load_one(&market_account_info, true).unwrap();
    let mut spot_market = test_quote_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
    let mut maps = AccountMaps::new(perp_market_map, spot_market_map, oracle_map);

    let mut user = User {
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: BASE_PRECISION_I64,
            quote_asset_amount: -150 * QUOTE_PRECISION_I64,
            quote_entry_amount: -150 * QUOTE_PRECISION_I64,
            quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
            open_orders: 2,
            open_bids: 2 * BASE_PRECISION_I64,
            ..PerpPosition::default()
        }),
        open_orders: 2,
        has_open_order: true,
        ..User::default()
    };
    let mut liquidator = funded_liquidator();
    let mut books = FakeBooks::with(
        0,
        CancelAllOutcomeV0 {
            bid_base_asset_amount: BASE_PRECISION_U64,
            bid_orders: 1,
            exhaustive: false,
            ..CancelAllOutcomeV0::default()
        },
    );

    liquidate_perp(
        0,
        BASE_PRECISION_U64,
        None,
        &mut user,
        &Pubkey::default(),
        &mut UserStats::default(),
        &mut liquidator,
        &Pubkey::default(),
        &mut UserStats::default(),
        &mut maps,
        slot,
        now,
        &test_state(),
        &mut books,
    )
    .unwrap();

    assert!(user.is_being_liquidated());
    assert_eq!(user.perp_positions[0].base_asset_amount, BASE_PRECISION_I64);
    assert_eq!(user.perp_positions[0].open_orders, 1);
    assert_eq!(user.perp_positions[0].open_bids, BASE_PRECISION_I64);
    assert_eq!(liquidator.perp_positions[0].base_asset_amount, 0);
}

/// An isolated liquidation of market 0 while a healthy cross position in
/// market 1 rests a book bid. The cross order is out of scope, so the
/// liquidation neither sweeps it nor waits for it.
#[test]
fn an_out_of_scope_book_order_no_longer_blocks_liquidation() {
    let now = 0_i64;
    let slot = 0_u64;
    let mut oracle_price = get_pyth_price(100, 6);
    let oracle_price_key =
        Pubkey::from_str("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix").unwrap();
    create_anchor_account_info!(
        oracle_price,
        &oracle_price_key,
        PythLazerOracle,
        oracle_account_info
    );
    let oracle_map =
        OracleMap::load_one(&oracle_account_info, slot, SlotClock::baseline(), None).unwrap();
    let mut market = test_perp_market(0, oracle_price_key, oracle_price.price);
    create_anchor_account_info!(market, PerpMarket, market_account_info);
    let mut other_market = test_perp_market(1, oracle_price_key, oracle_price.price);
    create_anchor_account_info!(other_market, PerpMarket, other_market_account_info);
    let perp_market_map =
        PerpMarketMap::load_multiple(vec![&market_account_info, &other_market_account_info], true)
            .unwrap();
    let mut spot_market = test_quote_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_account_info, true).unwrap();
    let mut maps = AccountMaps::new(perp_market_map, spot_market_map, oracle_map);

    let mut user = User {
        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 100 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        open_orders: 1,
        has_open_order: true,
        ..User::default()
    };
    user.perp_positions[0] = PerpPosition {
        market_index: 0,
        base_asset_amount: BASE_PRECISION_I64,
        quote_asset_amount: -150 * QUOTE_PRECISION_I64,
        quote_entry_amount: -150 * QUOTE_PRECISION_I64,
        quote_break_even_amount: -150 * QUOTE_PRECISION_I64,
        position_flag: PositionFlag::IsolatedPosition as u8,
        ..PerpPosition::default()
    };
    user.perp_positions[1] = PerpPosition {
        market_index: 1,
        open_orders: 1,
        open_bids: BASE_PRECISION_I64 / 10,
        ..PerpPosition::default()
    };
    let mut liquidator = funded_liquidator();
    let mut books = FakeBooks::default();

    liquidate_perp(
        0,
        BASE_PRECISION_U64,
        None,
        &mut user,
        &Pubkey::default(),
        &mut UserStats::default(),
        &mut liquidator,
        &Pubkey::default(),
        &mut UserStats::default(),
        &mut maps,
        slot,
        now,
        &test_state(),
        &mut books,
    )
    .unwrap();

    assert!(books.asked.is_empty());
    assert_eq!(user.perp_positions[0].base_asset_amount, 0);
    assert_eq!(user.perp_positions[1].open_orders, 1);
    assert_eq!(user.perp_positions[1].open_bids, BASE_PRECISION_I64 / 10);
}

fn test_perp_market(market_index: u16, oracle: Pubkey, oracle_price: i64) -> PerpMarket {
    PerpMarket {
        amm: AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            max_slippage_ratio: 50,
            max_fill_reserve_fraction: 100,
            base_asset_amount_with_amm: BASE_PRECISION_I128,
            ..AMM::default()
        },
        market_index,
        margin_ratio_initial: 1000,
        margin_ratio_maintenance: 500,
        number_of_users_with_base: 1,
        status: MarketStatus::Initialized,
        liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
        if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
        order_step_size: 10000000,
        quote_asset_amount: -150 * QUOTE_PRECISION_I128,
        oracle,
        oracle_source: OracleSource::PythLazer,
        market_stats: MarketStats {
            historical_oracle_data: HistoricalOracleData::default_price(oracle_price),
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    }
}

fn test_quote_market() -> SpotMarket {
    SpotMarket {
        market_index: 0,
        oracle_source: OracleSource::QuoteAsset,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        decimals: 6,
        initial_asset_weight: SPOT_WEIGHT_PRECISION,
        maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
        historical_oracle_data: HistoricalOracleData {
            last_oracle_price_twap: PRICE_PRECISION_I64,
            last_oracle_price_twap_5min: PRICE_PRECISION_I64,
            ..HistoricalOracleData::default()
        },
        ..SpotMarket::default()
    }
}

fn funded_liquidator() -> User {
    User {
        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 50 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        ..User::default()
    }
}

fn test_state() -> State {
    State {
        liquidation_margin_buffer_ratio: 10,
        initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
        liquidation_duration: legacy_slot_duration_u8(150),
        ..Default::default()
    }
}
