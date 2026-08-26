use {
    crate::{
        create_account_info,
        error::ErrorCode,
        math::{
            constants::{AMM_RESERVE_PRECISION, PRICE_PRECISION_I64, PRICE_PRECISION_U64},
            time::SlotClock,
        },
        state::{
            oracle::{
                get_oracle_price, HistoricalOracleData, OraclePriceData, OracleSource,
                PYTH_PUSH_ACCOUNT_TYPE_PRICE, PYTH_PUSH_MAGIC, PYTH_PUSH_VERSION,
            },
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
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
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
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    assert_eq!(mm_oracle_price_data.get_price(), oracle_price_data.price);
    assert_eq!(mm_oracle_price_data.get_delay(), oracle_price_data.delay,);

    // Update oracle price data to have no sequence id, fall back to using slot comparison
    oracle_price_data.sequence_id = None;

    // With no sequence id and delayed mm oracle slot, should fall back to using oracle price data
    market.market_stats.mm_oracle_slot = slot - 5;
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();
    assert_eq!(mm_oracle_price_data.get_price(), oracle_price_data.price);
    assert_eq!(mm_oracle_price_data.get_delay(), oracle_price_data.delay,);

    // With no sequence id and up to date mm oracle slot, should use mm oracle
    market.market_stats.mm_oracle_slot = slot;
    let mm_oracle_price_data = market
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
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
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
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
        .get_mm_oracle_price_data(
            oracle_price_data,
            slot,
            &state.oracle_guard_rails.validity,
            SlotClock::baseline(),
        )
        .unwrap();

    let expected_confidence = oracle_price_data.confidence
        + (mm_oracle_price_data._get_mm_oracle_price()
            - mm_oracle_price_data.get_exchange_oracle_price_data().price)
            .unsigned_abs();

    let confidence = mm_oracle_price_data.get_confidence();
    assert_eq!(confidence, expected_confidence);
}

/// A pyth-owned account is not a price feed until its header says so. The
/// pyth program owns mapping accounts and product accounts as well as price
/// accounts, and it creates an account for anybody who asks. Read as a price
/// account, such an account gives the caller the price and the exponent.
#[test]
fn pyth_account_without_a_price_header_is_rejected() {
    let mut oracle_price = get_hardcoded_pyth_price(8394, 10);
    oracle_price.magic = 0;
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
        get_oracle_price(&OracleSource::Pyth, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );

    let amm = AMM { ..AMM::default() };
    assert_eq!(
        amm.get_oracle_twap(&oracle_account_info, 0, OracleSource::Pyth),
        Err(ErrorCode::InvalidOracle)
    );
}

/// The pyth program writes version 2 price accounts. A later layout under the
/// same magic would move every field this code reads.
#[test]
fn pyth_account_with_the_wrong_version_is_rejected() {
    let mut oracle_price = get_hardcoded_pyth_price(8394, 10);
    oracle_price.ver = 3;
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
        get_oracle_price(&OracleSource::Pyth, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );
}

/// A mapping account and a product account carry the pyth magic and version,
/// so only the account type separates them from a price account.
#[test]
fn pyth_account_of_another_type_is_rejected() {
    let mut oracle_price = get_hardcoded_pyth_price(8394, 10);
    oracle_price.atype = 2; // AccountType::Product
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
        get_oracle_price(&OracleSource::Pyth, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );
}

/// An account shorter than a pyth price account must produce an error. The
/// cast that reads the price panics on a short slice.
#[test]
fn pyth_account_shorter_than_a_price_account_is_rejected() {
    let oracle_price_key =
        Pubkey::from_str("8ihFLu5FimgTQ1Unh4dVyEHUGodJ5gJQCrQf4KUVB9bN").unwrap();
    let pyth_program = crate::ids::pyth_program::id();
    let mut lamports = 0;
    let mut data = [0u8; 32];
    let oracle_account_info = crate::test_utils::create_account_info(
        &oracle_price_key,
        true,
        &mut lamports,
        &mut data,
        &pyth_program,
    );

    assert_eq!(
        get_oracle_price(&OracleSource::Pyth, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::UnableToLoadOracle
    );
}

/// A slice that is long enough but not aligned must produce an error. The
/// cast reads the first aligned `Price` in the slice, so an unaligned slice
/// would put the price at a different offset than the header.
#[test]
fn unaligned_pyth_account_data_is_rejected() {
    let mut oracle_price = get_hardcoded_pyth_price(8394, 10);
    let aligned = get_account_bytes(&mut oracle_price);

    // One byte of padding moves the price account off an eight-byte boundary.
    let mut unaligned = vec![0u8];
    unaligned.extend_from_slice(&aligned);
    assert_eq!(unaligned[1..].as_ptr().align_offset(8), 7);

    let oracle_price_key =
        Pubkey::from_str("8ihFLu5FimgTQ1Unh4dVyEHUGodJ5gJQCrQf4KUVB9bN").unwrap();
    let pyth_program = crate::ids::pyth_program::id();
    let mut lamports = 0;
    let oracle_account_info = crate::test_utils::create_account_info(
        &oracle_price_key,
        true,
        &mut lamports,
        &mut unaligned[1..],
        &pyth_program,
    );

    assert_eq!(
        get_oracle_price(&OracleSource::Pyth, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::UnableToLoadOracle
    );
}

/// `packages/sdk/src/oracles/pythClient.ts` repeats these four values as
/// literals, because it cannot read them across languages. A change here must
/// fail rather than let the program and the SDK disagree about which accounts
/// are price accounts. Rust fixtures import the constants instead.
#[test]
fn the_pyth_header_values_the_sdk_repeats() {
    assert_eq!(std::mem::size_of::<pyth_client::Price>(), 3312);
    assert_eq!(std::mem::align_of::<pyth_client::Price>(), 8);
    assert_eq!(PYTH_PUSH_MAGIC, 0xa1b2_c3d4);
    assert_eq!(PYTH_PUSH_VERSION, 2);
    assert_eq!(PYTH_PUSH_ACCOUNT_TYPE_PRICE, 3);
}

/// A pyth mapping account carries the price account's magic and version, and
/// it is larger than a price account. So magic and length alone do not prove
/// an account is a price feed: only the account type separates the two, and
/// the price offsets of a mapping account fall inside its `products` array.
#[test]
fn a_mapping_account_passes_the_magic_and_the_length() {
    let mut mapping = vec![0u8; std::mem::size_of::<pyth_client::Mapping>()];
    mapping[0..4].copy_from_slice(&PYTH_PUSH_MAGIC.to_le_bytes());
    mapping[4..8].copy_from_slice(&PYTH_PUSH_VERSION.to_le_bytes());
    mapping[8..12].copy_from_slice(&1u32.to_le_bytes()); // AccountType::Mapping

    // The two checks a magic-only rule would keep both pass.
    assert!(mapping.len() >= std::mem::size_of::<pyth_client::Price>());
    assert_eq!(mapping.as_ptr().align_offset(8), 0);

    let oracle_price_key =
        Pubkey::from_str("8ihFLu5FimgTQ1Unh4dVyEHUGodJ5gJQCrQf4KUVB9bN").unwrap();
    let pyth_program = crate::ids::pyth_program::id();
    let mut lamports = 0;
    let oracle_account_info = crate::test_utils::create_account_info(
        &oracle_price_key,
        true,
        &mut lamports,
        &mut mapping,
        &pyth_program,
    );

    assert_eq!(
        get_oracle_price(&OracleSource::Pyth, &oracle_account_info, 0).unwrap_err(),
        ErrorCode::InvalidOracle
    );
}
