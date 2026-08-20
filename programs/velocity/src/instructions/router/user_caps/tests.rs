//! What a maker's room comes out as, per reason it might be constrained.
//!
//! The verdicts, not the plumbing: which states answer zero, which answer
//! unconstrained, and that what a maker already has working on the DLOB comes
//! out of what a book is offered.

use {
    super::*,
    crate::{
        create_anchor_account_info,
        math::constants::{
            AMM_RESERVE_PRECISION, BASE_PRECISION_I64, PEG_PRECISION, PRICE_PRECISION,
            QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
            SPOT_WEIGHT_PRECISION,
        },
        state::{
            oracle::{HistoricalOracleData, OracleSource},
            perp_market::{MarketStats, PerpMarket, AMM},
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            user::{PerpPosition, SpotPosition, User, UserStats},
        },
        test_utils::{get_positions, get_pyth_price, get_spot_positions},
    },
    anchor_lang::prelude::Pubkey,
    std::str::FromStr,
};

const AUTHORITY: &str = "J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix";
const FLOOR: u64 = 100 * QUOTE_PRECISION_I64 as u64;

/// Reading the oracle far past the slot it was posted at is what makes a
/// floored maker unverifiable; reading it at its own slot leaves the floor
/// readable.
fn room(floor: u64, latched: bool, position_base: i64, open_bids: i64, stale_oracle: bool) -> u64 {
    let slot = if stale_oracle { 100_000 } else { 1 };

    let mut oracle_price = get_pyth_price(100, 6);
    let oracle_key = Pubkey::from_str(AUTHORITY).unwrap();
    create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
    let mut oracle_map =
        crate::state::oracle_map::OracleMap::load_one(&oracle_info, slot, None).unwrap();

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
        status: crate::state::market_status::MarketStatus::Active,
        order_step_size: 1000,
        order_tick_size: 1,
        oracle: oracle_key,
        oracle_source: OracleSource::PythLazer,
        market_stats: MarketStats {
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: (100 * PRICE_PRECISION) as i64,
                last_oracle_price_twap: (100 * PRICE_PRECISION) as i64,
                last_oracle_price_twap_5min: (100 * PRICE_PRECISION) as i64,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        ..PerpMarket::default_test()
    };
    market.amm.max_base_asset_reserve = u128::MAX;
    market.amm.min_base_asset_reserve = 0;
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map =
        crate::state::perp_market_map::PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = SpotMarket {
        market_index: 0,
        oracle_source: OracleSource::QuoteAsset,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        decimals: 6,
        initial_asset_weight: SPOT_WEIGHT_PRECISION,
        maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
        historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
        ..SpotMarket::default()
    };
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map =
        crate::state::spot_market_map::SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let authority = Pubkey::from_str(AUTHORITY).unwrap();
    let mut maker = User {
        authority,
        equity_floor: floor,
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: position_base,
            open_bids,
            ..PerpPosition::default()
        }),
        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1_000 * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        ..User::default()
    };
    let maker_key = Pubkey::new_unique();
    create_anchor_account_info!(maker, &maker_key, User, maker_info);
    let makers = crate::state::user_map::UserMap::load_one(&maker_info).unwrap();

    let mut stats = UserStats {
        authority,
        ..UserStats::default()
    };
    stats.set_equity_breaker_tripped(latched);
    create_anchor_account_info!(stats, UserStats, stats_info);
    let stats_map = crate::state::user_map::UserStatsMap::load_one(&stats_info).unwrap();

    maker_room(
        &mut CapInputs {
            makers_and_referrer: &makers,
            makers_and_referrer_stats: &stats_map,
            perp_market_map: &perp_market_map,
            spot_market_map: &spot_market_map,
            oracle_map: &mut oracle_map,
            slot,
            now: 0,
        },
        &maker_key,
        0,
        PositionDirection::Long,
    )
    .unwrap()
}

#[test]
fn a_solvent_maker_is_left_alone() {
    // The control for every case below: the same account, nothing wrong with
    // it, keeps its room.
    assert_eq!(
        room(0, false, BASE_PRECISION_I64, 0, false),
        u64::MAX,
        "a solvent maker is not constrained at all"
    );
}

#[test]
fn a_latched_authority_has_no_room() {
    // The breaker bars every subaccount from risk-increasing activity, so
    // there is nothing to price — and no margin walk is spent finding out.
    assert_eq!(room(0, true, BASE_PRECISION_I64, 0, false), 0);
}

#[test]
fn a_floor_that_cannot_be_verified_has_no_room() {
    // Not a judgement about the account: the program cannot read the price
    // that would settle the question, and a fill it cannot evaluate is one it
    // refuses.
    assert_eq!(room(FLOOR, false, BASE_PRECISION_I64, 0, true), 0);
}

#[test]
fn what_is_already_reserved_is_not_charged_twice() {
    // A resting order was priced at worst case when it was placed, so what a
    // maker already has working does not eat into what its resting orders may
    // fill. Subtracting it would deny a maker liquidity it is already backing
    // — the mistake a size-shaped answer invites and a verdict-shaped one
    // cannot make.
    let clear = room(0, false, BASE_PRECISION_I64, 0, false);
    let working = room(0, false, BASE_PRECISION_I64, BASE_PRECISION_I64, false);
    assert_eq!(clear, u64::MAX, "a solvent maker is unconstrained");
    assert_eq!(
        working, clear,
        "open orders the walk already prices do not narrow the verdict"
    );
}
