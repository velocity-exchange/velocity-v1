//! Who a routed fill pays, stamps and refuses.
//!
//! Each case routes one taker buy through the liquidity layer, or through the
//! whole order layer, against the vAMM and at most one mocked external book.

use {
    super::{
        super::{FillerSide, PricingRules, TakerSide},
        context::{FillConditions, FillParties, OfferedLiquidity},
        fill_perp_order, fill_within_taker_risk_limits, FillAmounts, FillRequest, PerpFillAccounts,
    },
    crate::{
        controller::position::PositionDirection,
        error::{ErrorCode, VelocityResult},
        instructions::optional_accounts::AccountMaps,
        math::{
            constants::{
                AMM_RESERVE_PRECISION, BASE_PRECISION_I64, BASE_PRECISION_U64, PEG_PRECISION,
                PRICE_PRECISION_I64, PRICE_PRECISION_U64, QUOTE_PRECISION_I64,
                SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION,
                SPOT_WEIGHT_PRECISION,
            },
            router::{FillerObligation, QuoterBook, RouterLeg},
            time::SlotClock,
        },
        state::{
            fill_mode::FillMode,
            market_status::MarketStatus,
            oracle::{HistoricalOracleData, OracleSource},
            oracle_map::OracleMap,
            perp_market::{MarketStats, PerpMarket, AMM},
            perp_market_map::PerpMarketMap,
            prop_amm::{
                DirectionV0, ExternalQuoterExecutor, PriceLevelV0, QuoterSubjects, QuoterType,
                ResponseLocationV0, UserBalanceChangeV0, UserRefV0,
            },
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            state::{FeeStructure, OracleGuardRails, State},
            user::{Order, OrderStatus, OrderType, PerpPosition, SpotPosition, User, UserStats},
            user_map::{UserMap, UserStatsMap},
        },
        test_utils::{
            create_account_info, get_anchor_account_bytes, get_positions, get_pyth_price,
            get_spot_positions,
        },
    },
    anchor_lang::{
        prelude::{AccountInfo, AccountLoader, Clock, Pubkey},
        Owner, ZeroCopy,
    },
};

const SLOT: u64 = 7;
const NOW: i64 = 100;
const TAKER_AUTHORITY: Pubkey = Pubkey::new_from_array([1; 32]);
const MAKER_AUTHORITY: Pubkey = Pubkey::new_from_array([2; 32]);

/// An account that outlives every loader over it. A unit test ends before the
/// leak matters.
fn leak_account<T: ZeroCopy + Owner>(mut account: T, key: Pubkey) -> &'static AccountInfo<'static> {
    let bytes = Box::leak(Box::new(get_anchor_account_bytes(&mut account)));
    let key = Box::leak(Box::new(key));
    let owner = Box::leak(Box::new(T::owner()));
    let lamports = Box::leak(Box::new(0u64));
    Box::leak(Box::new(create_account_info(
        key,
        true,
        lamports,
        &mut bytes[..],
        owner,
    )))
}

/// A response account holding the bytes a quoter would have written.
fn response_account(changes: &[UserBalanceChangeV0]) -> ResponseLocationV0<'static> {
    let bytes = quoter_spec::wincode::serialize(&quoter_spec::ExecuteResponseV0 {
        changes,
        cancelled: &[],
        completed: &[],
        partial: &[],
    })
    .unwrap();
    let len = bytes.len();
    let data: &'static mut [u8] = Box::leak(bytes.into_boxed_slice());
    let key: &'static Pubkey = Box::leak(Box::new(Pubkey::new_unique()));
    let owner: &'static Pubkey = Box::leak(Box::new(crate::ID));
    let lamports: &'static mut u64 = Box::leak(Box::new(0u64));
    ResponseLocationV0::new(
        AccountInfo::new(key, false, true, lamports, data, owner, false),
        &quoter_spec::ResponsePointerV0 {
            offset: 0,
            len: len as u32,
        },
    )
    .unwrap()
}

/// One custom book that fills its maker at one price.
struct MockBook {
    maker: Pubkey,
    maker_ref: UserRefV0,
    price: u64,
    requested: u64,
}

