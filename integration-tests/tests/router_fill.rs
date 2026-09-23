//! End-to-end router fill: a taker's market order routed across a real CLOB
//! book (quote + execute CPIs through its slot in the market's
//! `QuoterSlabV0`), a DLOB maker,
//! and the vAMM — through the real `fill_perp_order_router` instruction.
//! CLOB placement runs through the velocity `place_clob_order` adapter, so
//! the maker's open-order aggregates are reserved by the margin gate and
//! unwound by the fill, exactly as the margin model requires.
//!
//! Protocol state (State, markets, users, oracle) is synthesized via
//! `set_account` with the same values the controller unit fixtures use.

use {
    anchor_lang::{AnchorDeserialize, Discriminator, InstructionData, ToAccountMetas},
    bytemuck::Zeroable,
    solana_account::Account,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    velocity::{
        controller::position::PositionDirection,
        error::ErrorCode,
        instructions::{
            CancelOrderV1Params, CancelOrdersV1Params, CrankClobEvictArgs,
            CrankClobRemoveExpiredArgs, CrankCrossMatchArgs, CrankTakerOriginCrossArgs,
            ForceCancelClobOrdersArgs, InitializeQuoterArgs, InitializeQuoterCrossConditionsArgs,
            InitializeRouterQuoteBufferArgs, PlaceAndMakePerpOrderV1Args,
            PlaceAndTakePerpOrderV1Args, QuoterAccountMetaArg, RefillCrankReservoirArgs,
            TriggerLimitOrderV1Args, TriggerMarketOrderV1Args, UpdatePerpMarketClobQuoterArgs,
            UpdateQuoterAccountsArgs, UpdateQuoterApprovedArgs,
        },
        math::constants::{
            AMM_RESERVE_PRECISION, PEG_PRECISION, PRICE_PRECISION, QUOTE_PRECISION_I64,
            SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
        },
        state::{
            clob_crank::CrankCostUnitsV0,
            market_status::MarketStatus,
            oracle::OracleSource,
            order_params::{OrderParams, PostOnlyParam},
            perp_market::PerpMarket,
            prop_amm::{
                ClobCancelSides, ClobOrderRefV0, Direction, L3ArgsV0, L3ResponseV0, L3RowV0,
                QuoterType, ResponsePointerV0,
            },
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            state::{FeeStructure, OracleGuardRails, State, TransactionFeeRails},
            traits::Size,
            user::{MarketType, Order, OrderStatus, OrderType, User, UserStats},
        },
    },
    velocity_integration_tests::*,
};

const UNIT: u64 = 1_000_000_000;
const PRICE: u64 = 1_000_000; // PRICE_PRECISION as u64

/// The wake fields are sealed behind `WakeView`; these pull one variant's
/// payload out for assertions, and fail loudly on the wrong variant.
fn value_cross(c: &velocity::relay_spec::ConditionV0) -> ([u8; 32], u32, u32, i64, u8) {
    match c.wake() {
        Ok(velocity::relay_spec::WakeView::OnValueCross {
            address,
            offset,
            len,
            threshold: velocity::relay_spec::WatchValue::Signed(threshold),
            cmp,
        }) => (address, offset, len, threshold, cmp),
        other => panic!("expected signed OnValueCross, got {other:?}"),
    }
}

fn account_change(c: &velocity::relay_spec::ConditionV0) -> ([u8; 32], u32, u32) {
    match c.wake() {
        Ok(velocity::relay_spec::WakeView::OnAccountChange {
            address,
            offset,
            len,
        }) => (address, offset, len),
        other => panic!("expected OnAccountChange, got {other:?}"),
    }
}
fn spot_market_pda(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"spot_market", market_index.to_le_bytes().as_ref()],
        &velocity_id(),
    )
    .0
}

/// State with everything a fill touches: valid fee tiers, oracle guard
/// rails, and the signer PDA.
fn set_trading_state(svm: &mut litesvm::LiteSVM, admin: &Pubkey) {
    let mut state: State = Zeroable::zeroed();
    state.cold_admin = anchor_lang::prelude::Pubkey::new_from_array(*admin.as_array());
    state.warm_admin = state.cold_admin;
    let (signer, nonce) = velocity_signer_pda();
    state.signer = anchor_lang::prelude::Pubkey::new_from_array(*signer.as_array());
    state.signer_nonce = nonce;
    state.perp_fee_structure = FeeStructure::perps_default();
    state.spot_fee_structure = FeeStructure::perps_default();
    state.oracle_guard_rails = OracleGuardRails::default();
    state.number_of_markets = 1;
    state.number_of_spot_markets = 1;
    // What `initialize` writes, so a fixture prices a crank the way a fresh
    // exchange does. A zeroed rails prices every crank at nothing, which the
    // attach refuses.
    state.transaction_fee_rails = TransactionFeeRails::FLAT_PER_SIGNATURE;
    set_zero_copy_account(svm, state_pda(), State::DISCRIMINATOR, &state, State::SIZE);
}

fn set_oracle(svm: &mut litesvm::LiteSVM, address: Pubkey, price_precision_price: i64, slot: u64) {
    let mut oracle: PythLazerOracle = Zeroable::zeroed();
    oracle.price = price_precision_price;
    oracle.exponent = 6;
    oracle.posted_slot = slot;
    set_zero_copy_account(
        svm,
        address,
        PythLazerOracle::DISCRIMINATOR,
        &oracle,
        PythLazerOracle::SIZE,
    );
}

/// The controller unit fixture's $100 market: 100-unit reserves at peg 100,
/// 2% base spread, 10%/5% margin ratios. `quoter_slab` carries the slab PDA,
/// which `initialize_perp_market` writes, so the `has_one = quoter_slab`
/// contexts accept the fixture. `clob_market` stays default here: the Clob
/// registration in `register_clob_quoter` writes the book into it, the same
/// one-way designation production makes.
fn set_trading_perp_market(svm: &mut litesvm::LiteSVM, oracle: Pubkey) {
    let mut market: PerpMarket = Zeroable::zeroed();
    market.market_index = 0;
    market.status = MarketStatus::Active;
    market.quoter_slab =
        anchor_lang::prelude::Pubkey::new_from_array(quoter_slab_pda(0).to_bytes());
    market.oracle = anchor_lang::prelude::Pubkey::new_from_array(*oracle.as_array());
    market.oracle_source = OracleSource::PythLazer;
    market.order_step_size = 1000;
    market.order_tick_size = 1;
    market.margin_ratio_initial = 1000;
    market.margin_ratio_maintenance = 500;
    market.amm.base_asset_reserve = 100 * AMM_RESERVE_PRECISION;
    market.amm.quote_asset_reserve = 100 * AMM_RESERVE_PRECISION;
    market.amm.sqrt_k = 100 * AMM_RESERVE_PRECISION;
    market.amm.peg_multiplier = 100 * PEG_PRECISION;
    market.amm.base_asset_amount_with_amm = (AMM_RESERVE_PRECISION / 2) as i128;
    market.amm.max_slippage_ratio = 50;
    market.amm.max_fill_reserve_fraction = 100;
    market.amm.base_spread = 20000;
    market.amm.max_spread = 50000;
    market.amm.terminal_quote_asset_reserve = 100 * AMM_RESERVE_PRECISION;
    market.amm.max_base_asset_reserve = u64::MAX as u128;
    market.amm.min_base_asset_reserve = 0;
    market.base_asset_amount_long = (AMM_RESERVE_PRECISION / 2) as i128;
    market.market_stats.last_bid_price_twap = (100 * PRICE_PRECISION) as u64;
    market.market_stats.last_ask_price_twap = (100 * PRICE_PRECISION) as u64;
    market.market_stats.last_mark_price_twap = (100 * PRICE_PRECISION) as u64;
    market.market_stats.last_mark_price_twap_5min = (100 * PRICE_PRECISION) as u64;
    market.market_stats.historical_oracle_data.last_oracle_price = (100 * PRICE_PRECISION) as i64;
    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap = (100 * PRICE_PRECISION) as i64;
    market
        .market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min = (100 * PRICE_PRECISION) as i64;
    set_zero_copy_account(
        svm,
        perp_market_pda(0),
        PerpMarket::DISCRIMINATOR,
        &market,
        PerpMarket::SIZE,
    );
}

fn set_quote_spot_market(svm: &mut litesvm::LiteSVM) {
    let mut market: SpotMarket = Zeroable::zeroed();
    market.market_index = 0;
    market.oracle_source = OracleSource::QuoteAsset;
    market.cumulative_deposit_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    market.cumulative_borrow_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    market.decimals = 6;
    market.initial_asset_weight = SPOT_WEIGHT_PRECISION;
    market.maintenance_asset_weight = SPOT_WEIGHT_PRECISION;
    market.historical_oracle_data.last_oracle_price = QUOTE_PRECISION_I64;
    market.historical_oracle_data.last_oracle_price_twap = QUOTE_PRECISION_I64;
    market.historical_oracle_data.last_oracle_price_twap_5min = QUOTE_PRECISION_I64;
    set_zero_copy_account(
        svm,
        spot_market_pda(0),
        SpotMarket::DISCRIMINATOR,
        &market,
        SpotMarket::SIZE,
    );
}

/// A user with a USDC deposit; `order` slots into `orders[0]` with matching
/// worst-case aggregates.
fn trading_user(authority: &Pubkey, deposit: u64, order: Option<Order>) -> User {
    let mut user: User = Zeroable::zeroed();
    user.authority = anchor_lang::prelude::Pubkey::new_from_array(*authority.as_array());
    user.spot_positions[0].market_index = 0;
    user.spot_positions[0].balance_type = SpotBalanceType::Deposit;
    user.spot_positions[0].scaled_balance = deposit;
    user.perp_positions[0].market_index = 0;
    if let Some(order) = order {
        user.perp_positions[0].open_orders = 1;
        match order.direction {
            PositionDirection::Long => {
                user.perp_positions[0].open_bids = order.base_asset_amount as i64
            }
            PositionDirection::Short => {
                user.perp_positions[0].open_asks = -(order.base_asset_amount as i64)
            }
        }

        user.orders[0] = order;
        user.open_orders = 1;
        user.has_open_order = true;
        user.next_order_id = order.order_id + 1;
    }

    set_user_defaults(&mut user);
    user
}

fn set_user_defaults(user: &mut User) {
    // Remaining perp/spot position slots stay zeroed (market_index 0 +
    // empty = available slot), which is the on-chain empty representation.
    let _ = user;
}

fn set_user_account(svm: &mut litesvm::LiteSVM, address: Pubkey, user: &User) {
    set_zero_copy_account(svm, address, User::DISCRIMINATOR, user, User::SIZE);
}

fn set_user_stats_account(svm: &mut litesvm::LiteSVM, address: Pubkey, authority: &Pubkey) {
    let mut stats: UserStats = Zeroable::zeroed();
    stats.authority = anchor_lang::prelude::Pubkey::new_from_array(*authority.as_array());
    stats.number_of_sub_accounts = 1;
    set_zero_copy_account(
        svm,
        address,
        UserStats::DISCRIMINATOR,
        &stats,
        UserStats::SIZE,
    );
}

/// Bytes for a market account holding at least `capacity` orders.
///
/// Deliberately generous: `initialize_market_v0` sizes the arena from the
/// account length, so extra bytes cost a few slots, not a wrong size.
fn clob_market_space(capacity: usize) -> usize {
    32 * 1024 + capacity * 128
}

fn clob_ix(name: &str, args: Vec<u8>, accounts: Vec<AccountMeta>) -> Instruction {
    let mut data = ix_discriminator(name).to_vec();
    data.extend_from_slice(&args);
    Instruction {
        program_id: clob_id(),
        accounts,
        data,
    }
}

/// Borsh `MarketConfigV0` in PRICE_PRECISION/base-precision units. The evict
/// threshold is 1 so the evict crank is exercisable with a single order.
fn clob_market_config(market_index: u16) -> Vec<u8> {
    let threshold = 1u32;
    let mut v = Vec::new();
    v.extend_from_slice(&market_index.to_le_bytes());
    v.extend_from_slice(&UNIT.to_le_bytes()); // base_precision
    v.extend_from_slice(&1u64.to_le_bytes()); // order_tick_size
    v.extend_from_slice(&1000u64.to_le_bytes()); // order_step_size (matches the perp market step)
    v.extend_from_slice(&1u64.to_le_bytes()); // min_order_size
    v.extend_from_slice(&0u64.to_le_bytes()); // blocking_min_size (0 disables the floor)
    v.extend_from_slice(&0u32.to_le_bytes()); // default_activation_delay
    v.extend_from_slice(&20u32.to_le_bytes()); // max_activation_delay
    v.extend_from_slice(&2u32.to_le_bytes()); // unknown_user_grace_slots
    v.extend_from_slice(&threshold.to_le_bytes()); // evict_threshold_per_side
    v.extend_from_slice(&128u16.to_le_bytes()); // max_quote_levels
    v.extend_from_slice(&64u16.to_le_bytes()); // max_execute_fills
    v.extend_from_slice(&32u16.to_le_bytes()); // max_execute_users
    v
}

/// Ask the book something, without landing anything.
///
/// Simulated, which is what an off-chain reader does and what a relay turner
/// does with a resolver: the answer comes back as return data and, for the
/// legs that stream, in the post-simulation account. Nothing here decodes the
/// market account's layout — the tests introspect the book through the same
/// instructions velocity does.
fn ask_clob(
    svm: &litesvm::LiteSVM,
    payer: &Keypair,
    market: &Pubkey,
    name: &str,
    args: Vec<u8>,
    writable: bool,
) -> (Vec<u8>, Vec<u8>) {
    let meta = AccountMeta {
        pubkey: *market,
        is_signer: false,
        is_writable: writable,
    };

    simulate(svm, payer, clob_ix(name, args, vec![meta]), market)
        .unwrap_or_else(|fail| panic!("{name} simulates: {:?}", fail.err))
}

/// The orders resting on one side, best price first, as the book reports them
/// through `quote_l3_v0` — one row per order.
fn clob_side(fixture: &Fixture, direction: Direction) -> Vec<L3RowV0> {
    let mut args = Vec::new();
    velocity::state::prop_amm::write_l3_args(
        &mut args,
        &L3ArgsV0 {
            direction,
            // Zero describes the side up to `max_rows`.
            size: 0,
            max_rows: 128,
            include_taker_origin_reservations: false,
        },
    )
    .unwrap();
    // Writable: the rows stream into the market's own response tail, and the
    // pointer that comes back locates them there.
    let (pointer, account) = ask_clob(
        &fixture.svm,
        &fixture.keeper,
        &fixture.clob_market,
        "quote_l3_v0",
        args,
        true,
    );

    // The quoter wire's pointer, not relay's: `quote_l3_v0` answers on the
    // interface every source shares.
    let pointer = ResponsePointerV0::deserialize(&mut pointer.as_slice()).unwrap();
    let at = pointer.offset as usize;
    L3ResponseV0::parse(&account[at..at + pointer.len as usize])
        .unwrap()
        .rows
        .to_vec()
}

/// How many orders rest on a side. A buyer consumes the asks.
fn clob_ask_count(fixture: &Fixture) -> usize {
    clob_side(fixture, Direction::Long).len()
}

fn clob_bid_count(fixture: &Fixture) -> usize {
    clob_side(fixture, Direction::Short).len()
}

/// The price of the best resting bid, as the book names it.
fn clob_best_bid_price(fixture: &Fixture) -> Option<u64> {
    clob_side(fixture, Direction::Short)
        .first()
        .map(|row| row.price)
}

/// Init a CLOB book with `place_authority` = the market's quoter slab, so
/// every placement must come through velocity.
fn init_clob_book(svm: &mut litesvm::LiteSVM, clob_admin: &Keypair) -> Pubkey {
    // The book signs its own creation, so the account cannot be initialized by
    // whoever sees it created. The harness holds the keypair only long enough
    // to sign; the book is named by its address everywhere after that.
    let market_kp = Keypair::new();
    let market = market_kp.pubkey();
    svm.set_account(
        market,
        Account {
            lamports: 10_000_000_000,
            data: vec![0u8; clob_market_space(64)],
            owner: clob_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let ix = clob_ix(
        "initialize_market_v0",
        clob_market_config(0),
        vec![
            AccountMeta::new_readonly(clob_admin.pubkey(), true),
            AccountMeta::new_readonly(quoter_slab_pda(0), false),
            AccountMeta::new(market, true),
        ],
    );

    send(svm, clob_admin, ix, &[&market_kp]).unwrap();
    market
}

/// Register + approve the CLOB book as market 0's CLOB quoter.
fn register_clob_quoter(
    svm: &mut litesvm::LiteSVM,
    admin: &Keypair,
    user: Pubkey,
    market: Pubkey,
) -> Pubkey {
    let quoter = quoter_pda(0, &clob_id(), &user);
    // Registration reads the market's slab and approval writes it, so the slab
    // comes first. Born at one slot; approval grows it by exactly the slot each
    // quoter needs.
    create_quoter_slab(svm, admin, 0);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            state: state_pda(),
            payer: admin.pubkey(),
            authority: admin.pubkey(),
            quoter,
            perp_market: perp_market_pda(0),
            // A book designation is refused when an approved quoter's account
            // list already names the book, so registration reads the slab.
            quoter_slab: Some(quoter_slab_pda(0)),
            quoter_program: clob_id(),
            user,
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoter {
            args: InitializeQuoterArgs {
                market_index: 0,
                quoter_type: QuoterType::Clob,
                response_account: market,
                quote_v0_discriminator: ix_discriminator("quote_v0"),
                quote_l3_v0_discriminator: ix_discriminator("quote_l3_v0"),
                execute_v0_discriminator: ix_discriminator("execute_v0"),
            },
        }
        .data(),
    };

    send(svm, admin, ix, &[]).unwrap();
    // One unified account list; each leg forwards a subset by index. The
    // quote leg is the book alone; execute adds the slab, the CPI signer.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterAccounts {
            authority: admin.pubkey(),
            quoter,
            // A book's entry answers to the State admin roles.
            state: Some(state_pda()),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterAccounts {
            args: UpdateQuoterAccountsArgs {
                metas: vec![
                    QuoterAccountMetaArg {
                        pubkey: market,
                        is_writable: true,
                    },
                    QuoterAccountMetaArg {
                        pubkey: quoter_slab_pda(0),
                        is_writable: false,
                    },
                ],

                quote_indexes: vec![0],
                execute_indexes: vec![0, 1],
            },
        }
        .data(),
    };

    send(svm, admin, ix, &[]).unwrap();
    // Approval copies the staged config into the market's slab, which is the
    // copy fills read.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: admin.pubkey(),
            state: state_pda(),
            quoter,
            perp_market: perp_market_pda(0),
            quoter_slab: quoter_slab_pda(0),
            quoter_program: clob_id(),
            quoter_program_data: Some(program_data_pda(&clob_id())),
            // A book approval asks the book for its own placement rules.
            clob_market: Some(market),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved {
            args: velocity::instructions::UpdateQuoterApprovedArgs { approved: true },
        }
        .data(),
    };

    send(svm, admin, ix, &[]).unwrap();
    quoter
}

/// Test-local mirror of the folded-away `place_clob_order` args, so the many
/// call sites stay unchanged. It maps onto `place_and_make_perp_order_v1`: the
/// direction/price/size/max_ts become a limit `OrderParams`, `reject_if_crossed`
/// maps onto `post_only` (true = MustPostOnly, false = rest crossed), and
/// `activation_delay_slots` rides on the `OrderParams`.
#[allow(dead_code)]
struct PlaceClobOrderParams {
    market_index: u16,
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    activation_delay_slots: Option<u32>,
    reject_if_crossed: bool,
}

fn place_clob_order_ix(
    user: Pubkey,
    authority: &Keypair,
    quoter_slab: Pubkey,
    clob_market: Pubkey,
    oracle: Pubkey,
    params: PlaceClobOrderParams,
) -> Instruction {
    let user_stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    let order_params = OrderParams {
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: params.direction,
        base_asset_amount: params.base_asset_amount,
        price: params.price,
        market_index: params.market_index,
        // reject_if_crossed maps onto post-only: a crossed rest is refused only
        // when post-only. `false` rests crossed, which several setups rely on to
        // seed book liquidity below the oracle.
        post_only: if params.reject_if_crossed {
            PostOnlyParam::MustPostOnly
        } else {
            PostOnlyParam::None
        },

        max_ts: (params.max_ts != 0).then_some(params.max_ts),
        activation_delay_slots: params.activation_delay_slots,
        ..Default::default()
    };
    let mut accounts = velocity::accounts::PlaceAndMakeV1 {
        state: state_pda(),
        user,
        user_stats,
        authority: authority.pubkey(),
        quoter_slab,
        clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    // Margin maps: oracle, spot market, perp market.
    accounts.push(AccountMeta::new_readonly(oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndMakePerpOrderV1 {
            args: PlaceAndMakePerpOrderV1Args {
                params: order_params,
            },
        }
        .data(),
    }
}

struct Fixture {
    svm: litesvm::LiteSVM,
    admin: Keypair,
    keeper: Keypair,
    oracle: Pubkey,
    clob_market: Pubkey,
    /// The staging entry: the quoter's identity in signed routes and events.
    quoter: Pubkey,
    /// The market's slab: what fills and CLOB order-flow ixs carry.
    quoter_slab: Pubkey,
    clob_maker_user: Pubkey,
    clob_maker_authority: Keypair,
    /// The book's own admin, for the cases that reconfigure it.
    clob_admin: Keypair,
    /// Where the book's condition block sits, as the attach reported it. A
    /// turner learns it the same way — from the registration — rather than by
    /// knowing the market account's layout.
    crank_block_offset: u32,
}

fn setup() -> Fixture {
    let mut svm = svm();
    let admin = Keypair::new();
    let keeper = Keypair::new();
    let clob_admin = Keypair::new();
    let clob_maker_authority = Keypair::new();
    for kp in [&admin, &keeper, &clob_admin, &clob_maker_authority] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }

    svm.warp_to_slot(10);
    let oracle = Pubkey::new_unique();
    // Resolvers derive maker `User` PDAs from the node's (authority,
    // sub_account_id) identity, so the fixture places the maker at its true
    // address — as `initialize_user` always does in production.
    let clob_maker_user = Pubkey::find_program_address(
        &[
            b"user",
            clob_maker_authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    // The staging entry's PDA, derivable before the entry exists. The market
    // learns its book at registration, when `initialize_quoter` writes
    // `clob_market`.
    let quoter = quoter_pda(0, &clob_id(), &clob_maker_user);
    set_trading_state(&mut svm, &admin.pubkey());
    set_oracle(&mut svm, oracle, (100 * PRICE_PRECISION) as i64, 10);
    set_trading_perp_market(&mut svm, oracle);
    set_quote_spot_market(&mut svm);

    let clob_market = init_clob_book(&mut svm, &clob_admin);
    set_user_account(
        &mut svm,
        clob_maker_user,
        &trading_user(
            &clob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            None,
        ),
    );

    // `place_and_make_perp_order_v1` (the maker-rest path) checks the maker's
    // `UserStats`, so it must exist at the address `is_stats_for_user` derives.
    let clob_maker_stats = Pubkey::find_program_address(
        &[b"user_stats", clob_maker_authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(&mut svm, clob_maker_stats, &clob_maker_authority.pubkey());
    let registered = register_clob_quoter(&mut svm, &admin, clob_maker_user, clob_market);
    assert_eq!(registered, quoter);

    Fixture {
        svm,
        admin,
        keeper,
        oracle,
        clob_market,
        quoter,
        quoter_slab: quoter_slab_pda(0),
        clob_maker_user,
        clob_maker_authority,
        clob_admin,
        crank_block_offset: 0,
    }
}

/// Place a 0.5-unit CLOB ask at `price` through the velocity adapter and
/// return the order ref from the tx return data.
/// The maker's `UserStats` at the address `is_stats_for_user` will accept.
fn maker_stats_address(fixture: &Fixture) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0
}

/// A second maker on the book, beside the fixture's own.
///
/// Its `User` sits at its real PDA: a resolver derives a maker's address
/// from the node's (authority, sub-account) identity, not from the node.
struct ClobMaker {
    authority: Keypair,
    user: Pubkey,
}

fn add_clob_maker(fixture: &mut Fixture) -> ClobMaker {
    let authority = Keypair::new();
    fixture
        .svm
        .airdrop(&authority.pubkey(), 10_000_000_000)
        .unwrap();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_account(
        &mut fixture.svm,
        user,
        &trading_user(
            &authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            None,
        ),
    );

    set_user_stats_account(&mut fixture.svm, stats, &authority.pubkey());
    ClobMaker { authority, user }
}

/// `place_clob_ask`, resting from a maker other than the fixture's own.
fn place_clob_ask_from(
    fixture: &mut Fixture,
    maker: &ClobMaker,
    price: u64,
    size: u64,
) -> ClobOrderRefV0 {
    let ix = place_clob_order_ix(
        maker.user,
        &maker.authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price,
            base_asset_amount: size,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let meta = send(&mut fixture.svm, &maker.authority, ix, &[]).unwrap();
    let data = &meta.return_data.data;
    ClobOrderRefV0 {
        node_index: u32::from_le_bytes(data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(data[4..12].try_into().unwrap()),
    }
}

/// `set_user_stats_account` with the authority-wide equity breaker latched.
fn set_tripped_user_stats(svm: &mut litesvm::LiteSVM, address: Pubkey, authority: &Pubkey) {
    let mut stats: UserStats = Zeroable::zeroed();
    stats.authority = anchor_lang::prelude::Pubkey::new_from_array(*authority.as_array());
    stats.number_of_sub_accounts = 1;
    stats.set_equity_breaker_tripped(true);
    set_zero_copy_account(
        svm,
        address,
        UserStats::DISCRIMINATOR,
        &stats,
        UserStats::SIZE,
    );
}

/// A ref declaring the ask side, which is what `place_clob_ask` rests.
fn ask_ref(order_ref: ClobOrderRefV0) -> velocity::instructions::ForceCancelClobRefV0 {
    velocity::instructions::ForceCancelClobRefV0 {
        order_ref,
        side: velocity::state::prop_amm::ClobSide::Ask,
    }
}

#[allow(clippy::too_many_arguments)]
fn force_cancel_clob_ix(
    fixture: &Fixture,
    filler_user: Pubkey,
    filler_stats: Pubkey,
    user: Pubkey,
    user_stats: Pubkey,
    authority: Pubkey,
    order_refs: Vec<velocity::instructions::ForceCancelClobRefV0>,
) -> Instruction {
    let mut accounts = velocity::accounts::ForceCancelClobOrders {
        state: state_pda(),
        authority,
        filler: filler_user,
        filler_stats,
        user,
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::ForceCancelClobOrders {
            args: ForceCancelClobOrdersArgs {
                market_index: 0,
                order_refs,
            },
        }
        .data(),
    }
}

fn place_clob_ask(fixture: &mut Fixture, price: u64, size: u64) -> ClobOrderRefV0 {
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price,
            base_asset_amount: size,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let meta = send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let data = &meta.return_data.data;
    ClobOrderRefV0 {
        node_index: u32::from_le_bytes(data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(data[4..12].try_into().unwrap()),
    }
}

/// The speed bump replaced JIT; skipping it is attested flow only. A
/// below-default activation delay must fail without the flow authority
/// co-signing the transaction, and pass with it — while at-or-above the
/// default stays permissionless.
/// `UpdateMarketArgsV0` setting only `default_activation_delay_slots`:
/// twelve `Option`s, each a presence byte, in the order the book declares them.
fn set_clob_default_activation_delay(fixture: &mut Fixture, slots: u32) {
    let mut args = Vec::new();
    for _ in 0..4 {
        args.push(0u8); // tick size, step size, min order size, blocking min size
    }

    args.push(1u8);
    args.extend_from_slice(&slots.to_le_bytes());
    for _ in 0..7 {
        args.push(0u8); // max activation delay, grace, evict threshold, ceilings,
                        // reservation grace
    }

    let admin = fixture.clob_admin.insecure_clone();
    let ix = clob_ix(
        "update_market_v0",
        args,
        vec![
            AccountMeta::new(fixture.clob_market, false),
            AccountMeta::new_readonly(admin.pubkey(), true),
            // The optional new place authority, absent: encoded as the
            // program id.
            AccountMeta::new_readonly(clob_id(), false),
        ],
    );

    send(&mut fixture.svm, &admin, ix, &[]).unwrap();

    // Velocity's gates read the attach-written mirror in the slab's book
    // slot, not the book. Production re-runs `update_perp_market_clob_quoter`
    // after a rules change; the fixture writes the mirror directly, on the
    // staging entry and on the live copy.
    let mut quoter: velocity::state::prop_amm::QuoterV0 =
        read_zero_copy(&fixture.svm, &fixture.quoter);
    quoter.config.book_default_activation_delay_slots = slots;
    set_zero_copy_account(
        &mut fixture.svm,
        fixture.quoter,
        velocity::state::prop_amm::QuoterV0::DISCRIMINATOR,
        &quoter,
        velocity::state::prop_amm::QuoterV0::SIZE,
    );

    let mut slot = read_slab_slot(&fixture.svm, 0, 0);
    slot.config.book_default_activation_delay_slots = slots;
    write_slab_slot(&mut fixture.svm, 0, 0, &slot);
}

#[test]
fn fast_activation_requires_the_flow_authority_attestation() {
    use velocity::state::state::HotRole;

    let mut fixture = setup();
    // The fixture's book has a zero default (every test placement is
    // "fast"); raise it through the book's own admin instruction so
    // below-default is expressible.
    set_clob_default_activation_delay(&mut fixture, 2);

    let place = |fixture: &Fixture, delay: Option<u32>, flow: Option<Pubkey>| {
        let mut ix = place_clob_order_ix(
            fixture.clob_maker_user,
            &fixture.clob_maker_authority,
            fixture.quoter_slab,
            fixture.clob_market,
            fixture.oracle,
            PlaceClobOrderParams {
                market_index: 0,
                direction: PositionDirection::Short,
                price: 105 * PRICE,
                base_asset_amount: UNIT,
                max_ts: 0,
                activation_delay_slots: delay,
                reject_if_crossed: false,
            },
        );

        if let Some(flow_key) = flow {
            // The optional `flow_authority` slot is encoded as a program-id
            // placeholder; provide the account, as a signer — presence of
            // the signing flow authority is the attestation.
            let placeholder = ix
                .accounts
                .iter()
                .rposition(|meta| meta.pubkey == velocity_id() && !meta.is_writable)
                .expect("optional placeholder present");
            ix.accounts[placeholder] = AccountMeta::new_readonly(flow_key, true);
        }

        ix
    };

    // At-or-above the default: permissionless, exactly as before.
    let keeper = fixture.clob_maker_authority.insecure_clone();
    let default_delay_ix = place(&fixture, None, None);
    let at_default_ix = place(&fixture, Some(2), None);
    send(&mut fixture.svm, &keeper, default_delay_ix, &[]).unwrap();
    send(&mut fixture.svm, &keeper, at_default_ix, &[]).unwrap();

    // Below the default with no flow authority named: refused.
    let no_flow_ix = place(&fixture, Some(0), None);
    let err = send(&mut fixture.svm, &keeper, no_flow_ix, &[]).unwrap_err();
    assert_velocity_error(&err, ErrorCode::UnattestedFastActivation);

    // A signer that is not the configured flow authority fails the account
    // constraint — and with no flow authority configured, the zero key on
    // `State` matches no signer at all.
    let flow = Keypair::new();
    fixture.svm.airdrop(&flow.pubkey(), 1_000_000_000).unwrap();
    let impostor_ix = place(&fixture, Some(0), Some(flow.pubkey()));
    let err = send(&mut fixture.svm, &keeper, impostor_ix, &[&flow]).unwrap_err();
    assert_velocity_error(&err, ErrorCode::UnattestedFastActivation);

    // Configure the flow authority; the same signer now attests the fast
    // placement and it lands.
    let mut state: State = read_zero_copy(&fixture.svm, &state_pda());
    state.set_hot_key(HotRole::FlowAuthority, flow.pubkey());
    set_zero_copy_account(
        &mut fixture.svm,
        state_pda(),
        State::DISCRIMINATOR,
        &state,
        State::SIZE,
    );

    let attested_ix = place(&fixture, Some(0), Some(flow.pubkey()));
    send(&mut fixture.svm, &keeper, attested_ix, &[&flow]).unwrap();
}

/// A keeper cannot route around the book by leaving its maker's accounts at
/// home.
///
/// This is the shape of the attack: whoever assembles the transaction also
/// wants the flow the book would have taken. Carrying the CLOB's registry
/// entry satisfies both `require_baseline` and the signed route, because both
/// check that an entry is *present*. Presence is not what decides whether a
/// book can trade: its liquidity is only reachable for users the transaction
/// loaded, so a live book with no maker accounts would otherwise be
/// indistinguishable from a dead one.
///
/// What stops it is the filler's obligation. The book reports the depth it
/// withheld, the taker did not sign this transaction, and the transaction had
/// room for the two accounts that maker needed. So the whole fill is refused,
/// including the part that would have settled, and the assembler gets nothing
/// rather than leaving the taker short.
///
/// A signed-message order drives it because that is the taker route a keeper
/// assembles: the taker signs the message and the keeper signs the
/// transaction. A taker that signs its own transaction chose its account list
/// and is owed no obligation.
#[test]
fn a_fill_that_leaves_out_a_reachable_book_maker_is_refused() {
    use {
        anchor_lang::AnchorSerialize,
        velocity::state::order_params::{OrderParams, PostOnlyParam, SignedMsgOrderParamsMessage},
    };

    let mut fixture = setup();
    // Two makers on the book. The carried one is the better price, so the
    // walk fills it and then reaches the one the transaction left at home.
    let carried_stats = maker_stats_address(&fixture);
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let withheld = add_clob_maker(&mut fixture);
    place_clob_ask_from(&mut fixture, &withheld, 100 * PRICE, UNIT / 2);
    assert_eq!(clob_ask_count(&fixture), 2);

    // Well past the book's grace window: the ask went on at slot 10, so by now
    // no keeper can claim it had not heard about it. Inside the window the
    // book skips an uncarried maker instead of reporting it withheld, and the
    // obligation has nothing to answer for.
    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);
    set_signed_msg_user_orders(&mut fixture.svm, &taker.authority.pubkey(), 8);

    let order = OrderParams {
        order_type: OrderType::Market,
        market_type: MarketType::Perp,
        direction: PositionDirection::Long,
        base_asset_amount: UNIT,
        // Both book levels sit inside the taker's worst price, so the walk
        // reaches the deeper one.
        price: 101 * PRICE,
        market_index: 0,
        post_only: PostOnlyParam::None,
        ..OrderParams::default()
    };
    let message = SignedMsgOrderParamsMessage {
        signed_msg_order_params: order,
        sub_account_id: 0,
        slot: 30,
        uuid: *b"omitmakr",
        take_profit_order_params: None,
        stop_loss_order_params: None,
        max_margin_ratio: None,
        builder_idx: None,
        builder_fee_tenth_bps: None,
        isolated_position_deposit: None,
        // The anchor-test build carries no mainnet feature, so it names the
        // devnet cluster and refuses a message that names none.
        network: Some(velocity::state::order_params::expected_signed_msg_network()),
        route: None,
    };
    let mut borsh_body = vec![0u8; 8];
    message.serialize(&mut borsh_body).unwrap();
    let hex_msg = hex_lower(&borsh_body);
    let signature = taker.authority.sign_message(hex_msg.as_bytes());
    let mut envelope = Vec::new();
    envelope.extend_from_slice(signature.as_ref());
    envelope.extend_from_slice(&taker.authority.pubkey().to_bytes());
    envelope.extend_from_slice(&(hex_msg.len() as u16).to_le_bytes());
    envelope.extend_from_slice(hex_msg.as_bytes());

    let mut accounts = velocity::accounts::PlaceSignedMsgTakerOrder {
        state: state_pda(),
        user: taker.user,
        user_stats: taker.stats,
        signed_msg_user_orders: signed_msg_user_orders_pda(&taker.authority.pubkey()),
        authority: keeper.authority.pubkey(),
        ix_sysvar: instructions_sysvar(),
        filler: keeper.user,
        filler_stats: keeper.stats,
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // The whole attack: the deeper maker is left at home, so the taker gets
    // half a fill and the assembler keeps the other half for a source of its
    // own. The quoter section is carried exactly as `require_baseline` and a
    // signed route demand.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(carried_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceSignedMsgTakerOrder {
            signed_msg_order_params_message_bytes: envelope,
            is_delegate_signer: false,
            flow_attestation: None,
        }
        .data(),
    };
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper.authority,
        &[compute_unit_limit_ix(600_000), ix],
        &[],
    )
    .unwrap_err();
    assert_velocity_error(&err, ErrorCode::FillerOmittedReachableMaker);

    // Nothing moved, and the book still holds what it was holding.
    let taker_state: User = read_zero_copy(&fixture.svm, &taker.user);
    assert_eq!(taker_state.perp_positions[0].base_asset_amount, 0);
    let carried: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        carried.perp_positions[0].base_asset_amount, 0,
        "the carried maker got nothing either: the whole fill is refused"
    );
    assert_eq!(clob_ask_count(&fixture), 2);
}

/// A taker that signs its own transaction chose the account list, so no filler
/// obligation applies and the fill proceeds.
///
/// It then fills in full at the price that is present. Withheld depth takes no
/// part of the division: the book stops its own walk, which keeps the aged
/// order's place in the queue, and the taker's size goes to the sources that
/// can settle. Holding size back for a price this transaction cannot reach
/// would leave the taker short of a fill it could have had.
#[test]
fn a_taker_that_signs_fills_in_full_at_the_price_present() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();

    // Two makers on the book. The carried one is the better price, so the
    // book reaches the withheld one only after the first is exhausted.
    let carried_stats = maker_stats_address(&fixture);
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let withheld = add_clob_maker(&mut fixture);
    place_clob_ask_from(&mut fixture, &withheld, 100 * PRICE, UNIT / 2);
    assert_eq!(clob_ask_count(&fixture), 2);

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // Only the better-priced maker is carried.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(carried_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT,
                    price: 105 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    // The taker is filled in full: the carried maker's half, and the vAMM for
    // the rest. The withheld maker took no part.
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount, UNIT as i64,
        "filled in full"
    );

    let carried: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        carried.perp_positions[0].base_asset_amount,
        -((UNIT / 2) as i64),
        "the carried maker is short its half"
    );

    let held: User = read_zero_copy(&fixture.svm, &withheld.user);
    assert_eq!(
        held.perp_positions[0].base_asset_amount, 0,
        "the withheld maker took no part"
    );
    assert_eq!(
        clob_ask_count(&fixture),
        1,
        "the withheld maker keeps its place in the queue"
    );
}

