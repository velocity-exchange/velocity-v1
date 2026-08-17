use {
    crate::{
        create_account_info,
        error::ErrorCode,
        math::constants::{AMM_RESERVE_PRECISION, PRICE_PRECISION_I64, PRICE_PRECISION_U64},
        state::{
            oracle::{get_oracle_price, HistoricalOracleData, OraclePriceData, OracleSource},
            perp_market::{MarketStats, PerpMarket, AMM},
            state::State,
        },
        test_utils::*,
    },
    solana_program::pubkey::Pubkey,
    std::str::FromStr,
};

#[test]
fn pyth_1k() {
    let mut oracle_price = get_hardcoded_pyth_price(8394, 10);
    let oracle_price_key =
        Pubkey::from_str("8ihFLu5FimgTQ1Unh4dVyEHUGodJ5gJQCrQf4KUVB9bN").unwrap();
    let pyth_program = crate::ids::pyth_program::id();
    create_account_info!(
        oracle_price,
        &oracle_price_key,
        &pyth_program,
        oracle_account_info
    );

    let oracle_price_data =
        get_oracle_price(&OracleSource::Pyth1K, &oracle_account_info, 0).unwrap();
    assert_eq!(oracle_price_data.price, 839);

    let amm = AMM { ..AMM::default() };
    let twap = amm
        .get_oracle_twap(&oracle_account_info, 0, OracleSource::Pyth1K)
        .unwrap();
    assert_eq!(twap, Some(839));
}

#[test]
fn pyth_1m() {
    let mut oracle_price = get_hardcoded_pyth_price(8394, 10);
    let oracle_price_key =
        Pubkey::from_str("8ihFLu5FimgTQ1Unh4dVyEHUGodJ5gJQCrQf4KUVB9bN").unwrap();
    let pyth_program = crate::ids::pyth_program::id();
    create_account_info!(
        oracle_price,
        &oracle_price_key,
        &pyth_program,
        oracle_account_info
    );

    let oracle_price_data =
        get_oracle_price(&OracleSource::Pyth1M, &oracle_account_info, 0).unwrap();
    assert_eq!(oracle_price_data.price, 839400);

    let amm = AMM { ..AMM::default() };
    let twap = amm
        .get_oracle_twap(&oracle_account_info, 0, OracleSource::Pyth1M)
        .unwrap();
    assert_eq!(twap, Some(839400));
}