impl ExternalQuoterExecutor<'static> for MockBook {
    fn quoter_type(&self, _index: usize) -> QuoterType {
        QuoterType::Custom
    }

    fn quoter_user(&self, _index: usize) -> Pubkey {
        self.maker
    }

    fn quoter_key(&self, _index: usize) -> Pubkey {
        self.maker
    }

    fn subjects(
        &self,
        _index: usize,
        _direction: DirectionV0,
        _size: u64,
    ) -> VelocityResult<QuoterSubjects> {
        Ok(QuoterSubjects::Account(self.maker))
    }

    fn execute(
        &mut self,
        _index: usize,
        _direction: DirectionV0,
        size: u64,
    ) -> VelocityResult<ResponseLocationV0<'static>> {
        self.requested = size;
        let quote_size =
            ((size as u128) * (self.price as u128) / BASE_PRECISION_U64 as u128) as u64;
        Ok(response_account(&[UserBalanceChangeV0 {
            base_size: size,
            quote_size,
            user: self.maker_ref,
            _pad: [0; 6],
        }]))
    }
}

/// Who turns the fill.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Crank {
    /// A third-party keeper with its own loaded accounts.
    Keeper,
    /// The book's maker, which is already loaded in the maker map.
    Maker,
}

/// What a case varies.
struct Case {
    market_status: MarketStatus,
    /// The price the book quotes and fills at, or `None` for no book.
    book_price: Option<u64>,
    maker_authority: Pubkey,
    maker_sub_account_id: u16,
    maker_position_base: i64,
    crank: Crank,
    min_base_asset_reserve: u128,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            market_status: MarketStatus::Active,
            book_price: Some(99 * PRICE_PRECISION_U64),
            maker_authority: MAKER_AUTHORITY,
            maker_sub_account_id: 0,
            maker_position_base: 0,
            crank: Crank::Keeper,
            min_base_asset_reserve: 0,
        }
    }
}

fn market(case: &Case, oracle_key: Pubkey) -> PerpMarket {
    let mut market = PerpMarket {
        amm: AMM {
            base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            terminal_quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
            base_asset_amount_with_amm: (AMM_RESERVE_PRECISION / 2) as i128,
            sqrt_k: 100 * AMM_RESERVE_PRECISION,
            peg_multiplier: 100 * PEG_PRECISION,
            max_slippage_ratio: 50,
            max_fill_reserve_fraction: 100,
            base_spread: 20000,
            max_spread: 50000,
            ..AMM::default()
        },

        base_asset_amount_long: (AMM_RESERVE_PRECISION / 2) as i128,
        order_step_size: 1000,
        order_tick_size: 1,
        oracle: oracle_key,
        oracle_source: OracleSource::PythLazer,
        market_stats: MarketStats {
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: 100 * PRICE_PRECISION_I64,
                last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: 100 * PRICE_PRECISION_I64,
                last_oracle_price_twap_ts: NOW - 10,
                ..HistoricalOracleData::default()
            },
            last_bid_price_twap: 99 * PRICE_PRECISION_U64,
            last_ask_price_twap: 101 * PRICE_PRECISION_U64,
            last_mark_price_twap: 100 * PRICE_PRECISION_U64,
            last_mark_price_twap_ts: NOW - 10,
            ..MarketStats::default()
        },

        margin_ratio_initial: 1000,
        margin_ratio_maintenance: 500,
        status: case.market_status,
        ..PerpMarket::default_test()
    };

    market.amm.max_base_asset_reserve = u64::MAX as u128;
    market.amm.min_base_asset_reserve = case.min_base_asset_reserve;
    market
}

fn deposit(dollars: u64) -> [SpotPosition; 8] {
    get_spot_positions(SpotPosition {
        market_index: 0,
        balance_type: SpotBalanceType::Deposit,
        scaled_balance: dollars * SPOT_BALANCE_PRECISION_U64,
        ..SpotPosition::default()
    })
}

/// A taker buy of one base at a 105 limit.
fn taker_order() -> Order {
    Order {
        market_index: 0,
        status: OrderStatus::Open,
        order_type: OrderType::Limit,
        direction: PositionDirection::Long,
        base_asset_amount: BASE_PRECISION_U64,
        price: 105 * PRICE_PRECISION_U64,
        order_id: 1,
        ..Order::default()
    }
}