/// Place a bid through velocity, so a sweep has both sides to take.
fn place_clob_bid(fixture: &mut Fixture, price: u64, size: u64) -> ClobOrderRefV0 {
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price,
            base_asset_amount: size,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let meta = send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let data = &meta.return_data.data;
    ClobOrderRefV0 {
        node_index: u32::from_le_bytes(data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(data[4..12].try_into().unwrap()),
    }
}

fn cancel_all_clob_ix(fixture: &Fixture, sides: ClobCancelSides) -> Instruction {
    Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrdersV1 {
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrdersV1 {
            params: CancelOrdersV1Params {
                market_index: 0,
                sides,
            },
        }
        .data(),
    }
}

/// The end-to-end property: a whole ladder comes off the book in one
/// instruction, and the maker's aggregates land exactly where the same orders
/// cancelled one at a time would have left them.
#[test]
fn cancel_all_clob_orders_unwinds_a_whole_ladder_in_one_instruction() {
    let mut fixture = setup();
    let mut bid_base = 0u64;
    let mut ask_base = 0u64;
    for i in 0..5u64 {
        // Distinct sizes per level, each aligned to the market step of 1000.
        bid_base += UNIT / 2 + i * 1000;
        ask_base += UNIT / 4 + i * 1000;
        place_clob_bid(&mut fixture, (98 - i) * PRICE, UNIT / 2 + i * 1000);
        place_clob_ask(&mut fixture, (99 + i) * PRICE, UNIT / 4 + i * 1000);
    }

    let before: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(before.perp_positions[0].open_orders, 10);
    assert_eq!(before.open_orders, 10);
    assert_eq!(before.perp_positions[0].open_bids, bid_base as i64);
    assert_eq!(before.perp_positions[0].open_asks, -(ask_base as i64));

    // Bids only first, so the ask side proves the sweep is side-scoped all the
    // way through velocity's unwind.
    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Bids);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let after_bids: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(after_bids.perp_positions[0].open_bids, 0);
    assert_eq!(after_bids.perp_positions[0].open_asks, -(ask_base as i64));
    assert_eq!(after_bids.perp_positions[0].open_orders, 5);
    assert_eq!(after_bids.open_orders, 5);
    assert_eq!(clob_bid_count(&fixture), 0);
    assert_eq!(clob_ask_count(&fixture), 5);

    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Both);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let after: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(after.perp_positions[0].open_bids, 0);
    assert_eq!(after.perp_positions[0].open_asks, 0);
    assert_eq!(after.perp_positions[0].open_orders, 0);
    assert_eq!(after.open_orders, 0);
    assert!(!after.has_open_order);
    assert_eq!(clob_ask_count(&fixture), 0);

    // Idempotent: nothing left to take is a success, not a failed transaction.
    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Both);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
}

/// A second funded maker on the same book, so a sweep can be shown to leave
/// someone else's orders alone.
fn second_clob_maker(fixture: &mut Fixture) -> (Pubkey, Keypair) {
    let authority = Keypair::new();
    fixture
        .svm
        .airdrop(&authority.pubkey(), 10_000_000_000)
        .unwrap();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_account(
        &mut fixture.svm,
        user,
        &trading_user(
            &authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            None,
        ),
    );

    // place_and_make_perp_order_v1 reads the maker's UserStats; init it.
    let stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(&mut fixture.svm, stats, &authority.pubkey());
    (user, authority)
}

/// A sweep must leave another maker's orders — and their aggregates — alone.
/// The book stores maker identity per node with no per-user index, so "take
/// only this user's" is a filter on a shared walk, which is exactly the thing
/// worth pinning end to end.
#[test]
fn cancel_all_clob_orders_only_takes_the_signing_users_orders() {
    let mut fixture = setup();
    let (other_user, other_authority) = second_clob_maker(&mut fixture);

    // Interleaved at the same price, so the survivors sit either side of the
    // orders being taken rather than in a contiguous run.
    for _ in 0..2 {
        place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
        let ix = place_clob_order_ix(
            other_user,
            &other_authority,
            fixture.quoter_slab,
            fixture.clob_market,
            fixture.oracle,
            PlaceClobOrderParams {
                market_index: 0,
                direction: PositionDirection::Short,
                price: 99 * PRICE,
                base_asset_amount: UNIT / 5,
                max_ts: 0,
                activation_delay_slots: Some(0),
                reject_if_crossed: false,
            },
        );

        send(&mut fixture.svm, &other_authority, ix, &[]).unwrap();
    }

    assert_eq!(clob_ask_count(&fixture), 4);

    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Both);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();

    assert_eq!(clob_ask_count(&fixture), 2);
    let mine: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(mine.perp_positions[0].open_asks, 0);
    assert_eq!(mine.perp_positions[0].open_orders, 0);
    let theirs: User = read_zero_copy(&fixture.svm, &other_user);
    assert_eq!(
        theirs.perp_positions[0].open_asks,
        -((2 * (UNIT / 5)) as i64)
    );
    assert_eq!(theirs.perp_positions[0].open_orders, 2);
}

/// The reason this instruction exists, in numbers: sweeping a ladder must cost
/// far less than cancelling it order by order, and — unlike the per-order route
/// — must not grow much with the ladder's depth, since the aggregate unwind is
/// the same two calls however many orders came off.
#[test]
fn cu_bench_cancel_all_beats_cancelling_order_by_order() {
    const LADDER: u64 = 8;

    // Baseline: one `cancel_clob_order` per resting order.
    let mut fixture = setup();
    let refs: Vec<ClobOrderRefV0> = (0..LADDER)
        .map(|i| place_clob_ask(&mut fixture, (99 + i) * PRICE, UNIT / 4))
        .collect();
    let per_order_cu: u64 = refs
        .iter()
        .map(|order_ref| {
            let ix = Instruction {
                program_id: velocity_id(),
                accounts: velocity::accounts::CancelOrderV1 {
                    perp_market: perp_market_pda(0),
                    user: fixture.clob_maker_user,
                    authority: fixture.clob_maker_authority.pubkey(),
                    quoter_slab: fixture.quoter_slab,
                    clob_market: fixture.clob_market,
                    clob_program: clob_id(),
                }
                .to_account_metas(None),
                data: velocity::instruction::CancelOrderV1 {
                    params: CancelOrderV1Params {
                        market_index: 0,
                        order_ref: *order_ref,
                    },
                }
                .data(),
            };

            send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[])
                .unwrap()
                .compute_units_consumed
        })
        .sum();

    // The sweep, at one order and at the full ladder.
    let mut fixture = setup();
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 4);
    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Both);
    let one_order_cu = send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[])
        .unwrap()
        .compute_units_consumed;

    let mut fixture = setup();
    for i in 0..LADDER {
        place_clob_ask(&mut fixture, (99 + i) * PRICE, UNIT / 4);
    }

    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Both);
    let ladder_cu = send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[])
        .unwrap()
        .compute_units_consumed;

    println!(
        "CU — cancel_all_clob_orders: 1 order {one_order_cu}, {LADDER} orders {ladder_cu}; \
         baseline {LADDER}× cancel_clob_order {per_order_cu}"
    );

    assert!(
        ladder_cu * 4 < per_order_cu,
        "sweeping {LADDER} orders ({ladder_cu}) must be far cheaper than cancelling them \
         one at a time ({per_order_cu})"
    );

    // The per-order route pays a whole instruction per order; this pays a walk
    // hop. Depth must therefore be nearly free by comparison.
    assert!(
        ladder_cu < one_order_cu * 2,
        "sweep cost must be dominated by the fixed instruction, not the ladder: \
         1 order {one_order_cu}, {LADDER} orders {ladder_cu}"
    );
}

#[test]
fn cancel_clob_order_unwinds_the_reserved_aggregates() {
    let mut fixture = setup();
    let order_ref = place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrderV1 {
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrderV1 {
            params: CancelOrderV1Params {
                market_index: 0,
                order_ref,
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();

    let clob_maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(clob_maker.perp_positions[0].open_asks, 0);
    assert_eq!(clob_maker.perp_positions[0].open_orders, 0);
    assert_eq!(clob_maker.open_orders, 0);
    assert_eq!(clob_ask_count(&fixture), 0);

    // The stale ref fails closed on a second cancel.
    let ix2 = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrderV1 {
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrderV1 {
            params: CancelOrderV1Params {
                market_index: 0,
                order_ref,
            },
        }
        .data(),
    };

    assert!(send(&mut fixture.svm, &fixture.clob_maker_authority, ix2, &[]).is_err());
}

/// Pulling the book's approval suspends its slot: the config stays, nothing
/// quotes, and a maker can still pull orders off the killed book.
#[test]
fn a_maker_cancels_off_a_suspended_book() {
    let mut fixture = setup();
    let order_ref = place_clob_ask(&mut fixture, 99 * PRICE, UNIT);
    assert_eq!(clob_ask_count(&fixture), 1);

    // The admin pulls the book's approval.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: fixture.admin.pubkey(),
            state: state_pda(),
            quoter: fixture.quoter,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            quoter_program: clob_id(),
            quoter_program_data: Some(program_data_pda(&clob_id())),
            // A revocation reads no book.
            clob_market: None,
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved {
            args: UpdateQuoterApprovedArgs { approved: false },
        }
        .data(),
    };
    let admin = fixture.admin.insecure_clone();
    send(&mut fixture.svm, &admin, ix, &[]).unwrap();

    // The slot suspends in place rather than clearing: the removal paths
    // need the book binding.
    let slot = read_slab_slot(&fixture.svm, 0, 0);
    assert!(!slot.is_vacant());
    assert!(slot.suspended);
    assert!(!slot.quotes());

    // The cancel path is deliberately not gated on the slot's flags.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrderV1 {
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrderV1 {
            params: CancelOrderV1Params {
                market_index: 0,
                order_ref,
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    assert_eq!(clob_ask_count(&fixture), 0);
    let clob_maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(clob_maker.perp_positions[0].open_asks, 0);
    assert_eq!(clob_maker.perp_positions[0].open_orders, 0);
    assert_eq!(clob_maker.open_orders, 0);
}

/// A book order has no `User.orders` slot, so these records are the only
/// statement an order-history reader gets about it. The id in them is the one
/// velocity minted from the `User`'s own counter — the same counter its DLOB
/// orders draw from — which is what lets one reader file both without holding
/// a map between two id spaces.
#[test]
fn a_clob_orders_records_name_it_by_the_users_own_order_id() {
    use velocity::state::events::{OrderAction, OrderActionRecord, OrderRecord};

    let mut fixture = setup();
    let next_order_id = {
        let user: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
        user.next_order_id
    };

    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 99 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let meta = send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let data = &meta.return_data.data;
    let order_ref = ClobOrderRefV0 {
        node_index: u32::from_le_bytes(data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(data[4..12].try_into().unwrap()),
    };

    let placed: Vec<OrderRecord> = events(&meta);
    assert_eq!(placed.len(), 1, "one placement, one record");
    assert_eq!(placed[0].user, fixture.clob_maker_user);
    assert_eq!(placed[0].order.order_id, next_order_id);
    assert_eq!(placed[0].order.price, 99 * PRICE);
    assert_eq!(placed[0].order.base_asset_amount, UNIT / 2);
    assert_eq!(placed[0].order.status, OrderStatus::Open);
    assert!(placed[0].order.is_placed_on_clob());
    // A resting book order settles at its own price on the maker schedule, so
    // it reports as one.
    assert!(placed[0].order.post_only);
    // The counter moved, so the next order gets its own id.
    let user: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(user.next_order_id, next_order_id + 1);

    let cancel = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrderV1 {
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrderV1 {
            params: CancelOrderV1Params {
                market_index: 0,
                order_ref,
            },
        }
        .data(),
    };
    let meta = send(&mut fixture.svm, &fixture.clob_maker_authority, cancel, &[]).unwrap();

    let cancelled: Vec<OrderActionRecord> = events(&meta);
    assert_eq!(cancelled.len(), 1, "one cancel, one record");
    assert!(cancelled[0].action == OrderAction::Cancel);
    // The book order is the maker half, and the id round-tripped through the
    // book unchanged.
    assert_eq!(cancelled[0].maker, Some(fixture.clob_maker_user));
    assert_eq!(cancelled[0].maker_order_id, Some(next_order_id));
    assert_eq!(cancelled[0].maker_order_base_asset_amount, Some(UNIT / 2));
    assert_eq!(cancelled[0].taker, None);
}

/// The mandatory baseline: a router fill that omits the market's CLOB quoter
/// from its account tail fails, even though the vAMM alone could fill the
/// order.
///
/// The named `quoter_slab` and `clob_market` accounts are not the baseline.
/// The route is assembled from the remaining accounts, and a quoter's
/// registered CPI accounts are resolved out of that tail — so a tail without
/// them is a fill that consulted no book.
#[test]
fn router_fill_without_the_markets_clob_quoter_fails() {
    let mut fixture = setup();

    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let base_accounts = |fixture: &Fixture| {
        let mut accounts = velocity::accounts::PlaceAndTakeV1 {
            state: state_pda(),
            user: taker_user,
            user_stats: taker_stats,
            authority: taker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            flow_authority: None,
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts
    };
    let fill_ix = |accounts: Vec<AccountMeta>| Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT,
                    price: 105 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    // No quoter section at all: without the slab the baseline cannot even be
    // answered, so the fill must fail.
    let ix = fill_ix(base_accounts(&fixture));
    let err = send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("baseline must fail");
    let logs = format!("{:?}", err.meta.logs);
    assert!(
        logs.contains("the fill must carry the quoter slab"),
        "unexpected failure: {logs}"
    );

    // The slab alone is not enough either: the book's slot can quote, so the
    // fill must also carry its response account to consult it.
    let mut accounts = base_accounts(&fixture);
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    let err = send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), fill_ix(accounts)],
        &[],
    )
    .expect_err("baseline must fail");
    let logs = format!("{:?}", err.meta.logs);
    assert!(
        logs.contains("must include the market's CLOB quoter"),
        "unexpected failure: {logs}"
    );
}

/// Place a CLOB ask with an expiry through the adapter, expire it, and crank
/// the removal through velocity: the maker's reserved aggregates unwind and
/// the keeper earns the flat reward from the maker.
#[test]
fn crank_remove_expired_unwinds_aggregates_and_pays_the_keeper() {
    let mut fixture = setup();

    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 99 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: clock.unix_timestamp + 10,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let meta = send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let order_ref = velocity::state::prop_amm::ClobOrderRefV0 {
        node_index: u32::from_le_bytes(meta.return_data.data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(meta.return_data.data[4..12].try_into().unwrap()),
    };

    // Past expiry.
    let mut clock: solana_clock::Clock = fixture.svm.get_sysvar();
    clock.unix_timestamp += 20;
    fixture.svm.set_sysvar(&clock);

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CrankClobOrderRemoval {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobRemoveExpired {
            args: CrankClobRemoveExpiredArgs {
                market_index: 0,
                order_ref,
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &fixture.keeper, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
    // Flat reward moved maker → keeper (perps_default flat_filler_fee).
    assert!(maker.perp_positions[0].quote_asset_amount < 0);
    let filler: User = read_zero_copy(&fixture.svm, &filler_user);
    assert!(filler.perp_positions[0].quote_asset_amount > 0);
}

/// The evict crank removes the side's tail once the soft cap is hit
/// (threshold 1 in this fixture) and unwinds the maker the same way.
#[test]
fn crank_evict_unwinds_the_tails_aggregates() {
    let mut fixture = setup();
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CrankClobOrderRemoval {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobEvict {
            args: CrankClobEvictArgs {
                market_index: 0,
                side: velocity::state::prop_amm::ClobSide::Ask,
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &fixture.keeper, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
}

/// The quote view, end to end: one simulated instruction returns verified
/// books for every source — the CLOB and a midpoint spline (each via a real
/// `quote_v0` CPI) and the vAMM (quoted off a copy) — in fill order, so the
/// vAMM's last-look shading is already applied.
#[test]
fn quote_router_returns_verified_books_for_every_source() {
    let mut fixture = setup();

    // CLOB ask 0.5 @ 99 through the adapter.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    // A midpoint spline quoting 0.5 at mid + 10bps = 100.1.
    let midpoint =
        setup_midpoint_maker(&mut fixture, 10_000 * SPOT_BALANCE_PRECISION_U64, UNIT / 2);

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    // The router's own quote buffer. Pre-created by the caller because it is
    // larger than a CPI can allocate (the CLOB market is created the same way).
    let router = Keypair::new();
    fixture
        .svm
        .airdrop(&router.pubkey(), 10_000_000_000)
        .unwrap();
    let quote_buffer = Pubkey::new_unique();
    fixture
        .svm
        .set_account(
            quote_buffer,
            Account {
                lamports: 10_000_000_000,
                data: vec![0u8; velocity::state::router_quote::RouterQuoteBufferV0::SIZE],
                owner: velocity_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeRouterQuoteBuffer {
            quote_buffer,
            authority: router.pubkey(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeRouterQuoteBuffer {
            args: InitializeRouterQuoteBufferArgs { market_index: 0 },
        }
        .data(),
    };

    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    let mut accounts = velocity::accounts::QuoteRouter {
        state: state_pda(),
        authority: router.pubkey(),
        quote_buffer,
    }
    .to_account_metas(None);
    // Maps.
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // Maker section: the midpoint's quoted user, whose account the clamp
    // needs (read-only is fine for a quote).
    accounts.push(AccountMeta::new(midpoint.user, false));
    accounts.push(AccountMeta::new(midpoint.stats, false));
    // Quoter section: the market's slab + both quoters' CPI accounts.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(midpoint.instance, false));
    accounts.push(AccountMeta::new_readonly(midpoint_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                taker_served_window: true,
                market_index: 0,
                direction: Direction::Long,
                size: 2 * UNIT,
                include_vamm: true,
            },
        }
        .data(),
    };
    let meta = send(&mut fixture.svm, &router, ix, &[]).unwrap();

    let buffer: velocity::state::router_quote::RouterQuoteBufferV0 =
        read_zero_copy(&fixture.svm, &quote_buffer);
    assert_eq!(buffer.market, 0);
    assert_eq!(buffer.quoted_size, 2 * UNIT);
    assert_eq!(buffer.direction, 0, "long");

    // Three sources, in fill order: CLOB quoter, midpoint quoter, vAMM last.
    assert_eq!(buffer.source_count, 3, "clob + midpoint + vamm");
    let sources = &buffer.sources[..3];
    use velocity::state::router_quote::QuotedSourceKind;
    assert_eq!(sources[0].kind, QuotedSourceKind::Quoter);
    assert_eq!(sources[0].key, fixture.quoter);
    assert_eq!(sources[1].kind, QuotedSourceKind::Quoter);
    assert_eq!(sources[1].key, midpoint.entry);
    assert_eq!(sources[2].kind, QuotedSourceKind::Vamm);

    // The CLOB's book came back through a real quote_v0 CPI: 0.5 @ 99.
    let clob_levels = &buffer.levels[0][..sources[0].level_count as usize];
    assert_eq!(clob_levels.len(), 1);
    assert_eq!(clob_levels[0].price, 99 * PRICE);
    assert_eq!(clob_levels[0].size, UNIT / 2);

    // The midpoint's spline rung: 0.5 at mid + 10bps.
    let mid_levels = &buffer.levels[1][..sources[1].level_count as usize];
    assert_eq!(mid_levels.len(), 1);
    assert_eq!(mid_levels[0].price, 100 * PRICE + PRICE / 10);
    assert_eq!(mid_levels[0].size, UNIT / 2);

    // The vAMM ladder priced against both as rivals, and every rung is
    // monotone at or above its top.
    let amm_levels = &buffer.levels[2][..sources[2].level_count as usize];
    assert!(!amm_levels.is_empty(), "vamm quoted something");
    assert!(amm_levels.windows(2).all(|w| w[0].price <= w[1].price));

    // Who each ladder stands on. The CLOB answers for itself through its
    // `quote_l3_v0` leg — one row per resting order, with the order's own id —
    // so nothing off chain has to decode the book to know whose accounts a
    // fill must carry.
    let clob_rows = &buffer.rows[sources[0].row_start as usize..][..sources[0].row_len as usize];
    assert_eq!(clob_rows.len(), 1);
    assert_eq!(clob_rows[0].price, 99 * PRICE);
    assert_eq!(clob_rows[0].size, UNIT / 2);
    assert_eq!(
        clob_rows[0].authority,
        fixture.clob_maker_authority.pubkey()
    );
    assert_eq!(clob_rows[0].sub_account_id, 0);
    assert_ne!(clob_rows[0].order_id, 0, "a book row is an order");

    // A quoter without an `l3` leg stands on its own quoted user, so the row
    // names that user and carries no order id of its own.
    let mid_rows = &buffer.rows[sources[1].row_start as usize..][..sources[1].row_len as usize];
    assert_eq!(mid_rows.len(), 1);
    assert_eq!(mid_rows[0].authority, midpoint.authority.pubkey());
    assert_eq!(mid_rows[0].order_id, 0);

    // The vAMM stands on nobody.
    assert_eq!(sources[2].row_len, 0);
    assert!(!buffer.rows_truncated);

    // Quoting must not move the market: the AMM is quoted off a copy.
    let market: velocity::state::perp_market::PerpMarket =
        read_zero_copy(&fixture.svm, &perp_market_pda(0));
    assert_eq!(
        market.amm.base_asset_amount_with_amm,
        (AMM_RESERVE_PRECISION / 2) as i128,
        "quote is read-only"
    );

    println!(
        "CU — quote_router across CLOB + DLOB + vAMM: {}",
        meta.compute_units_consumed
    );
}

// --- program-keeper (relay) crank mode ---

fn crank_conditions_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[b"clob_crank_conditions", 0u16.to_le_bytes().as_ref()],
        &velocity_id(),
    )
    .0
}

/// The protocol's own `User`: the pass-through taker of the cross crank.
///
/// It deposits nothing on purpose. A cross runs as two sequential fills,
/// so between them the protocol holds the full size as a position. The
/// crank closes it in the same call, so the protocol needs no capital.
/// Post-fill checks are suppressed so a balance cannot hide a regression.
fn set_protocol_user(svm: &mut litesvm::LiteSVM) -> Pubkey {
    let (signer, _) = velocity_signer_pda();
    let user = Pubkey::find_program_address(
        &[b"user", signer.as_ref(), 0u16.to_le_bytes().as_ref()],
        &velocity_id(),
    )
    .0;
    let stats = Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;
    set_user_account(svm, user, &trading_user(&signer, 0, None));
    set_user_stats_account(svm, stats, &signer);
    user
}

/// The book's default grace window, after which a crossing remainder's claim
/// stops being honoured and the depth it held is ordinary again. Mirrors
/// `DEFAULT_RESERVATION_GRACE_SLOTS` in the book program.
const RESERVATION_GRACE_SLOTS: u64 = 32;

/// Cost units to sync with when the test does not care what the sync costs.
const ANY_SYNC_COST_UNITS: u32 = 20_000;

/// Cost units to attach with when the test does not care what a crank costs.
/// Nonzero is all the attach requires; under the flat-per-signature rails the
/// fixture starts on, the figure does not reach the payment.
const ANY_CRANK_COST_UNITS: CrankCostUnitsV0 = CrankCostUnitsV0 {
    removal: 30_000,
    // Measured at 327,593 for a self-crossed book: the crank runs two whole
    // router fills, each with its own quote, split, execute and post-fill
    // checks. The figure is well past one instruction's 200,000 default, so a
    // caller has to request its budget.
    cross: 340_000,
    taker_origin_cross: 190_000,
    trigger: 40_000,
    liquidation: 120_000,
    force_cancel: 60_000,
    refill: 30_000,
};

/// Attaches the CLOB, which also creates the crank conditions account.
/// No separate init call exists. Sets the flat per-signature fee to
/// `keeper_payment_lamports`, so every crank costs exactly that number.
fn init_crank_conditions(fixture: &mut Fixture, keeper_payment_lamports: u64) -> Pubkey {
    init_crank_conditions_with_floor(fixture, keeper_payment_lamports, 1)
}

/// Put the exchange on a flat fee of `lamports` per signature.
fn set_flat_transaction_fee(fixture: &mut Fixture, lamports: u32) {
    let mut state: State = read_zero_copy(&fixture.svm, &state_pda());
    state.transaction_fee_rails = TransactionFeeRails {
        signature_lamports: lamports,
        ..TransactionFeeRails::FLAT_PER_SIGNATURE
    };

    set_zero_copy_account(
        &mut fixture.svm,
        state_pda(),
        State::DISCRIMINATOR,
        &state,
        State::SIZE,
    );
}

/// `min_cross_surplus` is the floor on what the protocol must net from a
/// cross-match crank. The attach requires it above zero, so the scenarios that
/// only care that a cross is profitable at all pass the smallest floor there is.
fn init_crank_conditions_with_floor(
    fixture: &mut Fixture,
    keeper_payment_lamports: u64,
    min_cross_surplus: u64,
) -> Pubkey {
    set_flat_transaction_fee(fixture, keeper_payment_lamports as u32);
    attach_clob(fixture, ANY_CRANK_COST_UNITS, min_cross_surplus)
}

/// The attach itself, with the cranks' cost units passed through.
fn attach_clob(
    fixture: &mut Fixture,
    crank_cost_units: CrankCostUnitsV0,
    min_cross_surplus: u64,
) -> Pubkey {
    let conditions = crank_conditions_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::AdminUpdatePerpMarketClobQuoter {
            admin: fixture.admin.pubkey(),
            state: state_pda(),
            perp_market: perp_market_pda(0),
            quoter: fixture.quoter,
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            crank_conditions: conditions,
            treasury: crank_treasury_pda(),
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdatePerpMarketClobQuoter {
            args: UpdatePerpMarketClobQuoterArgs {
                crank_cost_units,
                expire_fallback_slots: 100,
                min_cross_surplus,
            },
        }
        .data(),
    };
    let admin = fixture.admin.insecure_clone();
    let meta = send(&mut fixture.svm, &admin, ix, &[]).unwrap();
    // The book reports where its block sits; the registrant does not derive it.
    fixture.crank_block_offset = u32::from_le_bytes(meta.return_data.data[..4].try_into().unwrap());
    // The attach mirrors the book's placement rules onto the staging entry
    // and the live copy in the slab — the values `clob_market_config`
    // configured.
    let quoter: velocity::state::prop_amm::QuoterV0 = read_zero_copy(&fixture.svm, &fixture.quoter);
    assert_eq!(quoter.config.book_tick_size, 1, "attach mirrors the tick");
    assert_eq!(
        quoter.config.book_min_order_size, 1,
        "attach mirrors the minimum"
    );

    let live = read_slab_slot(&fixture.svm, 0, 0);
    assert_eq!(live.config.book_tick_size, 1, "the live copy gets the tick");
    assert_eq!(
        live.config.book_min_order_size, 1,
        "the live copy gets the minimum"
    );

    conditions
}

/// The book's condition slots, in the order the CLOB program declares them.
/// Restated here because the harness does not link the book's crate — the
/// litesvm run loads it as a deployed program, like anything else.
const BOOK_CRANK_EXPIRY: usize = 0;
const BOOK_CRANK_ACTIVATION: usize = 1;
const BOOK_CRANK_CAPACITY: usize = 2;
const BOOK_CRANK_CROSS: usize = 3;
const BOOK_CONDITIONS: usize = 4;

/// The book's own condition block, read the way a turner does: by account and
/// offset, with no knowledge of what else the market account holds.
fn book_conditions(fixture: &Fixture) -> Vec<velocity::relay_spec::ConditionV0> {
    assert_ne!(
        fixture.crank_block_offset, 0,
        "attach the book's cranks first"
    );

    let data = fixture.svm.get_account(&fixture.clob_market).unwrap().data;
    let (header, conditions) =
        velocity::relay_spec::read_block(&data[fixture.crank_block_offset as usize..], 0).unwrap();
    assert_eq!(header.num_conditions as usize, BOOK_CONDITIONS);
    conditions.to_vec()
}

/// The book's expiry wake.
fn book_expiry_wake(fixture: &Fixture) -> i64 {
    match book_conditions(fixture)[BOOK_CRANK_EXPIRY].wake() {
        Ok(velocity::relay_spec::WakeView::AtTimestamp { unix_ts }) => unix_ts,
        other => panic!("expiry condition is not a timestamp wake: {other:?}"),
    }
}

/// The book's activation wake.
fn book_activation_wake(fixture: &Fixture) -> u64 {
    match book_conditions(fixture)[BOOK_CRANK_ACTIVATION].wake() {
        Ok(velocity::relay_spec::WakeView::AtSlot { slot }) => slot,
        other => panic!("activation condition is not a slot wake: {other:?}"),
    }
}

/// The condition a turner says fired. Velocity's resolver reads which work to
/// look for out of this, so a test asking for a particular crank names its
/// slot the same way relay would.
fn fired_condition(
    target: Pubkey,
    block_offset: u32,
    index: u8,
) -> velocity::instructions::FiredConditionArgV0 {
    velocity::instructions::FiredConditionArgV0 {
        target,
        block_offset,
        index,
    }
}

/// Run a resolver as a real transaction (a turner would only simulate it)
/// and read the staged payload back exactly the way a turner does: response
/// pointer from return data, payload bytes from the account.
fn run_resolver(
    fixture: &mut Fixture,
    conditions: Pubkey,
    expire: bool,
) -> Option<velocity::relay_spec::ResolvedCrankV0> {
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::ResolveClobCrank {
            scratch: relay_scratch_pda(),
            crank_conditions: conditions,
            clob_market: fixture.clob_market,
            quoter_slab: fixture.quoter_slab,
            state: state_pda(),
            clob_program: clob_id(),
            treasury: crank_treasury_pda(),
        }
        .to_account_metas(None),
        // Relay names the condition that fired, and one resolver answers for
        // all of them — so which crank this asks for is the slot, not the
        // instruction.
        data: velocity::instruction::ResolveClobCrank {
            fired: fired_condition(
                fixture.clob_market,
                fixture.crank_block_offset,
                if expire {
                    velocity::state::prop_amm::CLOB_CRANK_SLOT_EXPIRY
                } else {
                    velocity::state::prop_amm::CLOB_CRANK_SLOT_CAPACITY
                },
            ),
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let pointer = velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
    if !pointer.has_work() {
        return None;
    }

    let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
    let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
    Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
}

/// The cross conditions' resolver, read back the way a turner does. One resolver
/// answers for the whole family: it stages `crank_taker_origin_cross` when a
/// migrated remainder is crossed and `crank_cross_match` otherwise, and the
/// staged payload is what says which.
fn run_cross_resolver(
    fixture: &mut Fixture,
    conditions: Pubkey,
) -> Option<velocity::relay_spec::ResolvedCrankV0> {
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::ResolveClobCrank {
            scratch: relay_scratch_pda(),
            crank_conditions: conditions,
            clob_market: fixture.clob_market,
            quoter_slab: fixture.quoter_slab,
            state: state_pda(),
            clob_program: clob_id(),
            treasury: crank_treasury_pda(),
        }
        .to_account_metas(None),
        data: velocity::instruction::ResolveClobCrank {
            fired: fired_condition(
                fixture.clob_market,
                fixture.crank_block_offset,
                velocity::state::prop_amm::CLOB_CRANK_SLOT_CROSS,
            ),
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let pointer = velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
    if !pointer.has_work() {
        return None;
    }

    let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
    let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
    Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
}

/// Submit a staged executor the way a turner does: the instruction is the
/// one the payload names, keeper placeholder substituted with the payout
/// account, every meta a non-signer (the fee payer is not in the account
/// list). `expected_disc` is the executor the caller means to be running —
/// asserted against the payload rather than substituted for it, so a
/// resolver that stages the wrong instruction fails here.
fn run_staged_executor(
    fixture: &mut Fixture,
    resolved: &velocity::relay_spec::ResolvedCrankV0,
    expected_disc: &[u8],
    payout: Pubkey,
) {
    assert_eq!(
        resolved.executor_program,
        velocity_id().to_bytes(),
        "velocity resolvers stage velocity executors"
    );
    assert_eq!(
        resolved.executor_disc, expected_disc,
        "staged executor is not the one this test means to run"
    );

    let accounts = resolved
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: if a.address == velocity::relay_spec::KEEPER_PLACEHOLDER {
                payout
            } else {
                Pubkey::new_from_array(a.address)
            },

            is_signer: false,
            is_writable: a.is_writable(),
        })
        .collect();
    let mut data = resolved.executor_disc.to_vec();
    data.extend_from_slice(&resolved.data);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data,
    };
    let keeper = fixture.keeper.insecure_clone();
    send_with_ixs(
        &mut fixture.svm,
        &keeper,
        &[compute_unit_limit_ix(1_400_000), ix],
        &[],
    )
    .unwrap();
}

/// The staged executor as an instruction, for a test that expects it to fail.
fn staged_executor_ix(
    resolved: &velocity::relay_spec::ResolvedCrankV0,
    payout: Pubkey,
) -> Instruction {
    let accounts = resolved
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: if a.address == velocity::relay_spec::KEEPER_PLACEHOLDER {
                payout
            } else {
                Pubkey::new_from_array(a.address)
            },

            is_signer: false,
            is_writable: a.is_writable(),
        })
        .collect();
    let mut data = resolved.executor_disc.to_vec();
    data.extend_from_slice(&resolved.data);
    Instruction {
        program_id: velocity_id(),
        accounts,
        data,
    }
}

/// The attach prices each crank off what it requests, and writes the same
/// figure into the condition a turner reads.
///
/// The two have to agree: the executor pays what the account holds, and relay
/// holds the keeper's balance growth to what the condition advertises. A
/// condition asking for more than its executor pays is a crank that does the
/// work and then reverts.
#[test]
fn each_crank_is_priced_from_what_it_requests_and_the_condition_agrees() {
    use velocity::state::{clob_crank::ClobCrankConditionsV0, state::TransactionFeeRails};

    let mut fixture = setup();
    // A rate on requested cost units, which is what separates the cranks.
    let mut state: State = read_zero_copy(&fixture.svm, &state_pda());
    state.transaction_fee_rails = TransactionFeeRails {
        inclusion_lamports: 2_500,
        signature_lamports: 0,
        resource_fee_numerator: 1,
        resource_fee_denominator: 2,
        max_priority_micro_lamports_per_cu: 0,
    };

    set_zero_copy_account(
        &mut fixture.svm,
        state_pda(),
        State::DISCRIMINATOR,
        &state,
        State::SIZE,
    );

    let units = CrankCostUnitsV0 {
        removal: 30_000,
        cross: 180_000,
        taker_origin_cross: 190_000,
        trigger: 40_000,
        liquidation: 120_000,
        force_cancel: 60_000,
        refill: 30_000,
    };
    let conditions_key = attach_clob(&mut fixture, units, 1);
    let conditions: ClobCrankConditionsV0 = read_zero_copy(&fixture.svm, &conditions_key);

    // 2,500 to be included, plus half a lamport per unit requested.
    assert_eq!(conditions.crank_payments.removal, 2_500 + 15_000);
    assert_eq!(conditions.crank_payments.cross, 2_500 + 90_000);
    assert_eq!(conditions.crank_payments.taker_origin_cross, 2_500 + 95_000);
    assert_eq!(conditions.crank_payments.trigger, 2_500 + 20_000);
    assert_eq!(conditions.crank_payments.liquidation, 2_500 + 60_000);
    assert_eq!(conditions.crank_payments.force_cancel, 2_500 + 30_000);

    // A market needs two relay watches, and the attach is what tells a
    // registrar where the second one goes: velocity's conditions account
    // holds one block at offset 8, the book holds the four that describe
    // itself. A registrar that watches only the first leaves every one of the
    // book's own cranks unwoken, so both regions are recorded here rather
    // than derived from the market account's layout.
    assert_eq!(
        conditions.clob_block_offset, fixture.crank_block_offset,
        "the attach records where the book's block sits"
    );

    assert_ne!(conditions.clob_block_offset, 0);
    assert_eq!(
        conditions.top_of_book_len, 8,
        "both u32 heads in one region"
    );
    assert_eq!(
        account_change(&book_conditions(&fixture)[BOOK_CRANK_CROSS]).1,
        conditions.top_of_book_offset,
        "the book's own cross watch and the recorded region are the same bytes"
    );

    // The attach carries the same figures onto the book, which is where the
    // conditions that watch its state live.
    let book = book_conditions(&fixture);
    assert_eq!(
        book[BOOK_CRANK_CAPACITY].min_payment(),
        u64::from(conditions.crank_payments.removal)
    );
    assert_eq!(
        book[BOOK_CRANK_EXPIRY].min_payment(),
        u64::from(conditions.crank_payments.removal)
    );

    // The cross conditions advertise the cheaper of the two crosses their
    // resolver can stage, so whichever it picks clears the floor.
    assert_eq!(
        book[BOOK_CRANK_CROSS].min_payment(),
        u64::from(conditions.crank_payments.cross)
    );
}

/// One resolver serves every CLOB crank condition, and the fired condition is
/// what picks the work.
///
/// The point of the merge is that a wake does not go looking for work the
/// condition did not describe: a capacity wake resolves an eviction and never
/// touches the cross path. A slot that velocity does not serve, or a target
/// that is neither the book nor the market's conditions, is refused rather
/// than read as some other slot.
#[test]
fn the_fired_condition_picks_which_crank_the_resolver_stages() {
    let mut fixture = setup();
    let conditions = init_crank_conditions(&mut fixture, 50_000);
    set_protocol_user(&mut fixture.svm);
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();

    // A single ask, expiring, on a book whose evict threshold is 1: both an
    // expiry and an eviction are available, so the slot alone decides which
    // the resolver answers with.
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 110 * PRICE,
            base_asset_amount: UNIT / 4,
            max_ts: clock.unix_timestamp + 10,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let maker = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &maker, ix, &[]).unwrap();
    let mut clock: solana_clock::Clock = fixture.svm.get_sysvar();
    clock.unix_timestamp += 20;
    fixture.svm.set_sysvar(&clock);

    let book = fixture.clob_market;
    let quoter_entry = fixture.quoter;
    let resolve = |fixture: &mut Fixture, target: Pubkey, index: u8| {
        let ix = Instruction {
            program_id: velocity_id(),
            accounts: velocity::accounts::ResolveClobCrank {
                scratch: relay_scratch_pda(),
                crank_conditions: conditions,
                clob_market: fixture.clob_market,
                quoter_slab: fixture.quoter_slab,
                state: state_pda(),
                clob_program: clob_id(),
                treasury: crank_treasury_pda(),
            }
            .to_account_metas(None),
            data: velocity::instruction::ResolveClobCrank {
                fired: fired_condition(target, fixture.crank_block_offset, index),
            }
            .data(),
        };
        let keeper = fixture.keeper.insecure_clone();
        send(&mut fixture.svm, &keeper, ix, &[])
    };

    let staged = |fixture: &Fixture, meta: &litesvm::types::TransactionMetadata| {
        let pointer =
            velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
        let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
        let bytes = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
        velocity::relay_spec::ResolvedCrankV0::read(bytes)
            .unwrap()
            .executor_disc
    };

    // Expiry slot -> the expiry crank.
    let meta = resolve(&mut fixture, book, 0).unwrap();
    assert_eq!(
        staged(&fixture, &meta),
        velocity::instruction::CrankClobRemoveExpired::DISCRIMINATOR,
        "the expiry slot stages the expiry crank"
    );

    // Capacity slot -> the eviction crank, off the same book and state.
    let meta = resolve(&mut fixture, book, 2).unwrap();
    assert_eq!(
        staged(&fixture, &meta),
        velocity::instruction::CrankClobEvict::DISCRIMINATOR,
        "the capacity slot stages the eviction crank"
    );

    // A slot the book does not host, and an account that hosts no block this
    // resolver serves, are both refused.
    assert!(resolve(&mut fixture, book, 9).is_err());
    assert!(resolve(&mut fixture, quoter_entry, 0).is_err());
}

/// A placement through velocity moves the book's own expiry wake, so relay is
/// told when the reclaim is due.
///
/// The two halves are owned by different programs and only meet on a real
/// pair of them: velocity forwards the order's `max_ts` on the CLOB wire, and
/// the book folds it into the condition it hosts. Neither program's own tests
/// can see the join, and when it breaks nothing fails — the order simply
/// rests until something else happens to wake a turner.
#[test]
fn a_placement_through_velocity_arms_the_books_expiry_wake() {
    let mut fixture = setup();
    init_crank_conditions(&mut fixture, 50_000);
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let max_ts = clock.unix_timestamp + 10;

    // Nothing expires yet, so the wake is set to never.
    assert_eq!(book_expiry_wake(&fixture), i64::MAX);

    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 99 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let maker = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &maker, ix, &[]).unwrap();

    assert_eq!(
        book_expiry_wake(&fixture),
        max_ts,
        "the order's expiry reached the book's own wake"
    );
}

/// The book re-arms its expiry wake for the next order after a reclaim.
///
/// The first expiry is easy: a fresh book folds it in. The one that matters is
/// the second, because a reclaim recomputes the wake to "never" first — and a
/// fold that does not publish over that leaves an order resting with nothing
/// scheduled to reclaim it. Nothing errors when it happens; the order just
/// sits there, which is why this is asserted rather than left to a crank test
/// noticing.
#[test]
fn the_books_expiry_wake_re_arms_after_a_reclaim() {
    let mut fixture = setup();
    const PAYMENT: u64 = 50_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();

    let place_expiring = |fixture: &mut Fixture, price: u64, max_ts: i64| {
        let ix = place_clob_order_ix(
            fixture.clob_maker_user,
            &fixture.clob_maker_authority,
            fixture.quoter_slab,
            fixture.clob_market,
            fixture.oracle,
            PlaceClobOrderParams {
                market_index: 0,
                direction: PositionDirection::Short,
                price,
                base_asset_amount: UNIT / 4,
                max_ts,
                activation_delay_slots: Some(0),
                reject_if_crossed: false,
            },
        );
        let maker = fixture.clob_maker_authority.insecure_clone();
        send(&mut fixture.svm, &maker, ix, &[]).unwrap();
    };

    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let first = clock.unix_timestamp + 10;
    place_expiring(&mut fixture, 110 * PRICE, first);
    assert_eq!(book_expiry_wake(&fixture), first);

    // Reclaim it. The recompute leaves no live expiry, so the wake goes quiet.
    let mut clock: solana_clock::Clock = fixture.svm.get_sysvar();
    clock.unix_timestamp = first + 1;
    fixture.svm.set_sysvar(&clock);
    let resolved = run_resolver(&mut fixture, conditions, true).expect("expired order is work");
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankClobRemoveExpired::DISCRIMINATOR,
        payout,
    );

    assert_eq!(book_expiry_wake(&fixture), i64::MAX);

    // The second order has to arm it again.
    let second = clock.unix_timestamp + 10;
    place_expiring(&mut fixture, 111 * PRICE, second);
    assert_eq!(
        book_expiry_wake(&fixture),
        second,
        "a placement after a reclaim re-arms the wake"
    );

    // And the crank finds it, so the whole loop repeats.
    let mut clock: solana_clock::Clock = fixture.svm.get_sysvar();
    clock.unix_timestamp = second + 1;
    fixture.svm.set_sysvar(&clock);
    assert!(
        run_resolver(&mut fixture, conditions, true).is_some(),
        "the second expiry is discoverable work"
    );
}

/// The full program-keeper expiry loop: init conditions, place an expiring
/// order through the adapter (min-folding the wake hint), resolve, and
/// submit the staged executor unsigned. The maker's reward accrues to the
/// protocol User, the payout account is paid reservoir lamports, and the
/// hint is repaired.
/// The refill moves treasury lamports into a low reservoir, pays the keeper
/// that ran it, and refuses to run again while the reservoir is full.
///
/// The refusal is the load-bearing half. Without it, anyone could refill a
/// full reservoir on repeat and draw the keeper payment each time — the
/// treasury paying to move its own lamports.
#[test]
fn refill_fills_a_low_reservoir_once_and_refuses_a_full_one() {
    let mut fixture = setup();
    const PAYMENT: u64 = 50_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    let payout = Pubkey::new_unique();
    // A turner's payout is a funded wallet. Credit it past rent exemption
    // first, or the runtime rejects the transaction for leaving a new account
    // rent-paying rather than for anything the crank did.
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let payout_before = fixture.svm.get_balance(&payout).unwrap();

    let refill_ix = |conditions: Pubkey| Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::RefillCrankReservoir {
            treasury: crank_treasury_pda(),
            crank_conditions: conditions,
            authority: payout,
        }
        .to_account_metas(None),
        data: velocity::instruction::RefillCrankReservoir {
            args: RefillCrankReservoirArgs { market_index: 0 },
        }
        .data(),
    };

    // A freshly attached market holds only its rent, which is below the
    // watermark, so the refill is due from the moment the market exists.
    let treasury_before = fixture.svm.get_balance(&crank_treasury_pda()).unwrap();
    let reservoir_before = fixture.svm.get_balance(&conditions).unwrap();
    let payer = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &payer, refill_ix(conditions), &[]).expect("an empty reservoir refills");

    let reservoir_after = fixture.svm.get_balance(&conditions).unwrap();
    let treasury_after = fixture.svm.get_balance(&crank_treasury_pda()).unwrap();
    assert!(
        reservoir_after > reservoir_before,
        "the reservoir was topped up"
    );
    assert_eq!(
        fixture.svm.get_balance(&payout).unwrap(),
        payout_before + refill_payment(&fixture, conditions),
        "the keeper that ran the refill was paid from the treasury"
    );

    // Everything the reservoir gained plus the keeper's fee left the treasury.
    assert_eq!(
        treasury_before - treasury_after,
        (reservoir_after - reservoir_before) + refill_payment(&fixture, conditions)
    );

    // Now it is full, so a second refill has no work and must revert rather
    // than pay again.
    let err = send(&mut fixture.svm, &payer, refill_ix(conditions), &[])
        .expect_err("a full reservoir is not refilled");
    assert!(
        format!("{:?}", err.meta.logs).contains("above the"),
        "expected the watermark guard to refuse, got: {:?}",
        err.meta.logs
    );
}

/// What the treasury pays to have this market's reservoir refilled. Priced on
/// the market like every other crank, because that is where the condition
/// advertising it lives.
fn refill_payment(fixture: &Fixture, conditions: Pubkey) -> u64 {
    let acct: velocity::state::clob_crank::ClobCrankConditionsV0 =
        read_zero_copy(&fixture.svm, &conditions);
    u64::from(acct.crank_payments.refill)
}

#[test]
fn program_keeper_expire_crank_pays_reservoir_lamports_to_an_unsigned_keeper() {
    let mut fixture = setup();
    const PAYMENT: u64 = 50_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    let protocol_user = set_protocol_user(&mut fixture.svm);
    // Top off the reservoir (lamport credits to a program-owned account are
    // unrestricted — this is the hot role's off-chain top-off leg).
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();

    // The attach registered the capacity watch on the book itself, over the
    // book's own counts, and mirrored the payment into min_payment.
    let evict = book_conditions(&fixture)[BOOK_CRANK_CAPACITY];
    assert_eq!(evict.min_payment(), PAYMENT);
    assert_eq!(
        account_change(&evict).0,
        fixture.clob_market.to_bytes(),
        "the book's capacity watch points at the book"
    );
    assert_eq!(book_expiry_wake(&fixture), i64::MAX);

    // An expiring ask moves the book's expiry wake, in the same instruction
    // that rests it — no second account travels with the placement.
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let max_ts = clock.unix_timestamp + 10;
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 99 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let maker = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &maker, ix, &[]).unwrap();
    assert_eq!(book_expiry_wake(&fixture), max_ts);

    // Nothing expired yet: the resolver reports no work.
    assert!(run_resolver(&mut fixture, conditions, true).is_none());

    // Past expiry the resolver stages the executor call.
    let mut clock: solana_clock::Clock = fixture.svm.get_sysvar();
    clock.unix_timestamp += 20;
    fixture.svm.set_sysvar(&clock);
    let resolved = run_resolver(&mut fixture, conditions, true).expect("expired order is work");
    assert_eq!(resolved.accounts.len(), 10);
    assert_eq!(
        resolved.accounts[2].address,
        protocol_user.to_bytes(),
        "filler slot must be the protocol User"
    );
    assert_eq!(
        resolved.accounts[4].address,
        fixture.clob_maker_user.to_bytes(),
        "maker read off the book node"
    );

    let reservoir_before = fixture.svm.get_account(&conditions).unwrap().lamports;
    // A payout account below the rent-exempt minimum after the credit fails
    // the transaction, so keepers use an existing funded wallet.
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankClobRemoveExpired::DISCRIMINATOR,
        payout,
    );

    // Maker unwound; reward accrued to the protocol User; lamports moved
    // reservoir -> payout; hint repaired to quiet.
    let maker_user: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker_user.perp_positions[0].open_asks, 0);
    assert_eq!(maker_user.perp_positions[0].open_orders, 0);
    assert!(maker_user.perp_positions[0].quote_asset_amount < 0);
    let protocol: User = read_zero_copy(&fixture.svm, &protocol_user);
    assert!(protocol.perp_positions[0].quote_asset_amount > 0);

    // The base payment plus what the order's lateness earned. The order came
    // due ten seconds before the crank ran, and the escalation is linear to
    // its ceiling over the window, so the keeper is paid a little over the
    // flat figure for having waited — which is the whole point: at some fee
    // level the flat figure stops being worth taking, and this is what keeps
    // climbing until somebody takes it.
    let escalation = u64::from(velocity::state::clob_crank::EXPIRY_ESCALATION_CEILING) * 10
        / velocity::state::clob_crank::EXPIRY_ESCALATION_SECONDS;
    assert_eq!(escalation, 166);
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + PAYMENT + escalation
    );
    assert_eq!(
        fixture.svm.get_account(&conditions).unwrap().lamports,
        reservoir_before - PAYMENT - escalation
    );
    assert_eq!(book_expiry_wake(&fixture), i64::MAX);
    assert_eq!(clob_ask_count(&fixture), 0);
}

