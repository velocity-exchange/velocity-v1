//! What a maker's room comes out as, per reason it might be constrained.
//!
//! Three groups: the states that answer zero without pricing anything, which
//! of the two budgets binds, and the test that decides whether a maker is
//! worth one of the eight slots at all.

use {
    super::*,
    crate::{
        create_anchor_account_info,
        instructions::optional_accounts::AccountMaps,
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
    /// Base of `open_bids` that belongs to an order resting in a `User.orders`
    /// rather than a book.
    slot_bid: u64,
    taker_size: u64,
    books: u32,
    /// Quote held by the market's own isolated position, in whole dollars.
    /// `None` leaves the position cross-margined.
    isolated: Option<u64>,
    /// The maker is latched for liquidation.
    liquidated: bool,
    reduce_only_market: bool,
    /// Orders the maker rests on the book, and how many of them are
    /// reduce-only.
    clob_orders: u8,
    reduce_only_clob_orders: u16,
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
            slot_bid: 0,
            taker_size: BASE_PRECISION_I64 as u64,
            books: 1,
            isolated: None,
            liquidated: false,
            reduce_only_market: false,
            clob_orders: 0,
            reduce_only_clob_orders: 0,
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
/// Which of the two numbers a case is measured for.
enum Measure {
    /// The quote a resting maker may lose: [`CapInputs::maker_budget`].
    Budget,
    /// The base an unreserved quoter may take on:
    /// [`CapInputs::quoter_base_room`].
    QuoterRoom,
}

/// The quote a resting maker may lose on the swept side.
fn budget(case: Case) -> u64 {
    measure(case, Measure::Budget)
}

/// The base a custom quoter's own account can carry on the swept side.
fn quoter_room(case: Case) -> u64 {
    measure(case, Measure::QuoterRoom)
}

/// The maker account a case describes.
fn maker_account(case: &Case, authority: Pubkey) -> User {
    let mut orders = [Order::default(); 32];
    if case.slot_bid > 0 {
        orders[0] = Order {
            status: OrderStatus::Open,
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            market_index: 0,
            direction: PositionDirection::Long,
            base_asset_amount: case.slot_bid,
            ..Order::default()
        };
    }

    User {
        orders,
        authority,
        equity_floor: case.floor,
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            base_asset_amount: case.position_base,
            open_bids: case.open_bids,
            open_orders: case.clob_orders,
            reduce_only_clob_orders: case.reduce_only_clob_orders,
            position_flag: case
                .isolated
                .map(|_| crate::state::user::PositionFlag::IsolatedPosition as u8)
                .unwrap_or(0),
            isolated_position_scaled_balance: case
                .isolated
                .map(|quote| quote * SPOT_BALANCE_PRECISION_U64)
                .unwrap_or(0),
            ..PerpPosition::default()
        }),

        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: case.deposit * SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        }),
        status: if case.liquidated {
            crate::state::user::UserStatus::BeingLiquidated as u8
        } else {
            0
        },
        ..User::default()
    }
}