/// A taker-signed fill, so no withheld book arms a filler obligation.
fn taker_signed_standing() -> crate::instructions::FillerStanding {
    crate::instructions::FillerStanding {
        protocol_authority: Pubkey::default(),
        taker_exposure_closed_by_caller: false,
        obligation: FillerObligation {
            taker_signed: true,
            tx_accounts: None,
            unrouted_quoters: 0,
        },
    }
}

fn pricing_rules(fee_structure: &FeeStructure) -> PricingRules<'_> {
    PricingRules {
        fee_structure,
        validity_guard_rails: Box::leak(Box::default()),
        promo_fee_tier: 0,
        referrer_is_accelerated: false,
        vamm_maker_rebate: false,
        builder_fee_allowed: false,
    }
}

/// Everything a case fills against, built once per case.
struct Scenario {
    maps: AccountMaps<'static>,
    makers: UserMap<'static>,
    maker_stats: UserStatsMap<'static>,
    maker_key: Pubkey,
    book: MockBook,
    levels: Vec<PriceLevelV0>,
}

impl Scenario {
    fn new(case: &Case) -> Self {
        let oracle_key = Pubkey::new_unique();
        let oracle = leak_account(
            PythLazerOracle {
                posted_slot: SLOT,
                ..get_pyth_price(100, 6)
            },
            oracle_key,
        );
        let oracle_map = OracleMap::load_one(oracle, SLOT, SlotClock::baseline(), None).unwrap();
        let market_info = leak_account(market(case, oracle_key), Pubkey::new_unique());
        let perp_market_map = PerpMarketMap::load_one(market_info, true).unwrap();
        let spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            maintenance_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData::default_price(QUOTE_PRECISION_I64),
            ..SpotMarket::default()
        };
        let spot_market_map =
            SpotMarketMap::load_one(leak_account(spot_market, Pubkey::new_unique()), true).unwrap();

        let maker_key = Pubkey::new_unique();
        let maker = User {
            authority: case.maker_authority,
            sub_account_id: case.maker_sub_account_id,
            spot_positions: deposit(10_000),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: case.maker_position_base,
                quote_asset_amount: -case.maker_position_base * 100 * QUOTE_PRECISION_I64
                    / BASE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            ..User::default()
        };
        let makers = UserMap::load_one(leak_account(maker, maker_key)).unwrap();
        let maker_stats = UserStats {
            authority: case.maker_authority,
            ..UserStats::default()
        };
        let maker_stats =
            UserStatsMap::load_one(leak_account(maker_stats, Pubkey::new_unique())).unwrap();

        let levels = case
            .book_price
            .map(|price| {
                vec![PriceLevelV0 {
                    price,
                    size: BASE_PRECISION_U64,
                }]
            })
            .unwrap_or_default();