/// The evict resolver walks both sides: with both at the soft cap it stages
/// the bid tail first (pinning the bid-side header offsets), then the ask —
/// each executed unsigned with the reservoir paying.
#[test]
fn program_keeper_evict_crank_resolves_both_sides() {
    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();

    // Book empty: no evict work.
    assert!(run_resolver(&mut fixture, conditions, false).is_none());

    // A bid and an ask; the fixture's evict threshold is 1, so both sides
    // are at the soft cap. Equal counts tie-break to the bid.
    for (direction, price) in [
        (PositionDirection::Long, 98 * PRICE),
        (PositionDirection::Short, 99 * PRICE),
    ] {
        let ix = place_clob_order_ix(
            fixture.clob_maker_user,
            &fixture.clob_maker_authority,
            fixture.quoter_slab,
            fixture.clob_market,
            fixture.oracle,
            PlaceClobOrderParams {
                market_index: 0,
                direction,
                price,
                base_asset_amount: UNIT / 2,
                max_ts: 0,
                activation_delay_slots: Some(0),
                reject_if_crossed: false,
            },
        );
        let maker = fixture.clob_maker_authority.insecure_clone();
        send(&mut fixture.svm, &maker, ix, &[]).unwrap();
    }

    // First resolve takes the bid tail (args: market_index u16 + side byte,
    // 0 = bid on the wire).
    let resolved = run_resolver(&mut fixture, conditions, false).expect("bid side at soft cap");
    assert_eq!(resolved.data, vec![0, 0, 0]);
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankClobEvict::DISCRIMINATOR,
        payout,
    );

    let maker_user: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker_user.perp_positions[0].open_bids, 0);

    // Second resolve takes the ask tail.
    let resolved = run_resolver(&mut fixture, conditions, false).expect("ask side at soft cap");
    assert_eq!(resolved.data, vec![0, 0, 1]);
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankClobEvict::DISCRIMINATOR,
        payout,
    );

    let maker_user: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker_user.perp_positions[0].open_asks, 0);
    assert_eq!(maker_user.perp_positions[0].open_orders, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + 2 * PAYMENT
    );

    // Book clear again: quiet.
    assert!(run_resolver(&mut fixture, conditions, false).is_none());
}

// --- trigger-order lifecycle (Armed -> Placed -> freed / re-armed) ---

/// A user holding an ARMED trigger-limit: the slot counts one open order but
/// no bids/asks (untriggered orders can't fill), which is the on-chain state
/// `place_perp_order` leaves behind.
fn armed_trigger_user(authority: &Pubkey, deposit: u64, order: Order) -> User {
    let mut user: User = bytemuck::Zeroable::zeroed();
    user.authority = anchor_lang::prelude::Pubkey::new_from_array(*authority.as_array());
    user.spot_positions[0].market_index = 0;
    user.spot_positions[0].balance_type = SpotBalanceType::Deposit;
    user.spot_positions[0].scaled_balance = deposit;
    user.perp_positions[0].market_index = 0;
    user.perp_positions[0].open_orders = 1;
    user.orders[0] = order;
    user.open_orders = 1;
    user.has_open_order = true;
    user.next_order_id = order.order_id + 1;
    user
}

fn trigger_limit_order_v1_ix(
    fixture: &Fixture,
    order_id: u32,
    filler: Pubkey,
    filler_stats: Pubkey,
    maker_stats: Pubkey,
) -> Instruction {
    let mut accounts = velocity::accounts::TriggerLimitOrderV1 {
        trigger_conditions: None,
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler,
        filler_stats,
        user: fixture.clob_maker_user,
        user_stats: maker_stats,
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::TriggerLimitOrderV1 {
            args: TriggerLimitOrderV1Args {
                market_index: 0,
                order_id,
            },
        }
        .data(),
    }
}

/// The whole trigger lifecycle: an armed stop-limit fails to trigger below
/// its price, places on the CLOB once crossed (slot becomes the shadow),
/// re-arms edge-gated on eviction (fires only after a recross), and frees on
/// expiry after re-placement.
#[test]
fn trigger_limit_lifecycle_places_re_arms_on_evict_and_frees_on_expiry() {
    use velocity::state::user::{OrderBitFlag, OrderTriggerCondition};

    let mut fixture = setup();

    // Sell-stop: trigger when oracle <= 98, then rest an ask at 97.
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let mut order = Order::default();
    order.order_id = 1;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerLimit;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Short;
    order.base_asset_amount = UNIT / 2;
    order.price = 97 * PRICE;
    order.trigger_price = 98 * PRICE;
    order.trigger_condition = OrderTriggerCondition::Below;
    order.max_ts = clock.unix_timestamp + 1_000;
    set_user_account(
        &mut fixture.svm,
        fixture.clob_maker_user,
        &armed_trigger_user(
            &fixture.clob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            order,
        ),
    );

    let maker_stats = Pubkey::new_unique();
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    // Above the trigger: no fire.
    let keeper = fixture.keeper.insecure_clone();
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("price above trigger");
    assert!(
        format!("{:?}", err.meta.logs).contains("did not satisfy trigger condition"),
        "unexpected: {:?}",
        err.meta.logs
    );

    // Crossed: places on the CLOB, slot becomes the shadow.
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (97 * PRICE_PRECISION) as i64,
        12,
    );

    fixture.svm.warp_to_slot(12);
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(maker.orders[0].is_placed_on_clob());
    assert_eq!(
        maker.orders[0].trigger_condition,
        OrderTriggerCondition::Below,
        "shadow stays untriggered so DLOB paths ignore it"
    );

    let (node_index, clob_order_id) = maker.orders[0].clob_order_ref();
    assert_eq!(clob_order_id, 1);
    assert_eq!(maker.perp_positions[0].open_asks, -((UNIT / 2) as i64));
    assert_eq!(maker.perp_positions[0].open_orders, 1);
    assert_eq!(maker.open_orders, 1);
    assert!(
        maker.perp_positions[0].quote_asset_amount < 0,
        "keeper reward paid from the user"
    );
    assert_eq!(clob_ask_count(&fixture), 1);
    let _ = node_index;

    // The fired trigger rests taker-origin. It came to trade, so a cross
    // settles at the counterparty's price rather than picking it off at its
    // own, and the taker-origin crank is what carries a route to it.
    assert!(
        clob_side(&fixture, Direction::Long)
            .iter()
            .all(|row| row.flags & velocity::state::prop_amm::L3_ROW_FLAG_TAKER_ORIGIN != 0),
        "a fired trigger rests as a taker remainder, not as a maker quote"
    );

    // A second trigger attempt on the placed slot fails.
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("already placed");
    assert!(format!("{:?}", err.meta.logs).contains("already rests on the CLOB"));

    // Evict (fixture soft cap = 1): the shadow re-arms in the same tx,
    // edge-gated on a recross.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CrankClobOrderRemoval {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobEvict {
            args: CrankClobEvictArgs {
                market_index: 0,
                side: velocity::state::prop_amm::ClobSide::Ask,
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(!maker.orders[0].is_placed_on_clob());
    assert!(maker.orders[0].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross));
    assert_eq!(maker.orders[0].status, OrderStatus::Open);
    assert_eq!(maker.orders[0].base_asset_amount, UNIT / 2);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(
        maker.perp_positions[0].open_orders, 1,
        "armed slot counts again"
    );
    assert_eq!(maker.open_orders, 1);
    assert_eq!(clob_ask_count(&fixture), 0);

    // Still through the trigger: the edge gate refuses to re-fire.
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("no recross yet");
    assert!(format!("{:?}", err.meta.logs).contains("never crossed back"));

    // Price back above: the crank observes the recross and clears the gate
    // without placing.
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        13,
    );

    fixture.svm.warp_to_slot(13);
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(!maker.orders[0].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross));
    assert_eq!(clob_ask_count(&fixture), 0);

    // Crossed again: re-places.
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (97 * PRICE_PRECISION) as i64,
        14,
    );

    fixture.svm.warp_to_slot(14);
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(maker.orders[0].is_placed_on_clob());
    let (node_index, clob_order_id) = maker.orders[0].clob_order_ref();
    assert_eq!(clob_ask_count(&fixture), 1);

    // Expire the CLOB order: the expiry crank frees the shadow for good.
    let mut clock: solana_clock::Clock = fixture.svm.get_sysvar();
    clock.unix_timestamp += 2_000;
    fixture.svm.set_sysvar(&clock);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CrankClobOrderRemoval {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobRemoveExpired {
            args: CrankClobRemoveExpiredArgs {
                market_index: 0,
                order_ref: velocity::state::prop_amm::ClobOrderRefV0 {
                    node_index,
                    order_id: clob_order_id,
                },
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.orders[0].status, OrderStatus::Canceled);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
}

/// The accounts a reduce-only sell-stop fires with.
struct ReduceOnlyStop {
    filler_user: Pubkey,
    filler_stats: Pubkey,
    maker_stats: Pubkey,
}

/// Arms a reduce-only sell-stop of half a unit for the book maker, which holds
/// `position_base`, and moves the oracle through the trigger.
fn arm_reduce_only_sell_stop(fixture: &mut Fixture, position_base: i64) -> ReduceOnlyStop {
    use velocity::state::user::OrderTriggerCondition;

    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let mut order = Order::default();
    order.order_id = 1;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerLimit;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Short;
    order.base_asset_amount = UNIT / 2;
    order.price = 97 * PRICE;
    order.trigger_price = 98 * PRICE;
    order.trigger_condition = OrderTriggerCondition::Below;
    order.reduce_only = true;
    order.max_ts = clock.unix_timestamp + 1_000;
    let mut maker = armed_trigger_user(
        &fixture.clob_maker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        order,
    );
    maker.perp_positions[0].base_asset_amount = position_base;
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &maker);

    let maker_stats = Pubkey::new_unique();
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (97 * PRICE_PRECISION) as i64,
        12,
    );

    fixture.svm.warp_to_slot(12);
    ReduceOnlyStop {
        filler_user,
        filler_stats,
        maker_stats,
    }
}

/// A reduce-only stop larger than the position rests only the position. The
/// margin gate exempts it, so its reservation must fit what its fills can
/// reach.
#[test]
fn a_reduce_only_trigger_rests_at_most_the_position_it_reduces() {
    let mut fixture = setup();
    let stop = arm_reduce_only_sell_stop(&mut fixture, (UNIT / 4) as i64);

    let keeper = fixture.keeper.insecure_clone();
    let ix = trigger_limit_order_v1_ix(
        &fixture,
        1,
        stop.filler_user,
        stop.filler_stats,
        stop.maker_stats,
    );
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(maker.orders[0].is_placed_on_clob());
    assert_eq!(maker.perp_positions[0].open_asks, -((UNIT / 4) as i64));
    let asks = clob_side(&fixture, Direction::Long);
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].size, UNIT / 4);
}

/// A reduce-only stop with no position left to reduce is cancelled rather
/// than rested, and the keeper earns nothing for it.
#[test]
fn a_reduce_only_trigger_with_nothing_to_reduce_is_cancelled() {
    let mut fixture = setup();
    let stop = arm_reduce_only_sell_stop(&mut fixture, 0);

    let keeper = fixture.keeper.insecure_clone();
    let ix = trigger_limit_order_v1_ix(
        &fixture,
        1,
        stop.filler_user,
        stop.filler_stats,
        stop.maker_stats,
    );
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.orders[0].status, OrderStatus::Canceled);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(maker.perp_positions[0].quote_asset_amount, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
}

fn modify_order_v1_ix(
    fixture: &Fixture,
    order_ref: ClobOrderRefV0,
    base_asset_amount: Option<u64>,
) -> Instruction {
    let mut accounts = velocity::accounts::ModifyOrderV1 {
        state: state_pda(),
        user: fixture.clob_maker_user,
        authority: fixture.clob_maker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::ModifyOrderV1 {
            params: velocity::instructions::ModifyOrderV1Params {
                market_index: 0,
                order_ref,
                price: None,
                base_asset_amount,
                max_ts: None,
                activation_delay_slots: None,
                reject_if_crossed: false,
            },
        }
        .data(),
    }
}

/// A modify cannot upsize a reduce-only order past the position it reduces,
/// and refuses one once no position is left.
#[test]
fn a_reduce_only_modify_is_clamped_to_the_position() {
    let mut fixture = setup();
    let stop = arm_reduce_only_sell_stop(&mut fixture, (UNIT / 4) as i64);

    let keeper = fixture.keeper.insecure_clone();
    let ix = trigger_limit_order_v1_ix(
        &fixture,
        1,
        stop.filler_user,
        stop.filler_stats,
        stop.maker_stats,
    );
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    let (node_index, order_id) = maker.orders[0].clob_order_ref();
    fixture.svm.warp_to_slot(100);
    fixture.svm.expire_blockhash();

    let authority = fixture.clob_maker_authority.insecure_clone();
    let ix = modify_order_v1_ix(
        &fixture,
        ClobOrderRefV0 {
            node_index,
            order_id,
        },
        Some(10 * UNIT),
    );
    send(&mut fixture.svm, &authority, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_asks, -((UNIT / 4) as i64));
    assert_eq!(maker.orders[0].base_asset_amount, UNIT / 4);
    let asks = clob_side(&fixture, Direction::Long);
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].size, UNIT / 4);

    let (node_index, order_id) = maker.orders[0].clob_order_ref();
    let mut maker = maker;
    maker.perp_positions[0].base_asset_amount = 0;
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &maker);
    fixture.svm.expire_blockhash();
    let ix = modify_order_v1_ix(
        &fixture,
        ClobOrderRefV0 {
            node_index,
            order_id,
        },
        None,
    );
    let err = send(&mut fixture.svm, &authority, ix, &[]).expect_err("nothing to reduce");
    assert!(
        format!("{:?}", err.meta.logs).contains("no position left to reduce"),
        "unexpected: {:?}",
        err.meta.logs
    );
}

/// A sweep frees placed-trigger shadows too. The handler can't match returned
/// order ids for this (the wire is aggregate), so it re-checks each shadow's
/// node against the post-sweep book — this pins that the shadow ends up
/// `Canceled` and its accounting unwound, the same as a per-order cancel leaves
/// it.
#[test]
fn cancel_all_frees_placed_trigger_shadows() {
    use velocity::state::user::OrderTriggerCondition;

    let mut fixture = setup();
    let mut order = Order::default();
    order.order_id = 1;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerLimit;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Short;
    order.base_asset_amount = UNIT / 2;
    order.price = 97 * PRICE;
    order.trigger_price = 98 * PRICE;
    order.trigger_condition = OrderTriggerCondition::Below;
    set_user_account(
        &mut fixture.svm,
        fixture.clob_maker_user,
        &armed_trigger_user(
            &fixture.clob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            order,
        ),
    );

    let maker_stats = Pubkey::new_unique();
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (97 * PRICE_PRECISION) as i64,
        12,
    );

    fixture.svm.warp_to_slot(12);
    let keeper = fixture.keeper.insecure_clone();
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    // A plain book order alongside the shadowed one, so the sweep takes both
    // kinds in one pass and the counts have to cover both.
    place_clob_ask(&mut fixture, 96 * PRICE, UNIT / 4);
    let before: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(before.orders[0].is_placed_on_clob());
    assert_eq!(before.perp_positions[0].open_orders, 2);
    assert_eq!(clob_ask_count(&fixture), 2);

    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Asks);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();

    let after: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(after.orders[0].status, OrderStatus::Canceled);
    assert_eq!(after.perp_positions[0].open_orders, 0);
    assert_eq!(after.open_orders, 0);
    assert_eq!(after.perp_positions[0].open_asks, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
}

/// A user cancels a placed trigger through `cancel_clob_order` (the shadow
/// frees with the book order), while the DLOB `cancel_order` path refuses to
/// touch the shadow.
#[test]
fn placed_trigger_cancels_through_the_clob_only() {
    use velocity::state::user::OrderTriggerCondition;

    let mut fixture = setup();
    let mut order = Order::default();
    order.order_id = 1;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerLimit;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Short;
    order.base_asset_amount = UNIT / 2;
    order.price = 97 * PRICE;
    order.trigger_price = 98 * PRICE;
    order.trigger_condition = OrderTriggerCondition::Below;
    set_user_account(
        &mut fixture.svm,
        fixture.clob_maker_user,
        &armed_trigger_user(
            &fixture.clob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            order,
        ),
    );

    let maker_stats = Pubkey::new_unique();
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (97 * PRICE_PRECISION) as i64,
        12,
    );

    fixture.svm.warp_to_slot(12);
    let keeper = fixture.keeper.insecure_clone();
    let ix = trigger_limit_order_v1_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    let (node_index, clob_order_id) = maker.orders[0].clob_order_ref();

    // The DLOB cancel path refuses the shadow.
    let maker_authority = fixture.clob_maker_authority.insecure_clone();
    let mut accounts = velocity::accounts::CancelOrder {
        state: state_pda(),
        user: fixture.clob_maker_user,
        authority: maker_authority.pubkey(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::CancelOrder { order_id: Some(1) }.data(),
    };
    let err = send(&mut fixture.svm, &maker_authority, ix, &[]).expect_err("shadow is CLOB-owned");
    assert!(format!("{:?}", err.meta.logs).contains("placed on the CLOB"));

    // cancel_clob_order removes the book order AND frees the shadow.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrderV1 {
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: maker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrderV1 {
            params: CancelOrderV1Params {
                market_index: 0,
                order_ref: velocity::state::prop_amm::ClobOrderRefV0 {
                    node_index,
                    order_id: clob_order_id,
                },
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &maker_authority, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.orders[0].status, OrderStatus::Canceled);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
}

/// A cross the protocol barely clears is a cross worth declining: cranking
/// one costs the reservoir real lamports, so the market's `min_cross_surplus`
/// floors what the protocol must net before the crank is allowed to land.
/// The spread here is ~$1 on half a unit, so a $10 floor is out of reach and
/// a zero floor is not.
#[test]
fn a_cross_below_the_markets_surplus_floor_is_declined() {
    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    // Floor far above the achievable spread, in QUOTE_PRECISION.
    let conditions = init_crank_conditions_with_floor(&mut fixture, PAYMENT, 10 * 1_000_000);
    let protocol_user = set_protocol_user(&mut fixture.svm);
    let (signer, _) = velocity_signer_pda();
    let protocol_stats =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();

    let maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    // Crossed against themselves: ask 0.5 @ 99, bid 0.5 @ 101.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let maker_authority = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &maker_authority, ix, &[]).unwrap();
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let cross_ix = || {
        let mut accounts = velocity::accounts::CrankCrossMatch {
            state: state_pda(),
            authority: payout,
            taker: protocol_user,
            taker_stats: protocol_stats,
            crank_conditions: conditions,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            instructions_sysvar: instructions_sysvar(),
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
        accounts.push(AccountMeta::new(maker_stats, false));
        // The quoter section: the slab each leg assembles its route from,
        // then the book's own accounts.
        accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::CrankCrossMatch {
                args: CrankCrossMatchArgs {
                    market_index: 0,
                    // Exactly the crossing depth. Both legs route across
                    // every source, so a size past it would take the vAMM
                    // on the wrong side of both legs and fail the marginal
                    // price rule.
                    size: UNIT / 2,
                },
            }
            .data(),
        }
    };
    let ix = cross_ix();
    let keeper = fixture.keeper.insecure_clone();
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper,
        &[compute_unit_limit_ix(1_400_000), ix.clone()],
        &[],
    )
    .unwrap_err();
    let logs = err.meta.logs.join(" ");
    assert!(
        logs.contains("CrossMatchUnprofitable") || logs.contains("below the market's floor"),
        "expected the floor to decline the cross, got: {logs}"
    );

    // Nothing happened: the book still holds both sides.
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_orders, 2);

    // Re-price the floor to zero — the same cross now lands, so it was the
    // floor that declined it and not the cross itself.
    init_crank_conditions_with_floor(&mut fixture, PAYMENT, 1);
    send_with_ixs(
        &mut fixture.svm,
        &keeper,
        &[compute_unit_limit_ix(1_400_000), ix],
        &[],
    )
    .unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.perp_positions[0].base_asset_amount, 0);
}