fn measure(case: Case, measure: Measure) -> u64 {
    let Case {
        latched,
        stale_oracle,
        taker_size,
        books,
        reduce_only_market,
        ..
    } = case;
    let slot = if stale_oracle { 100_000 } else { 1 };

    let mut oracle_price = get_pyth_price(100, 6);
    let oracle_key = Pubkey::from_str(AUTHORITY).unwrap();
    create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
    let mut oracle_map = crate::state::oracle_map::OracleMap::load_one(
        &oracle_info,
        slot,
        crate::math::time::SlotClock::baseline(),
        None,
    )
    .unwrap();

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
        status: if reduce_only_market {
            crate::state::market_status::MarketStatus::ReduceOnly
        } else {
            crate::state::market_status::MarketStatus::Active
        },
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
    let mut maps = AccountMaps::new(perp_market_map, spot_market_map, oracle_map);

    let authority = Pubkey::from_str(AUTHORITY).unwrap();
    let mut maker = maker_account(&case, authority);
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

    let mut inputs = CapInputs {
        taker_key: &Pubkey::new_unique(),
        makers_and_referrer: &makers,
        makers_and_referrer_stats: &stats_map,
        maps: &mut maps,
    };

    match measure {
        Measure::Budget => inputs
            .maker_budget(&maker_key, 0, SideV0::Bid, taker_size, ORACLE, books)
            .unwrap(),
        // A taker sweeping the bid side leaves the quoter long.
        Measure::QuoterRoom => inputs
            .quoter_base_room(&maker_key, 0, PositionDirection::Long)
            .unwrap(),
    }
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
fn a_route_with_no_book_leaves_the_budget_unbounded() {
    // Nothing reserved this maker's depth, so no budget can be spent against
    // it. The count also divides the budget, and zero would fail the fill.
    assert_eq!(
        budget(Case {
            books: 0,
            ..thin_maker()
        }),
        u64::MAX
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
fn a_slot_order_is_not_mistaken_for_book_depth() {
    // `open_bids` reserves for every open order on the market, so a slot share has to come off
    // before what is left reads as depth on a book. A maker whose bids are all in slots is
    // unreachable by a book budget, and answering that from the account is what keeps a margin
    // walk off a heap that cannot give the memory back.
    assert_eq!(
        budget(Case {
            slot_bid: 200 * BASE_PRECISION_I64 as u64,
            ..thin_maker()
        }),
        u64::MAX,
        "every reserved bid is accounted for in a slot, so none is on a book"
    );

    // Half on each: the book's half is still worth pricing.
    assert_eq!(
        budget(Case {
            slot_bid: 100 * BASE_PRECISION_I64 as u64,
            ..thin_maker()
        }),
        45 * QUOTE
    );
}

#[test]
fn a_maker_under_liquidation_has_no_room() {
    assert_eq!(
        budget(Case {
            liquidated: true,
            ..thin_maker()
        }),
        0
    );
}

/// The book ignores `base_cap` on an ordinary order, so in a `ReduceOnly`
/// market only exclusion keeps such an order from growing its owner's position.
#[test]
fn a_reduce_only_market_excludes_a_maker_whose_ordinary_orders_grow_it() {
    let flat_with_an_ordinary_order = Case {
        reduce_only_market: true,
        clob_orders: 1,
        ..thin_maker()
    };

    assert_eq!(budget(flat_with_an_ordinary_order), 0);

    // Every order is reduce-only, so the book holds each one to its cover.
    assert!(
        budget(Case {
            reduce_only_market: true,
            clob_orders: 1,
            reduce_only_clob_orders: 1,
            ..thin_maker()
        }) > 0
    );

    // A short of 300 covers the 200 of bids, so every fill reduces it.
    assert!(
        budget(Case {
            reduce_only_market: true,
            clob_orders: 1,
            position_base: -300 * BASE_PRECISION_I64,
            deposit: 100_000,
            ..thin_maker()
        }) > 0
    );
}

/// An isolated maker is budgeted from the collateral of the isolated position
/// itself, not from whatever the account holds in cross.
///
/// The book is position-blind and velocity is not: it picks the margin scope
/// from the maker's live position, so a maker flush in cross but holding
/// nothing against its isolated position has no room for a fill that grows it.
#[test]
fn an_isolated_maker_is_budgeted_from_its_isolated_collateral() {
    // Flush in cross, empty in the isolated position the fill would land in.
    assert_eq!(
        budget(Case {
            deposit: 100_000,
            position_base: 0,
            open_bids: 200 * BASE_PRECISION_I64,
            taker_size: 200 * BASE_PRECISION_I64 as u64,
            isolated: Some(0),
            ..Case::default()
        }),
        0,
        "cross collateral does not back an isolated position's fill"
    );

    // The control: the same numbers, cross-margined, do have room. So the zero
    // above is the isolated scope refusing cross collateral, not the fill
    // being unaffordable.
    assert!(
        budget(Case {
            deposit: 100_000,
            position_base: 0,
            open_bids: 200 * BASE_PRECISION_I64,
            taker_size: 200 * BASE_PRECISION_I64 as u64,
            isolated: None,
            ..Case::default()
        }) > 0,
        "the same deposit backs a cross-margined fill"
    );

    // The same maker, with the isolated position funded, has room.
    assert!(
        budget(Case {
            deposit: 100_000,
            position_base: 0,
            open_bids: 200 * BASE_PRECISION_I64,
            taker_size: 200 * BASE_PRECISION_I64 as u64,
            isolated: Some(50_000),
            ..Case::default()
        }) > 0,
        "an isolated position funded for the fill is budgeted for it"
    );
}

/// The second number, for the other kind of counterparty.
///
/// A custom quoter reserves nothing at placement, so what a fill costs it is
/// initial margin on the base it takes, not the gap between a resting limit
/// and the mark. The two are measured apart for that reason.
mod quoter_base_room {
    use super::*;

    #[test]
    fn a_thin_account_carries_less_base_than_a_deep_one() {
        // Ten percent initial margin at a 100 oracle, so a dollar of free
        // collateral backs a tenth of a base unit. The point is the ordering
        // and that both bind, not the exact figure.
        let thin = quoter_room(Case {
            deposit: 1_000,
            ..Case::default()
        });
        let deep = quoter_room(Case {
            deposit: 100_000,
            ..Case::default()
        });

        assert!(thin > 0, "a solvent account carries some base");
        assert!(
            deep > thin,
            "more collateral carries more base: {} {}",
            thin,
            deep
        );
    }

    #[test]
    fn an_account_with_no_collateral_carries_nothing() {
        assert_eq!(
            quoter_room(Case {
                deposit: 0,
                ..Case::default()
            }),
            0
        );
    }

    /// A short the quoter's long fill would reduce.
    const SHORT: i64 = -5 * BASE_PRECISION_I64;

    #[test]
    fn a_quoter_user_under_liquidation_has_no_room() {
        assert_eq!(
            quoter_room(Case {
                deposit: 100_000,
                liquidated: true,
                ..Case::default()
            }),
            0
        );
    }

    #[test]
    fn a_latched_quoter_user_keeps_only_the_room_that_reduces() {
        let reducing = quoter_room(Case {
            deposit: 100_000,
            position_base: SHORT,
            latched: true,
            ..Case::default()
        });

        assert_eq!(reducing, SHORT.unsigned_abs());

        assert_eq!(
            quoter_room(Case {
                deposit: 100_000,
                position_base: 0,
                latched: true,
                ..Case::default()
            }),
            0,
            "a flat user has nothing to reduce"
        );
    }

    #[test]
    fn an_unverifiable_floor_keeps_only_the_room_that_reduces() {
        assert_eq!(
            quoter_room(Case {
                deposit: 100_000,
                position_base: SHORT,
                floor: FLOOR,
                stale_oracle: true,
                ..Case::default()
            }),
            SHORT.unsigned_abs()
        );
    }

    #[test]
    fn a_reduce_only_market_keeps_only_the_room_that_reduces() {
        let open = quoter_room(Case {
            deposit: 100_000,
            position_base: SHORT,
            ..Case::default()
        });
        let reduce_only = quoter_room(Case {
            deposit: 100_000,
            position_base: SHORT,
            reduce_only_market: true,
            ..Case::default()
        });

        assert!(open > SHORT.unsigned_abs());
        assert_eq!(reduce_only, SHORT.unsigned_abs());
    }

    #[test]
    fn a_position_already_held_costs_room() {
        // The walk sizes the order against the position it settles into, so
        // base held the same way the fill would add it leaves less room.
        let flat = quoter_room(Case {
            deposit: 10_000,
            position_base: 0,
            ..Case::default()
        });
        let long = quoter_room(Case {
            deposit: 10_000,
            position_base: 50 * BASE_PRECISION_I64,
            ..Case::default()
        });

        assert!(
            long < flat,
            "an open long leaves less room: {} {}",
            long,
            flat
        );
    }
}