        Self {
            maps: AccountMaps::new(perp_market_map, spot_market_map, oracle_map),
            makers,
            maker_stats,
            maker_key,
            book: MockBook {
                maker: maker_key,
                maker_ref: UserRefV0 {
                    authority: case.maker_authority,
                    sub_account_id: case.maker_sub_account_id,
                },
                price: case.book_price.unwrap_or(0),
                requested: 0,
            },
            levels,
        }
    }

    fn market(&self) -> std::cell::Ref<'_, PerpMarket> {
        self.maps.perp_market_map.get_ref(&0).unwrap()
    }

    fn maker(&self) -> std::cell::Ref<'_, User> {
        self.makers.get_ref(&self.maker_key).unwrap()
    }

    /// Route the taker's buy through the liquidity layer.
    fn fill(&mut self, crank: Crank) -> VelocityResult<FillAmounts> {
        let fee_structure = FeeStructure::test_default();
        let mut taker = User {
            authority: TAKER_AUTHORITY,
            spot_positions: deposit(1_000),
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                ..PerpPosition::default()
            }),
            ..User::default()
        };
        let mut taker_stats = UserStats {
            authority: TAKER_AUTHORITY,
            ..UserStats::default()
        };
        let mut keeper = User::default();
        let mut keeper_stats = UserStats::default();
        let (mut filler, mut filler_stats, filler_key) = match crank {
            Crank::Keeper => (
                Some(&mut keeper),
                Some(&mut keeper_stats),
                Pubkey::new_unique(),
            ),
            Crank::Maker => (None, None, self.maker_key),
        };

        let books = [QuoterBook {
            priority: QuoterType::Custom.default_priority(),
            levels: &self.levels,
            withheld: PriceLevelV0::default(),
        }];
        let book_count = usize::from(!self.levels.is_empty());
        let mut router = RouterLeg {
            books: &books[..book_count],
            executor: &mut self.book,
            standing: taker_signed_standing(),
            worst_fill_price: None,
        };

        let mut order = taker_order();
        fill_within_taker_risk_limits(
            &mut TakerSide::bind(
                &mut taker,
                &mut taker_stats,
                Pubkey::new_unique(),
                &mut order,
                false,
            )?,
            &pricing_rules(&fee_structure),
            &FillConditions::for_layer_test(
                FillMode::Fill,
                NOW,
                SLOT,
                Some(100 * PRICE_PRECISION_I64),
                true,
                false,
            ),
            &mut FillParties {
                maps: &mut self.maps,
                makers_and_referrer: &self.makers,
                makers_and_referrer_stats: &self.maker_stats,
            },
            &mut OfferedLiquidity {
                router: &mut router,
            },
            &mut FillerSide {
                user: &mut filler,
                stats: &mut filler_stats,
                key: filler_key,
                rev_share_escrow: &mut None,
            },
        )
    }
}

fn run(case: Case) -> (Scenario, VelocityResult<FillAmounts>) {
    let mut scenario = Scenario::new(&case);
    let filled = scenario.fill(case.crank);
    (scenario, filled)
}

/// A maker that cranks a fill earns the filler reward on the vAMM slice. The
/// vAMM settles first, so the reward cannot wait for the maker to fill.
#[test]
fn a_cranking_maker_earns_the_filler_reward_on_the_vamm_slice() {
    let (scenario, filled) = run(Case {
        book_price: None,
        crank: Crank::Maker,
        ..Case::default()
    });

    assert_eq!(filled.unwrap().base, BASE_PRECISION_U64);
    let maker = scenario.maker();
    let reward = maker.perp_positions[0].quote_asset_amount;
    assert!(
        reward > 0,
        "the maker was paid {} for the vAMM slice",
        reward
    );
    assert_eq!(maker.perp_positions[0].base_asset_amount, 0);
}

/// A maker of the taker's own authority shares the taker's stats, so it has
/// no seat to take a filler reward on. The fill goes through and pays none.
#[test]
fn a_cranking_maker_of_the_takers_authority_does_not_fail_the_fill() {
    let (scenario, filled) = run(Case {
        maker_authority: TAKER_AUTHORITY,
        maker_sub_account_id: 1,
        crank: Crank::Maker,
        ..Case::default()
    });

    assert_eq!(filled.unwrap().base, BASE_PRECISION_U64);
    assert_eq!(scenario.book.requested, BASE_PRECISION_U64);
}

/// Every maker a fill moves is stamped active, so a quoter user that only
/// makes fills does not age toward the liquidation fee and force-delete gates.
#[test]
fn a_filled_maker_is_stamped_active() {
    let (scenario, filled) = run(Case::default());

    assert_eq!(filled.unwrap().base, BASE_PRECISION_U64);
    assert_eq!(scenario.maker().last_active_slot, SLOT);
}

/// A book does not know the market is `ReduceOnly`. A fill that grows its
/// maker's position is refused.
#[test]
fn a_maker_in_a_reduce_only_market_may_only_reduce() {
    let (_, flat) = run(Case {
        market_status: MarketStatus::ReduceOnly,
        ..Case::default()
    });
    assert_eq!(flat.unwrap_err(), ErrorCode::QuoterReportExceedsReservation);

    // A long of one covers the whole sale.
    let (scenario, long) = run(Case {
        market_status: MarketStatus::ReduceOnly,
        maker_position_base: BASE_PRECISION_I64,
        ..Case::default()
    });
    assert_eq!(long.unwrap().base, BASE_PRECISION_U64);
    assert_eq!(scenario.maker().perp_positions[0].base_asset_amount, 0);
}