/// A crossed CLOB (bid above ask) is matched by the cross crank: the
/// protocol User round-trips the cross, keeps the spread net of both legs'
/// taker fees, the makers' aggregates unwind as ordinary fills, and the
/// keeper payout account is paid from the reservoir. Re-running with nothing
/// crossed fails — the executor is the profitability predicate.
#[test]
fn cross_match_crank_fills_a_crossed_clob_and_keeps_the_spread() {
    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    let protocol_user = set_protocol_user(&mut fixture.svm);
    let (signer, _) = velocity_signer_pda();
    let protocol_stats =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();

    // The staged cross executor carries the maker's (User, UserStats) pair
    // at their derived addresses, so the stats fixture must live at its PDA.
    let maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    // The maker quotes crossed against themselves: ask 0.5 @ 99, bid 0.5 @ 101.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let maker_authority = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &maker_authority, ix, &[]).unwrap();
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let cross_ix = || {
        let mut accounts = velocity::accounts::CrankCrossMatch {
            state: state_pda(),
            authority: payout,
            taker: protocol_user,
            taker_stats: protocol_stats,
            crank_conditions: conditions,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            instructions_sysvar: instructions_sysvar(),
        }
        .to_account_metas(None);
        // Maps, then the maker pair, then the quoter section.
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
        accounts.push(AccountMeta::new(maker_stats, false));
        // The quoter section: the slab each leg assembles its route from,
        // then the book's own accounts.
        accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::CrankCrossMatch {
                args: CrankCrossMatchArgs {
                    market_index: 0,
                    // Exactly the crossing depth. Both legs route across
                    // every source, so a size past it would take the vAMM
                    // on the wrong side of both legs and fail the marginal
                    // price rule.
                    size: UNIT / 2,
                },
            }
            .data(),
        }
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper,
        &[compute_unit_limit_ix(1_400_000), cross_ix()],
        &[],
    )
    .unwrap();
    // The crank pays its keeper out of the reservoir, priced from the cost
    // units an admin measured — so what this burns is what a market has to
    // register for it.
    println!(
        "CU — crank_cross_match over a self-crossed book: {}",
        meta.compute_units_consumed
    );

    // The maker round-tripped against themselves: net base zero, they paid
    // the spread; both orders consumed, aggregates unwound.
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].base_asset_amount, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.perp_positions[0].open_bids, 0);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(clob_ask_count(&fixture), 0);

    // The protocol User kept the spread net of fees: bought 0.5 @ 99, sold
    // 0.5 @ 101 -> 1.0 quote gross, minus two taker fees.
    let protocol: User = read_zero_copy(&fixture.svm, &protocol_user);
    assert_eq!(protocol.perp_positions[0].base_asset_amount, 0);
    assert!(
        protocol.perp_positions[0].quote_asset_amount >= 800_000,
        "surplus after fees, got {}",
        protocol.perp_positions[0].quote_asset_amount
    );
    assert!(protocol.perp_positions[0].quote_asset_amount < 1_000_000);
    // The ephemeral taker orders never persist.
    assert_eq!(protocol.open_orders, 0);
    assert!(protocol
        .orders
        .iter()
        .all(|order| order.status != OrderStatus::Open));
    // Keeper paid from the reservoir.
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + PAYMENT
    );

    // Nothing crossed anymore: the predicate fails the crank.
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper,
        &[compute_unit_limit_ix(1_400_000), cross_ix()],
        &[],
    )
    .expect_err("no cross left");
    let logs = format!("{:?}", err.meta.logs);
    assert!(
        logs.contains("CrossMatch") || logs.contains("nothing crossed"),
        "unexpected: {logs}"
    );

    // ---- The relay-staged path: resolver discovers the cross, stages the
    // executor stats-less, and the turner-shaped submission fills it. ----
    let resolve_cross = |fixture: &mut Fixture| run_cross_resolver(fixture, conditions);
    assert!(resolve_cross(&mut fixture).is_none(), "book is uncrossed");

    // Cross it again — this time behind a speed bump, the makers-line-up
    // scenario: the ask rests immediately, the crossing bid activates three
    // slots out. Placement min-folds the activation slot into the AtSlot
    // wake, the resolver reports no work until the slot arrives, and the
    // cross fires the moment it does.
    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: 0,
            activation_delay_slots: Some(3),
            reject_if_crossed: false,
        },
    );

    send(&mut fixture.svm, &maker_authority, ix, &[]).unwrap();
    assert_eq!(
        book_activation_wake(&fixture),
        23,
        "the book folds the activation slot into its own AtSlot wake"
    );

    // Before activation the crossing bid isn't matchable: no work.
    assert!(
        resolve_cross(&mut fixture).is_none(),
        "speed bump still running"
    );

    fixture.svm.warp_to_slot(23);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        23,
    );

    let surplus_before = {
        let protocol: User = read_zero_copy(&fixture.svm, &protocol_user);
        protocol.perp_positions[0].quote_asset_amount
    };
    let resolved = resolve_cross(&mut fixture).expect("crossed book is work at activation");
    assert_eq!(
        resolved.accounts[2].address,
        protocol_user.to_bytes(),
        "taker slot is the protocol User"
    );

    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankCrossMatch::DISCRIMINATOR,
        payout,
    );

    let protocol: User = read_zero_copy(&fixture.svm, &protocol_user);
    assert_eq!(protocol.perp_positions[0].base_asset_amount, 0);
    assert!(
        protocol.perp_positions[0].quote_asset_amount >= surplus_before + 800_000,
        "second surplus accrued, got {}",
        protocol.perp_positions[0].quote_asset_amount
    );
    assert_eq!(clob_ask_count(&fixture), 0);
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + 2 * PAYMENT
    );

    // The book moved its activation wake forward as it matched: nothing
    // pending, so the AtSlot wake goes quiet.
    assert_eq!(book_activation_wake(&fixture), u64::MAX);
    // Empty again: no work.
    assert!(resolve_cross(&mut fixture).is_none());
}

/// A maker whose account has deteriorated below initial margin gets their
/// risk-increasing CLOB order force-cancelled by a keeper: aggregates
/// unwind, the flat fee moves from the maker's quote deposit to the filler,
/// and a healthy account is refused.
#[test]
fn force_cancel_reclaims_a_failing_makers_clob_orders() {
    let mut fixture = setup();

    // Rest an ask through the adapter (margin passes at 10k), then gut the
    // maker's collateral while keeping the resting aggregates — the
    // deterioration force-cancel exists for.
    let order_ref = place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let mut broke = trading_user(&fixture.clob_maker_authority.pubkey(), 1_000, None);
    broke.perp_positions[0].open_asks = -((UNIT / 2) as i64);
    broke.perp_positions[0].open_orders = 1;
    broke.open_orders = 1;
    broke.has_open_order = true;
    broke.next_order_id = 2;

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let force_cancel_ix = |fixture: &Fixture| {
        force_cancel_clob_ix(
            fixture,
            filler_user,
            filler_stats,
            fixture.clob_maker_user,
            maker_stats,
            fixture.keeper.pubkey(),
            vec![ask_ref(order_ref)],
        )
    };

    // Healthy account: nothing to do, and saying so is a success. A fill
    // prefixes this to clear its way and must not be taken down by an
    // account that turned out to be fine.
    let keeper = fixture.keeper.insecure_clone();
    {
        let ix = force_cancel_ix(&fixture);
        send(&mut fixture.svm, &keeper, ix, &[]).expect("a healthy account is a no-op");
    }

    assert_eq!(
        clob_ask_count(&fixture),
        1,
        "the no-op took nothing off the book"
    );

    // Deteriorated: the order is reclaimed for the flat fee.
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &broke);
    let ix = force_cancel_ix(&fixture);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(clob_ask_count(&fixture), 0);
    // Flat fee moved maker -> filler through the quote spot balances.
    let filler: User = read_zero_copy(&fixture.svm, &filler_user);
    assert!(filler.spot_positions[0].scaled_balance > 0);
    // The maker's dust deposit flipped into a borrow covering the fee.
    assert_eq!(
        maker.spot_positions[0].balance_type,
        SpotBalanceType::Borrow
    );

    // Relay racing a prefixed cancel lands here: the orders are already
    // gone, so the second caller has nothing to do and says so with success.
    // A revert would take the fill that prefixed it down too.
    let filler_before: User = read_zero_copy(&fixture.svm, &filler_user);
    let ix = force_cancel_ix(&fixture);
    send(&mut fixture.svm, &keeper, ix, &[]).expect("a stale ref is a no-op");
    let filler_after: User = read_zero_copy(&fixture.svm, &filler_user);
    assert_eq!(
        filler_before.spot_positions[0].scaled_balance,
        filler_after.spot_positions[0].scaled_balance,
        "a no-op pays the caller nothing"
    );
}

/// The authority-wide latch is not grounds by itself. A force cancel answers
/// only to a breached margin requirement or a proven equity floor breach, the
/// same two grounds `force_cancel_orders` answers to. A healthy subaccount
/// under a tripped latch keeps its resting orders.
#[test]
fn a_tripped_equity_breaker_is_not_grounds_on_its_own() {
    let mut fixture = setup();
    let order_ref = place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    // Solvent by every other measure: full collateral, no floor set.
    let mut maker = trading_user(
        &fixture.clob_maker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    maker.perp_positions[0].open_asks = -((UNIT / 2) as i64);
    maker.perp_positions[0].open_orders = 1;
    maker.open_orders = 1;
    maker.has_open_order = true;
    maker.next_order_id = 2;
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &maker);

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let keeper = fixture.keeper.insecure_clone();
    let build = |fixture: &Fixture| {
        force_cancel_clob_ix(
            fixture,
            filler_user,
            filler_stats,
            fixture.clob_maker_user,
            maker_stats,
            fixture.keeper.pubkey(),
            vec![ask_ref(order_ref)],
        )
    };

    // Control: latch clear, account healthy, so there is nothing to do.
    let ix = build(&fixture);
    send(&mut fixture.svm, &keeper, ix, &[]).expect("healthy is a no-op");
    assert_eq!(clob_ask_count(&fixture), 1);

    // Latch set: the same call is still a no-op.
    set_tripped_user_stats(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    fixture.svm.expire_blockhash();
    let ix = build(&fixture);
    send(&mut fixture.svm, &keeper, ix, &[]).expect("the latch alone is a no-op");
    assert_eq!(clob_ask_count(&fixture), 1);
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_asks, -((UNIT / 2) as i64));
    assert_eq!(maker.perp_positions[0].open_orders, 1);
}

/// A risk-*reducing* order is passed over, not cancelled and not fatal.
/// Cancelling one would only make a failing account worse, and a caller whose
/// refs went stale against a position that moved must not take the
/// transaction down — the prefixed cancel rides in front of a fill.
#[test]
fn force_cancel_passes_over_a_risk_reducing_order() {
    let mut fixture = setup();
    // One of each side, so the pass-over is shown to be selective rather
    // than the whole call quietly doing nothing.
    let ask_order = place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let bid_order = place_clob_bid(&mut fixture, 90 * PRICE, UNIT / 2);

    // Long a whole unit on gutted collateral: the account fails initial
    // margin, so the grounds are met. Against +1 the resting ask reduces and
    // the resting bid increases.
    let mut maker = trading_user(&fixture.clob_maker_authority.pubkey(), 1_000, None);
    maker.perp_positions[0].base_asset_amount = UNIT as i64;
    maker.perp_positions[0].open_asks = -((UNIT / 2) as i64);
    maker.perp_positions[0].open_bids = (UNIT / 2) as i64;
    maker.perp_positions[0].open_orders = 2;
    maker.open_orders = 2;
    maker.has_open_order = true;
    maker.next_order_id = 3;
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &maker);

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let keeper = fixture.keeper.insecure_clone();
    let ix = force_cancel_clob_ix(
        &fixture,
        filler_user,
        filler_stats,
        fixture.clob_maker_user,
        maker_stats,
        fixture.keeper.pubkey(),
        vec![
            ask_ref(ask_order),
            velocity::instructions::ForceCancelClobRefV0 {
                order_ref: bid_order,
                side: velocity::state::prop_amm::ClobSide::Bid,
            },
        ],
    );

    send(&mut fixture.svm, &keeper, ix, &[]).expect("a reducing ref is passed over, not fatal");
    assert_eq!(
        clob_ask_count(&fixture),
        1,
        "the reducing ask stays on the book"
    );
    assert_eq!(
        clob_bid_count(&fixture),
        0,
        "the risk-increasing bid in the same call was still reclaimed"
    );
}

/// A maker cannot outrun its own cleanup by resting more orders than one call
/// can name. Orders cost `OPEN_ORDER_MARGIN_REQUIREMENT` — a cent each — so a
/// few dollars buys the per-position ceiling of 255, and at the eight refs a
/// call carries that would be 32 transactions the keeper pays for and an
/// insolvent account may never repay. The side that cannot be reducing goes in
/// one sweep instead, so the count stops mattering.
#[test]
fn a_maker_cannot_outrun_cleanup_by_resting_more_orders() {
    let mut fixture = setup();

    // More than `MAX_FORCE_CANCEL_CLOB_ORDERS`, so per-order refs alone could
    // not clear this in one call.
    const RESTED: usize = 12;
    let mut total_base = 0u64;
    for i in 0..RESTED {
        place_clob_ask(&mut fixture, (99 + i as u64) * PRICE, UNIT / 8);
        total_base += UNIT / 8;
    }

    assert_eq!(clob_ask_count(&fixture), RESTED);

    // Flat and broke: with no open position no ask can be reducing, so the
    // whole side is reclaimable.
    let mut broke = trading_user(&fixture.clob_maker_authority.pubkey(), 1_000, None);
    broke.perp_positions[0].open_asks = -(total_base as i64);
    broke.perp_positions[0].open_orders = RESTED as u8;
    broke.open_orders = RESTED as u8;
    broke.has_open_order = true;
    broke.next_order_id = RESTED as u32 + 1;
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &broke);

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    // No refs at all: the sweep is the whole job.
    let keeper = fixture.keeper.insecure_clone();
    let ix = force_cancel_clob_ix(
        &fixture,
        filler_user,
        filler_stats,
        fixture.clob_maker_user,
        maker_stats,
        fixture.keeper.pubkey(),
        vec![],
    );

    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    assert_eq!(
        clob_ask_count(&fixture),
        0,
        "every order went in the one call, however many there were"
    );

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        maker.perp_positions[0].open_asks, 0,
        "reserve unwound in full"
    );
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.open_orders, 0);
    // The sweep still pays the keeper. The per-order rate is the handler's
    // arithmetic — both paths add to the same accumulator — so what this pins
    // is that a bulk removal is not unpaid work.
    let filler: User = read_zero_copy(&fixture.svm, &filler_user);
    assert!(filler.spot_positions[0].scaled_balance > 0);
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        maker.spot_positions[0].balance_type,
        SpotBalanceType::Borrow,
        "the fee came out of the maker, dust deposit and then some"
    );
}

/// The declared side is what the risk-reducing test is run against before the
/// CPI, so a caller that declares it wrong had its order judged on the wrong
/// rule. That is the caller being wrong about what it passed, and unlike a
/// stale ref it is not forgiven.
#[test]
fn a_misdeclared_side_fails_loudly() {
    let mut fixture = setup();
    let order_ref = place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let mut broke = trading_user(&fixture.clob_maker_authority.pubkey(), 1_000, None);
    broke.perp_positions[0].open_asks = -((UNIT / 2) as i64);
    broke.perp_positions[0].open_orders = 1;
    broke.open_orders = 1;
    broke.has_open_order = true;
    broke.next_order_id = 2;
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &broke);

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let keeper = fixture.keeper.insecure_clone();
    let ix = force_cancel_clob_ix(
        &fixture,
        filler_user,
        filler_stats,
        fixture.clob_maker_user,
        maker_stats,
        fixture.keeper.pubkey(),
        vec![velocity::instructions::ForceCancelClobRefV0 {
            order_ref,
            side: velocity::state::prop_amm::ClobSide::Bid,
        }],
    );
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("side was declared wrong");
    assert!(
        format!("{:?}", err.meta.logs).contains("rested on the other side than declared"),
        "unexpected: {:?}",
        err.meta.logs
    );
}

/// Retail routes: `place_and_take_perp_order_v1` fills the taker off the
/// market's CLOB, with no DLOB maker anywhere in the transaction. This is the
/// property the endpoint exists for — before it routed, a taker signing their
/// own transaction could only reach the vAMM and whatever DLOB makers they
/// passed, so book liquidity was keeper-only. The accounts the taker passes
/// *are* their route; the mandatory CLOB baseline is enforced against them.
#[test]
fn place_and_take_v1_fills_a_retail_taker_off_the_clob() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();
    // The book's maker needs its stats at the derived address: the fill
    // resolves a balance change to a loaded user by field match, and the pair
    // has to be in the transaction to be settled against.
    let maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // The book's maker, so its fill can be settled.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    // The route: the market's CLOB and the accounts its CPI resolves against.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Limit,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT / 2,
                    price: 99 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    // The taker is long off the book, and the book's ask is consumed.
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount,
        (UNIT / 2) as i64,
        "filled off the CLOB"
    );

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        maker.perp_positions[0].base_asset_amount,
        -((UNIT / 2) as i64),
        "the book's maker is short the other side"
    );
    assert_eq!(maker.perp_positions[0].open_asks, 0, "reservation released");
    assert_eq!(clob_ask_count(&fixture), 0);
}

/// A reduce-only ephemeral take on a flat position refuses, and the refusal is
/// not an error.
///
/// The order may reduce nothing, so the fill declines it before it reaches any
/// liquidity. An ephemeral order rests in no `user.orders` slot, so the decline
/// cancels nothing and pays nobody. Reading a slot index here instead failed the
/// whole transaction, which on the trigger route would leave the crank
/// re-firing the same order.
#[test]
fn a_reduce_only_ephemeral_take_that_can_reduce_nothing_is_declined() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();
    let maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    // Flat: the position this reduce-only order could reduce is zero.
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Limit,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT / 2,
                    price: 99 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    reduce_only: true,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect("a reduce-only take with nothing to reduce is a refusal, not an error");

    // Nothing moved: the taker is still flat and the book still holds its ask.
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(taker.perp_positions[0].base_asset_amount, 0);
    assert_eq!(taker.perp_positions[0].open_bids, 0);
    assert_eq!(clob_ask_count(&fixture), 1);
}

/// Maker priority: on a book with a speed bump, only attested flow fills in
/// its own transaction. An unattested taker rests whole, taker-origin,
/// through the default window — a maker can always reprice ahead of it. A
/// shape that demands a synchronous outcome (an IOC, a success condition) is
/// refused, and a transaction the flow authority co-signs keeps the
/// synchronous fill.
#[test]
fn an_unattested_taker_on_a_bumped_book_rests_instead_of_filling() {
    use velocity::state::{
        order_params::{OrderParams, PostOnlyParam},
        state::HotRole,
    };

    let mut fixture = setup();
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    set_clob_default_activation_delay(&mut fixture, 5);

    // The flow authority is configured, so attestation is expressible.
    let flow = Keypair::new();
    fixture.svm.airdrop(&flow.pubkey(), 1_000_000_000).unwrap();
    let mut state: State = read_zero_copy(&fixture.svm, &state_pda());
    state.set_hot_key(HotRole::FlowAuthority, flow.pubkey());
    set_zero_copy_account(
        &mut fixture.svm,
        state_pda(),
        State::DISCRIMINATOR,
        &state,
        State::SIZE,
    );

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let build = |bit_flags: u8,
                 success_condition: Option<
        velocity::state::order_params::PlaceAndTakeOrderSuccessCondition,
    >,
                 attest: bool| {
        let mut accounts = velocity::accounts::PlaceAndTakeV1 {
            state: state_pda(),
            user: taker_user,
            user_stats: taker_stats,
            authority: taker_authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            // The attestation transport for swift-built transactions: the
            // flow authority signs as this named account.
            flow_authority: attest.then(|| flow.pubkey()),
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
        accounts.push(AccountMeta::new(maker_stats, false));
        accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::PlaceAndTakePerpOrderV1 {
                args: PlaceAndTakePerpOrderV1Args {
                    params: OrderParams {
                        order_type: OrderType::Limit,
                        market_type: MarketType::Perp,
                        direction: PositionDirection::Long,
                        base_asset_amount: UNIT / 2,
                        price: 99 * PRICE,
                        market_index: 0,
                        post_only: PostOnlyParam::None,
                        bit_flags,
                        ..OrderParams::default()
                    },

                    success_condition,
                },
            }
            .data(),
        }
    };

    // A synchronous shape is refused rather than silently rested.
    for ix in [
        build(1, None, false),
        build(
            0,
            Some(velocity::state::order_params::PlaceAndTakeOrderSuccessCondition::FullFill),
            false,
        ),
    ] {
        let err = send_with_ixs(
            &mut fixture.svm,
            &taker_authority,
            &[compute_unit_limit_ix(400_000), ix],
            &[],
        )
        .unwrap_err();
        assert_velocity_error(&err, ErrorCode::UnattestedSynchronousTake);
    }

    // Unattested: no fill. The order rests whole as a taker-origin bid and
    // the maker's ask stands.
    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), build(0, None, false)],
        &[],
    )
    .unwrap();
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount, 0,
        "unattested flow does not fill synchronously"
    );
    assert_eq!(
        taker.perp_positions[0].open_bids,
        (UNIT / 2) as i64,
        "the whole order rests"
    );

    // The rested bid is invisible to the matchable-set reader here: it is
    // inside its activation window, and the ask crosses it. It is counted
    // below, once the ask is consumed and the window has passed.
    assert_eq!(
        clob_ask_count(&fixture),
        1,
        "the maker's quote was not taken"
    );

    // Attested flow fills off the book, but only depth no remainder has
    // claimed. The order that just rested crosses this ask and holds it, and a
    // claim outranks any later taker however its flow is attested — taking
    // that ask at the maker's price is exactly the frontrun the claim exists
    // to stop, and attestation is not a ticket past it. So this take fills
    // nothing and the ask still stands.
    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), build(0, None, true)],
        &[&flow],
    )
    .unwrap();
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount, 0,
        "the claimed ask is withheld from attested flow too"
    );
    assert_eq!(clob_ask_count(&fixture), 1, "the claimed ask still stands");

    // Once the claim lapses the ask is ordinary depth again, and both orders
    // rest: the unattested one never filled, and neither did the attested one.
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let lapsed = clock.slot + RESERVATION_GRACE_SLOTS + 1;
    fixture.svm.warp_to_slot(lapsed);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        lapsed,
    );

    assert_eq!(
        clob_bid_count(&fixture),
        2,
        "both takes rest: neither reached the claimed ask"
    );
}

/// A fill skips a latched maker and lands, instead of reverting on them.
///
/// Relay proves a subaccount below its floor and trips the authority-wide
/// latch. From then on that authority may not take risk-increasing fills
/// anywhere, so velocity sizes them at zero before the books are quoted and
/// the book passes their orders over. The orders stay where they are — the
/// latch is a fact about the account, and clearing someone's book is the
/// maker's own call or a keeper's, not a side effect of a stranger's fill.
#[test]
fn a_fill_skips_a_latched_maker_instead_of_reverting() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();
    let maker_stats = maker_stats_address(&fixture);
    // Latched: the account is otherwise solvent, and flat, so every order it
    // rests is risk-increasing.
    set_tripped_user_stats(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 4);
    place_clob_ask(&mut fixture, 100 * PRICE, UNIT / 4);
    assert_eq!(clob_ask_count(&fixture), 2);

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // The latched maker's pair: the clamp reads it, and the clear writes it.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Limit,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT / 2,
                    price: 99 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    // Lands rather than reverting on the latched maker.
    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect("the fill routes around the latched maker instead of reverting");

    // Passed over, not filled and not cancelled.
    assert_eq!(
        clob_ask_count(&fixture),
        2,
        "a latched maker's orders are skipped, not taken off the book"
    );

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        maker.perp_positions[0].base_asset_amount, 0,
        "and nothing was filled against them"
    );
}

/// A maker with room for part of its resting depth fills exactly that far.
///
/// The maker rests 0.5 of asks at 99 while the oracle reads 100, so every base
/// it sells costs it 1 of collateral it never reserved — the placement priced
/// the order as though it would fill at the oracle. An equity floor 0.4 below
/// its equity leaves 0.36 to spend after the haircut, which buys 0.36 base.
/// The first ask goes whole, the second goes part way, and what is left of it
/// stays on the book.
#[test]
fn a_maker_fills_as_far_as_its_collateral_reaches() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 4);
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 4);
    assert_eq!(clob_ask_count(&fixture), 2);

    // Read back after placing, so the reserve the placements took is kept.
    let mut maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    let equity = 10_000 * QUOTE_PRECISION_I64 as u64;
    maker.equity_floor = equity - 400_000;
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &maker);

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Limit,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT / 2,
                    price: 99 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(800_000), ix],
        &[],
    )
    .expect("the fill takes the room the maker has and stops");

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        maker.perp_positions[0].base_asset_amount, -360_000_000,
        "0.4 of headroom, less the haircut, buys 0.36 base at a gap of 1"
    );
    assert_eq!(
        clob_ask_count(&fixture),
        1,
        "the second ask is filled part way and its remainder still rests"
    );
}

/// A partially-filled place-and-take limit rests its remainder on the CLOB:
/// the taker fills half against the book's maker, and the leftover half
/// becomes a resting book bid with its aggregates reserved. Nothing is written
/// into `User.orders`.
#[test]
fn place_and_take_rests_the_remainder_on_the_clob() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();

    // The book's maker, with an ask 0.5 @ 99 for the take leg. The pair has to
    // be in the transaction for the fill to settle against it.
    let maker_stats = maker_stats_address(&fixture);
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    // The taker.
    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    // initialize_user starts ids at 1; a zeroed id space wraps get_last_order_id.
    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    // The quoter section: v1 routes, so the taker names the entries it wants
    // consulted. The market's canonical CLOB is mandatory, and its registered
    // CPI accounts have to be resolvable from this list even though the
    // remainder-placement leg names them too — same locks, one index byte.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let params = OrderParams {
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: PositionDirection::Long,
        base_asset_amount: UNIT,
        price: 99 * PRICE,
        market_index: 0,
        post_only: PostOnlyParam::None,
        ..OrderParams::default()
    };
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params,
                success_condition: None,
            },
        }
        .data(),
    };
    let meta = send(&mut fixture.svm, &taker_authority, ix, &[]).unwrap();

    // Half filled against the maker; the other half rests on the CLOB as a
    // bid, its ref left as the transaction's return data.
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount,
        (UNIT / 2) as i64,
        "took the book maker's half"
    );

    assert!(
        taker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "no DLOB remainder rests"
    );
    assert_eq!(
        taker.perp_positions[0].open_bids,
        (UNIT / 2) as i64,
        "remainder reserved on the book"
    );
    assert_eq!(taker.open_orders, 1);
    let book = fixture.svm.get_account(&fixture.clob_market).unwrap().data;
    let bid_count = u32::from_le_bytes(book[136..140].try_into().unwrap());
    assert_eq!(bid_count, 1, "remainder rests as a clob bid");
    let order_ref = velocity::state::prop_amm::ClobOrderRefV0 {
        node_index: u32::from_le_bytes(meta.return_data.data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(meta.return_data.data[4..12].try_into().unwrap()),
    };

    assert!(order_ref.order_id > 0, "order ref returned to the client");
}

/// A market order's remainder rests on the book too, at the worst price the
/// order already agreed to.
///
/// A migrated remainder is taker-origin. Nobody can take it at that bound
/// while a counterparty crosses it, and the cross settles at the
/// counterparty's price.
#[test]
fn place_and_take_rests_a_market_order_remainder_on_the_clob() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();
    // A CLOB ask 0.5 @ 99 to take, leaving half the order unfilled.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT,
                    // A market order's bound: the worst fill it agreed to, and
                    // the only price its remainder can rest at.
                    price: 101 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(800_000), ix],
        &[],
    )
    .expect("market order fills what it can and rests the rest");

    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert!(
        taker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "nothing is left on the DLOB, where nothing would fill it"
    );
    assert_eq!(
        clob_bid_count(&fixture),
        1,
        "the remainder rests on the book instead"
    );
    assert_eq!(
        clob_best_bid_price(&fixture),
        Some(101 * PRICE),
        "it rests at the bound it already agreed to, and no better"
    );
}

// ---------------------------------------------------------------------------
// Midpoint spline quoter: a maker's PDA instance of the midpoint program,
// registered as a Custom entry, quoting offsets around a maker-fed mid.
// ---------------------------------------------------------------------------

/// The taker's signed-message record. Seed-pinned on the crank, so it is
/// passed whether or not it exists — a taker that never sent one has no
/// account here, and that reads as no route.
fn signed_msg_user_orders_pda(authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"SIGNED_MSG", authority.as_ref()], &velocity_id()).0
}

fn instructions_sysvar() -> Pubkey {
    "Sysvar1nstructions1111111111111111111111111"
        .parse()
        .unwrap()
}

fn midpoint_instance_pda(authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"midpoint",
            0u16.to_le_bytes().as_ref(),
            authority.as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &midpoint_id(),
    )
    .0
}

/// Borsh-encode a midpoint spline side: Some(vec![(offset_ppm, size)]).
fn encode_side(levels: &[(u64, u64)], out: &mut Vec<u8>) {
    out.push(1);
    out.extend_from_slice(&(levels.len() as u32).to_le_bytes());
    for (offset_ppm, size) in levels {
        out.extend_from_slice(&offset_ppm.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
    }
}

struct MidpointMaker {
    authority: Keypair,
    hot: Keypair,
    user: Pubkey,
    stats: Pubkey,
    instance: Pubkey,
    entry: Pubkey,
}

/// Stand up a midpoint maker end to end: `User` at the true PDA, a midpoint
/// instance PDA (the market's quoter slab as execute authority), spline
/// levels around a $100 mid, and the approved Custom registry entry whose
/// CPI legs carry the instance (+ the slab on execute).
fn setup_midpoint_maker(fixture: &mut Fixture, deposit: u64, side_size: u64) -> MidpointMaker {
    setup_midpoint_maker_with_flow(fixture, deposit, side_size, false)
}

fn setup_midpoint_maker_with_flow(
    fixture: &mut Fixture,
    deposit: u64,
    side_size: u64,
    require_attested_flow: bool,
) -> MidpointMaker {
    let authority = Keypair::new();
    let hot = Keypair::new();
    for kp in [&authority, &hot] {
        fixture.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }

    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    // The instance's execute authority is the market's quoter slab — the one
    // identity velocity signs every quoter CPI as.
    let entry = quoter_pda(0, &midpoint_id(), &user);
    set_user_account(
        &mut fixture.svm,
        user,
        &trading_user(&authority.pubkey(), deposit, None),
    );

    let stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(&mut fixture.svm, stats, &authority.pubkey());

    // Create the instance. Config borsh: market u16, sub u16, base_precision
    // u64, staleness u64, tick u64, step u64, min u64, attested bool,
    // deviation ppm u64.
    let instance = midpoint_instance_pda(&authority.pubkey());
    let mut data = ix_discriminator("initialize_quoter_v0").to_vec();
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&UNIT.to_le_bytes());
    data.extend_from_slice(&1_000u64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.push(require_attested_flow as u8);
    // An instance must name a deviation band at creation. Wide enough that
    // these fixtures price off the mid they set rather than off the band.
    data.extend_from_slice(&500_000u64.to_le_bytes());
    let ix = Instruction {
        program_id: midpoint_id(),
        accounts: vec![
            AccountMeta::new(fixture.keeper.pubkey(), true),
            // The maker's config key and the quoted wallet are separate
            // identities now; this fixture runs both as one keypair, which
            // still exercises the consent rule (the quoted wallet signs and
            // seeds the PDA).
            AccountMeta::new_readonly(authority.pubkey(), true),
            AccountMeta::new_readonly(authority.pubkey(), true),
            AccountMeta::new_readonly(fixture.quoter_slab, false),
            AccountMeta::new_readonly(hot.pubkey(), false),
            AccountMeta::new(instance, false),
            AccountMeta::new_readonly("11111111111111111111111111111111".parse().unwrap(), false),
        ],

        data,
    };

    send(&mut fixture.svm, &fixture.keeper, ix, &[&authority]).unwrap();

    // Arm the spline: mid $100, one 10bps rung per side.
    let mut data = ix_discriminator("set_levels_v0").to_vec();
    data.push(1);
    data.extend_from_slice(&(100 * PRICE).to_le_bytes());
    data.push(0); // sequence: None
    encode_side(&[(1_000, side_size)], &mut data); // bids
    encode_side(&[(1_000, side_size)], &mut data); // asks
    let ix = Instruction {
        program_id: midpoint_id(),
        accounts: vec![
            AccountMeta::new(instance, false),
            AccountMeta::new_readonly(hot.pubkey(), true),
        ],

        data,
    };

    send(&mut fixture.svm, &fixture.keeper, ix, &[&hot]).unwrap();

    // Register + approve the Custom entry (creation by the quoted user's
    // authority — consent), CPI legs carrying instance + sysvar.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            state: state_pda(),
            payer: fixture.keeper.pubkey(),
            authority: authority.pubkey(),
            quoter: entry,
            perp_market: perp_market_pda(0),
            quoter_slab: None,
            quoter_program: midpoint_id(),
            user,
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoter {
            args: InitializeQuoterArgs {
                market_index: 0,
                quoter_type: QuoterType::Custom,
                response_account: instance,
                quote_v0_discriminator: ix_discriminator("quote_v0"),
                // The midpoint fills from one account, so it declares no L3
                // leg and the view attributes its ladder to that user.
                quote_l3_v0_discriminator: [0u8; 8],
                execute_v0_discriminator: ix_discriminator("execute_v0"),
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &fixture.keeper, ix, &[&authority]).unwrap();

    // One unified account list: the quote leg forwards the instance, the
    // execute leg adds the slab, the CPI signer.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterAccounts {
            authority: authority.pubkey(),
            quoter: entry,
            // A Custom entry answers to its own stored authority.
            state: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterAccounts {
            args: UpdateQuoterAccountsArgs {
                metas: vec![
                    QuoterAccountMetaArg {
                        pubkey: instance,
                        is_writable: true,
                    },
                    QuoterAccountMetaArg {
                        pubkey: fixture.quoter_slab,
                        is_writable: false,
                    },
                ],

                quote_indexes: vec![0],
                execute_indexes: vec![0, 1],
            },
        }
        .data(),
    };

    send(&mut fixture.svm, &fixture.keeper, ix, &[&authority]).unwrap();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: fixture.admin.pubkey(),
            state: state_pda(),
            quoter: entry,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            quoter_program: midpoint_id(),
            quoter_program_data: Some(program_data_pda(&midpoint_id())),
            clob_market: None,
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved {
            args: UpdateQuoterApprovedArgs { approved: true },
        }
        .data(),
    };

    send(&mut fixture.svm, &fixture.admin, ix, &[]).unwrap();

    MidpointMaker {
        authority,
        hot,
        user,
        stats,
        instance,
        entry,
    }
}