#[test]
fn pyth_pull_oracles_are_rejected() {
    let mut oracle_price = get_hardcoded_pyth_price(8394, 10);
    let oracle_price_key =
        Pubkey::from_str("8ihFLu5FimgTQ1Unh4dVyEHUGodJ5gJQCrQf4KUVB9bN").unwrap();
    let pyth_program = crate::ids::pyth_program::id();
    create_account_info!(
        oracle_price,
        &oracle_price_key,
        &pyth_program,
        oracle_account_info
    );

    assert_eq!(
        get_oracle_price(&OracleSource::PythPull, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );
    assert_eq!(
        get_oracle_price(&OracleSource::Pyth1KPull, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );
    assert_eq!(
        get_oracle_price(&OracleSource::Pyth1MPull, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );
    assert_eq!(
        get_oracle_price(&OracleSource::PythStableCoinPull, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );

    let amm = AMM { ..AMM::default() };

    assert_eq!(
        amm.get_oracle_twap(&oracle_account_info, 0, OracleSource::PythPull),
        Err(ErrorCode::InvalidOracle)
    );
}

#[test]
fn removed_oracle_source_slots_return_none() {
    assert_eq!(OracleSource::from_u8(1), None);
    assert_eq!(OracleSource::from_u8(7), None);
    assert_eq!(OracleSource::from_u8(8), None);
    assert_eq!(OracleSource::from_u8(9), None);
    assert_eq!(OracleSource::from_u8(10), None);
    assert_eq!(OracleSource::from_u8(11), None);
    assert_eq!(OracleSource::from_u8(2), Some(OracleSource::QuoteAsset));
    assert_eq!(OracleSource::from_u8(12), Some(OracleSource::PythLazer));
}

#[test]
fn use_mm_oracle() {
    let slot = 303030303;
    let mut oracle_price_data = OraclePriceData {
        price: 130 * PRICE_PRECISION_I64 + 873,
        confidence: PRICE_PRECISION_U64 / 10,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: Some(1756262481),
    };
    let mut market = PerpMarket {
        market_index: 0,
        amm: AMM {
            base_asset_reserve: 512295081967,
            quote_asset_reserve: 488 * AMM_RESERVE_PRECISION,
            sqrt_k: 500 * AMM_RESERVE_PRECISION,
            peg_multiplier: 22_100_000_000,
            base_asset_amount_with_amm: (12295081967_i128),
            max_spread: 1000,
            // assume someone else has other half same entry,
            ..AMM::default()
        },
        margin_ratio_initial: 1000,
        margin_ratio_maintenance: 500,
        imf_factor: 1000, // 1_000/1_000_000 = .001
        unrealized_pnl_initial_asset_weight: 100,
        unrealized_pnl_maintenance_asset_weight: 100,
        market_stats: MarketStats {
            mm_oracle_price: 130 * PRICE_PRECISION_I64 + 973,
            mm_oracle_slot: slot,
            mm_oracle_sequence_id: 1756262481,
            historical_oracle_data: HistoricalOracleData::default_with_current_oracle(
                oracle_price_data,
                0,
            ),
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };
    let state = State::default();

    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(oracle_price_data, slot, &state.oracle_guard_rails.validity)
        .unwrap();

    // Use the MM oracle when it's recent and it's valid to use
    assert_eq!(
        mm_oracle_price_data.get_price(),
        mm_oracle_price_data.mm_oracle_price
    );
    assert_eq!(
        mm_oracle_price_data.get_delay(),
        mm_oracle_price_data.mm_oracle_delay
    );

    // Update the MM oracle slot to be equal but the sequence number to be behind, should use exchange oracle
    market.market_stats.mm_oracle_sequence_id = 1756262481 - 10;
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(oracle_price_data, slot, &state.oracle_guard_rails.validity)
        .unwrap();
    assert_eq!(mm_oracle_price_data.get_price(), oracle_price_data.price);
    assert_eq!(mm_oracle_price_data.get_delay(), oracle_price_data.delay,);

    // Update oracle price data to have no sequence id, fall back to using slot comparison
    oracle_price_data.sequence_id = None;

    // With no sequence id and delayed mm oracle slot, should fall back to using oracle price data
    market.market_stats.mm_oracle_slot = slot - 5;
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(oracle_price_data, slot, &state.oracle_guard_rails.validity)
        .unwrap();
    assert_eq!(mm_oracle_price_data.get_price(), oracle_price_data.price);
    assert_eq!(mm_oracle_price_data.get_delay(), oracle_price_data.delay,);

    // With no sequence id and up to date mm oracle slot, should use mm oracle
    market.market_stats.mm_oracle_slot = slot;
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(oracle_price_data, slot, &state.oracle_guard_rails.validity)
        .unwrap();
    assert_eq!(
        mm_oracle_price_data.get_price(),
        mm_oracle_price_data.mm_oracle_price
    );
    assert_eq!(
        mm_oracle_price_data.get_delay(),
        mm_oracle_price_data.mm_oracle_delay
    );

    // With really off sequence id and up to date mm oracle slot, should fall back to slot comparison
    market.market_stats.mm_oracle_sequence_id = 1756262481000; // wrong resolution
    market.market_stats.mm_oracle_slot = slot - 5;
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(oracle_price_data, slot, &state.oracle_guard_rails.validity)
        .unwrap();
    assert_eq!(mm_oracle_price_data.get_price(), oracle_price_data.price);
    assert_eq!(mm_oracle_price_data.get_delay(), oracle_price_data.delay);
}

#[test]
fn mm_oracle_confidence() {
    let slot = 303030303;
    let oracle_price_data = OraclePriceData {
        price: 130 * PRICE_PRECISION_I64 + 873,
        confidence: PRICE_PRECISION_U64 / 10,
        delay: 1,
        has_sufficient_number_of_data_points: true,
        sequence_id: Some(0),
    };
    let market = PerpMarket {
        market_index: 0,
        amm: AMM {
            base_asset_reserve: 512295081967,
            quote_asset_reserve: 488 * AMM_RESERVE_PRECISION,
            sqrt_k: 500 * AMM_RESERVE_PRECISION,
            peg_multiplier: 22_100_000_000,
            base_asset_amount_with_amm: (12295081967_i128),
            max_spread: 1000,
            // assume someone else has other half same entry,
            ..AMM::default()
        },
        margin_ratio_initial: 1000,
        margin_ratio_maintenance: 500,
        imf_factor: 1000, // 1_000/1_000_000 = .001
        unrealized_pnl_initial_asset_weight: 100,
        unrealized_pnl_maintenance_asset_weight: 100,
        market_stats: MarketStats {
            mm_oracle_price: 130 * PRICE_PRECISION_I64 + 999,
            mm_oracle_slot: slot,
            mm_oracle_sequence_id: 1,
            historical_oracle_data: HistoricalOracleData::default_with_current_oracle(
                oracle_price_data,
                0,
            ),
            ..MarketStats::default()
        },
        ..PerpMarket::default()
    };
    let state = State::default();

    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(oracle_price_data, slot, &state.oracle_guard_rails.validity)
        .unwrap();

    let expected_confidence = oracle_price_data.confidence
        + (mm_oracle_price_data._get_mm_oracle_price()
            - mm_oracle_price_data.get_exchange_oracle_price_data().price)
            .unsigned_abs();

    let confidence = mm_oracle_price_data.get_confidence();
    assert_eq!(confidence, expected_confidence);
}

mod settled_oracle_twaps {
    use crate::{
        math::constants::{PRICE_PRECISION_I64, SETTLED_ORACLE_TWAP_INTERVAL},
        state::oracle::{HistoricalOracleData, SettledOracleTwaps},
    };

    const NOW: i64 = 1_700_000_000;

    fn live(price: i64) -> HistoricalOracleData {
        HistoricalOracleData::default_price(price)
    }

    #[test]
    fn unseeded_reads_the_live_twaps() {
        let settled = SettledOracleTwaps::default();
        let live = live(100 * PRICE_PRECISION_I64);

        assert_eq!(
            settled.oracle_price_twap_5min(&live),
            100 * PRICE_PRECISION_I64
        );
        assert_eq!(settled.oracle_price_twap(&live), 100 * PRICE_PRECISION_I64);
    }

    #[test]
    fn a_zero_anchor_reads_the_live_twap() {
        // A market can be seeded while its TWAPs are still zero. Zero is not a
        // price, and every gate that reads the anchor divides by it.
        let settled = SettledOracleTwaps {
            ts: NOW,
            ..SettledOracleTwaps::default()
        };
        let live = live(100 * PRICE_PRECISION_I64);

        assert_eq!(
            settled.oracle_price_twap_5min(&live),
            100 * PRICE_PRECISION_I64
        );
    }

    #[test]
    fn the_first_roll_seeds_from_the_live_twaps() {
        let mut settled = SettledOracleTwaps::default();
        let live = live(100 * PRICE_PRECISION_I64);

        settled.roll(&live, NOW).unwrap();

        assert_eq!(
            settled.last_oracle_price_twap_5min,
            100 * PRICE_PRECISION_I64
        );
        assert_eq!(settled.ts, NOW);
    }

    #[test]
    fn a_roll_inside_the_interval_does_nothing() {
        let mut settled = SettledOracleTwaps::seeded(100 * PRICE_PRECISION_I64, NOW);

        settled
            .roll(
                &live(50 * PRICE_PRECISION_I64),
                NOW + SETTLED_ORACLE_TWAP_INTERVAL - 1,
            )
            .unwrap();

        assert_eq!(
            settled.last_oracle_price_twap_5min,
            100 * PRICE_PRECISION_I64,
            "an anchor that moves inside the interval is reachable from one \
             transaction, which is the defect this exists to close"
        );
        assert_eq!(settled.ts, NOW);
    }

    #[test]
    fn a_roll_is_clamped_to_one_step() {
        let mut settled = SettledOracleTwaps::seeded(100 * PRICE_PRECISION_I64, NOW);

        settled
            .roll(
                &live(50 * PRICE_PRECISION_I64),
                NOW + SETTLED_ORACLE_TWAP_INTERVAL,
            )
            .unwrap();

        // 10% of 100, not the whole 50% drop.
        assert_eq!(
            settled.last_oracle_price_twap_5min,
            90 * PRICE_PRECISION_I64
        );
        assert_eq!(settled.last_oracle_price_twap, 90 * PRICE_PRECISION_I64);
        assert_eq!(settled.ts, NOW + SETTLED_ORACLE_TWAP_INTERVAL);
    }

    #[test]
    fn a_move_within_one_step_lands_whole() {
        let mut settled = SettledOracleTwaps::seeded(100 * PRICE_PRECISION_I64, NOW);

        settled
            .roll(
                &live(95 * PRICE_PRECISION_I64),
                NOW + SETTLED_ORACLE_TWAP_INTERVAL,
            )
            .unwrap();

        assert_eq!(
            settled.last_oracle_price_twap_5min,
            95 * PRICE_PRECISION_I64
        );
    }

    #[test]
    fn a_sustained_move_reaches_the_anchor_over_several_intervals() {
        let mut settled = SettledOracleTwaps::seeded(100 * PRICE_PRECISION_I64, NOW);
        let live = live(50 * PRICE_PRECISION_I64);

        let mut now = NOW;
        for _ in 0..8 {
            now += SETTLED_ORACLE_TWAP_INTERVAL;
            settled.roll(&live, now).unwrap();
        }

        // 100 * 0.9^8 is about 43, so eight intervals of the manipulated price
        // walk the anchor past 50. Fewer do not.
        assert!(settled.last_oracle_price_twap_5min <= 50 * PRICE_PRECISION_I64);
    }
}