/// The mark TWAP records the price the fill traded at. Two fills that only
/// take the book, at different prices, record different samples. The vAMM
/// quote is the same in both.
#[test]
fn the_mark_twap_records_the_price_the_fill_traded_at() {
    let ask_twap = |price: u64| {
        let (scenario, filled) = run(Case {
            book_price: Some(price),
            ..Case::default()
        });
        assert_eq!(filled.unwrap().base, BASE_PRECISION_U64);
        assert_eq!(scenario.book.requested, BASE_PRECISION_U64);
        let ask_twap = scenario.market().market_stats.last_ask_price_twap;
        ask_twap
    };

    assert!(ask_twap(100 * PRICE_PRECISION_U64) < ask_twap(101 * PRICE_PRECISION_U64));
}

/// A vAMM whose reserves are past their bound refuses the fill, so a corrupt
/// curve does not read as an empty one.
#[test]
fn a_vamm_past_its_reserve_bound_refuses_the_fill() {
    let (_, filled) = run(Case {
        book_price: None,
        min_base_asset_reserve: 200 * AMM_RESERVE_PRECISION,
        ..Case::default()
    });

    assert_eq!(filled.unwrap_err(), ErrorCode::InvalidAmmForFillDetected);
}

fn loader<T: ZeroCopy + Owner>(account: T) -> AccountLoader<'static, T> {
    AccountLoader::try_from(leak_account(account, Pubkey::new_unique())).unwrap()
}

/// Fill a taker bid at 99.5 through the whole order layer. The bid crosses
/// the book's 99 ask and not the vAMM. Returns the base filled and the base
/// the book was asked for.
fn fill_bid_through_order_layer(post_only: bool) -> (u64, u64) {
    let mut scenario = Scenario::new(&Case::default());
    let taker = loader(User {
        authority: TAKER_AUTHORITY,
        spot_positions: deposit(1_000),
        ..User::default()
    });
    let taker_stats = loader(UserStats {
        authority: TAKER_AUTHORITY,
        ..UserStats::default()
    });
    let keeper = loader(User::default());
    let keeper_stats = loader(UserStats::default());

    let books = [QuoterBook {
        priority: QuoterType::Custom.default_priority(),
        levels: &scenario.levels,
        withheld: PriceLevelV0::default(),
    }];
    let mut router = RouterLeg {
        books: &books,
        executor: &mut scenario.book,
        standing: taker_signed_standing(),
        worst_fill_price: None,
    };

    let mut order = Order {
        post_only,
        price: 99 * PRICE_PRECISION_U64 + PRICE_PRECISION_U64 / 2,
        ..taker_order()
    };
    let filled = fill_perp_order(
        FillRequest {
            order: &mut order,
            reserved: false,
            mode: FillMode::Fill,
            referrer_is_accelerated: false,
        },
        &State {
            oracle_guard_rails: OracleGuardRails::default(),
            ..State::default()
        },
        &Clock {
            slot: SLOT,
            unix_timestamp: NOW,
            ..Clock::default()
        },
        PerpFillAccounts {
            user: &taker,
            user_stats: &taker_stats,
            filler: &keeper,
            filler_stats: &keeper_stats,
            rev_share_escrow: &mut None,
        },
        &mut FillParties {
            maps: &mut scenario.maps,
            makers_and_referrer: &scenario.makers,
            makers_and_referrer_stats: &scenario.maker_stats,
        },
        &mut router,
    )
    .unwrap();

    (filled.base, scenario.book.requested)
}

/// A post-only order never takes maker liquidity, so it fills nothing and the
/// book is never asked. The same bid without the flag takes the book.
#[test]
fn a_post_only_order_takes_no_book() {
    assert_eq!(fill_bid_through_order_layer(true), (0, 0));
    assert_eq!(
        fill_bid_through_order_layer(false),
        (BASE_PRECISION_U64, BASE_PRECISION_U64)
    );
}