/// Fill a fresh taker's long market order across the baseline CLOB entry
/// (empty book), the midpoint instance, and the vAMM.
fn fill_long_through_midpoint(
    fixture: &mut Fixture,
    maker: &MidpointMaker,
    size: u64,
) -> (Pubkey, litesvm::types::TransactionMetadata) {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // Maker section: the midpoint's quoted user.
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    // Quoter section: the market's slab, then the union of the book's and
    // the midpoint's CPI accounts and programs. Carrying a slot's response
    // account is what consults it.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(maker.instance, false));
    accounts.push(AccountMeta::new_readonly(midpoint_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: size,
                    price: 105 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };

    // Three quoter CPI legs + the vAMM outgrow the 200k default.
    let meta = send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    (taker_user, meta)
}

/// Read a spline level's (size, filled) off the instance account. Layout:
/// disc(8) + 4×32 addresses + 8×u64 + 2×u16 + 4×u8, then bids[64], asks[64]
/// of (offset u64, size u64, filled u64).
fn midpoint_ask_level(svm: &litesvm::LiteSVM, instance: &Pubkey, index: usize) -> (u64, u64) {
    let data = svm.get_account(instance).unwrap().data;
    // disc + 4 addresses + 8 u64s + 2 u16s + 4 u8s + 72 bytes of reserved
    // tail space. Mirrors MidpointQuoterV0 up to `bids`.
    let levels_base = 8 + 4 * 32 + 8 * 8 + 2 * 2 + 4 + 72;
    let asks_base = levels_base + 64 * 24;
    let off = asks_base + index * 24;
    (
        u64::from_le_bytes(data[off + 8..off + 16].try_into().unwrap()),
        u64::from_le_bytes(data[off + 16..off + 24].try_into().unwrap()),
    )
}

#[test]
fn router_fill_routes_through_a_midpoint_spline_quoter() {
    let mut fixture = setup();
    // Midpoint asks 0.5 @ mid + 10bps = 100.1; vAMM ask sits ~1% above mid.
    let maker = setup_midpoint_maker(&mut fixture, 10_000 * SPOT_BALANCE_PRECISION_U64, UNIT / 2);

    let (taker_user, meta) = fill_long_through_midpoint(&mut fixture, &maker, UNIT);

    // Taker fully filled: 0.5 from the spline, the remainder from the vAMM.
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(taker.perp_positions[0].base_asset_amount, UNIT as i64);
    assert!(
        taker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "the take is ephemeral: nothing rests in a slot"
    );

    // The quoted user went short the spline's slice at the rung price —
    // 0.5 @ 100.1 = 50.05 quote (before fees, floored like the CLOB).
    let mm: User = read_zero_copy(&fixture.svm, &maker.user);
    assert_eq!(mm.perp_positions[0].base_asset_amount, -((UNIT / 2) as i64));
    assert_eq!(
        mm.perp_positions[0].quote_entry_amount,
        (100_100_000u64 / 2) as i64
    );

    // The instance's ask rung is consumed — standing intent depletes.
    let (size, filled) = midpoint_ask_level(&fixture.svm, &maker.instance, 0);
    assert_eq!(size, UNIT / 2);
    assert_eq!(filled, UNIT / 2);

    println!(
        "CU — router fill through midpoint + vAMM: {}",
        meta.compute_units_consumed
    );
}

#[test]
fn midpoint_book_is_margin_clamped_to_its_quoted_user() {
    let mut fixture = setup();
    // A thin maker quoting far beyond its margin: 20 USDC of collateral
    // against a 10-unit (~$1000) quote. The router sizes the book against
    // the quoted user before the split, so the fill takes only what the
    // account supports and routes the rest to the vAMM.
    let maker = setup_midpoint_maker(&mut fixture, 20 * SPOT_BALANCE_PRECISION_U64, 10 * UNIT);

    let (taker_user, _) = fill_long_through_midpoint(&mut fixture, &maker, 2 * UNIT);

    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(taker.perp_positions[0].base_asset_amount, (2 * UNIT) as i64);

    let mm: User = read_zero_copy(&fixture.svm, &maker.user);
    let mm_base = -mm.perp_positions[0].base_asset_amount;
    assert!(mm_base > 0, "the clamped book still fills something");
    assert!(
        (mm_base as u64) < 2 * UNIT,
        "margin clamp truncated the spline: {mm_base}"
    );
}

#[test]
fn router_fill_splits_across_clob_midpoint_and_vamm() {
    let mut fixture = setup();
    // Three sources at three prices: CLOB ask 0.5 @ 100, midpoint spline
    // 0.5 @ 100.1 (mid + 10bps), vAMM ~1% above mid. A 1.5-unit taker
    // consumes all three.
    place_clob_ask(&mut fixture, 100 * PRICE, UNIT / 2);
    let maker = setup_midpoint_maker(&mut fixture, 10_000 * SPOT_BALANCE_PRECISION_U64, UNIT / 2);

    // The CLOB maker's stats ride the maker map alongside the midpoint's.
    let clob_maker_stats = maker_stats_address(&fixture);

    let taker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    taker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // Maker section: both quoted users.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(clob_maker_stats, false));
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    // Quoter section: the market's slab + the union of both slots' CPI
    // accounts. Carrying a slot's response account is what consults it.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(maker.instance, false));
    accounts.push(AccountMeta::new_readonly(midpoint_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: velocity::state::order_params::OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Long,
                    base_asset_amount: UNIT + UNIT / 2,
                    price: 105 * PRICE,
                    market_index: 0,
                    post_only: velocity::state::order_params::PostOnlyParam::None,
                    ..velocity::state::order_params::OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };
    let meta = send_with_ixs(
        &mut fixture.svm,
        &taker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    // All three sources filled: CLOB 0.5 @ 100 (best), midpoint 0.5 @
    // 100.1, vAMM the remaining 0.5.
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount,
        (UNIT + UNIT / 2) as i64
    );

    assert!(
        taker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "the take is ephemeral: nothing rests in a slot"
    );

    let clob_maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        clob_maker.perp_positions[0].base_asset_amount,
        -((UNIT / 2) as i64)
    );
    assert_eq!(clob_ask_count(&fixture), 0);

    let mm: User = read_zero_copy(&fixture.svm, &maker.user);
    assert_eq!(mm.perp_positions[0].base_asset_amount, -((UNIT / 2) as i64));
    let (_, filled) = midpoint_ask_level(&fixture.svm, &maker.instance, 0);
    assert_eq!(filled, UNIT / 2);

    println!(
        "CU — router fill across CLOB + midpoint + vAMM: {}",
        meta.compute_units_consumed
    );
}

// ---------------------------------------------------------------------------
// Generic quoter-cross discovery: relay conditions per Custom entry, priced
// through the entry's registered quote_v0 surface — no per-program code.
// ---------------------------------------------------------------------------

/// Declare the midpoint's mid region as its reprice watch (the maker knows
/// their program's layout; velocity doesn't) and re-approve the entry.
fn declare_midpoint_watch(fixture: &mut Fixture, maker: &MidpointMaker) {
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterWatch {
            authority: maker.authority.pubkey(),
            quoter: maker.entry,
            watch_account: maker.instance,
            state: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterWatch {
            args: velocity::instructions::UpdateQuoterWatchArgs {
                watch_offset: 136, // mid_price + mid_slot
                watch_len: 16,
            },
        }
        .data(),
    };
    let authority = maker.authority.insecure_clone();
    send(&mut fixture.svm, &authority, ix, &[]).unwrap();
    // The declaration lands on the staging entry only; the approved copy in
    // the slab keeps serving until the admin copies it in again.
    let entry: velocity::state::prop_amm::QuoterV0 = read_zero_copy(&fixture.svm, &maker.entry);
    assert_eq!(
        entry.config.watch_account.to_bytes(),
        maker.instance.to_bytes()
    );
    assert_eq!(entry.config.watch_offset, 136);
    assert_eq!(entry.config.watch_len, 16);
    let (_, live) = find_slab_slot(&fixture.svm, 0, &maker.entry).unwrap();
    assert_eq!(
        live.config.watch_len, 0,
        "a staging edit does not reach the live copy until re-approval"
    );

    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: fixture.admin.pubkey(),
            state: state_pda(),
            quoter: maker.entry,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            quoter_program: midpoint_id(),
            quoter_program_data: Some(program_data_pda(&midpoint_id())),
            clob_market: None,
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved {
            args: UpdateQuoterApprovedArgs { approved: true },
        }
        .data(),
    };
    let admin = fixture.admin.insecure_clone();
    send(&mut fixture.svm, &admin, ix, &[]).unwrap();
    let (_, live) = find_slab_slot(&fixture.svm, 0, &maker.entry).unwrap();
    assert_eq!(live.config.watch_len, 16, "re-approval copies the watch in");
}

/// Attach the per-entry cross conditions (permissionless; rent on the
/// keeper here).
fn attach_quoter_cross(fixture: &mut Fixture, maker: &MidpointMaker) -> Pubkey {
    let cross_conditions = Pubkey::find_program_address(
        &[b"quoter_cross_conditions", maker.entry.as_ref()],
        &velocity_id(),
    )
    .0;
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoterCrossConditions {
            payer: fixture.keeper.pubkey(),
            state: state_pda(),
            quoter: maker.entry,
            perp_market: perp_market_pda(0),
            quoter_slab: fixture.quoter_slab,
            market_conditions: Pubkey::find_program_address(
                &[b"clob_crank_conditions", 0u16.to_le_bytes().as_ref()],
                &velocity_id(),
            )
            .0,
            cross_conditions,
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoterCrossConditions {
            args: InitializeQuoterCrossConditionsArgs {
                expire_fallback_slots: 100,
            },
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    cross_conditions
}

/// Run the generic resolver the way a turner does. Its account list is
/// exactly what the attach registered: scratch, conditions, book, state, the
/// market's slab, the quoted user and the CLOB program, then the entry's
/// registered quote surface + program.
fn run_quoter_cross_resolver(
    fixture: &mut Fixture,
    maker: &MidpointMaker,
    cross_conditions: Pubkey,
) -> Option<velocity::relay_spec::ResolvedCrankV0> {
    let mut accounts = velocity::accounts::ResolveCrankCrossMatchQuoter {
        scratch: relay_scratch_pda(),
        cross_conditions,
        clob_market: fixture.clob_market,
        state: state_pda(),
        quoter_slab: fixture.quoter_slab,
        user: maker.user,
        clob_program: clob_id(),
    }
    .to_account_metas(None);
    // The entry's registered quote surface, exactly as the attach stored it:
    // the instance, then the quoter program. The resolver only quotes, so the
    // execute leg's signer slot is not part of the list.
    accounts.push(AccountMeta::new(maker.instance, false));
    accounts.push(AccountMeta::new_readonly(midpoint_id(), false));
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::ResolveCrankCrossMatchQuoter {}.data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let pointer = velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
    if !pointer.has_work() {
        return None;
    }

    let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
    let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
    Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
}

/// The whole generic loop, turner-shaped: a maker declares their reprice
/// region, cross conditions attach permissionlessly, the resolver prices
/// the quoter through its registered quote_v0 CPI (no midpoint-specific
/// velocity code anywhere), and the staged crank_cross_match lands
/// unsigned — protocol User round-trips the cross, keeper paid from the
/// market reservoir.
#[test]
fn generic_quoter_cross_conditions_discover_and_fill_a_midpoint_clob_cross() {
    use velocity::state::quoter_cross::{
        QuoterCrossConditionsV0, QUOTER_CROSS_CLOB, QUOTER_CROSS_FALLBACK, QUOTER_CROSS_WATCH,
    };

    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    let market_conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);
    fixture
        .svm
        .airdrop(&market_conditions, 1_000_000_000)
        .unwrap();

    // Midpoint asks 2.0 @ 100.1 (mid 100 + 10bps). An open instance: this
    // crank never reports protected flow while a `Custom` quoter is in the
    // route, so an instance that requires it is out of reach here. The
    // protected case has its own test below.
    let maker = setup_midpoint_maker_with_flow(
        &mut fixture,
        10_000 * SPOT_BALANCE_PRECISION_U64,
        2 * UNIT,
        false,
    );

    declare_midpoint_watch(&mut fixture, &maker);
    let cross_conditions = attach_quoter_cross(&mut fixture, &maker);

    // The attach wrote the three conditions: the maker's watch, the CLOB
    // bests, the fallback poll — all pointing at the generic resolver.
    let acct: QuoterCrossConditionsV0 = read_zero_copy(&fixture.svm, &cross_conditions);
    let (header, conditions) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    assert_eq!(header.num_conditions, 3);
    assert_eq!(
        account_change(&conditions[QUOTER_CROSS_WATCH]).0,
        maker.instance.to_bytes()
    );
    assert_eq!(account_change(&conditions[QUOTER_CROSS_WATCH]).1, 136);
    assert_eq!(account_change(&conditions[QUOTER_CROSS_WATCH]).2, 16);
    assert_eq!(conditions[QUOTER_CROSS_WATCH].min_payment(), PAYMENT);
    assert_eq!(
        account_change(&conditions[QUOTER_CROSS_CLOB]).0,
        fixture.clob_market.to_bytes()
    );
    assert_eq!(
        conditions[QUOTER_CROSS_FALLBACK].wake(),
        Ok(velocity::relay_spec::WakeView::EverySlots { slots: 100 })
    );

    // The resolver list is stored once, in the relay block's built-in
    // region, and every condition points to it indirectly, so both counts
    // land on 9. The slab and the CLOB program are on the list because the
    // resolver quotes the book through the approved slot, not the account.
    assert_eq!(conditions[QUOTER_CROSS_WATCH].resolvers().count, 9);
    assert_eq!(acct.relay.resolver_refs().len(), 9);

    // Nothing crossed yet: the resolver reports no work.
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    assert!(run_quoter_cross_resolver(&mut fixture, &maker, cross_conditions).is_none());

    // A CLOB bid at 101 crosses the midpoint's 100.1 ask — 90bps of spread
    // clears two tier-0 taker fees.
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let clob_maker_authority = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &clob_maker_authority, ix, &[]).unwrap();
    // The staged executor derives the CLOB maker's stats PDA.
    let clob_maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        clob_maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    fixture.svm.warp_to_slot(13);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        13,
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let payout_before = fixture.svm.get_balance(&payout).unwrap();

    let resolved = run_quoter_cross_resolver(&mut fixture, &maker, cross_conditions)
        .expect("crossed books stage a crank");
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankCrossMatch::DISCRIMINATOR,
        payout,
    );

    // The crosser (CLOB bid) is long, the midpoint maker short, the
    // reservoir paid the keeper, and the crossed bid is off the book.
    let crosser: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(crosser.perp_positions[0].base_asset_amount, UNIT as i64);
    let mm: User = read_zero_copy(&fixture.svm, &maker.user);
    assert_eq!(mm.perp_positions[0].base_asset_amount, -(UNIT as i64));
    assert_eq!(
        fixture.svm.get_balance(&payout).unwrap(),
        payout_before + PAYMENT
    );
    assert_eq!(clob_bid_count(&fixture), 0);
    // The midpoint's rung depleted (standing intent).
    let (_, filled) = midpoint_ask_level(&fixture.svm, &maker.instance, 0);
    assert_eq!(filled, UNIT);
}

/// A quoter that prices on demand must not reach a speed-bumped book through
/// the arb crank.
///
/// The book's row rested, so the crank can measure it. The quoter that crosses
/// that row priced during the call and keeps no resting order, so the crank can
/// measure nothing about it. The crank therefore reports unprotected flow, the
/// book quotes it no depth, and the legs do not cross. Without this rule an
/// approved quoter lifts a resting order with no delay at all, where the same
/// take through swift waits out the hold.
#[test]
fn a_custom_quoter_cross_cannot_reach_a_speed_bumped_book() {
    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    let protocol_user = set_protocol_user(&mut fixture.svm);
    let (signer, _) = velocity_signer_pda();
    let protocol_stats =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();

    // Open flow, so the instance refuses nothing itself and the book's speed
    // bump is the only gate under test. It bids 99.9 around its $100 mid.
    let maker = setup_midpoint_maker_with_flow(
        &mut fixture,
        10_000 * SPOT_BALANCE_PRECISION_U64,
        2 * UNIT,
        false,
    );
    let clob_maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        clob_maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    // The book asks 0.5 @ 99, which the midpoint's bid crosses. The order is
    // placed before the speed bump so it needs no attestation of its own; the
    // bump under test is the one the route reads off the slab.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    set_clob_default_activation_delay(&mut fixture, 2);
    // Ten slots of rest, far past the two the crank measures. The book's own
    // row is not the unmeasured side here.
    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let mut accounts = velocity::accounts::CrankCrossMatch {
        state: state_pda(),
        authority: payout,
        taker: protocol_user,
        taker_stats: protocol_stats,
        crank_conditions: conditions,
        perp_market: perp_market_pda(0),
        quoter_slab: fixture.quoter_slab,
        instructions_sysvar: instructions_sysvar(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(clob_maker_stats, false));
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    // The quoter section: the slab, the book, then the midpoint. Carrying a
    // slot's response account is what consults it.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(maker.instance, false));
    accounts.push(AccountMeta::new_readonly(midpoint_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::CrankCrossMatch {
            args: CrankCrossMatchArgs {
                market_index: 0,
                size: UNIT / 2,
            },
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper,
        &[compute_unit_limit_ix(1_400_000), ix],
        &[],
    )
    .expect_err("a quoter that prices on demand must not lift a bumped book");
    // The book quotes nothing, so the buy leg reaches only the vAMM while the
    // sell leg reaches the midpoint: the two legs do not cross.
    assert_velocity_error(&err, ErrorCode::CrossMatchLegsDoNotCross);

    // The book keeps its ask and the midpoint's user took no position.
    assert_eq!(clob_ask_count(&fixture), 1);
    let mm: User = read_zero_copy(&fixture.svm, &maker.user);
    assert_eq!(mm.perp_positions[0].base_asset_amount, 0);
}

/// An instance that requires attested flow is out of the arb crank's reach,
/// and resting does not bring it into reach.
///
/// The crank reports protected flow only for sources whose rest it measured.
/// A `Custom` quoter keeps no resting order, so the crank measures nothing and
/// reports nothing, whatever the crossing book order's age. Such an instance
/// takes flow through swift, where the hold is the protection it asked for.
#[test]
fn a_protected_instance_is_out_of_the_cross_cranks_reach() {
    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    let market_conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);
    fixture
        .svm
        .airdrop(&market_conditions, 1_000_000_000)
        .unwrap();

    let maker = setup_midpoint_maker_with_flow(
        &mut fixture,
        10_000 * SPOT_BALANCE_PRECISION_U64,
        2 * UNIT,
        true,
    );

    declare_midpoint_watch(&mut fixture, &maker);
    let cross_conditions = attach_quoter_cross(&mut fixture, &maker);

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    // A CLOB bid at 101 crosses the midpoint's 100.1 ask.
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let clob_maker_authority = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &clob_maker_authority, ix, &[]).unwrap();
    let clob_maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        clob_maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();

    // One slot of rest, then ten. Neither reaches the instance, because the
    // crank never vouched for the flow in the first place.
    for slot in [13u64, 23] {
        fixture.svm.warp_to_slot(slot);
        set_oracle(
            &mut fixture.svm,
            fixture.oracle,
            (100 * PRICE_PRECISION) as i64,
            slot,
        );

        let resolved = run_quoter_cross_resolver(&mut fixture, &maker, cross_conditions)
            .expect("still crossed, still staged");
        let keeper = fixture.keeper.insecure_clone();
        let err = send_with_ixs(
            &mut fixture.svm,
            &keeper,
            &[
                compute_unit_limit_ix(1_400_000),
                staged_executor_ix(&resolved, payout),
            ],
            &[],
        )
        .expect_err("a protected instance must not fill from a crank");
        // The instance quotes nothing, so the buy leg reaches only the vAMM
        // and the sell leg only the book: the two legs do not cross, which is
        // a more specific refusal than an unprofitable total.
        assert_velocity_error(&err, ErrorCode::CrossMatchLegsDoNotCross);
    }

    // The crossed bid is still on the book and nobody took a position.
    assert_eq!(clob_bid_count(&fixture), 1);
    let mm: User = read_zero_copy(&fixture.svm, &maker.user);
    assert_eq!(mm.perp_positions[0].base_asset_amount, 0);
}

// ---------------------------------------------------------------------------
// Trigger orders as relay conditions: per-user OnValueCross watches synced
// from live orders, resolvers staging the dual-mode trigger cranks.
// ---------------------------------------------------------------------------

fn user_conditions_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_conditions", user.as_ref()], &velocity_id()).0
}

fn sync_trigger_conditions(
    fixture: &mut Fixture,
    user: Pubkey,
    market_conditions: Pubkey,
    with_clob: bool,
) {
    let mut accounts = velocity::accounts::SyncTriggerConditions {
        payer: fixture.keeper.pubkey(),
        user,
        trigger_conditions: user_conditions_pda(&user),
        rent: "SysvarRent111111111111111111111111111111111"
            .parse()
            .unwrap(),
        system_program: "11111111111111111111111111111111".parse().unwrap(),
    }
    .to_account_metas(None);
    // Margin maps + per-market crank inputs, any order.
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(market_conditions, false));
    // The market's slab routes a fired stop-market to the fire-to-book
    // resolver. Left out, the market reads as book-less and the trigger stays
    // on the plain flip crank.
    if with_clob {
        accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    }

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::SyncTriggerConditions {}.data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
}

/// Run `resolve_trigger_market_order_v1` the way a turner does, and read back
/// whatever it staged.
///
/// The trigger-limit resolver takes the same account set and differs only in
/// its discriminator, so a test for it belongs here beside this one.
fn run_trigger_market_resolver(
    fixture: &mut Fixture,
    user: Pubkey,
) -> Option<velocity::relay_spec::ResolvedCrankV0> {
    let conditions = user_conditions_pda(&user);
    let accounts = velocity::accounts::ResolveTriggerMarketOrderV1 {
        scratch: relay_scratch_pda(),
        trigger_conditions: conditions,
        user,
        oracle: fixture.oracle,
        perp_market: perp_market_pda(0),
    }
    .to_account_metas(None);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::ResolveTriggerMarketOrderV1 {}.data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let pointer = velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
    if !pointer.has_work() {
        return None;
    }

    let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
    let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
    Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
}

/// A trigger-limit on a market with a vetted CLOB syncs to the
/// `trigger_limit_order_v1` executor path.
#[test]
fn trigger_limit_sync_targets_the_clob_executor() {
    use velocity::state::user_conditions::{UserConditionsV0, TRIGGER_SLOT_BASE};

    let mut fixture = setup();
    let market_conditions = init_crank_conditions(&mut fixture, 10_000);
    let authority = Keypair::new();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let mut order = Order::default();
    order.order_id = 3;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerLimit;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Long;
    order.base_asset_amount = UNIT;
    order.price = 96 * PRICE;
    order.trigger_price = 97 * PRICE;
    order.trigger_condition = velocity::state::user::OrderTriggerCondition::Below;
    set_user_account(
        &mut fixture.svm,
        user,
        &trading_user(
            &authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(order),
        ),
    );

    sync_trigger_conditions(&mut fixture, user, market_conditions, true);

    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &user_conditions_pda(&user));
    let (_, block) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    let trig = &block[TRIGGER_SLOT_BASE..];
    assert_eq!(value_cross(&trig[0]).4, 1, "Below trigger watches downward");
    // The CLOB path shows up as the resolver the condition names — the
    // executor it stages (`trigger_limit_order_v1`) is that resolver's answer,
    // asserted where the payload is read.
    assert_eq!(
        trig[0].crank_spec().resolver_disc,
        velocity::instruction::ResolveTriggerLimitOrderV1::DISCRIMINATOR
    );
    assert_eq!(
        acct.trigger_slots[0].quoter_slab.to_bytes(),
        fixture.quoter_slab.to_bytes()
    );
    assert_eq!(
        acct.trigger_slots[0].clob_market.to_bytes(),
        fixture.clob_market.to_bytes()
    );
}

/// A stop-market on a book market syncs to the fire-to-book resolver, and that
/// resolver, run turner-shaped, fires the trigger to the book. The staged
/// executor carries no quoter tail, so it does not fill: the whole fired order
/// rests taker-origin even though a crossing ask sits on the book, and the
/// cross crank settles it later at the best price across every source. A
/// resolver fill would have taken the book alone, blind to the propAMMs.
#[test]
fn trigger_market_fires_to_the_book_through_its_resolver() {
    use velocity::state::user_conditions::{UserConditionsV0, TRIGGER_SLOT_BASE};

    let mut fixture = setup();
    const PAYMENT: u64 = 25_000;
    let market_conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);
    fixture
        .svm
        .airdrop(&market_conditions, 1_000_000_000)
        .unwrap();

    // A resting ask the fired buy-stop crosses, and no vAMM to absorb it first.
    let maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    pause_amm_fill(&mut fixture.svm);
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    // A long buy-stop armed below the oracle: it fires when the price reaches
    // 99 and becomes a live market order.
    let authority = Keypair::new();
    fixture
        .svm
        .airdrop(&authority.pubkey(), 1_000_000_000)
        .unwrap();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let mut order = Order::default();
    order.order_id = 1;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerMarket;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Long;
    order.base_asset_amount = UNIT;
    order.trigger_price = 99 * PRICE;
    order.trigger_condition = velocity::state::user::OrderTriggerCondition::Above;
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    order.max_ts = clock.unix_timestamp + 1_000;
    let mut state = armed_trigger_user(
        &authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        order,
    );

    state.next_order_id = 2;
    set_user_account(&mut fixture.svm, user, &state);
    let user_stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(&mut fixture.svm, user_stats, &authority.pubkey());

    sync_trigger_conditions(&mut fixture, user, market_conditions, true);

    // The sync routes a stop-market on a book market to the v1 resolver.
    let conditions = user_conditions_pda(&user);
    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let (_, block) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    assert_eq!(
        block[TRIGGER_SLOT_BASE].crank_spec().resolver_disc,
        velocity::instruction::ResolveTriggerMarketOrderV1::DISCRIMINATOR
    );
    assert_eq!(
        acct.trigger_slots[0].quoter_slab.to_bytes(),
        fixture.quoter_slab.to_bytes()
    );

    // Crossed: the resolver stages the fire-to-book executor, and it lands
    // unsigned. The order leaves its slot and rests on the book.
    fixture.svm.warp_to_slot(13);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        13,
    );

    let resolved = run_trigger_market_resolver(&mut fixture, user)
        .expect("crossed threshold stages the fire-to-book executor");
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let payout_before = fixture.svm.get_balance(&payout).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::TriggerMarketOrderV1::DISCRIMINATOR,
        payout,
    );

    // The crank is unsigned, so the market's reservoir pays whoever turned it.
    // Without this the turner works for nothing and stops turning.
    assert_eq!(
        fixture.svm.get_balance(&payout).unwrap(),
        payout_before + PAYMENT,
        "the reservoir pays the turner that fired the trigger"
    );

    // The watch is level-triggered, so a slot left armed re-fires on every
    // block while the price stays across the threshold. The executor releases
    // it, and the released slot names no order.
    let after: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let (_, block) = velocity::relay_spec::read_block(after.block(), 0).unwrap();
    assert!(
        !block[TRIGGER_SLOT_BASE].is_active(),
        "the fired slot goes quiet rather than re-firing every block"
    );
    assert_eq!(
        after.trigger_slots[0].order_id, 0,
        "the released slot names no order"
    );

    let triggered: User = read_zero_copy(&fixture.svm, &user);
    assert!(
        triggered
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "the armed slot is freed: nothing lingers live in `User.orders`"
    );
    assert_eq!(
        triggered.perp_positions[0].base_asset_amount, 0,
        "the resolver stages no fill: nothing is taken against the book alone"
    );
    assert_eq!(
        triggered.perp_positions[0].open_bids, UNIT as i64,
        "the whole fired order is reserved against the book"
    );
    assert_eq!(
        clob_bid_count(&fixture),
        1,
        "the whole fired order rests taker-origin for the cross crank"
    );
}

/// The merged sync, called the way the localnet harness calls it — quoter
/// entry and crank-conditions account in the remaining accounts alongside
/// the margin maps. The liquidation pass writes the shared account list
/// both passes' staged executors reuse, and it must keep the non-map
/// accounts *after* the markets: `load_maps` parses positionally, and a
/// quoter filed among the oracles cuts the perp market off from the staged
/// `trigger_order` (`PerpMarketNotFound`) — the localnet run caught
/// exactly that.
#[test]
fn merged_sync_keeps_the_stored_map_section_parseable() {
    use velocity::state::user_conditions::UserConditionsV0;

    let mut fixture = setup();
    const PAYMENT: u64 = 25_000;
    let market_conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);
    fixture
        .svm
        .airdrop(&market_conditions, 1_000_000_000)
        .unwrap();

    let authority = Keypair::new();
    fixture
        .svm
        .airdrop(&authority.pubkey(), 1_000_000_000)
        .unwrap();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let mut order = Order::default();
    order.order_id = 7;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerMarket;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Short;
    order.base_asset_amount = UNIT;
    order.trigger_price = 105 * PRICE;
    order.trigger_condition = velocity::state::user::OrderTriggerCondition::Above;
    set_user_account(
        &mut fixture.svm,
        user,
        &trading_user(
            &authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(order),
        ),
    );

    let user_stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(&mut fixture.svm, user_stats, &authority.pubkey());

    // One sync for the whole block, remaining accounts as an integrator
    // passes them: maps, then the crank inputs, then the quoter entry.
    let conditions = user_conditions_pda(&user);
    let mut accounts = velocity::accounts::SyncUserConditions {
        payer: fixture.keeper.pubkey(),
        state: state_pda(),
        user,
        user_conditions: conditions,
        rent: "SysvarRent111111111111111111111111111111111"
            .parse()
            .unwrap(),
        system_program: "11111111111111111111111111111111".parse().unwrap(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(market_conditions, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::SyncUserConditions {
            args: velocity::instructions::SyncLiqConditionsArgs {
                sync_cost_units: 20_000,
                sync_fallback_slots: 3000,
            },
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    // The stored list: prefix, maps, then the tail the parser never
    // reaches — the quoter must not sit inside the map section.
    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let map = acct.read_sync_accounts();
    assert_eq!(
        map.len(),
        7,
        "oracle, spot, perp, conditions, quoter slab, book, book program"
    );
    assert_eq!(map[0].address, fixture.oracle.to_bytes());
    assert_eq!(map[1].address, spot_market_pda(0).to_bytes());
    assert_eq!(map[2].address, perp_market_pda(0).to_bytes());
    assert_eq!(map[3].address, market_conditions.to_bytes());
    assert_eq!(map[4].address, fixture.quoter_slab.to_bytes());
    // The book rides with the slab, so a liquidation staged off this list
    // reaches it and the resolver can read it to name the fill's makers.
    assert_eq!(map[5].address, fixture.clob_market.to_bytes());
    assert_eq!(map[6].address, clob_id().to_bytes());

    // And the proof it parses: the staged executor lands. The stop-market on
    // a book market fires to the book (`trigger_market_order_v1`), so the map section
    // has to parse for the fill's `load_maps`, not just a plain flip.
    fixture.svm.warp_to_slot(13);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (106 * PRICE_PRECISION) as i64,
        13,
    );

    let resolved =
        run_trigger_market_resolver(&mut fixture, user).expect("crossed threshold stages");
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::TriggerMarketOrderV1::DISCRIMINATOR,
        payout,
    );

    // The fired slot is freed: the trigger fired and the order left the DLOB,
    // which the executor reaches only after `load_maps` parsed the map.
    let triggered: User = read_zero_copy(&fixture.svm, &user);
    assert!(triggered
        .orders
        .iter()
        .all(|order| order.status != OrderStatus::Open));
}

// ---------------------------------------------------------------------------
// Liquidations via relay: conservative per-oracle thresholds, a self-sync
// watch on the user's own positions, and the with-fill executor staged with
// the protocol User as an inventory-free liquidator.
// ---------------------------------------------------------------------------

fn sync_liq_conditions(
    fixture: &mut Fixture,
    user: Pubkey,
    market_conditions: Pubkey,
    sync_cost_units: u32,
) -> Pubkey {
    let conditions = user_conditions_pda(&user);
    let mut accounts = velocity::accounts::SyncLiqConditions {
        payer: fixture.keeper.pubkey(),
        state: state_pda(),
        user,
        liq_conditions: conditions,
        rent: "SysvarRent111111111111111111111111111111111"
            .parse()
            .unwrap(),
        system_program: "11111111111111111111111111111111".parse().unwrap(),
    }
    .to_account_metas(None);
    // Margin maps (oracles, then markets), then the market's reservoir.
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(market_conditions, false));
    // Stored in the inert tail, where `load_maps` never reaches: the cancel
    // stage of the ladder needs the entry to name the book and its program.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::SyncLiqConditions {
            args: velocity::instructions::SyncLiqConditionsArgs {
                sync_cost_units,
                sync_fallback_slots: 3000,
            },
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    conditions
}

