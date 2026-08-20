//! What a maker's room comes out as, per reason it might be constrained.
//!
//! Three groups: the states that answer zero without pricing anything, which
//! of the two budgets binds, and the test that decides whether a maker is
//! worth one of the eight slots at all.

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
            user::{Order, OrderStatus, OrderType, PerpPosition, SpotPosition, User, UserStats},
        },
        test_utils::{get_positions, get_pyth_price, get_spot_positions},
    },
    anchor_lang::prelude::Pubkey,
    std::str::FromStr,
};

const AUTHORITY: &str = "J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix";
const FLOOR: u64 = 100 * QUOTE_PRECISION_I64 as u64;
const ORACLE: i64 = 100 * PRICE_PRECISION as i64;
const QUOTE: u64 = QUOTE_PRECISION_I64 as u64;

/// One maker, and the fill being sized against it.
struct Case {
    /// Quote deposited, in whole dollars.
    deposit: u64,
    floor: u64,
    latched: bool,
    position_base: i64,
    /// Base the maker is resting on the bid side, which is the side the taker
    /// sweeps in every case here.
    open_bids: i64,
    stale_oracle: bool,
    /// Base of `open_bids` that belongs to an order resting on the DLOB
    /// rather than a book.
    dlob_bid: u64,
    taker_size: u64,
    books: u32,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            deposit: 1_000,
            floor: 0,
            latched: false,
            position_base: BASE_PRECISION_I64,
            open_bids: 0,
            stale_oracle: false,
            dlob_bid: 0,
            taker_size: BASE_PRECISION_I64 as u64,
            books: 1,
        }
    }
}

/// A maker holding 200 base of bids against a 1550 deposit: 20,000 of notional
/// at the 7.5% fill tier reserves 1,500, which leaves 50 of free collateral.
/// The taker wants all 200, so nothing about the fill's size lets this maker
/// off.
fn thin_maker() -> Case {
    Case {
        deposit: 1_550,
        position_base: 0,
        open_bids: 200 * BASE_PRECISION_I64,
        taker_size: 200 * BASE_PRECISION_I64 as u64,
        ..Case::default()
    }
}

/// Reading the oracle far past the slot it was posted at is what makes a
/// floored maker unverifiable; reading it at its own slot leaves the floor
/// readable.
fn budget(case: Case) -> u64 {
    let Case {
        deposit,
        floor,
        latched,
        position_base,
        open_bids,
        stale_oracle,
        dlob_bid,
        taker_size,
        books,
    } = case;
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
    let mut orders = [Order::default(); 32];
    if dlob_bid > 0 {
        orders[0] = Order {
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            market_index: 0,
            direction: PositionDirection::Long,
            base_asset_amount: dlob_bid,
            ..Order::default()
        };
    }
    let mut maker = User {
        orders,
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
            scaled_balance: deposit * SPOT_BALANCE_PRECISION_U64,
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

    maker_budget(
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
        ClobSide::Bid,
        taker_size,
        ORACLE,
        books,
    )
    .unwrap()
}

#[test]
fn a_maker_resting_nothing_cannot_be_hurt() {
    // The control: a solvent maker with no bids on the book gives up no base,
    // so no fill can cost it anything and it never takes a slot.
    assert_eq!(budget(Case::default()), u64::MAX);
}

#[test]
fn a_latched_authority_has_no_room() {
    // The breaker bars every subaccount from risk-increasing activity, so
    // there is nothing to price — and no margin walk is spent finding out.
    assert_eq!(
        budget(Case {
            latched: true,
            ..thin_maker()
        }),
        0
    );
}

#[test]
fn a_floor_that_cannot_be_verified_has_no_room() {
    // Not a judgement about the account: the program cannot read the price
    // that would settle the question, and a fill it cannot evaluate is one it
    // refuses.
    assert_eq!(
        budget(Case {
            floor: FLOOR,
            stale_oracle: true,
            ..thin_maker()
        }),
        0
    );
}

#[test]
fn free_collateral_is_the_budget_when_no_floor_is_set() {
    // 50 of free collateral, less the 10% haircut.
    assert_eq!(budget(thin_maker()), 45 * QUOTE);
}

#[test]
fn the_equity_floor_binds_when_it_is_the_tighter_budget() {
    // Net equity is the 1,550 deposit. A floor at 1,530 leaves 20 above it,
    // which is less than the 50 of free collateral, so the floor sets the
    // budget.
    assert_eq!(
        budget(Case {
            floor: 1_530 * QUOTE,
            ..thin_maker()
        }),
        18 * QUOTE
    );
}

#[test]
fn a_maker_on_two_books_is_offered_half_its_budget_each() {
    // Every book in the route is quoted against the same budget and they all
    // execute afterwards, so a maker named on two of them could spend the
    // full amount twice. Splitting keeps the total inside it.
    assert_eq!(
        budget(Case {
            books: 2,
            ..thin_maker()
        }),
        45 * QUOTE / 2
    );
}

#[test]
fn a_fill_too_small_to_reach_the_budget_costs_no_slot() {
    // The eight slots are scarce, so a maker this fill cannot reach should
    // not hold one. The taker wants 0.1 base; even sold at nothing that is 10
    // of loss, and the maker has 45 to spend. No slot.
    assert_eq!(
        budget(Case {
            taker_size: BASE_PRECISION_I64 as u64 / 10,
            ..thin_maker()
        }),
        u64::MAX
    );
}

#[test]
fn the_slot_test_reads_what_the_maker_rests_not_only_what_is_asked() {
    // The mirror of the case above: the taker wants plenty, but this maker is
    // only resting 0.1 base, so that is all it can give up and 45 covers it.
    assert_eq!(
        budget(Case {
            open_bids: BASE_PRECISION_I64 / 10,
            deposit: 1_550,
            position_base: 0,
            taker_size: 200 * BASE_PRECISION_I64 as u64,
            ..Case::default()
        }),
        u64::MAX
    );
}

#[test]
fn what_is_already_reserved_is_not_charged_twice() {
    // A resting order was priced at worst case when it was placed, so what a
    // maker already has working does not come out of its budget. The budget
    // is free collateral, and free collateral already has the reservation
    // subtracted — charging again would deny a maker liquidity it is backing.
    let with_more_working = budget(Case {
        open_bids: 400 * BASE_PRECISION_I64,
        deposit: 4_550,
        position_base: 0,
        taker_size: 400 * BASE_PRECISION_I64 as u64,
        ..Case::default()
    });
    // 40,000 of notional reserves 3,000 at the fill tier and leaves 1,550,
    // less the haircut. Twice the orders of `thin_maker`, and the budget
    // tracks the collateral behind them rather than the orders themselves.
    assert_eq!(with_more_working, 1_395 * QUOTE);
}

#[test]
fn a_dlob_order_is_not_mistaken_for_book_depth() {
    // `open_bids` reserves for every open order on the market, so the DLOB's
    // share has to come off before what is left reads as depth on a book. A
    // maker whose bids are all on the DLOB is unreachable by a book budget,
    // and answering that from the account is what keeps a margin walk off a
    // heap that cannot give the memory back.
    assert_eq!(
        budget(Case {
            dlob_bid: 200 * BASE_PRECISION_I64 as u64,
            ..thin_maker()
        }),
        u64::MAX,
        "every reserved bid is accounted for on the DLOB, so none is on a book"
    );
    // Half on each: the book's half is still worth pricing.
    assert_eq!(
        budget(Case {
            dlob_bid: 100 * BASE_PRECISION_I64 as u64,
            ..thin_maker()
        }),
        45 * QUOTE
    );
}