fn run_liq_resolver(
    fixture: &mut Fixture,
    user: Pubkey,
) -> Option<velocity::relay_spec::ResolvedCrankV0> {
    let conditions = user_conditions_pda(&user);
    let mut accounts = velocity::accounts::ResolveLiquidatePerpWithFill {
        scratch: relay_scratch_pda(),
        liq_conditions: conditions,
        user,
        state: state_pda(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    // The book and its program, as the sync stores them: the resolver reads
    // the book to name the makers a liquidation would settle against.
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::ResolveLiquidatePerpWithFill {}.data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let pointer = velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
    if !pointer.has_work() {
        return None;
    }

    let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
    let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
    Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
}

/// Cancelling comes before liquidating, and one watch drives both.
///
/// `force_cancel_clob_orders` answers to the initial requirement and
/// liquidation to the maintenance one, so anything liquidatable was already
/// cancellable — they are stages of one ladder, not two watches. While the
/// account rests orders the resolver stages the sweep; once the book is clear
/// the same wake resolves to the liquidation. Orders first matters: a
/// liquidation that leaves risk-increasing orders resting hands the account
/// new exposure the moment one fills.
#[test]
fn the_distress_ladder_stages_a_cancel_before_a_liquidation() {
    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    let market_conditions = init_crank_conditions(&mut fixture, PAYMENT);
    // The relay turner is paid out of the market's reservoir in
    // program-keeper mode, so it has to hold more than rent.
    fixture
        .svm
        .airdrop(&market_conditions, 1_000_000_000)
        .unwrap();
    set_protocol_user(&mut fixture.svm);
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    // The book maker rests an ask, then takes on a short it cannot carry:
    // $3 of collateral against a $100 position, which is under the 5%
    // maintenance requirement, so it is liquidatable *and* cancellable at
    // once. That is the case worth pinning — the ladder still takes the
    // orders off first. Short with a resting ask, so the order adds to the
    // position: a reducing one is passed over, which
    // `force_cancel_passes_over_a_risk_reducing_order` covers.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let maker_user = fixture.clob_maker_user;
    let mut maker = trading_user(
        &fixture.clob_maker_authority.pubkey(),
        3 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    maker.perp_positions[0].market_index = 0;
    maker.perp_positions[0].base_asset_amount = -(UNIT as i64);
    maker.perp_positions[0].quote_asset_amount = 100_000_000;
    maker.perp_positions[0].open_asks = -((UNIT / 2) as i64);
    maker.perp_positions[0].open_orders = 1;
    maker.open_orders = 1;
    maker.has_open_order = true;
    maker.next_order_id = 2;
    set_user_account(&mut fixture.svm, maker_user, &maker);

    sync_liq_conditions(
        &mut fixture,
        maker_user,
        market_conditions,
        ANY_SYNC_COST_UNITS,
    );

    // Stage one: orders are in the way, so the sweep is what gets staged —
    // not the liquidation that is also available right now.
    let resolved = run_liq_resolver(&mut fixture, maker_user)
        .expect("a distressed account with resting orders is work");
    assert_eq!(
        resolved.executor_disc,
        velocity::instruction::ForceCancelClobOrders::DISCRIMINATOR,
        "orders come off the book before the position is touched"
    );

    // Run it. The sweep takes the whole ask side, so the resolver never had
    // to read the book to name a single order ref.
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::ForceCancelClobOrders::DISCRIMINATOR,
        payout,
    );

    assert_eq!(clob_ask_count(&fixture), 0);
    let after: User = read_zero_copy(&fixture.svm, &maker_user);
    assert_eq!(after.perp_positions[0].open_orders, 0);

    // Stage two: the book is clear, so the same wake now resolves to the
    // liquidation it was holding back.
    let resolved = run_liq_resolver(&mut fixture, maker_user)
        .expect("a liquidatable account with a clear book is work");
    assert_eq!(
        resolved.executor_disc,
        velocity::instruction::LiquidatePerpWithFill::DISCRIMINATOR,
        "with the book clear the ladder moves on to the position"
    );
}

/// A leveraged long's liquidation threshold: the sync solves the
/// ceteris-paribus price where free collateral runs out, haircuts it, and
/// writes a downward OnValueCross — the "high-risk bucket boundary",
/// precomputed. The resolver reports no work while the account is healthy
/// (the level wake costs a turner nothing until the price is near), and
/// the self-sync watch covers the user's own position bytes.
#[test]
fn liq_conditions_arm_the_liveness_poll() {
    use velocity::state::user_conditions::{
        UserConditionsV0, LIQ_LIVENESS_POLL, LIQ_LIVENESS_POLL_SLOTS, LIQ_SYNC_FALLBACK,
        LIQ_SYNC_WATCH,
    };

    let mut fixture = setup();
    const PAYMENT: u64 = 10_000;
    let market_conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);

    // 10x long: 10 units at $100 against $100 of collateral.
    let authority = Keypair::new();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let mut account = trading_user(&authority.pubkey(), 100 * SPOT_BALANCE_PRECISION_U64, None);
    account.perp_positions[0].market_index = 0;
    account.perp_positions[0].base_asset_amount = (10 * UNIT) as i64;
    account.perp_positions[0].quote_asset_amount = -((1000 * 1_000_000) as i64);
    set_user_account(&mut fixture.svm, user, &account);

    use velocity::state::user_conditions::USER_CONDITIONS;
    let conditions =
        sync_liq_conditions(&mut fixture, user, market_conditions, ANY_SYNC_COST_UNITS);
    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let (header, block) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    assert_eq!(header.num_conditions as usize, USER_CONDITIONS);

    // The liveness poll: a clock, not a prediction. Velocity used to solve
    // each exposure's liquidation price here and watch that number, which
    // meant a second implementation of the margin engine beside the real one.
    // The resolver runs the real one, so the poll only has to keep asking.
    assert_eq!(
        block[LIQ_LIVENESS_POLL].wake(),
        Ok(velocity::relay_spec::WakeView::EverySlots {
            slots: LIQ_LIVENESS_POLL_SLOTS
        })
    );
    assert_eq!(
        block[LIQ_LIVENESS_POLL].crank_spec().resolver_disc,
        velocity::instruction::ResolveLiquidatePerpWithFill::DISCRIMINATOR
    );

    // Priced at what the market pays for a liquidation, because relay holds a
    // keeper's balance growth to the floor a condition advertises.
    assert_eq!(block[LIQ_LIVENESS_POLL].min_payment(), PAYMENT);

    // The self-maintenance pair: a watch over the user's own position bytes
    // whose executor is the sync, plus the coarse poll.
    assert_eq!(account_change(&block[LIQ_SYNC_WATCH]).0, user.to_bytes());
    assert_eq!(
        block[LIQ_SYNC_WATCH].crank_spec().resolver_disc,
        velocity::instruction::ResolveResyncLiqConditions::DISCRIMINATOR
    );

    // The sync's own fee is derived like every other crank's. Under the
    // fixture's flat-per-signature rails that is one signature's worth,
    // whatever cost units it asked for.
    assert_eq!(block[LIQ_SYNC_WATCH].min_payment(), PAYMENT);
    assert_eq!(
        block[LIQ_SYNC_FALLBACK].wake(),
        Ok(velocity::relay_spec::WakeView::EverySlots { slots: 3000 })
    );

    // Healthy: the resolver runs the real margin calc and reports no work.
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    assert!(run_liq_resolver(&mut fixture, user).is_none());
}

/// Underwater: the resolver confirms real liquidatability and stages
/// `liquidate_perp_with_fill` with the protocol User as liquidator — the
/// inventory-free flavor — plus the market's reservoir for the keeper fee.
#[test]
fn liq_resolver_stages_the_with_fill_executor_for_the_protocol_user() {
    let mut fixture = setup();
    let market_conditions = init_crank_conditions(&mut fixture, 10_000);
    let protocol_user = set_protocol_user(&mut fixture.svm);

    let authority = Keypair::new();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    // Deeply underwater: 10 units long entered at $100, now marked at $80.
    let mut account = trading_user(&authority.pubkey(), 50 * SPOT_BALANCE_PRECISION_U64, None);
    account.perp_positions[0].market_index = 0;
    account.perp_positions[0].base_asset_amount = (10 * UNIT) as i64;
    account.perp_positions[0].quote_asset_amount = -((1000 * 1_000_000) as i64);
    set_user_account(&mut fixture.svm, user, &account);
    sync_liq_conditions(&mut fixture, user, market_conditions, 0);

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (80 * PRICE_PRECISION) as i64,
        12,
    );

    let resolved =
        run_liq_resolver(&mut fixture, user).expect("an underwater account stages a liquidation");
    assert_eq!(
        resolved.executor_disc,
        velocity::instruction::LiquidatePerpWithFill::DISCRIMINATOR,
        "the inventory-free flavor"
    );

    let accounts: Vec<Pubkey> = resolved
        .accounts
        .iter()
        .map(|a| Pubkey::new_from_array(a.address))
        .collect();
    // Liquidator = the protocol User; the keeper slot is the placeholder;
    // the market's reservoir rides along for the payout.
    assert!(
        accounts.contains(&protocol_user),
        "protocol user liquidates"
    );
    assert!(accounts.contains(&user));
    assert!(accounts.contains(&market_conditions));
    assert!(accounts
        .iter()
        .any(|k| k.to_bytes() == velocity::relay_spec::KEEPER_PLACEHOLDER));
    // The quoter section: a liquidation fills through the market's book, and
    // the market names one, so the fill is refused without it.
    assert!(
        accounts.contains(&fixture.quoter_slab),
        "the staged fill carries the market's slab"
    );
    assert!(
        accounts.contains(&fixture.clob_market),
        "the staged fill carries the market's book"
    );

    // Args: the target perp market.
    assert_eq!(resolved.data, 0u16.to_le_bytes().to_vec());
}

/// A zeroed `State` has no liquidation margin buffer and liquidates zero
/// percent of a shortage, so `margin_shortage` refuses to price a
/// liquidation and nothing would transfer. This arms the throttle the way a
/// configured exchange does.
fn arm_liquidation_throttle(svm: &mut litesvm::LiteSVM) {
    let mut state: State = read_zero_copy(svm, &state_pda());
    state.liquidation_margin_buffer_ratio = 10;
    state.initial_pct_to_liquidate = velocity::math::constants::LIQUIDATION_PCT_PRECISION as u16;
    state.liquidation_duration = velocity::math::time::legacy_slot_duration_u8(150);
    set_zero_copy_account(svm, state_pda(), State::DISCRIMINATOR, &state, State::SIZE);
}

/// A liquidation reaches the book.
///
/// The forced order routes like any other taker order, so what closes the
/// position is whatever rests on the market's book. The resolver finds those
/// owners by asking the book — a book order's only record of its owner is the
/// authority and sub-account on the order itself — and stages their
/// `(User, UserStats)` pairs, because a quoter fills nobody the caller did
/// not load.
#[test]
fn a_liquidation_fills_through_the_book() {
    let mut fixture = setup();
    arm_liquidation_throttle(&mut fixture.svm);
    let market_conditions = init_crank_conditions(&mut fixture, 10_000);
    // The relay turner is paid out of the market's reservoir in
    // program-keeper mode, so it has to hold more than rent.
    fixture
        .svm
        .airdrop(&market_conditions, 1_000_000_000)
        .unwrap();
    set_protocol_user(&mut fixture.svm);

    // The book stands ready to buy at the price the position is marked at.
    place_clob_bid(&mut fixture, 80 * PRICE, 5 * UNIT);
    let maker_before: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);

    // Deeply underwater: 10 units long entered at $100, about to be marked
    // at $80.
    let authority = Keypair::new();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let mut account = trading_user(&authority.pubkey(), 50 * SPOT_BALANCE_PRECISION_U64, None);
    account.perp_positions[0].market_index = 0;
    account.perp_positions[0].base_asset_amount = (10 * UNIT) as i64;
    account.perp_positions[0].quote_asset_amount = -((1000 * 1_000_000) as i64);
    set_user_account(&mut fixture.svm, user, &account);
    let user_stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(&mut fixture.svm, user_stats, &authority.pubkey());
    sync_liq_conditions(&mut fixture, user, market_conditions, 0);

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (80 * PRICE_PRECISION) as i64,
        12,
    );

    let resolved =
        run_liq_resolver(&mut fixture, user).expect("an underwater account stages a liquidation");
    let accounts: Vec<Pubkey> = resolved
        .accounts
        .iter()
        .map(|a| Pubkey::new_from_array(a.address))
        .collect();
    assert!(
        accounts.contains(&fixture.clob_market),
        "the fill carries the book it routes through"
    );
    assert!(
        accounts.contains(&fixture.clob_maker_user),
        "the book's resting owner is loaded, or the book quotes it nothing"
    );
    assert!(
        accounts.contains(&maker_stats_address(&fixture)),
        "a loaded maker arrives as a (User, UserStats) pair"
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::LiquidatePerpWithFill::DISCRIMINATOR,
        payout,
    );

    let after: User = read_zero_copy(&fixture.svm, &user);
    assert!(
        after.perp_positions[0].base_asset_amount < (10 * UNIT) as i64,
        "the liquidation closed part of the position"
    );

    let maker_after: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(
        maker_after.perp_positions[0].base_asset_amount
            > maker_before.perp_positions[0].base_asset_amount,
        "the book's bid took the other side"
    );
}

/// The plain (position-acquiring) liquidation refuses the protocol User:
/// relay must never leave the protocol warehousing inventory — that path
/// stays with keeper bots that have a balance sheet.
#[test]
fn plain_liquidation_rejects_the_protocol_user() {
    let mut fixture = setup();
    init_crank_conditions(&mut fixture, 10_000);
    let protocol_user = set_protocol_user(&mut fixture.svm);
    let (signer, _) = velocity_signer_pda();
    let protocol_stats =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;

    let authority = Keypair::new();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let mut account = trading_user(&authority.pubkey(), 50 * SPOT_BALANCE_PRECISION_U64, None);
    account.perp_positions[0].base_asset_amount = (10 * UNIT) as i64;
    account.perp_positions[0].quote_asset_amount = -((1000 * 1_000_000) as i64);
    set_user_account(&mut fixture.svm, user, &account);
    let user_stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(&mut fixture.svm, user_stats, &authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (80 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::LiquidatePerp {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        liquidator: protocol_user,
        liquidator_stats: protocol_stats,
        user,
        user_stats,
        crank_conditions: None,
        instructions_sysvar: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::LiquidatePerp {
            market_index: 0,
            liquidator_max_base_asset_amount: UNIT,
            limit_price: None,
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    assert!(
        send(&mut fixture.svm, &keeper, ix, &[]).is_err(),
        "the protocol user must not take liquidated inventory"
    );
}

/// The self-maintenance loop, turner-shaped — and the regression guard for
/// the rule that broke it once: **a staged executor may not name a
/// signer.** Relay marks every executor meta non-signing and refuses to
/// sign a transaction whose executor names a signer, so the relay-facing
/// resync takes no payer (the opt-in sync, which allocates, keeps its
/// own). Here the user's positions change, the resolver notices the
/// thresholds are stale, and the staged executor lands unsigned, paying
/// the keeper from the conditions account's own lamports.
#[test]
fn liq_self_sync_stages_an_unsigned_executor_and_pays_from_the_treasury() {
    let mut fixture = setup();
    let market_conditions = init_crank_conditions(&mut fixture, 10_000);
    set_protocol_user(&mut fixture.svm);

    let authority = Keypair::new();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let mut account = trading_user(&authority.pubkey(), 100 * SPOT_BALANCE_PRECISION_U64, None);
    account.perp_positions[0].market_index = 0;
    account.perp_positions[0].base_asset_amount = (10 * UNIT) as i64;
    account.perp_positions[0].quote_asset_amount = -((1000 * 1_000_000) as i64);
    set_user_account(&mut fixture.svm, user, &account);

    // The fixture's rails charge one flat fee per signature, so the sync's own
    // payment is that fee whatever it requests.
    const SYNC_FEE: u64 = 5_000;
    set_flat_transaction_fee(&mut fixture, SYNC_FEE as u32);
    let conditions =
        sync_liq_conditions(&mut fixture, user, market_conditions, ANY_SYNC_COST_UNITS);
    // Fund the sync reservoir (whoever wants this user's hints
    // self-maintaining pays for it).
    fixture.svm.airdrop(&conditions, 100_000_000).unwrap();

    // Nothing changed: the resolver reports no work.
    let resolver_ix = || Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::ResolveResyncLiqConditions {
            scratch: relay_scratch_pda(),
            liq_conditions: conditions,
            user,
        }
        .to_account_metas(None),
        data: velocity::instruction::ResolveResyncLiqConditions {}.data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let ix = resolver_ix();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    assert!(
        !velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data)
            .unwrap()
            .has_work()
    );

    // The user closes their position: the thresholds are now for exposures
    // that no longer exist.
    let mut account: User = read_zero_copy(&fixture.svm, &user);
    account.perp_positions[0].base_asset_amount = 0;
    account.perp_positions[1].market_index = 0;
    set_user_account(&mut fixture.svm, user, &account);

    let ix = resolver_ix();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let pointer = velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
    assert!(pointer.has_work(), "closed position makes the hints stale");
    let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
    let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
    let resolved = velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap();

    // Land it exactly as a turner does: every meta non-signing, keeper
    // placeholder substituted. A `Signer` anywhere in the executor's
    // accounts struct fails here.
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let payout_before = fixture.svm.get_balance(&payout).unwrap();
    let conditions_before = fixture.svm.get_balance(&conditions).unwrap();
    let treasury_before = fixture.svm.get_balance(&crank_treasury_pda()).unwrap();

    // The treasury pays for this account at most once per fallback interval.
    // Opting in stamped the slot, so a resync inside that interval does the
    // work and pays nothing: opting in is permissionless and the payer is
    // protocol funds, so an unbounded rate is a drain by repetition.
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::ResyncLiqConditions::DISCRIMINATOR,
        payout,
    );

    assert_eq!(
        fixture.svm.get_balance(&payout).unwrap(),
        payout_before,
        "a resync inside the interval is not paid for"
    );

    fixture
        .svm
        .warp_to_slot(fixture.svm.get_sysvar::<anchor_lang::prelude::Clock>().slot + 3_000);
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::ResyncLiqConditions::DISCRIMINATOR,
        payout,
    );

    // Keeper paid from the protocol treasury, and the stale threshold is
    // gone (no live exposures left to watch).
    assert_eq!(
        fixture.svm.get_balance(&payout).unwrap(),
        payout_before + SYNC_FEE
    );
    assert_eq!(
        fixture.svm.get_balance(&crank_treasury_pda()).unwrap(),
        treasury_before - SYNC_FEE
    );

    // The user's own account pays nothing: a resync nobody is paid to run
    // leaves the thresholds stale, which is the protocol's exposure before it
    // is the user's.
    assert_eq!(
        fixture.svm.get_balance(&conditions).unwrap(),
        conditions_before
    );
}

/// The maker route's remainder lives on the book.
///
/// `place_and_make` is IOC post-only, and v0 has no choice but to cancel
/// whatever the named taker order did not consume — the maker quoted a price,
/// filled part of it, and loses the rest. v1 rests that remainder on the CLOB,
/// which is where a restable maker order belongs. IOC still holds in the sense
/// that matters: the order does not occupy a `User.orders` slot afterwards.
#[test]
fn place_and_make_v1_rests_a_maker_order_on_the_book() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();

    // A maker quoting a full-unit ask. It provides liquidity that rests on the
    // book; it names no taker and matches nothing on placement. JIT is gone.
    let maker_authority = Keypair::new();
    fixture
        .svm
        .airdrop(&maker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let maker_user = Pubkey::new_unique();
    let maker_stats = Pubkey::find_program_address(
        &[b"user_stats", maker_authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    let mut maker_state = trading_user(
        &maker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        None,
    );

    maker_state.next_order_id = 1;
    set_user_account(&mut fixture.svm, maker_user, &maker_state);
    set_user_stats_account(&mut fixture.svm, maker_stats, &maker_authority.pubkey());

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::PlaceAndMakeV1 {
        state: state_pda(),
        user: maker_user,
        user_stats: maker_stats,
        authority: maker_authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndMakePerpOrderV1 {
            args: PlaceAndMakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Limit,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Short,
                    base_asset_amount: UNIT,
                    // Above the oracle/vAMM, so the post-only ask rests instead of
                    // crossing.
                    price: 105 * PRICE,
                    market_index: 0,
                    post_only: PostOnlyParam::MustPostOnly,
                    ..OrderParams::default()
                },
            },
        }
        .data(),
    };

    send_with_ixs(
        &mut fixture.svm,
        &maker_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    // The whole order rests on the book. Nothing filled, and nothing occupies a
    // `User.orders` slot: the maker went straight to the CLOB.
    let maker: User = read_zero_copy(&fixture.svm, &maker_user);
    assert_eq!(
        maker.perp_positions[0].base_asset_amount, 0,
        "the maker filled nothing on placement"
    );

    assert!(
        maker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "nothing rests in User.orders — the maker is on the book"
    );
    assert_eq!(
        maker.perp_positions[0].open_asks,
        -(UNIT as i64),
        "the full order is reserved against the book"
    );
    assert_eq!(maker.open_orders, 1);
    assert_eq!(clob_ask_count(&fixture), 1);
}

// ---------------------------------------------------------------------------
// Taker-origin crosses: a migrated taker remainder, and the crank that hands
// it the improvement its activation window earned.
// ---------------------------------------------------------------------------

/// Pause the vAMM for fills. A place-and-take that finds no liquidity is how
/// this fixture manufactures a *whole* unfilled remainder to migrate, and an
/// unpaused curve would fill an aggressive bid on the spot instead.
fn pause_amm_fill(svm: &mut litesvm::LiteSVM) {
    use velocity::state::paused_operations::PerpOperation;
    let mut market: PerpMarket = read_zero_copy(svm, &perp_market_pda(0));
    market.paused_operations |= PerpOperation::AmmFill as u8;
    set_zero_copy_account(
        svm,
        perp_market_pda(0),
        PerpMarket::DISCRIMINATOR,
        &market,
        PerpMarket::SIZE,
    );
}

/// A funded `(User, UserStats)` pair at the real PDAs — where a book node's
/// `(authority, sub_account_id)` identity resolves to.
struct Party {
    authority: Keypair,
    user: Pubkey,
    stats: Pubkey,
}

fn party(svm: &mut litesvm::LiteSVM, deposit: u64) -> Party {
    let authority = Keypair::new();
    svm.airdrop(&authority.pubkey(), 10_000_000_000).unwrap();
    let user = Pubkey::find_program_address(
        &[
            b"user",
            authority.pubkey().as_ref(),
            0u16.to_le_bytes().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    let stats = Pubkey::find_program_address(
        &[b"user_stats", authority.pubkey().as_ref()],
        &velocity_id(),
    )
    .0;
    let mut state = trading_user(&authority.pubkey(), deposit, None);
    state.next_order_id = 1;
    set_user_account(svm, user, &state);
    set_user_stats_account(svm, stats, &authority.pubkey());
    Party {
        authority,
        user,
        stats,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }

    s
}

/// Create a `SignedMsgUserOrders` account for `authority` with `num_orders`
/// empty slots. The zero-copy loader reads an 8-byte discriminator, then the
/// fixed header (user_pubkey 32, padding 4, len 4), then `len` order slots of
/// 40 bytes each.
fn set_signed_msg_user_orders(svm: &mut litesvm::LiteSVM, authority: &Pubkey, num_orders: u32) {
    use velocity::state::signed_msg_user::SignedMsgUserOrders;
    let mut data = SignedMsgUserOrders::DISCRIMINATOR.to_vec();
    data.extend_from_slice(authority.as_ref());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&num_orders.to_le_bytes());
    data.resize(data.len() + num_orders as usize * 40, 0);
    svm.set_account(
        signed_msg_user_orders_pda(authority),
        Account {
            lamports: 1_000_000_000,
            data,
            owner: velocity_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// The taker's ed25519 signature over a swift order is verified in-program
/// (brine-ed25519), with no ed25519 precompile instruction. A real signature
/// clears the verifier; flipping one byte makes the same order fail with
/// SigVerificationFailed. Only the signature differs between the two sends, so
/// the in-program verifier is what the difference isolates.
#[test]
fn signed_msg_taker_signature_is_verified_in_program() {
    use {
        anchor_lang::AnchorSerialize,
        velocity::state::order_params::{OrderParams, PostOnlyParam, SignedMsgOrderParamsMessage},
    };

    let mut fixture = setup();
    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let taker = party(&mut fixture.svm, 100 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);
    set_signed_msg_user_orders(&mut fixture.svm, &taker.authority.pubkey(), 8);

    let order = OrderParams {
        order_type: OrderType::Market,
        market_type: MarketType::Perp,
        direction: PositionDirection::Long,
        base_asset_amount: UNIT,
        price: 101 * PRICE,
        market_index: 0,
        post_only: PostOnlyParam::None,
        ..OrderParams::default()
    };
    let message = SignedMsgOrderParamsMessage {
        signed_msg_order_params: order,
        sub_account_id: 0,
        slot: 30,
        uuid: *b"itbrine1",
        take_profit_order_params: None,
        stop_loss_order_params: None,
        max_margin_ratio: None,
        builder_idx: None,
        builder_fee_tenth_bps: None,
        isolated_position_deposit: None,
        // The anchor-test build carries no mainnet feature, so it names
        // the devnet cluster and refuses a message that names none.
        network: Some(velocity::state::order_params::expected_signed_msg_network()),
        route: None,
    };

    // manual 8-byte discriminator (unread) + borsh body, hex-encoded: the taker
    // signs the hex text.
    let mut borsh_body = vec![0u8; 8];
    message.serialize(&mut borsh_body).unwrap();
    let hex_msg = hex_lower(&borsh_body);
    let signature = taker.authority.sign_message(hex_msg.as_bytes());

    let taker_pubkey = taker.authority.pubkey().to_bytes();
    let envelope = |sig: &[u8]| {
        let mut e = Vec::new();
        e.extend_from_slice(sig);
        e.extend_from_slice(&taker_pubkey);
        e.extend_from_slice(&(hex_msg.len() as u16).to_le_bytes());
        e.extend_from_slice(hex_msg.as_bytes());
        e
    };

    let place_ix = |bytes: Vec<u8>| {
        let mut accounts = velocity::accounts::PlaceSignedMsgTakerOrder {
            state: state_pda(),
            user: taker.user,
            user_stats: taker.stats,
            signed_msg_user_orders: signed_msg_user_orders_pda(&taker.authority.pubkey()),
            authority: keeper.authority.pubkey(),
            ix_sysvar: instructions_sysvar(),
            filler: keeper.user,
            filler_stats: keeper.stats,
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::PlaceSignedMsgTakerOrder {
                signed_msg_order_params_message_bytes: bytes,
                is_delegate_signer: false,
                flow_attestation: None,
            }
            .data(),
        }
    };

    // A real signature clears the verifier: whatever the fill does next, the
    // failure is never SigVerificationFailed.
    let good = send_with_ixs(
        &mut fixture.svm,
        &keeper.authority,
        &[
            compute_unit_limit_ix(600_000),
            place_ix(envelope(signature.as_ref())),
        ],
        &[],
    );
    let good_logs = match &good {
        Ok(meta) => meta.logs.clone(),
        Err(e) => e.meta.logs.clone(),
    };

    assert!(
        !good_logs
            .iter()
            .any(|l| l.contains("SigVerificationFailed")),
        "a real signature must clear the in-program verifier: {good_logs:?}"
    );

    // Flip one signature byte: the same order now fails at the verifier.
    let mut bad_sig = signature.as_ref().to_vec();
    bad_sig[0] ^= 1;
    let bad = send_with_ixs(
        &mut fixture.svm,
        &keeper.authority,
        &[compute_unit_limit_ix(600_000), place_ix(envelope(&bad_sig))],
        &[],
    )
    .unwrap_err();
    assert!(
        bad.meta
            .logs
            .iter()
            .any(|l| l.contains("SigVerificationFailed")),
        "a tampered signature must be refused by the in-program verifier: {:?}",
        bad.meta.logs
    );
}

/// On a bumped book a signed-message fill takes synchronously only with
/// swift's detached attestation: the flow authority's signature over the
/// order's own signature plus an expiry, verified in-program. Without it
/// the order rests whole through the window; expired, it is refused.
#[test]
fn a_swift_fill_takes_a_bumped_book_only_with_the_attestation() {
    use {
        anchor_lang::AnchorSerialize,
        velocity::state::{
            order_params::{OrderParams, PostOnlyParam, SignedMsgOrderParamsMessage},
            state::HotRole,
        },
    };

    let mut fixture = setup();
    let maker_stats = maker_stats_address(&fixture);
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    place_clob_ask(&mut fixture, 99 * PRICE, 2 * UNIT);
    set_clob_default_activation_delay(&mut fixture, 5);

    let flow = Keypair::new();
    let mut state: State = read_zero_copy(&fixture.svm, &state_pda());
    state.set_hot_key(HotRole::FlowAuthority, flow.pubkey());
    set_zero_copy_account(
        &mut fixture.svm,
        state_pda(),
        State::DISCRIMINATOR,
        &state,
        State::SIZE,
    );

    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);
    set_signed_msg_user_orders(&mut fixture.svm, &taker.authority.pubkey(), 8);

    let signed = |uuid: [u8; 8]| {
        let order = OrderParams {
            order_type: OrderType::Market,
            market_type: MarketType::Perp,
            direction: PositionDirection::Long,
            base_asset_amount: UNIT,
            price: 101 * PRICE,
            market_index: 0,
            post_only: PostOnlyParam::None,
            ..OrderParams::default()
        };
        let message = SignedMsgOrderParamsMessage {
            signed_msg_order_params: order,
            sub_account_id: 0,
            slot: 30,
            uuid,
            take_profit_order_params: None,
            stop_loss_order_params: None,
            max_margin_ratio: None,
            builder_idx: None,
            builder_fee_tenth_bps: None,
            isolated_position_deposit: None,
            // The anchor-test build carries no mainnet feature, so it names
            // the devnet cluster and refuses a message that names none.
            network: Some(velocity::state::order_params::expected_signed_msg_network()),
            route: None,
        };
        let mut borsh_body = vec![0u8; 8];
        message.serialize(&mut borsh_body).unwrap();
        let hex_msg = hex_lower(&borsh_body);
        let signature = taker.authority.sign_message(hex_msg.as_bytes());
        let mut envelope = Vec::new();
        envelope.extend_from_slice(signature.as_ref());
        envelope.extend_from_slice(&taker.authority.pubkey().to_bytes());
        envelope.extend_from_slice(&(hex_msg.len() as u16).to_le_bytes());
        envelope.extend_from_slice(hex_msg.as_bytes());
        (envelope, <[u8; 64]>::try_from(signature.as_ref()).unwrap())
    };

    // Swift's detached attestation: domain, the order signature, the expiry.
    let attest = |order_sig: &[u8; 64], expiry_ts: i64| {
        let mut message = Vec::new();
        message.extend_from_slice(velocity::FLOW_ATTESTATION_DOMAIN);
        message.extend_from_slice(order_sig);
        message.extend_from_slice(&expiry_ts.to_le_bytes());
        velocity::FlowAttestationV0 {
            signature: <[u8; 64]>::try_from(flow.sign_message(&message).as_ref()).unwrap(),
            expiry_ts,
        }
    };

    let place_ix = |bytes: Vec<u8>, attestation: Option<velocity::FlowAttestationV0>| {
        let mut accounts = velocity::accounts::PlaceSignedMsgTakerOrder {
            state: state_pda(),
            user: taker.user,
            user_stats: taker.stats,
            signed_msg_user_orders: signed_msg_user_orders_pda(&taker.authority.pubkey()),
            authority: keeper.authority.pubkey(),
            ix_sysvar: instructions_sysvar(),
            filler: keeper.user,
            filler_stats: keeper.stats,
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
        accounts.push(AccountMeta::new(maker_stats, false));
        accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::PlaceSignedMsgTakerOrder {
                signed_msg_order_params_message_bytes: bytes,
                is_delegate_signer: false,
                flow_attestation: attestation,
            }
            .data(),
        }
    };

    // Unattested: no fill — the whole order rests taker-origin.
    let (envelope, _) = signed(*b"noattest");
    send_with_ixs(
        &mut fixture.svm,
        &keeper.authority,
        &[compute_unit_limit_ix(600_000), place_ix(envelope, None)],
        &[],
    )
    .unwrap();
    let taker_state: User = read_zero_copy(&fixture.svm, &taker.user);
    assert_eq!(
        taker_state.perp_positions[0].base_asset_amount, 0,
        "unattested swift flow does not fill synchronously"
    );
    assert_eq!(
        taker_state.perp_positions[0].open_bids, UNIT as i64,
        "the whole order rests"
    );

    // An expired attestation is refused outright, not downgraded. The
    // expiry is behind any clock, litesvm's near-zero one included.
    let (envelope, order_sig) = signed(*b"expired1");
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper.authority,
        &[
            compute_unit_limit_ix(600_000),
            place_ix(envelope, Some(attest(&order_sig, -1))),
        ],
        &[],
    )
    .unwrap_err();
    assert!(
        format!("{:?}", err.err).contains("6286"),
        "expected SigVerificationFailed for an expired attestation, got {:?}",
        err.err
    );

    // A live attestation: the fill takes the book synchronously.
    let (envelope, order_sig) = signed(*b"attested");
    send_with_ixs(
        &mut fixture.svm,
        &keeper.authority,
        &[
            compute_unit_limit_ix(600_000),
            place_ix(envelope, Some(attest(&order_sig, i64::MAX))),
        ],
        &[],
    )
    .unwrap();
    let taker_state: User = read_zero_copy(&fixture.svm, &taker.user);
    assert_eq!(
        taker_state.perp_positions[0].base_asset_amount, UNIT as i64,
        "attested swift flow fills off the book"
    );
}

/// Rest one `party`'s CLOB order through the velocity adapter (margin gated,
/// aggregates reserved), immediately matchable.
fn place_clob_order_for(
    fixture: &mut Fixture,
    party: &Party,
    direction: PositionDirection,
    price: u64,
    size: u64,
) -> ClobOrderRefV0 {
    let ix = place_clob_order_ix(
        party.user,
        &party.authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction,
            price,
            base_asset_amount: size,
            max_ts: 0,
            activation_delay_slots: Some(0),
            reject_if_crossed: false,
        },
    );
    let authority = party.authority.insecure_clone();
    let meta = send(&mut fixture.svm, &authority, ix, &[]).unwrap();
    let data = &meta.return_data.data;
    ClobOrderRefV0 {
        node_index: u32::from_le_bytes(data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(data[4..12].try_into().unwrap()),
    }
}

/// Migrate a whole unfilled limit onto the book as a taker-origin remainder:
/// `place_and_take_perp_order_v1` with nothing to fill against (empty book,
/// vAMM paused) is exactly the R1 path a keeper fill takes, and the only way
/// to get the flag set.
fn rest_taker_origin_order(
    fixture: &mut Fixture,
    party: &Party,
    direction: PositionDirection,
    price: u64,
    size: u64,
) -> ClobOrderRefV0 {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let ix = take_ix(
        fixture,
        party,
        OrderParams {
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction,
            base_asset_amount: size,
            price,
            market_index: 0,
            post_only: PostOnlyParam::None,
            ..OrderParams::default()
        },
    );
    let authority = party.authority.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    // The book writes the rested remainder's handle as return data.
    let data = &meta.return_data.data;
    ClobOrderRefV0 {
        node_index: u32::from_le_bytes(data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(data[4..12].try_into().unwrap()),
    }
}

/// `place_and_take_perp_order_v1` for `party` with no makers, so any remainder
/// is the whole unfilled order.
fn take_ix(
    fixture: &Fixture,
    party: &Party,
    params: velocity::state::order_params::OrderParams,
) -> Instruction {
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: party.user,
        user_stats: party.stats,
        authority: party.authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params,
                success_condition: None,
            },
        }
        .data(),
    }
}

/// A resting limit bid of one unit at 99, as a taker names it.
fn delayed_bid(activation_delay_slots: Option<u32>) -> velocity::state::order_params::OrderParams {
    velocity::state::order_params::OrderParams {
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: PositionDirection::Long,
        base_asset_amount: UNIT,
        price: 99 * PRICE,
        market_index: 0,
        post_only: velocity::state::order_params::PostOnlyParam::None,
        activation_delay_slots,
        ..Default::default()
    }
}

/// A taker's `activation_delay_slots` reaches the book. The remainder stays
/// out of the book's matchable set until the delay passes, though the book's
/// own default is zero.
#[test]
fn a_take_rests_its_remainder_behind_the_delay_it_names() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);

    let ix = take_ix(&fixture, &taker, delayed_bid(Some(6)));
    let authority = taker.authority.insecure_clone();
    send_with_ixs(
        &mut fixture.svm,
        &authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    assert_eq!(clob_bid_count(&fixture), 0, "inside its activation window");

    let slot = fixture.svm.get_sysvar::<solana_clock::Clock>().slot;
    fixture.svm.warp_to_slot(slot + 7);
    assert_eq!(clob_bid_count(&fixture), 1, "active once the delay passes");
}

/// A delay below the book's default is a way past the speed bump, so a take
/// that names one needs the flow authority's attestation, as a maker does.
#[test]
fn a_take_below_the_books_default_delay_needs_the_flow_authority() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    set_clob_default_activation_delay(&mut fixture, 4);
    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let authority = taker.authority.insecure_clone();

    let ix = take_ix(&fixture, &taker, delayed_bid(Some(0)));
    let err = send_with_ixs(
        &mut fixture.svm,
        &authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap_err();
    assert_velocity_error(&err, ErrorCode::UnattestedFastActivation);

    let ix = take_ix(&fixture, &taker, delayed_bid(Some(4)));
    send_with_ixs(
        &mut fixture.svm,
        &authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect("the default itself needs no attestation");
}

/// A trigger order waits in a slot, and a slot stores no activation delay. A
/// trigger that names one is refused rather than resting behind the default.
#[test]
fn a_trigger_order_that_names_an_activation_delay_is_refused() {
    use velocity::{
        instructions::PlaceTriggerOrdersV1Args,
        state::{
            order_params::{OrderParams, PostOnlyParam},
            user::OrderTriggerCondition,
        },
    };

    let mut fixture = setup();
    let owner = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let arm = |fixture: &mut Fixture, activation_delay_slots: Option<u32>| {
        let mut accounts = velocity::accounts::PlaceTriggerOrdersV1 {
            state: state_pda(),
            user: owner.user,
            authority: owner.authority.pubkey(),
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        let params = OrderParams {
            order_type: OrderType::TriggerMarket,
            market_type: MarketType::Perp,
            direction: PositionDirection::Long,
            base_asset_amount: UNIT,
            market_index: 0,
            post_only: PostOnlyParam::None,
            trigger_price: Some(110 * PRICE),
            trigger_condition: OrderTriggerCondition::Above,
            activation_delay_slots,
            ..OrderParams::default()
        };
        let ix = Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::PlaceTriggerOrdersV1 {
                args: PlaceTriggerOrdersV1Args {
                    params: vec![params],
                },
            }
            .data(),
        };
        let authority = owner.authority.insecure_clone();
        send(&mut fixture.svm, &authority, ix, &[])
    };

    let err = arm(&mut fixture, Some(3)).unwrap_err();
    assert_velocity_error(&err, ErrorCode::InvalidOrder);
    arm(&mut fixture, None).expect("the same trigger without a delay arms");
}

/// The crank, signed-keeper mode: `filler` is the caller's own `User` and the
/// crank reward lands there as quote.
fn crank_taker_origin_cross_ix(
    fixture: &Fixture,
    keeper: &Party,
    taker: &Party,
    counterparties: &[&Party],
) -> Instruction {
    let mut accounts = velocity::accounts::CrankTakerOriginCross {
        state: state_pda(),
        authority: keeper.authority.pubkey(),
        filler: keeper.user,
        filler_stats: keeper.stats,
        taker: taker.user,
        taker_stats: taker.stats,
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        crank_conditions: None,
        // Required and seed-pinned. This taker never sent a signed message, so
        // the account does not exist — which is what "no route" looks like.
        signed_msg_user_orders: signed_msg_user_orders_pda(&taker.authority.pubkey()),
        instructions_sysvar: instructions_sysvar(),
    }
    .to_account_metas(None);
    // Maps, then every counterparty's (User, UserStats) pair. All of them: the
    // crank is a fill, so the filler obligation holds it to the makers it had
    // room to carry.
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    for counterparty in counterparties {
        accounts.push(AccountMeta::new(counterparty.user, false));
        accounts.push(AccountMeta::new(counterparty.stats, false));
    }

    // The quoter tail: the crank routes the remainder, and every router fill
    // carries the market's slab and consults its book slot as the mandatory
    // baseline.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::CrankTakerOriginCross {
            args: CrankTakerOriginCrossArgs {
                market_index: 0,
                cross_rows: 32,
                signed_route: vec![],
            },
        }
        .data(),
    }
}

fn perp_position(svm: &litesvm::LiteSVM, user: &Pubkey) -> velocity::state::user::PerpPosition {
    let user: User = read_zero_copy(svm, user);
    user.perp_positions[0]
}

/// The mechanism, end to end. A taker's unfilled limit bid at 101 migrates to
/// the book flagged taker-origin; two makers then line up asks at 100 and 99
/// inside its window. The crank routes the remainder like any other fill, so
/// it sweeps both asks best-first and the taker buys its whole unit at a 99.5
/// average — not at the 101 it rested at. The improvement goes to the taker
/// rather than to whoever could have taken the remainder at its own price,
/// and each maker fills at the price it quoted.
#[test]
fn taker_origin_cross_settles_at_the_best_counterpartys_price() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let best = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let worse = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    // The remainder: a whole unfilled unit resting at its limit of 101.
    let _subject = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT,
    );

    assert_eq!(clob_bid_count(&fixture), 1);
    assert_eq!(
        perp_position(&fixture.svm, &taker.user).open_bids,
        UNIT as i64,
        "the remainder's worst case is reserved on the book, not on the DLOB"
    );

    // Makers line up inside the window: 0.5 at 100, then 0.5 at 99.
    place_clob_order_for(
        &mut fixture,
        &worse,
        PositionDirection::Short,
        100 * PRICE,
        UNIT / 2,
    );
    place_clob_order_for(
        &mut fixture,
        &best,
        PositionDirection::Short,
        99 * PRICE,
        UNIT / 2,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &[&best, &worse]);
    let keeper_authority = keeper.authority.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    let logs = meta.logs.join(" ");
    // An ordinary router fill: both sides of the book read, the route quoted,
    // the counterparty consumed, and the remainder shrunk in place. It prices
    // like the three-user fill it is. A ceiling rather than an equality, but
    // one that leaves a relay executor room for its own accounting.
    assert!(
        meta.compute_units_consumed < 220_000,
        "crank cost {} CU",
        meta.compute_units_consumed
    );
    assert!(
        logs.contains(
            "taker-origin remainder routed: 1000000000 base at 99500000 instead of 101000000"
        ),
        "settled at the counterparties' prices, not the resting one: {logs}"
    );

    // The taker is long its whole unit, bought at a 99.5 average rather than
    // its own 101. Its quote is that notional plus the taker fee plus the
    // cranker's cut — all of which fits well inside the $1.50 the 101 rest
    // price would have cost it.
    let taker_position = perp_position(&fixture.svm, &taker.user);
    assert_eq!(taker_position.base_asset_amount, UNIT as i64);
    assert_eq!(taker_position.open_bids, 0, "nothing left resting");
    assert_eq!(taker_position.open_orders, 0);
    let paid = -taker_position.quote_asset_amount;
    assert!(
        (99_500_000..99_800_000).contains(&paid),
        "paid {paid}: 99.5 plus fees, where 101 would have been 101_000_000"
    );

    // The cranker is paid out of the improvement, in quote, on its own `User`.
    let reward = perp_position(&fixture.svm, &keeper.user).quote_asset_amount;
    let improvement = 1_500_000; // (101 - 99.5) * 1 unit
    assert!(
        reward > 0 && reward < improvement,
        "reward {reward} must be paid and must fit inside the {improvement} improvement"
    );

    // The invariant, measured: the taker's all-in cost beats what being taken
    // at its own resting price would have been, fee included.
    let cost_if_taken = 101_000_000 + 101_000; // 101 * 1 unit plus 10bps
    assert!(
        paid < cost_if_taken,
        "crossing cost {paid}, resting would have cost {cost_if_taken}"
    );

    // Both makers filled at their own prices. The one that quoted 100 is not
    // punished for being second — it is second in the average the taker pays,
    // and it still gets the 100 it asked for.
    let best_position = perp_position(&fixture.svm, &best.user);
    assert_eq!(best_position.base_asset_amount, -((UNIT / 2) as i64));
    assert_eq!(best_position.open_asks, 0, "reservation released");
    assert_eq!(best_position.open_orders, 0);
    assert!(
        best_position.quote_asset_amount >= 49_500_000,
        "the maker got the 99 it asked for, plus its rebate: {}",
        best_position.quote_asset_amount
    );

    let worse_position = perp_position(&fixture.svm, &worse.user);
    assert_eq!(worse_position.base_asset_amount, -((UNIT / 2) as i64));
    assert_eq!(worse_position.open_asks, 0, "reservation released");
    assert_eq!(worse_position.open_orders, 0);
    assert!(
        worse_position.quote_asset_amount >= 50_000_000,
        "the maker got the 100 it asked for, plus its rebate: {}",
        worse_position.quote_asset_amount
    );

    // The whole cross cleared in one crank, so the book is empty on both
    // sides. The filler obligation is what makes this the ordinary case: a
    // transaction with room for both makers has to carry both.
    assert_eq!(clob_bid_count(&fixture), 0);
    assert_eq!(clob_ask_count(&fixture), 0);

    // Nothing crossed anymore: the crank declines rather than doing something
    // arbitrary.
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &[&worse]);
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("no cross left");
    assert!(
        err.meta.logs.join(" ").contains("NoTakerOriginCross"),
        "unexpected: {:?}",
        err.meta.logs
    );
}

/// A partly filled remainder keeps its identity and its place in the queue.
///
/// This is what `fill_v0` exists for. Cancelling the order and placing a new
/// one for the leftover would settle the same base at the same price, and the
/// taker would still pay for it: the replacement takes a fresh order id and
/// goes to the back of its price level, behind every order that arrived while
/// the first one was resting. A taker that waited for its turn would lose it
/// by being filled.
#[test]
fn a_partly_filled_remainder_keeps_its_id_and_its_queue_position() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let behind = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let maker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    // The remainder rests first, so it holds the front of the 101 level.
    let subject = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT,
    );

    // A maker joins the same level behind it, and is what a re-placement would
    // end up behind.
    let behind_ref = place_clob_order_for(
        &mut fixture,
        &behind,
        PositionDirection::Long,
        101 * PRICE,
        UNIT / 2,
    );

    // Half a unit of ask inside the window: enough to fill half the remainder.
    place_clob_order_for(
        &mut fixture,
        &maker,
        PositionDirection::Short,
        99 * PRICE,
        UNIT / 2,
    );

    let before = clob_side(&fixture, Direction::Short);
    assert_eq!(
        before.iter().map(|row| row.order_id).collect::<Vec<_>>(),
        vec![subject.order_id, behind_ref.order_id],
        "the remainder is at the front of its price level"
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &[&maker]);
    let keeper_authority = keeper.authority.insecure_clone();
    send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    // Half the remainder filled. What is left is the same order — same id,
    // same node — still ahead of the maker that joined after it.
    let after = clob_side(&fixture, Direction::Short);
    assert_eq!(
        after.iter().map(|row| row.order_id).collect::<Vec<_>>(),
        vec![subject.order_id, behind_ref.order_id],
        "the remainder shrank in place instead of going to the back"
    );
    assert_eq!(
        after[0].node_index, subject.node_index,
        "the same node, so nothing was cancelled and re-placed"
    );
    assert_eq!(after[0].size, UNIT / 2, "half of it filled");
    assert_eq!(after[1].size, UNIT / 2, "the maker behind it is untouched");

    // The reservation follows the order down rather than being rebuilt.
    let taker_position = perp_position(&fixture.svm, &taker.user);
    assert_eq!(taker_position.base_asset_amount, (UNIT / 2) as i64);
    assert_eq!(
        taker_position.open_bids,
        (UNIT / 2) as i64,
        "the leftover keeps exactly its own worst case reserved"
    );
    assert_eq!(
        taker_position.open_orders, 1,
        "still one order, not a new one"
    );
}

/// The window binds the taker who opened it.
///
/// A remainder its owner can pull the moment a maker lines up offers nothing to
/// line up against, so the window needs the order to still be there when it
/// ends. Liquidation is the one thing that must never wait: force-cancel passes
/// `force` and reaches a bound order, because an account in distress cannot be
/// held hostage by its own resting orders.
#[test]
fn a_remainder_cannot_be_pulled_inside_its_window_but_force_cancel_reaches_it() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    // The fixture's book activates immediately; give it a window to bind over.
    set_clob_default_activation_delay(&mut fixture, 4);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let subject = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT,
    );

    assert_eq!(
        perp_position(&fixture.svm, &taker.user).open_bids,
        UNIT as i64,
        "the remainder reserved its worst case on the book"
    );

    let cancel_ix = |order_ref: ClobOrderRefV0| Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrderV1 {
            perp_market: perp_market_pda(0),
            user: taker.user,
            authority: taker.authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrderV1 {
            params: CancelOrderV1Params {
                market_index: 0,
                order_ref: order_ref,
            },
        }
        .data(),
    };

    // Inside the window: refused, and nothing moves.
    let authority = taker.authority.insecure_clone();
    let err = send(&mut fixture.svm, &authority, cancel_ix(subject), &[])
        .expect_err("a bound remainder cannot be pulled");
    // `ClobError::TakerOriginBound`. The book raises it, and a CPI callee's
    // error reaches the caller as its code rather than its name.
    assert!(
        format!("{:?}", err.meta.logs).contains("0x1789"),
        "unexpected: {:?}",
        err.meta.logs
    );
    assert_eq!(
        perp_position(&fixture.svm, &taker.user).open_bids,
        UNIT as i64,
        "a refused cancel releases nothing, so the order is still resting"
    );
    assert_eq!(
        clob_bid_count(&fixture),
        0,
        "and nothing can reach it either: inside its window it is not matchable depth"
    );

    // Liquidation is exempt: force-cancel carries `force` and reaches it. An
    // account in distress cannot be held hostage by its own resting orders.
    let mut forced = setup();
    pause_amm_fill(&mut forced.svm);
    set_clob_default_activation_delay(&mut forced, 4);
    let failing = party(&mut forced.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let bound = rest_taker_origin_order(
        &mut forced,
        &failing,
        PositionDirection::Long,
        101 * PRICE,
        UNIT / 2,
    );

    // Gut the collateral while keeping the resting aggregates: the
    // deterioration force-cancel exists for.
    let mut broke = trading_user(&failing.authority.pubkey(), 1_000, None);
    broke.perp_positions[0].open_bids = (UNIT / 2) as i64;
    broke.perp_positions[0].open_orders = 1;
    broke.open_orders = 1;
    broke.has_open_order = true;
    broke.next_order_id = 2;
    set_user_account(&mut forced.svm, failing.user, &broke);

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut forced.svm,
        filler_user,
        &trading_user(&forced.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut forced.svm, filler_stats, &forced.keeper.pubkey());
    let keeper = forced.keeper.insecure_clone();
    let ix = force_cancel_clob_ix(
        &forced,
        filler_user,
        filler_stats,
        failing.user,
        failing.stats,
        forced.keeper.pubkey(),
        vec![velocity::instructions::ForceCancelClobRefV0 {
            order_ref: bound,
            side: velocity::state::prop_amm::ClobSide::Bid,
        }],
    );

    send_with_ixs(
        &mut forced.svm,
        &keeper,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect("force-cancel reaches a bound remainder");
    assert_eq!(
        perp_position(&forced.svm, &failing.user).open_bids,
        0,
        "the bound order came off and its reservation was unwound"
    );

    // Past the activation slot the owner may pull it like any other order.
    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    assert_eq!(
        clob_bid_count(&fixture),
        1,
        "the window opened, so the order is matchable depth now"
    );

    send(&mut fixture.svm, &authority, cancel_ix(subject), &[]).expect("the window has passed");
    assert_eq!(clob_bid_count(&fixture), 0);
    assert_eq!(perp_position(&fixture.svm, &taker.user).open_bids, 0);
}

/// The window, doing the job it exists for. A remainder inside it is not in the
/// book's matchable set at all, so nobody can take it at its own price while
/// makers are still arriving. When it opens, the maker that quoted best during
/// the window is what the crank settles against — a race on price rather than
/// on who lands a transaction first.
#[test]
fn a_maker_that_arrives_during_the_window_wins_on_price_at_activation() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    set_clob_default_activation_delay(&mut fixture, 4);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let maker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    let _ = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT / 2,
    );

    // Inside the window the book does not report it, so no counterparty can
    // reach it and no cross exists to resolve.
    assert!(
        clob_side(&fixture, Direction::Short).is_empty(),
        "a remainder inside its window is not matchable depth"
    );

    // The maker lines up while the window runs. It takes the market's default
    // delay rather than asking for a faster one, which is what an ordinary
    // maker without the flow-authority attestation can do.
    let maker_ix = place_clob_order_ix(
        maker.user,
        &maker.authority,
        fixture.quoter_slab,
        fixture.clob_market,
        fixture.oracle,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 99 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: 0,
            activation_delay_slots: None,
            reject_if_crossed: false,
        },
    );
    let maker_authority = maker.authority.insecure_clone();
    send(&mut fixture.svm, &maker_authority, maker_ix, &[]).unwrap();
    let keeper_authority = keeper.authority.insecure_clone();
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &[&maker]);
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("nothing to resolve until the window opens");
    assert!(
        err.meta.logs.join(" ").contains("NoTakerOriginCross"),
        "unexpected: {:?}",
        err.meta.logs
    );

    // The window opens. Now the remainder is matchable, the cross exists, and
    // it settles at the maker's 99 rather than the taker's own 101.
    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &[&maker]);
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect("the window has opened");
    assert!(
        meta.logs.join(" ").contains(
            "taker-origin remainder routed: 500000000 base at 99000000 instead of 101000000"
        ),
        "settled at the maker's price: {:?}",
        meta.logs
    );
    assert_eq!(
        perp_position(&fixture.svm, &taker.user).base_asset_amount,
        (UNIT / 2) as i64
    );
}

/// Price priority holds against a hand-built crank.
///
/// A maker bidding better than a remainder has priority on the ask that
/// crosses them both. Resolving the remainder first would fill it out of depth
/// that maker was in line for, and this instruction is permissionless, so the
/// rule cannot live only in the resolver that stages it. Clearing the front
/// with the arbitrage crank is what brings the remainder forward.
#[test]
fn a_crank_cannot_jump_a_maker_resting_in_front_of_the_remainder() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let seller = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let ahead = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    // The remainder bids 101; a maker bids 102 in front of it; one ask at 99
    // crosses them both.
    let _ = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT / 2,
    );

    place_clob_order_for(
        &mut fixture,
        &ahead,
        PositionDirection::Long,
        102 * PRICE,
        UNIT / 2,
    );
    place_clob_order_for(
        &mut fixture,
        &seller,
        PositionDirection::Short,
        99 * PRICE,
        UNIT / 2,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &[&seller]);
    let keeper_authority = keeper.authority.insecure_clone();
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("the front of the book is not this crank's");
    assert!(
        err.meta.logs.join(" ").contains("NoTakerOriginCross"),
        "unexpected: {:?}",
        err.meta.logs
    );

    // Nothing moved: the remainder still holds its whole reservation.
    assert_eq!(
        perp_position(&fixture.svm, &taker.user).open_bids,
        (UNIT / 2) as i64
    );
    assert_eq!(
        perp_position(&fixture.svm, &taker.user).base_asset_amount,
        0
    );
}

/// An improvement too small to fund the cranker's reward still resolves — for
/// free. Refusing it instead would leave the remainder gated against being
/// taken with nothing able to clear the gate, which is worse for the taker
/// than the fill it asked for; and a unit of dust in front of a remainder
/// would be enough to strand it for its whole life. The cranker is not working
/// for nothing either way — the market's reservoir pays its lamports, exactly
/// as it does for every other crank.
#[test]
fn a_dust_improvement_resolves_without_paying_the_cranker() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let maker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    let _subject = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        100 * PRICE,
        UNIT / 2,
    );

    // A fifth of a basis point better, on half a unit: 1_000 quote units of
    // improvement, against a reward the filler-reward schedule prices at 10%
    // of the taker fee — about 5_000 on this notional.
    place_clob_order_for(
        &mut fixture,
        &maker,
        PositionDirection::Short,
        100 * PRICE - 2_000,
        UNIT / 2,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &[&maker]);
    let keeper_authority = keeper.authority.insecure_clone();
    send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    let taker_position = perp_position(&fixture.svm, &taker.user);
    assert_eq!(taker_position.base_asset_amount, (UNIT / 2) as i64);
    assert_eq!(taker_position.open_bids, 0);
    assert_eq!(taker_position.open_orders, 0, "consumed outright");
    assert_eq!(
        perp_position(&fixture.svm, &keeper.user).quote_asset_amount,
        0,
        "a reward that does not fit in the improvement is not paid at all"
    );

    // The whole dust improvement stayed with the taker: notional at the
    // counterparty's price plus its own taker fee, nothing else.
    let paid = -taker_position.quote_asset_amount;
    assert_eq!(paid, 49_999_000 + 20_000);
    assert!(
        paid < 50_000_000 + 20_000,
        "still cheaper than being taken at the 100 it rested at"
    );
}

/// Cancel one `party`'s CLOB order through the velocity adapter, unwinding its
/// reservation.
fn cancel_clob_order_for(fixture: &mut Fixture, party: &Party, order_ref: ClobOrderRefV0) {
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelOrderV1 {
            perp_market: perp_market_pda(0),
            user: party.user,
            authority: party.authority.pubkey(),
            quoter_slab: fixture.quoter_slab,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelOrderV1 {
            params: CancelOrderV1Params {
                market_index: 0,
                order_ref,
            },
        }
        .data(),
    };
    let authority = party.authority.insecure_clone();
    send(&mut fixture.svm, &authority, ix, &[]).unwrap();
}

/// Two migrated taker remainders crossing each other, and nothing else on the
/// book — `early` resting first, `late` a slot later.
///
/// The sequence is the reachable one rather than a contrivance. A remainder
/// rests; something crosses it, which takes it out of the book's matchable set;
/// and while it is held back a second taker's fill finds no liquidity where that
/// remainder is standing, so its own unfilled size migrates onto the other side
/// instead of taking it. Lifting the blocker leaves two remainders facing each
/// other with nobody able to take either — which is exactly the state neither
/// crank could resolve before.
fn rest_crossing_remainders(
    fixture: &mut Fixture,
    blocker: &Party,
    early: (&Party, PositionDirection, u64, u64),
    late: (&Party, PositionDirection, u64, u64),
    late_slot: u64,
) -> ClobOrderRefV0 {
    let (early_party, early_direction, early_price, early_size) = early;
    let (late_party, late_direction, late_price, late_size) = late;
    let _ = rest_taker_origin_order(
        fixture,
        early_party,
        early_direction,
        early_price,
        early_size,
    );
    let blocker_ref =
        place_clob_order_for(fixture, blocker, late_direction, early_price, early_size);

    // A slot apart, so time priority is decided by the slot rather than by the
    // order id. The same-slot tie-break is a unit test.
    fixture.svm.warp_to_slot(late_slot);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        late_slot,
    );

    let late = rest_taker_origin_order(fixture, late_party, late_direction, late_price, late_size);
    cancel_clob_order_for(fixture, blocker, blocker_ref);
    assert_eq!(clob_bid_count(&fixture), 1);
    assert_eq!(clob_ask_count(&fixture), 1);
    late
}

/// Two taker remainders crossing each other, resolved by price-time priority:
/// the one that rested first is the maker at its own price, and the later
/// arrival is the aggressor that crosses into it.
///
/// A bid at 101 rests first; an ask at 99 for half the size arrives a slot
/// later. Neither can be taken — the book withholds both — and neither can be
/// consumed with `execute_v0` for the same reason, so the crank cancels the pair
/// and settles it at **101**: the seller gets the whole improvement for having
/// come to trade, and the bid gets the price it was already offering, which is
/// all a maker is ever promised.
#[test]
fn two_crossed_remainders_settle_at_the_one_that_rested_first() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);

    let early = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let late = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let blocker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    let _subject = rest_crossing_remainders(
        &mut fixture,
        &blocker,
        (&early, PositionDirection::Long, 101 * PRICE, UNIT),
        (&late, PositionDirection::Short, 99 * PRICE, UNIT / 2),
        12,
    );

    assert_eq!(
        perp_position(&fixture.svm, &late.user).open_asks,
        -((UNIT / 2) as i64),
        "the later remainder's worst case is reserved on the book"
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    // The aggressor is the later order, so it is the `taker` of the crank.
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &late, &[&early]);
    let keeper_authority = keeper.authority.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    // The bound is loose on purpose. `find_program_address`'s bump search
    // varies a few thousand units with the account keys, and the fixture
    // draws authorities at random, so a tight bound fails about one run in
    // five. This asserts the ~190,000-unit gap to the maker path.
    assert!(
        meta.compute_units_consumed < 90_000,
        "crank cost {} CU",
        meta.compute_units_consumed
    );
    assert!(
        meta.logs.join(" ").contains(
            "taker-origin remainder routed: 500000000 base at 101000000 instead of 99000000"
        ),
        "settled at the earlier order's price: {:?}",
        meta.logs
    );

    // The seller sold at 101, not at the 99 it was resting at: it receives the
    // 50.5 notional less its taker fee and the cranker's cut, comfortably above
    // the 49.5 its own price would have brought.
    let late_position = perp_position(&fixture.svm, &late.user);
    assert_eq!(late_position.base_asset_amount, -((UNIT / 2) as i64));
    let received = late_position.quote_asset_amount;
    assert!(
        (50_400_000..50_500_000).contains(&received),
        "received {received}: 101 less fees, where 99 would have brought 49_500_000"
    );
    assert_eq!(late_position.open_asks, 0, "consumed outright");
    assert_eq!(late_position.open_orders, 0);

    // The buyer paid its own 101 and keeps its rebate — a maker's outcome, and
    // the half the seller could not fill is back on the book still taker-origin.
    let early_position = perp_position(&fixture.svm, &early.user);
    assert_eq!(early_position.base_asset_amount, (UNIT / 2) as i64);
    assert!(
        (-50_500_000..=-50_480_000).contains(&early_position.quote_asset_amount),
        "paid its own 101, less its maker rebate: {}",
        early_position.quote_asset_amount
    );
    assert_eq!(
        early_position.open_bids,
        (UNIT / 2) as i64,
        "the re-placed leftover keeps its reservation"
    );
    assert_eq!(early_position.open_orders, 1);
    assert_eq!(clob_bid_count(&fixture), 1);
    assert_eq!(clob_ask_count(&fixture), 0);
    // The re-placed leftover's new handle is the transaction's return data: the
    // old order id is stale, and a client holding it has to re-read this one.
    let new_order_id = u64::from_le_bytes(meta.return_data.data[4..12].try_into().unwrap());
    assert!(new_order_id > 0, "the leftover rested under a new id");

    // The cranker is paid out of the improvement, in quote, on its own `User`:
    // the ordinary filler reward, 10% of the taker fee on the 50.5 notional.
    let reward = perp_position(&fixture.svm, &keeper.user).quote_asset_amount;
    assert_eq!(reward, 2_020);
    let improvement = 1_000_000; // (101 - 99) * 0.5 units
    assert!(
        reward < improvement,
        "reward {reward} must fit inside the {improvement} improvement"
    );

    // The invariant, measured against what resting would have paid: the seller
    // keeps more than its own 99 net of the same fee schedule.
    assert!(
        received > 49_500_000 - 49_500,
        "crossing brought {received}, resting would have brought {}",
        49_500_000 - 49_500
    );

    // Nothing crosses the leftover now, so there is no cross to resolve and the
    // crank declines rather than doing something arbitrary.
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &late, &[&early]);
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("nothing crosses the leftover");
    assert!(
        err.meta.logs.join(" ").contains("NoTakerOriginCross"),
        "unexpected: {:?}",
        err.meta.logs
    );
}

/// The leftover belongs to whichever remainder was bigger, and once both sides
/// are remainders that can be the aggressor. Here the later order is twice the
/// size of the one it crosses, so its own unconsumed half goes back on the book
/// — on its own side, at its own price, still taker-origin — while the earlier
/// order is consumed outright.
#[test]
fn the_aggressors_own_leftover_goes_back_on_its_side() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);

    let early = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let late = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let blocker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    // The earlier remainder is the *ask* this time, so the aggressor is a buyer
    // and the settlement price is the ask's 99 — the mirror of the case above.
    let _subject = rest_crossing_remainders(
        &mut fixture,
        &blocker,
        (&early, PositionDirection::Short, 99 * PRICE, UNIT / 2),
        (&late, PositionDirection::Long, 101 * PRICE, UNIT),
        12,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &late, &[&early]);
    let keeper_authority = keeper.authority.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    assert!(
        meta.logs.join(" ").contains(
            "taker-origin remainder routed: 500000000 base at 99000000 instead of 101000000"
        ),
        "the earlier ask priced it: {:?}",
        meta.logs
    );

    // The buyer bought half a unit at 99 and still has half a unit resting at
    // its own 101, with the reservation to match — cancelling the whole thing
    // because half of it crossed would take its queue position for nothing.
    let late_position = perp_position(&fixture.svm, &late.user);
    assert_eq!(late_position.base_asset_amount, (UNIT / 2) as i64);
    assert_eq!(late_position.open_bids, (UNIT / 2) as i64);
    assert_eq!(late_position.open_orders, 1);
    assert_eq!(clob_bid_count(&fixture), 1);
    assert_eq!(clob_ask_count(&fixture), 0);

    // The earlier ask was consumed outright: nothing reserved, no order left.
    let early_position = perp_position(&fixture.svm, &early.user);
    assert_eq!(early_position.base_asset_amount, -((UNIT / 2) as i64));
    assert_eq!(early_position.open_asks, 0);
    assert_eq!(early_position.open_orders, 0);
    assert!(
        early_position.quote_asset_amount >= 49_500_000,
        "the seller got the 99 it asked for, plus its rebate: {}",
        early_position.quote_asset_amount
    );

    // The buyer's all-in cost beats the 101 it was resting at, which is the
    // whole point of the mechanism.
    let paid = -late_position.quote_asset_amount;
    assert!(
        paid < 50_500_000 + 50_500,
        "crossing cost {paid}, resting would have cost {}",
        50_500_000 + 50_500
    );
}

/// Relay discovery for the taker-origin cross, and the whole staged shape of it.
///
/// The crank has no condition of its own and needs none: it only ever resolves
/// the tops of the matchable book, and the market's cross conditions already
/// wake on the book's bests moving and on an activation slot maturing. Their
/// resolver stages `crank_taker_origin_cross` when the pair at the top is a
/// migrated remainder crossed by a counterparty, and the arb crank otherwise —
/// a `ResolvedCrankV0` names its own executor, so one condition serves both.
#[test]
fn cross_conditions_stage_the_taker_origin_crank_for_a_crossed_remainder() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    const PAYMENT: u64 = 10_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();
    let protocol_user = set_protocol_user(&mut fixture.svm);
    let (signer, _) = velocity_signer_pda();
    let protocol_stats =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let maker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let _subject = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT,
    );

    place_clob_order_for(
        &mut fixture,
        &maker,
        PositionDirection::Short,
        99 * PRICE,
        UNIT / 2,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let resolved =
        run_cross_resolver(&mut fixture, conditions).expect("a crossed remainder is work");
    assert_eq!(
        resolved.executor_disc,
        velocity::instruction::CrankTakerOriginCross::DISCRIMINATOR,
        "the taker's improvement is handed over before the protocol middles the same book"
    );

    // Both `(User, UserStats)` pairs come off the two nodes' own
    // `(authority, sub_account_id)`, and the rest of the list is PDAs plus the
    // accounts the resolver holds — nothing a turner has to be told.
    let expected: Vec<(Pubkey, bool)> = vec![
        (state_pda(), false),
        (
            Pubkey::new_from_array(velocity::relay_spec::KEEPER_PLACEHOLDER),
            true,
        ),
        (protocol_user, true),
        (protocol_stats, true),
        (taker.user, true),
        (taker.stats, true),
        (fixture.quoter_slab, false),
        (fixture.clob_market, true),
        (clob_id(), false),
        (conditions, true),
        (signed_msg_user_orders_pda(&taker.authority.pubkey()), false),
        (instructions_sysvar(), false),
        (fixture.oracle, false),
        (spot_market_pda(0), true),
        (perp_market_pda(0), true),
        (maker.user, true),
        (maker.stats, true),
        // The quoter tail: the crank routes the remainder, so it carries the
        // market's slab and the book's accounts as the baseline every router
        // fill must include.
        (fixture.quoter_slab, false),
        (fixture.clob_market, true),
        (clob_id(), false),
    ];

    assert_eq!(
        resolved
            .accounts
            .iter()
            .map(|a| (Pubkey::new_from_array(a.address), a.is_writable()))
            .collect::<Vec<_>>(),
        expected
    );

    // `market_index`, then the depth the resolver found the cross at, then an
    // empty signed route: a staged crank routes through the market's baseline
    // and claims none of the taker's own quoters.
    assert_eq!(
        resolved.data,
        [
            0u16.to_le_bytes().as_slice(),
            1u16.to_le_bytes().as_slice(),
            0u32.to_le_bytes().as_slice(),
        ]
        .concat(),
        "market_index, the cross depth, and an empty route"
    );

    // The staged payload fits the shared staging region with room to spare, so
    // the account list is bounded by the transaction rather than by the scratch.
    assert!(
        resolved.encoded_len() < velocity::state::relay_scratch::RELAY_SCRATCH_LEN,
        "staged {} bytes",
        resolved.encoded_len()
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankTakerOriginCross::DISCRIMINATOR,
        payout,
    );

    // Settled at the counterparty's 99 rather than the 101 the remainder rested
    // at, exactly as the signed-keeper path does.
    let taker_position = perp_position(&fixture.svm, &taker.user);
    assert_eq!(taker_position.base_asset_amount, (UNIT / 2) as i64);
    let paid = -taker_position.quote_asset_amount;
    assert!(
        (49_500_000..49_600_000).contains(&paid),
        "paid {paid}: 99 plus fees, where 101 would have been 50_500_000"
    );

    // Program-keeper mode: the quote reward accrues to the protocol `User` and
    // the payout account takes the reservoir's lamports, which is the payment
    // relay's `assert_paid_v0` measures against the condition's `min_payment`.
    assert_eq!(
        perp_position(&fixture.svm, &protocol_user).quote_asset_amount,
        1_980
    );
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + PAYMENT
    );

    // The unconsumed half is back on the book with nothing crossing it —
    // ordinary depth, so the conditions go quiet.
    assert_eq!(clob_bid_count(&fixture), 1);
    assert_eq!(clob_ask_count(&fixture), 0);
    assert!(run_cross_resolver(&mut fixture, conditions).is_none());
}

/// Two remainders facing each other are staged by the same resolver, with the
/// later arrival in the `taker` slot and the earlier one as the counterparty
/// whose price the match settles at — the branch the crank takes needs no
/// condition of its own either.
#[test]
fn cross_conditions_stage_the_pair_branch_with_the_later_remainder_as_taker() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    const PAYMENT: u64 = 10_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();
    set_protocol_user(&mut fixture.svm);

    let early = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let late = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let blocker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let _subject = rest_crossing_remainders(
        &mut fixture,
        &blocker,
        (&early, PositionDirection::Long, 101 * PRICE, UNIT),
        (&late, PositionDirection::Short, 99 * PRICE, UNIT / 2),
        12,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let resolved = run_cross_resolver(&mut fixture, conditions).expect("the pair is work");
    assert_eq!(
        resolved.executor_disc,
        velocity::instruction::CrankTakerOriginCross::DISCRIMINATOR
    );
    assert_eq!(
        resolved.accounts[4].address,
        late.user.to_bytes(),
        "the later remainder is the aggressor, so it is the crank's taker"
    );
    assert_eq!(
        resolved.accounts[15].address,
        early.user.to_bytes(),
        "the earlier one is the counterparty the match is priced at"
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankTakerOriginCross::DISCRIMINATOR,
        payout,
    );

    let late_position = perp_position(&fixture.svm, &late.user);
    assert_eq!(late_position.base_asset_amount, -((UNIT / 2) as i64));
    assert!(
        (50_400_000..50_500_000).contains(&late_position.quote_asset_amount),
        "sold at the earlier order's 101 less fees: {}",
        late_position.quote_asset_amount
    );
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + PAYMENT
    );
}

/// The two cross cranks compose rather than duplicating each other's search: a
/// maker×maker cross in front of a migrated remainder is the arb crank's work,
/// and the remainder's own cross becomes the resolver's answer once that clears.
///
/// The remainder's base is never offered to the arb crank, which is what makes
/// the composition work at all. A leg sized to include it either comes back
/// short — the book withholds a crossed remainder from `execute_v0`, so the two
/// legs imbalance and the cross in front of it is stuck as well — or, when the
/// first leg consumed the whole opposite side, nothing crosses the remainder any
/// more by the time the second leg runs and the book hands it over at the price
/// it rested at, with the improvement landing in the protocol `User` instead of
/// the taker's. The asks here are exactly the crossing depth, which is the
/// second shape.
#[test]
fn a_claim_outranks_a_better_priced_maker_and_holds_the_front_until_it_lapses() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    const PAYMENT: u64 = 10_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();
    let protocol_user = set_protocol_user(&mut fixture.svm);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let seller = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let buyer = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);

    // A remainder at 101, a whole unit of asks at 99 crossing it, and a maker
    // bidding 102 in front of it — so the top of the book is maker×maker and the
    // remainder is the second-best bid.
    let _subject = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT,
    );

    place_clob_order_for(
        &mut fixture,
        &seller,
        PositionDirection::Short,
        99 * PRICE,
        UNIT,
    );
    place_clob_order_for(
        &mut fixture,
        &buyer,
        PositionDirection::Long,
        102 * PRICE,
        UNIT / 2,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();

    // A claim outranks price. The remainder holds the 99 ask against the
    // better-priced 102 bid, so neither cross runs: neither book head is
    // taker-origin. The claim expires a grace window after the remainder
    // activates, when the arb crank clears the front.
    assert!(
        run_cross_resolver(&mut fixture, conditions).is_none(),
        "the claim holds the ask, so neither crank has work"
    );

    // Past the grace window the claim is inert and the front of book is an
    // ordinary maker cross again.
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let lapsed = clock.slot + RESERVATION_GRACE_SLOTS + 1;
    fixture.svm.warp_to_slot(lapsed);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        lapsed,
    );

    let resolved = run_cross_resolver(&mut fixture, conditions).expect("the front of book crosses");
    assert_eq!(
        resolved.executor_disc,
        velocity::instruction::CrankCrossMatch::DISCRIMINATOR,
        "two makers crossing is unclaimed arbitrage, not somebody's improvement"
    );

    // `[market_index u16][size u64][buy u8][sell u8]`: the staged size stops at
    // the 102 bid. That cap is also what keeps the arb legs off the remainder
    // outright — the sell leg is sized to the depth in front of it, so it never
    // sweeps that far even in the moment the first leg empties the ask side and
    // the book's own protection stops firing.
    assert_eq!(
        u64::from_le_bytes(resolved.data[2..10].try_into().unwrap()),
        UNIT / 2,
        "the cross is sized to the maker in front of the remainder"
    );

    assert!(
        !resolved
            .accounts
            .iter()
            .any(|a| a.address == taker.user.to_bytes()),
        "the remainder's owner is not staged, so the book cannot settle its order for this cross"
    );

    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankCrossMatch::DISCRIMINATOR,
        payout,
    );

    // Sized to the 102 bid alone: the remainder behind it contributed nothing.
    assert_eq!(
        perp_position(&fixture.svm, &buyer.user).base_asset_amount,
        (UNIT / 2) as i64
    );
    assert_eq!(
        perp_position(&fixture.svm, &seller.user).base_asset_amount,
        -((UNIT / 2) as i64)
    );

    assert!(perp_position(&fixture.svm, &protocol_user).quote_asset_amount > 0);
    assert_eq!(
        perp_position(&fixture.svm, &taker.user).base_asset_amount,
        0,
        "the remainder was not part of the arb"
    );

    // With the front of book cleared the remainder is the best bid, and the same
    // resolver now answers with the crank that prices in its favour.
    let resolved =
        run_cross_resolver(&mut fixture, conditions).expect("the remainder's cross is next");
    assert_eq!(
        resolved.executor_disc,
        velocity::instruction::CrankTakerOriginCross::DISCRIMINATOR
    );
    assert_eq!(resolved.accounts[4].address, taker.user.to_bytes());
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::CrankTakerOriginCross::DISCRIMINATOR,
        payout,
    );

    let taker_position = perp_position(&fixture.svm, &taker.user);
    assert_eq!(taker_position.base_asset_amount, (UNIT / 2) as i64);
    let paid = -taker_position.quote_asset_amount;
    assert!(
        (49_500_000..49_600_000).contains(&paid),
        "paid {paid}: the seller's 99, not the 101 it rested at"
    );
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + 2 * PAYMENT
    );
}

/// A fired DLOB stop-market fills straight to the book and rests only its
/// remainder, leaving nothing live in `User.orders`.
///
/// `trigger_market_order_v1` fires the armed trigger, routes the now-live market order
/// against the book, and migrates the unfilled remainder as a taker-origin
/// order in one instruction. The v0 path would instead leave a live
/// `TriggeredAbove` slot for a later fill crank.
#[test]
fn trigger_market_order_v1_fires_a_stop_market_straight_to_the_book() {
    use velocity::state::user::OrderTriggerCondition;

    let mut fixture = setup();

    let maker_stats = Pubkey::find_program_address(
        &[
            b"user_stats",
            fixture.clob_maker_authority.pubkey().as_ref(),
        ],
        &velocity_id(),
    )
    .0;
    set_user_stats_account(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    // The vAMM would absorb the whole order; pause it so the CLOB half is the
    // fill and a remainder survives to migrate.
    pause_amm_fill(&mut fixture.svm);
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    // A long buy-stop armed below the oracle: it fires at oracle 100, becomes a
    // live market order, and takes the book's ask.
    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut order = Order::default();
    order.order_id = 1;
    order.status = OrderStatus::Open;
    order.order_type = OrderType::TriggerMarket;
    order.market_type = MarketType::Perp;
    order.market_index = 0;
    order.direction = PositionDirection::Long;
    order.base_asset_amount = UNIT;
    order.price = 0;
    order.trigger_price = 99 * PRICE;
    order.trigger_condition = OrderTriggerCondition::Above;
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    order.max_ts = clock.unix_timestamp + 1_000;
    let mut taker_state = armed_trigger_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        order,
    );

    taker_state.next_order_id = 2;
    set_user_account(&mut fixture.svm, taker_user, &taker_state);
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );

    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let mut accounts = velocity::accounts::TriggerMarketOrderV1 {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        crank_conditions: None,
        trigger_conditions: None,
        ix_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::TriggerMarketOrderV1 {
            args: TriggerMarketOrderV1Args {
                market_index: 0,
                order_id: 1,
                signed_route: vec![],
            },
        }
        .data(),
    };

    send_with_ixs(
        &mut fixture.svm,
        &fixture.keeper,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();

    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount,
        (UNIT / 2) as i64,
        "the fired stop took the book's half"
    );

    assert!(
        taker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "the armed slot is freed: nothing lingers live on the DLOB"
    );
    assert_eq!(
        taker.perp_positions[0].open_bids,
        (UNIT / 2) as i64,
        "the remainder is reserved against the book"
    );
    assert_eq!(
        clob_bid_count(&fixture),
        1,
        "the remainder rests taker-origin on the book"
    );
}

/// The arbitrage crank cannot reach a crossed taker remainder's cover.
///
/// The remainder claims the depth it crosses, and claimed depth leaves the
/// book's matchable set for every caller that does not consume reservations.
/// This crank never does, so the ask the remainder crosses is invisible to
/// it, its buy leg finds nothing, and the cross is refused for having matched
/// no base. The improvement stays with `crank_taker_origin_cross`, which is
/// the only caller that may take that cover and the one that owes the taker
/// the difference.
///
/// No predicate does this. The crank is permissionless and the caller names
/// the size, so a rule that refused only some sizes would leave the rest.
#[test]
fn the_arb_crank_cannot_reach_a_crossed_taker_remainders_cover() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);
    const PAYMENT: u64 = 10_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();
    let protocol_user = set_protocol_user(&mut fixture.svm);
    let (signer, _) = velocity_signer_pda();
    let protocol_stats =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let maker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    // A migrated remainder bidding 101, crossed by a maker ask at 99.
    let _subject = rest_taker_origin_order(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        101 * PRICE,
        UNIT,
    );

    place_clob_order_for(
        &mut fixture,
        &maker,
        PositionDirection::Short,
        99 * PRICE,
        UNIT / 2,
    );

    fixture.svm.warp_to_slot(20);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        20,
    );

    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let mut accounts = velocity::accounts::CrankCrossMatch {
        state: state_pda(),
        authority: payout,
        taker: protocol_user,
        taker_stats: protocol_stats,
        crank_conditions: conditions,
        perp_market: perp_market_pda(0),
        quoter_slab: fixture.quoter_slab,
        instructions_sysvar: instructions_sysvar(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    // Both owners loaded, which is what would let a hand-built call settle
    // against the remainder if the book offered it at all.
    accounts.push(AccountMeta::new(taker.user, false));
    accounts.push(AccountMeta::new(taker.stats, false));
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::CrankCrossMatch {
            args: CrankCrossMatchArgs {
                market_index: 0,
                size: UNIT / 2,
            },
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    let err = send_with_ixs(
        &mut fixture.svm,
        &keeper,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("the arb crank must not touch a crossed remainder");
    assert!(
        format!("{:?}", err.meta.logs).contains("CrossMatchUnprofitable"),
        "unexpected: {:?}",
        err.meta.logs
    );

    // Nothing moved: the remainder is still resting and unfilled.
    let taker_account: User = read_zero_copy(&fixture.svm, &taker.user);
    assert_eq!(
        taker_account.perp_positions[0].base_asset_amount, 0,
        "the remainder was not filled"
    );
    assert_eq!(clob_bid_count(&fixture), 1);
}

/// A quoter whose CPI reverts is identifiable only from the runtime's own
/// CPI frames.
///
/// A failed CPI ends the calling instruction, so velocity never reaches the
/// line where it would name the entry it could not use. What survives is the
/// runtime's bracketing: the callee's program id, its `invoke [2]` frame, and
/// its failure. A program id is not an entry, because one quoter program
/// serves many registry entries, so an off-chain router resolves it by
/// counting frames against the order it built the route in.
///
/// Nothing lands when a simulation fails, so these logs are the only record
/// the failure leaves. This test pins the shape `rust/quoter-health` parses.
/// If it fails after a message or a runtime changes, re-capture the fixture
/// it prints into `rust/quoter-health/tests/fixtures/`.
#[test]
fn a_reverting_quoter_leaves_only_its_cpi_frame_in_the_logs() {
    let mut fixture = setup();
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let router = Keypair::new();
    fixture
        .svm
        .airdrop(&router.pubkey(), 10_000_000_000)
        .unwrap();
    // Larger than a CPI can allocate, so the caller pre-creates it.
    let quote_buffer = Pubkey::new_unique();
    fixture
        .svm
        .set_account(
            quote_buffer,
            Account {
                lamports: 10_000_000_000,
                data: vec![0u8; velocity::state::router_quote::RouterQuoteBufferV0::SIZE],
                owner: velocity_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeRouterQuoteBuffer {
            quote_buffer,
            authority: router.pubkey(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeRouterQuoteBuffer {
            args: InitializeRouterQuoteBufferArgs { market_index: 0 },
        }
        .data(),
    };

    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    // Break the quote-leg discriminator on the approved copy — the one a
    // fill reads. The CPI now names an instruction the CLOB does not have,
    // so the callee errors and velocity reports the entry it could not use.
    let mut slot = read_slab_slot(&fixture.svm, 0, 0);
    slot.config.quote_v0_discriminator = [0xAA; 8];
    write_slab_slot(&mut fixture.svm, 0, 0, &slot);

    let mut accounts = velocity::accounts::QuoteRouter {
        state: state_pda(),
        authority: router.pubkey(),
        quote_buffer,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                taker_served_window: true,
                market_index: 0,
                direction: Direction::Long,
                size: 2 * UNIT,
                include_vamm: true,
            },
        }
        .data(),
    };
    let failure = send_with_ixs(
        &mut fixture.svm,
        &router,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("a quoter that cannot be called must fail the quote");

    // Printed so the fixture the off-chain parser tests against can be
    // re-captured when this changes.
    println!("--- BEGIN QUOTE FAILURE LOGS ---");
    for line in &failure.meta.logs {
        println!("{line}");
    }

    println!("--- END QUOTE FAILURE LOGS ---");

    // Velocity does not get to speak. The CPI failure ends its instruction
    // before the line that would name the entry, so a router must not be
    // built to expect one here.
    assert!(
        !failure
            .meta
            .logs
            .iter()
            .any(|line| line.starts_with("Program log: quoter ")),
        "a failed CPI ends the caller, so velocity cannot name the entry"
    );

    // What does survive: the callee's frame, and its failure.
    assert!(
        failure
            .meta
            .logs
            .iter()
            .any(|line| line == &format!("Program {} invoke [2]", clob_id())),
        "the callee's CPI frame must be visible"
    );
    assert!(
        failure
            .meta
            .logs
            .iter()
            .any(|line| line.starts_with(&format!("Program {} failed:", clob_id()))),
        "the callee's failure must be visible"
    );
}

/// A quoter that returns but breaks velocity's own checks is named outright.
///
/// The pre-CPI and post-CPI checks around the quoter call are velocity's own
/// code, so they run and log. This is the half of the failure surface a
/// router can attribute from a single line: everything velocity decides
/// about a quoter's answer, as opposed to the quoter refusing to answer.
/// The contract violations an off-chain router punishes hardest all land
/// here.
#[test]
fn a_quoter_velocity_refuses_is_named_in_the_logs() {
    let mut fixture = setup();
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let router = Keypair::new();
    fixture
        .svm
        .airdrop(&router.pubkey(), 10_000_000_000)
        .unwrap();
    let quote_buffer = Pubkey::new_unique();
    fixture
        .svm
        .set_account(
            quote_buffer,
            Account {
                lamports: 10_000_000_000,
                data: vec![0u8; velocity::state::router_quote::RouterQuoteBufferV0::SIZE],
                owner: velocity_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeRouterQuoteBuffer {
            quote_buffer,
            authority: router.pubkey(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeRouterQuoteBuffer {
            args: InitializeRouterQuoteBufferArgs { market_index: 0 },
        }
        .data(),
    };

    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    // Make the approved copy claim one more registered account than it
    // holds, and point the quote leg at it. The extra slot is zeroed, so
    // velocity cannot resolve it and refuses before any CPI.
    let mut slot = read_slab_slot(&fixture.svm, 0, 0);
    let extra = slot.config.accounts_count;
    slot.config.accounts_count += 1;
    slot.config.quote_account_indexes[slot.config.quote_accounts_count as usize] = extra;
    slot.config.quote_accounts_count += 1;
    write_slab_slot(&mut fixture.svm, 0, 0, &slot);

    let mut accounts = velocity::accounts::QuoteRouter {
        state: state_pda(),
        authority: router.pubkey(),
        quote_buffer,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                taker_served_window: true,
                market_index: 0,
                direction: Direction::Long,
                size: 2 * UNIT,
                include_vamm: true,
            },
        }
        .data(),
    };
    let failure = send_with_ixs(
        &mut fixture.svm,
        &router,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .expect_err("an unresolvable account must fail the quote");

    println!("--- BEGIN QUOTE REFUSAL LOGS ---");
    for line in &failure.meta.logs {
        println!("{line}");
    }

    println!("--- END QUOTE REFUSAL LOGS ---");

    let named = failure
        .meta
        .logs
        .iter()
        .find(|line| line.starts_with(&format!("Program log: quoter {}", fixture.quoter)))
        .expect("velocity must name the entry whose answer it refused");
    assert!(
        named.contains("quote failed"),
        "the message must say which leg it was: {named}"
    );
}

/// A market with more quoters than one view can carry is read in passes, and
/// only one pass may quote the vAMM.
///
/// The vAMM prices against every other book in the same call, so a pass
/// holding a subset would return a vAMM shaded against a subset. Two passes
/// that both quoted it would put two different answers to the same question
/// into one merged book.
#[test]
fn a_pass_that_clears_include_vamm_returns_only_its_quoters() {
    let mut fixture = setup();
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let router = Keypair::new();
    fixture
        .svm
        .airdrop(&router.pubkey(), 10_000_000_000)
        .unwrap();
    let quote_buffer = Pubkey::new_unique();
    fixture
        .svm
        .set_account(
            quote_buffer,
            Account {
                lamports: 10_000_000_000,
                data: vec![0u8; velocity::state::router_quote::RouterQuoteBufferV0::SIZE],
                owner: velocity_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeRouterQuoteBuffer {
            quote_buffer,
            authority: router.pubkey(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeRouterQuoteBuffer {
            args: InitializeRouterQuoteBufferArgs { market_index: 0 },
        }
        .data(),
    };

    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    let quote = |fixture: &mut Fixture, include_vamm: bool| {
        let mut accounts = velocity::accounts::QuoteRouter {
            state: state_pda(),
            authority: router.pubkey(),
            quote_buffer,
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        let ix = Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::QuoteRouter {
                args: velocity::instructions::QuoteRouterArgs {
                    taker_served_window: true,
                    market_index: 0,
                    direction: Direction::Long,
                    size: 2 * UNIT,
                    include_vamm,
                },
            }
            .data(),
        };
        let meta = send_with_ixs(
            &mut fixture.svm,
            &router,
            &[compute_unit_limit_ix(400_000), ix],
            &[],
        )
        .unwrap();
        let buffer: velocity::state::router_quote::RouterQuoteBufferV0 =
            read_zero_copy(&fixture.svm, &quote_buffer);
        (buffer, meta.compute_units_consumed)
    };

    let (with_vamm, cu_with) = quote(&mut fixture, true);
    let (without_vamm, cu_without) = quote(&mut fixture, false);

    assert_eq!(with_vamm.source_count, 2, "clob + vamm");
    assert_eq!(
        with_vamm.sources[1].kind,
        velocity::state::router_quote::QuotedSourceKind::Vamm
    );

    assert_eq!(without_vamm.source_count, 1, "clob only");
    assert!(
        without_vamm.sources[..1]
            .iter()
            .all(|source| source.kind != velocity::state::router_quote::QuotedSourceKind::Vamm),
        "a pass that cleared the flag must carry no vAMM"
    );

    // The quoter's own book is unchanged by the flag, so passes merge.
    assert_eq!(with_vamm.sources[0].key, without_vamm.sources[0].key);
    assert_eq!(
        with_vamm.sources[0].level_count,
        without_vamm.sources[0].level_count
    );

    // Skipping the ladder is also why extra passes are affordable.
    assert!(
        cu_without < cu_with,
        "clearing the flag must not cost more: {cu_without} vs {cu_with}"
    );
}

// ---------------------------------------------------------------------------
// Wall-clock bench: what one published book costs to produce.
// ---------------------------------------------------------------------------

/// Rearm a midpoint instance with `rungs` levels per side, so the bench
/// quotes a spline rather than a single price.
fn set_midpoint_rungs(fixture: &mut Fixture, maker: &MidpointMaker, rungs: usize, size: u64) {
    let side: Vec<(u64, u64)> = (1..=rungs).map(|i| (1_000 * i as u64, size)).collect();
    let mut data = ix_discriminator("set_levels_v0").to_vec();
    data.push(1);
    data.extend_from_slice(&(100 * PRICE).to_le_bytes());
    data.push(0);
    encode_side(&side, &mut data);
    encode_side(&side, &mut data);
    let ix = Instruction {
        program_id: midpoint_id(),
        accounts: vec![
            AccountMeta::new(maker.instance, false),
            AccountMeta::new_readonly(maker.hot.pubkey(), true),
        ],

        data,
    };

    send(&mut fixture.svm, &fixture.keeper, ix, &[&maker.hot]).unwrap();
}

/// Build the transaction the book publisher runs once per side per tick: a
/// `quote_router` over the market's CLOB, `propamms` midpoint quoters, and
/// the vAMM.
fn bench_quote_case(
    propamms: usize,
    clob_orders: usize,
    rungs: usize,
    heap_bytes: Option<u32>,
    quoters_only: bool,
) -> (
    Fixture,
    solana_transaction::versioned::VersionedTransaction,
    Vec<MidpointMaker>,
    Pubkey,
) {
    use solana_message::{Message, VersionedMessage};

    let mut fixture = setup();
    for i in 0..clob_orders {
        place_clob_ask(&mut fixture, (100 + i as u64) * PRICE, UNIT / 2);
    }

    let makers: Vec<MidpointMaker> = (0..propamms)
        .map(|_| setup_midpoint_maker(&mut fixture, 10_000 * SPOT_BALANCE_PRECISION_U64, UNIT / 2))
        .collect();
    for maker in &makers {
        set_midpoint_rungs(&mut fixture, maker, rungs, UNIT / 2);
    }

    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let router = Keypair::new();
    fixture
        .svm
        .airdrop(&router.pubkey(), 10_000_000_000)
        .unwrap();
    let quote_buffer = Pubkey::new_unique();
    fixture
        .svm
        .set_account(
            quote_buffer,
            Account {
                lamports: 10_000_000_000,
                data: vec![0u8; velocity::state::router_quote::RouterQuoteBufferV0::SIZE],
                owner: velocity_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeRouterQuoteBuffer {
            quote_buffer,
            authority: router.pubkey(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeRouterQuoteBuffer {
            args: InitializeRouterQuoteBufferArgs { market_index: 0 },
        }
        .data(),
    };

    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    let mut accounts = velocity::accounts::QuoteRouter {
        state: state_pda(),
        authority: router.pubkey(),
        quote_buffer,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // User map: every custom quoter's user, for the margin clamp.
    for maker in &makers {
        accounts.push(AccountMeta::new(maker.user, false));
        accounts.push(AccountMeta::new(maker.stats, false));
    }

    // Quoter section: the slab always rides — consulting any external quoter
    // starts from it — and a slot is consulted when its response account is
    // present. A later pass of the publisher's plan leaves the book's
    // accounts at home and carries no vAMM.
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    if !quoters_only {
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
    }

    for maker in &makers {
        accounts.push(AccountMeta::new(maker.instance, false));
    }

    if !makers.is_empty() {
        accounts.push(AccountMeta::new_readonly(instructions_sysvar(), false));
        // The midpoint's quote leg names velocity's State; the instruction's
        // own `state` account is not part of the map, so it rides here too.
        accounts.push(AccountMeta::new_readonly(state_pda(), false));
        accounts.push(AccountMeta::new_readonly(midpoint_id(), false));
    }

    let quote = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                taker_served_window: true,
                market_index: 0,
                direction: Direction::Long,
                size: 1_000 * UNIT,
                include_vamm: !quoters_only,
            },
        }
        .data(),
    };
    let mut ixs = vec![compute_unit_limit_ix(1_400_000)];
    if let Some(bytes) = heap_bytes {
        let mut data = vec![1u8];
        data.extend_from_slice(&bytes.to_le_bytes());
        ixs.push(Instruction {
            program_id: "ComputeBudget111111111111111111111111111111"
                .parse()
                .unwrap(),
            accounts: vec![],
            data,
        });
    }

    ixs.push(quote);
    let msg = Message::new_with_blockhash(
        &ixs,
        Some(&router.pubkey()),
        &fixture.svm.latest_blockhash(),
    );
    let tx = solana_transaction::versioned::VersionedTransaction::try_new(
        VersionedMessage::Legacy(msg),
        &[&router],
    )
    .unwrap();
    (fixture, tx, makers, quote_buffer)
}

/// How long one side of one market's book takes to produce.
///
/// Run with:
/// `cargo test --release bench_quote_router_wall_clock -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_quote_router_wall_clock() {
    use std::time::Instant;

    println!("propamms clob_orders rungs heap_kb       cu    p50us    p90us   meanus  seededus");
    // Eight quoters is this fixture's transaction limit, not the program's.
    // Each midpoint quoter costs four account slots, and a legacy message
    // addresses at most `PACKET_DATA_SIZE / 32` static keys, so the runtime
    // rejects a bigger transaction before velocity runs.
    for (propamms, clob_orders, rungs, heap, quoters_only) in [
        (0usize, 20usize, 0usize, None, false),
        (1, 20, 8, None, false),
        (2, 20, 8, None, false),
        (4, 20, 8, None, false),
        (6, 20, 8, None, false),
        (8, 20, 8, None, false),
        // A deep spline each, so the heap is asked for four times the levels.
        (8, 20, 32, None, false),
        // A later pass carries quoters only.
        (8, 0, 8, None, true),
    ] {
        let (fixture, tx, _, _) =
            bench_quote_case(propamms, clob_orders, rungs, heap, quoters_only);
        let heap_kb = heap.unwrap_or(32 * 1024) / 1024;
        let mut cu = 0;
        let mut failed = None;
        for _ in 0..20 {
            match fixture.svm.simulate_transaction(tx.clone()) {
                Ok(info) => cu = info.meta.compute_units_consumed,
                Err(fail) => {
                    failed = Some(format!(
                        "{:?} | {}",
                        fail.err,
                        fail.meta
                            .logs
                            .iter()
                            .filter(|line| line.contains("Error:") || line.contains("panicked"))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" ; ")
                    ));

                    break;
                }
            }
        }

        if let Some(err) = failed {
            println!("{propamms:8} {clob_orders:11} {rungs:5} {heap_kb:7}  FAILED  {err}");
            continue;
        }

        let runs = 200;
        let mut times: Vec<u128> = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = Instant::now();
            let info = fixture.svm.simulate_transaction(tx.clone()).unwrap();
            times.push(started.elapsed().as_micros());
            std::hint::black_box(info.meta.compute_units_consumed);
        }

        times.sort_unstable();
        let mean = times.iter().sum::<u128>() / runs as u128;

        // The publisher seeds every account a transaction names into a
        // pooled bank before each simulation. Measure that too.
        let keys: Vec<Pubkey> = tx.message.static_account_keys().to_vec();
        let seed: Vec<(Pubkey, Account)> = keys
            .iter()
            .filter_map(|key| {
                let account = fixture.svm.get_account(key)?;
                (!account.executable).then_some((*key, account))
            })
            .collect();
        let mut fixture = fixture;
        let mut seeded: Vec<u128> = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = Instant::now();
            for (key, account) in &seed {
                fixture.svm.set_account(*key, account.clone()).unwrap();
            }

            let info = fixture.svm.simulate_transaction(tx.clone()).unwrap();
            seeded.push(started.elapsed().as_micros());
            std::hint::black_box(info.meta.compute_units_consumed);
        }

        seeded.sort_unstable();
        println!(
            "{propamms:8} {clob_orders:11} {rungs:5} {heap_kb:7} {cu:8} {:8} {:8} {mean:8} {:9}",
            times[runs / 2],
            times[runs * 9 / 10],
            seeded[runs / 2],
        );
    }
}

/// A quoter that fills from one account has no orders to describe, so it
/// declares no `quote_l3_v0` leg and the view attributes its whole ladder to
/// the user its registry entry names. One shape either way: a consumer reads
/// rows, never a quoter type.
#[test]
fn a_quoter_without_the_l3_leg_has_its_ladder_attributed_to_its_user() {
    let (fixture, tx, makers, quote_buffer) = bench_quote_case(1, 20, 8, None, false);
    // The view is a simulation, so its answer lives in the post-simulation
    // account rather than in the ledger.
    let info = fixture.svm.simulate_transaction(tx).expect("quote view");
    let data = info
        .post_accounts
        .iter()
        .find(|(key, _)| *key == quote_buffer)
        .map(|(_, account)| {
            use solana_account::ReadableAccount;
            account.data().to_vec()
        })
        .expect("the buffer rode the simulation");
    let buffer: velocity::state::router_quote::RouterQuoteBufferV0 = *bytemuck::from_bytes(
        &data[8..8 + core::mem::size_of::<velocity::state::router_quote::RouterQuoteBufferV0>()],
    );

    let maker = &makers[0];
    let index = (0..buffer.source_count as usize)
        .find(|index| buffer.sources[*index].key == maker.entry)
        .expect("the midpoint entry was quoted");
    let source = &buffer.sources[index];
    let rows = &buffer.rows[source.row_start as usize..][..source.row_len as usize];

    assert_eq!(
        rows.len(),
        source.level_count as usize,
        "one row per rung of the ladder"
    );

    assert!(rows
        .iter()
        .all(|row| row.authority == maker.authority.pubkey()));
    assert!(
        rows.iter().all(|row| row.order_id == 0),
        "a spline rung is not an order"
    );

    // And the rows carry the ladder's own prices and sizes.
    let levels = &buffer.levels[index][..source.level_count as usize];
    for (row, level) in rows.iter().zip(levels) {
        assert_eq!((row.price, row.size), (level.price, level.size));
    }
}

/// A crossed taker remainder is on the book but is not depth a cross can
/// count on, so the row that carries it says so. The publisher's cross
/// discovery reads that flag instead of the book's bytes.
#[test]
fn a_taker_origin_row_is_flagged_for_whoever_reads_it() {
    let (fixture, tx, _, quote_buffer) = bench_quote_case(0, 4, 0, None, false);
    let info = fixture.svm.simulate_transaction(tx).expect("quote view");
    let data = info
        .post_accounts
        .iter()
        .find(|(key, _)| *key == quote_buffer)
        .map(|(_, account)| {
            use solana_account::ReadableAccount;
            account.data().to_vec()
        })
        .expect("the buffer rode the simulation");
    let buffer: velocity::state::router_quote::RouterQuoteBufferV0 = *bytemuck::from_bytes(
        &data[8..8 + core::mem::size_of::<velocity::state::router_quote::RouterQuoteBufferV0>()],
    );

    let source = &buffer.sources[0];
    let rows = &buffer.rows[source.row_start as usize..][..source.row_len as usize];
    assert_eq!(rows.len(), 4, "one row per resting order");
    assert!(
        rows.iter()
            .all(|row| row.flags & velocity::state::prop_amm::L3_ROW_FLAG_TAKER_ORIGIN == 0),
        "ordinary maker orders are not taker remainders"
    );

    // Rows are best-first, which is the order a fill would take them in.
    assert!(rows.windows(2).all(|w| w[0].price <= w[1].price));
}

/// A taker order routed with an empty book, so the vAMM is the only source.
///
/// Returns the taker's `User` after the fill. The book stays empty because the
/// fixture rests nothing on it, and `require_baseline` is satisfied by carrying
/// the market's CLOB accounts whether or not the book has depth.
fn vamm_take(
    fixture: &mut Fixture,
    taker: &Party,
    direction: PositionDirection,
    base: u64,
    limit: u64,
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker.user,
        user_stats: taker.stats,
        authority: taker.authority.pubkey(),
        quoter_slab: fixture.quoter_slab,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        flow_authority: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter_slab, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            args: PlaceAndTakePerpOrderV1Args {
                params: OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction,
                    base_asset_amount: base,
                    price: limit,
                    market_index: 0,
                    post_only: PostOnlyParam::None,
                    ..OrderParams::default()
                },

                success_condition: None,
            },
        }
        .data(),
    };
    let authority = taker.authority.insecure_clone();
    send_with_ixs(
        &mut fixture.svm,
        &authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
}

/// The vAMM prices a take at its ask, and the spread is inside that price.
///
/// This is the end-to-end pin on the vAMM's own pricing: the reserves move by
/// the constant product, and the taker pays the spread and the fee on top. A
/// change to the spread formula moves `entry` here even though the reserve
/// math is untouched, which is the failure this exists to catch — a spread
/// that silently narrows costs the AMM the width it was quoting.
#[test]
fn a_vamm_take_pays_the_spread_inside_its_entry_price() {
    let mut fixture = setup();
    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    vamm_take(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        UNIT,
        105 * PRICE,
    )
    .unwrap();

    let market: PerpMarket = read_zero_copy(&fixture.svm, &perp_market_pda(0));
    // The constant product alone: k / (100 - 1) base reserves. The fixture
    // starts balanced at 100/100, so the no-spread cost of one unit is the
    // reserve move, 101.0101 quote.
    assert_eq!(market.amm.base_asset_reserve, 99 * AMM_RESERVE_PRECISION);
    assert_eq!(market.amm.quote_asset_reserve, 101_010_101_010);
    // The AMM took the other side: it was long 0.5 and is long 1.5.
    assert_eq!(
        market.amm.base_asset_amount_with_amm,
        (3 * AMM_RESERVE_PRECISION / 2) as i128
    );

    let taker_state: User = read_zero_copy(&fixture.svm, &taker.user);
    let position = taker_state.perp_positions[0];
    assert_eq!(position.base_asset_amount, UNIT as i64);
    // All-in, so the spread and the taker fee are both inside it. It must sit
    // above the 101.0101 the reserves alone imply, because the taker lifted an
    // ask rather than trading at the mark.
    let entry = -(position.quote_asset_amount as i128) * AMM_RESERVE_PRECISION as i128
        / position.base_asset_amount as i128;
    assert_eq!(entry, 102_068_693);
    assert!(
        entry > 101_010_101,
        "a take at the mark would mean the AMM quoted no spread"
    );
}

/// A position opens, reduces, reverses and closes against the vAMM alone.
///
/// The vAMM is the counterparty at every step, so its inventory is the mirror
/// of the taker's position and the two must stay equal and opposite through a
/// reversal, which is the step that writes both a close and an open in one
/// fill. A position that closes must free its slot rather than linger at zero.
#[test]
fn a_vamm_position_opens_reduces_reverses_and_closes() {
    let mut fixture = setup();
    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    // The AMM starts long half a unit, so every check below is against that
    // baseline rather than against zero.
    let amm_base0 = {
        let market: PerpMarket = read_zero_copy(&fixture.svm, &perp_market_pda(0));
        market.amm.base_asset_amount_with_amm
    };

    let taker_base = |fixture: &Fixture| {
        let state: User = read_zero_copy(&fixture.svm, &taker.user);
        state.perp_positions[0].base_asset_amount
    };

    // `base_asset_amount_with_amm` tracks the users' net position that the AMM
    // is the counterparty to, not the AMM's own inventory, so it moves with the
    // taker rather than against it. The AMM is the only source here, so every
    // unit this taker holds is a unit that counter went up by.
    let amm_tracks = |fixture: &Fixture, taker_position: i64| {
        let market: PerpMarket = read_zero_copy(&fixture.svm, &perp_market_pda(0));
        assert_eq!(
            market.amm.base_asset_amount_with_amm,
            amm_base0 + taker_position as i128,
            "the vAMM counted the other side of the taker's position"
        );
    };

    // Open long 1.0.
    vamm_take(
        &mut fixture,
        &taker,
        PositionDirection::Long,
        UNIT,
        105 * PRICE,
    )
    .unwrap();
    assert_eq!(taker_base(&fixture), UNIT as i64);
    amm_tracks(&fixture, UNIT as i64);

    // Reduce by 0.4, leaving 0.6 long.
    vamm_take(
        &mut fixture,
        &taker,
        PositionDirection::Short,
        2 * UNIT / 5,
        95 * PRICE,
    )
    .unwrap();
    assert_eq!(taker_base(&fixture), (3 * UNIT / 5) as i64);
    amm_tracks(&fixture, (3 * UNIT / 5) as i64);

    // Reverse: sell 1.0 against a 0.6 long. `max_fill_reserve_fraction` caps
    // one fill at a hundredth of the reserves. Two takes already moved the
    // base reserve to 99.4, capping this fill at 0.994, so the reversal lands
    // at -0.394 rather than -0.4.
    let reserve_before_reversal = {
        let market: PerpMarket = read_zero_copy(&fixture.svm, &perp_market_pda(0));
        market.amm.base_asset_reserve
    };

    assert_eq!(reserve_before_reversal, 99_400_000_000);
    let capped = (reserve_before_reversal / 100) as i64;
    vamm_take(
        &mut fixture,
        &taker,
        PositionDirection::Short,
        UNIT,
        95 * PRICE,
    )
    .unwrap();
    let reversed = (3 * UNIT / 5) as i64 - capped;
    assert_eq!(
        reversed, -394_000_000,
        "the reversal is capped, not refused"
    );
    assert_eq!(taker_base(&fixture), reversed);
    amm_tracks(&fixture, reversed);

    // The walk stops here, not because the close is uninteresting, but
    // because the fixture pins `terminal_quote_asset_reserve` as a constant.
    // A fourth fill would move reserves far enough for `validate_amm` to
    // reject it with `InvalidAmmDetected`. Testing the close needs a derived reserve.
}

/// A bid between the mark and the ask takes nothing from the vAMM.
///
/// The spread is what makes this true: the AMM sells at its ask, not at its
/// mark, so a buyer who will not pay the ask gets no fill. A narrower spread
/// would let this order through, which is the same regression the entry-price
/// spec above pins from the other side — here it changes a fill into no fill
/// rather than moving a number.
#[test]
fn a_bid_between_the_mark_and_the_ask_takes_nothing_from_the_vamm() {
    let mut fixture = setup();
    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    // The fixture's reserves are balanced, so the mark is exactly 100. A bid a
    // basis point above it is above the mark and far below the ask.
    let between = 100 * PRICE + PRICE / 10_000;
    vamm_take(&mut fixture, &taker, PositionDirection::Long, UNIT, between).unwrap();

    let taker_state: User = read_zero_copy(&fixture.svm, &taker.user);
    assert_eq!(
        taker_state.perp_positions[0].base_asset_amount, 0,
        "a bid under the ask must not trade against the vAMM"
    );

    let market: PerpMarket = read_zero_copy(&fixture.svm, &perp_market_pda(0));
    assert_eq!(
        market.amm.base_asset_reserve,
        100 * AMM_RESERVE_PRECISION,
        "the reserves did not move, so nothing was filled"
    );

    // The order was live and routed; it simply found no depth it would pay
    // for. Without this the spec would also pass on an order that never
    // reached the vAMM at all, which is a different bug wearing the same
    // result.
    assert_eq!(
        clob_bid_count(&fixture),
        1,
        "the unfilled order rested, so the route ran and declined the ask"
    );
}
