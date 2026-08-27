//! End-to-end router fill: a taker's market order routed across a real CLOB
//! book (quote + execute CPIs through its `QuoterV0` entry), a DLOB maker,
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
        instructions::{
            CancelAllClobOrdersParams, CancelClobOrderParams, InitializeQuoterArgs,
            PlaceClobOrderParams, QuoterAccountMetaArg, UpdateQuoterAccountsArgs,
        },
        math::constants::{
            AMM_RESERVE_PRECISION, PEG_PRECISION, PRICE_PRECISION, QUOTE_PRECISION_I64,
            SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
        },
        state::{
            clob_crank::CrankCostUnitsV0,
            market_status::MarketStatus,
            oracle::OracleSource,
            perp_market::PerpMarket,
            prop_amm::{
                ClobCancelSides, ClobOrderRefV0, Direction, L3ArgsV0, L3ResponseV0, L3RowV0,
                QuoterCpiLeg, QuoterType, ResponsePointerV0,
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
/// 2% base spread, 10%/5% margin ratios. `clob_quoter` names the canonical
/// CLOB entry every router fill must carry (the mandatory baseline).
fn set_trading_perp_market(svm: &mut litesvm::LiteSVM, oracle: Pubkey, clob_quoter: Pubkey) {
    let mut market: PerpMarket = Zeroable::zeroed();
    market.market_index = 0;
    market.status = MarketStatus::Active;
    market.clob_quoter = anchor_lang::prelude::Pubkey::new_from_array(*clob_quoter.as_array());
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

/// Bytes to give a market account, for a book of at least `capacity` orders.
///
/// Deliberately generous rather than exact: `initialize_market_v0` derives the
/// arena's capacity from the account's length, so a header that grows costs
/// this book a few slots rather than silently sizing it wrong. Restating the
/// header's width here is what used to go stale.
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

/// Init a CLOB book with `place_authority` = the velocity signer, so every
/// placement must come through velocity.
fn init_clob_book(svm: &mut litesvm::LiteSVM, clob_admin: &Keypair) -> Pubkey {
    let market = Pubkey::new_unique();
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
    let (clob_authority, _) = clob_authority_pda();
    let ix = clob_ix(
        "initialize_market_v0",
        clob_market_config(0),
        vec![
            AccountMeta::new_readonly(clob_admin.pubkey(), true),
            AccountMeta::new_readonly(clob_authority, false),
            AccountMeta::new(market, false),
        ],
    );
    send(svm, clob_admin, ix, &[]).unwrap();
    market
}

/// Register + approve the CLOB book as market 0's CLOB quoter.
fn register_clob_quoter(
    svm: &mut litesvm::LiteSVM,
    admin: &Keypair,
    user: Pubkey,
    market: Pubkey,
) -> Pubkey {
    let (clob_authority, _) = clob_authority_pda();
    let quoter = quoter_pda(0, &clob_id(), &user);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            state: state_pda(),
            payer: admin.pubkey(),
            authority: admin.pubkey(),
            quoter,
            perp_market: perp_market_pda(0),
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
    for (leg, metas) in [
        (
            QuoterCpiLeg::Quote,
            vec![QuoterAccountMetaArg {
                pubkey: market,
                is_writable: true,
            }],
        ),
        (
            QuoterCpiLeg::Execute,
            vec![
                QuoterAccountMetaArg {
                    pubkey: market,
                    is_writable: true,
                },
                QuoterAccountMetaArg {
                    pubkey: clob_authority,
                    is_writable: false,
                },
            ],
        ),
    ] {
        let ix = Instruction {
            program_id: velocity_id(),
            accounts: velocity::accounts::UpdateQuoterAccounts {
                authority: admin.pubkey(),
                quoter,
            }
            .to_account_metas(None),
            data: velocity::instruction::UpdateQuoterAccounts {
                args: UpdateQuoterAccountsArgs {
                    leg,
                    index: 0,
                    metas,
                },
            }
            .data(),
        };
        send(svm, admin, ix, &[]).unwrap();
    }
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: admin.pubkey(),
            state: state_pda(),
            quoter,
            quoter_program: clob_id(),
            quoter_program_data: Some(program_data_pda(&clob_id())),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved { approved: true }.data(),
    };
    send(svm, admin, ix, &[]).unwrap();
    quoter
}

fn place_clob_order_ix(
    user: Pubkey,
    authority: &Keypair,
    quoter: Pubkey,
    clob_market: Pubkey,
    oracle: Pubkey,
    params: PlaceClobOrderParams,
) -> Instruction {
    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::PlaceClobOrder {
        state: state_pda(),
        user,
        authority: authority.pubkey(),
        quoter,
        clob_market,
        clob_program: clob_id(),
        clob_authority,
        instructions_sysvar: None,
    }
    .to_account_metas(None);
    // Margin maps: oracle, spot market, perp market.
    accounts.push(AccountMeta::new_readonly(oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceClobOrder { params }.data(),
    }
}

struct Fixture {
    svm: litesvm::LiteSVM,
    admin: Keypair,
    keeper: Keypair,
    oracle: Pubkey,
    clob_market: Pubkey,
    quoter: Pubkey,
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
    // The quoter PDA is derivable before the entry exists, so the market can
    // name its canonical CLOB from birth.
    let quoter = quoter_pda(0, &clob_id(), &clob_maker_user);
    set_trading_state(&mut svm, &admin.pubkey());
    set_oracle(&mut svm, oracle, (100 * PRICE_PRECISION) as i64, 10);
    set_trading_perp_market(&mut svm, oracle, quoter);
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
    let registered = register_clob_quoter(&mut svm, &admin, clob_maker_user, clob_market);
    assert_eq!(registered, quoter);

    Fixture {
        svm,
        admin,
        keeper,
        oracle,
        clob_market,
        quoter,
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
    user_stats: Pubkey,
    authority: Pubkey,
    order_refs: Vec<velocity::instructions::ForceCancelClobRefV0>,
) -> Instruction {
    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::ForceCancelClobOrders {
        state: state_pda(),
        authority,
        filler: filler_user,
        filler_stats,
        user: fixture.clob_maker_user,
        user_stats,
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
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
            market_index: 0,
            order_refs,
        }
        .data(),
    }
}

fn place_clob_ask(fixture: &mut Fixture, price: u64, size: u64) -> ClobOrderRefV0 {
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter,
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
/// eleven `Option`s, each a presence byte, in the order the book declares them.
fn set_clob_default_activation_delay(fixture: &mut Fixture, slots: u32) {
    let mut args = Vec::new();
    for _ in 0..4 {
        args.push(0u8); // tick size, step size, min order size, blocking min size
    }
    args.push(1u8);
    args.extend_from_slice(&slots.to_le_bytes());
    for _ in 0..6 {
        args.push(0u8); // max activation delay, grace, evict threshold, ceilings
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
}

#[test]
fn fast_activation_requires_the_flow_authority_attestation() {
    use velocity::state::state::HotRole;

    let mut fixture = setup();
    // The fixture's book has a zero default (every test placement is
    // "fast"); raise it through the book's own admin instruction so
    // below-default is expressible.
    set_clob_default_activation_delay(&mut fixture, 2);

    let place = |fixture: &Fixture, delay: Option<u32>, with_sysvar: bool| {
        let mut ix = place_clob_order_ix(
            fixture.clob_maker_user,
            &fixture.clob_maker_authority,
            fixture.quoter,
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
        if with_sysvar {
            // The optional slot is encoded as a program-id placeholder;
            // swap the real sysvar in.
            let placeholder = ix
                .accounts
                .iter()
                .rposition(|meta| meta.pubkey == velocity_id() && !meta.is_writable)
                .expect("optional placeholder present");
            ix.accounts[placeholder].pubkey = "Sysvar1nstructions1111111111111111111111111"
                .parse()
                .unwrap();
        }
        ix
    };

    // At-or-above the default: permissionless, exactly as before.
    let keeper = fixture.clob_maker_authority.insecure_clone();
    let default_delay_ix = place(&fixture, None, false);
    let at_default_ix = place(&fixture, Some(2), false);
    let fast_ix = place(&fixture, Some(0), true);
    send(&mut fixture.svm, &keeper, default_delay_ix, &[]).unwrap();
    send(&mut fixture.svm, &keeper, at_default_ix, &[]).unwrap();

    // Below the default with no flow authority configured: refused.
    let err = send(&mut fixture.svm, &keeper, fast_ix.clone(), &[]).unwrap_err();
    assert!(
        format!("{:?}", err.err).contains("6381"),
        "expected UnattestedFastActivation, got {:?}",
        err.err
    );

    // Configure the flow authority.
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

    // Still refused when the transaction is not co-signed.
    let err = send(&mut fixture.svm, &keeper, fast_ix.clone(), &[]).unwrap_err();
    assert!(format!("{:?}", err.err).contains("6381"));

    // Co-signed by the flow authority (a signer meta anywhere in the
    // transaction — here, appended to the placement's own accounts): the
    // fast activation is attested and lands.
    let mut ix = fast_ix;
    ix.accounts
        .push(AccountMeta::new_readonly(flow.pubkey(), true));
    send(&mut fixture.svm, &keeper, ix, &[&flow]).unwrap();
}

/// A keeper cannot route around the book by leaving its makers' accounts at
/// home.
///
/// This is the shape of the attack: whoever assembles the transaction also
/// runs liquidity of their own — a registered quoter, or a maker on the DLOB —
/// and wants the flow the book would have taken. Carrying the CLOB's registry
/// entry satisfies both `require_baseline` and the signed route, because both
/// check that an entry is *present*. Presence is not what decides whether a
/// book can trade: its liquidity is only reachable for users the transaction
/// loaded, so a live book with no maker accounts would otherwise be
/// indistinguishable from a dead one.
///
/// What stops it is the filler's obligation. The book reports the depth it was
/// holding, the taker did not sign this transaction, and the transaction had
/// room for the two accounts that maker needed. So the fill is refused, and the
/// assembler gets nothing rather than a smaller share.
#[test]
fn a_fill_that_leaves_out_a_reachable_book_maker_is_refused() {
    let mut fixture = setup();

    // The book: 1.0 @ 99, aged well past the grace window by fill time.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT);
    assert_eq!(clob_ask_count(&fixture), 1);

    // The assembler's own liquidity, priced worse than the book.
    let dlob_maker_authority = Keypair::new();
    let dlob_maker_user = Pubkey::new_unique();
    let dlob_maker_stats = Pubkey::new_unique();
    let mut dlob_order = Order::default();
    dlob_order.order_id = 1;
    dlob_order.status = OrderStatus::Open;
    dlob_order.order_type = OrderType::Limit;
    dlob_order.market_type = MarketType::Perp;
    dlob_order.market_index = 0;
    dlob_order.direction = PositionDirection::Short;
    dlob_order.post_only = true;
    dlob_order.base_asset_amount = UNIT;
    dlob_order.price = 100 * PRICE;
    set_user_account(
        &mut fixture.svm,
        dlob_maker_user,
        &trading_user(
            &dlob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(dlob_order),
        ),
    );
    set_user_stats_account(
        &mut fixture.svm,
        dlob_maker_stats,
        &dlob_maker_authority.pubkey(),
    );

    // A limit taker, so its remainder can rest rather than cancel.
    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Limit;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    taker_order.price = 105 * PRICE;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            100 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );
    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    // Well past the grace window: the book order went on at slot 10, so by
    // now no keeper can claim it had not heard about it.
    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // The whole attack: the DLOB maker is loaded, the book's maker is not.
    accounts.push(AccountMeta::new(dlob_maker_user, false));
    accounts.push(AccountMeta::new(dlob_maker_stats, false));
    // Carried, exactly as `require_baseline` and a signed route demand.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
        }
        .data(),
    };
    let err = send(&mut fixture.svm, &fixture.keeper, ix, &[]).unwrap_err();
    assert!(
        format!("{:?}", err.err).contains("6395"),
        "expected FillerOmittedReachableMaker, got {:?}",
        err.err
    );

    // Nothing moved, and the book still holds what it was holding.
    let dlob_maker: User = read_zero_copy(&fixture.svm, &dlob_maker_user);
    assert_eq!(dlob_maker.perp_positions[0].base_asset_amount, 0);
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(taker.perp_positions[0].base_asset_amount, 0);
    assert_eq!(clob_ask_count(&fixture), 1);
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
    let mut fixture = setup();
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT);

    let dlob_maker_authority = Keypair::new();
    let dlob_maker_user = Pubkey::new_unique();
    let dlob_maker_stats = Pubkey::new_unique();
    let mut dlob_order = Order::default();
    dlob_order.order_id = 1;
    dlob_order.status = OrderStatus::Open;
    dlob_order.order_type = OrderType::Limit;
    dlob_order.market_type = MarketType::Perp;
    dlob_order.market_index = 0;
    dlob_order.direction = PositionDirection::Short;
    dlob_order.post_only = true;
    dlob_order.base_asset_amount = UNIT;
    dlob_order.price = 100 * PRICE;
    set_user_account(
        &mut fixture.svm,
        dlob_maker_user,
        &trading_user(
            &dlob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(dlob_order),
        ),
    );
    set_user_stats_account(
        &mut fixture.svm,
        dlob_maker_stats,
        &dlob_maker_authority.pubkey(),
    );

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Limit;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    taker_order.price = 105 * PRICE;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            100 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    // The one difference from the case above: the taker's own authority signs,
    // and so it is also the filler's authority. A taker that signs chose the
    // account list, and no filler owes it anything.
    fixture
        .svm
        .airdrop(&taker_authority.pubkey(), 10_000_000_000)
        .unwrap();
    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&taker_authority.pubkey(), 0, None),
    );
    set_user_stats_account(&mut fixture.svm, filler_stats, &taker_authority.pubkey());

    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: taker_authority.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(dlob_maker_user, false));
    accounts.push(AccountMeta::new(dlob_maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
        }
        .data(),
    };
    send(&mut fixture.svm, &taker_authority, ix, &[]).unwrap();

    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(
        taker.perp_positions[0].base_asset_amount, UNIT as i64,
        "a taker that signs fills in full at the price present"
    );
}

/// A full transaction still owes the taker every maker it could have carried.
///
/// The filler here pads the account list with a user that fills nothing, which
/// is the way to reach the account cap without carrying the maker the book
/// wanted. Counting accounts alone would call that transaction full and let it
/// through, so the fill also asks whether every loaded user did something.
#[test]
fn a_padded_account_list_does_not_excuse_the_missing_maker() {
    let mut fixture = setup();
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT);

    // A DLOB maker the assembler owns, priced worse than the book.
    let dlob_maker_authority = Keypair::new();
    let dlob_maker_user = Pubkey::new_unique();
    let dlob_maker_stats = Pubkey::new_unique();
    let mut dlob_order = Order::default();
    dlob_order.order_id = 1;
    dlob_order.status = OrderStatus::Open;
    dlob_order.order_type = OrderType::Limit;
    dlob_order.market_type = MarketType::Perp;
    dlob_order.market_index = 0;
    dlob_order.direction = PositionDirection::Short;
    dlob_order.base_asset_amount = UNIT;
    dlob_order.price = 101 * PRICE;
    dlob_order.post_only = true;
    set_user_account(
        &mut fixture.svm,
        dlob_maker_user,
        &trading_user(
            &dlob_maker_authority.pubkey(),
            100 * SPOT_BALANCE_PRECISION_U64,
            Some(dlob_order),
        ),
    );
    set_user_stats_account(
        &mut fixture.svm,
        dlob_maker_stats,
        &dlob_maker_authority.pubkey(),
    );

    // The padding: a loaded user with no orders at all.
    let idle_authority = Keypair::new();
    let idle_user = Pubkey::new_unique();
    let idle_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        idle_user,
        &trading_user(
            &idle_authority.pubkey(),
            100 * SPOT_BALANCE_PRECISION_U64,
            None,
        ),
    );
    set_user_stats_account(&mut fixture.svm, idle_stats, &idle_authority.pubkey());

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Limit;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    taker_order.price = 102 * PRICE;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            100 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );
    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    fixture.svm.warp_to_slot(30);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        30,
    );

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(dlob_maker_user, false));
    accounts.push(AccountMeta::new(dlob_maker_stats, false));
    accounts.push(AccountMeta::new(idle_user, false));
    accounts.push(AccountMeta::new(idle_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
        }
        .data(),
    };
    // This transaction is nowhere near the account cap, so it fails on the
    // count first. The padding rule is what catches the same list once the cap
    // is reached, and `math::router` pins that arm directly.
    let err = send(&mut fixture.svm, &fixture.keeper, ix, &[]).unwrap_err();
    assert!(
        format!("{:?}", err.err).contains("6395"),
        "expected FillerOmittedReachableMaker, got {:?}",
        err.err
    );
    let idle: User = read_zero_copy(&fixture.svm, &idle_user);
    assert_eq!(idle.perp_positions[0].base_asset_amount, 0);
}

#[test]
fn router_fill_splits_across_clob_dlob_and_vamm_sources() {
    let mut fixture = setup();

    // CLOB ask 0.5 @ 99 through the adapter: aggregates reserved + margin
    // gated by velocity, book state on the CLOB.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let clob_maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(clob_maker.perp_positions[0].open_asks, -((UNIT / 2) as i64));
    assert_eq!(clob_maker.perp_positions[0].open_orders, 1);
    assert_eq!(clob_maker.open_orders, 1);
    assert_eq!(clob_ask_count(&fixture), 1);

    // DLOB maker: post-only ask 0.5 @ 100.
    let dlob_maker_authority = Keypair::new();
    let dlob_maker_user = Pubkey::new_unique();
    let dlob_maker_stats = Pubkey::new_unique();
    let mut dlob_order = Order::default();
    dlob_order.order_id = 1;
    dlob_order.status = OrderStatus::Open;
    dlob_order.order_type = OrderType::Limit;
    dlob_order.market_type = MarketType::Perp;
    dlob_order.market_index = 0;
    dlob_order.direction = PositionDirection::Short;
    dlob_order.post_only = true;
    dlob_order.base_asset_amount = UNIT / 2;
    dlob_order.price = 100 * PRICE;
    set_user_account(
        &mut fixture.svm,
        dlob_maker_user,
        &trading_user(
            &dlob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(dlob_order),
        ),
    );
    set_user_stats_account(
        &mut fixture.svm,
        dlob_maker_stats,
        &dlob_maker_authority.pubkey(),
    );

    // Taker: market long 1.0, limit 105.
    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Market;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    taker_order.price = 105 * PRICE;
    taker_order.auction_end_price = (105 * PRICE) as i64;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            100 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    // Keeper's filler user.
    let filler_user = Pubkey::new_unique();
    let filler_stats = Pubkey::new_unique();
    set_user_account(
        &mut fixture.svm,
        filler_user,
        &trading_user(&fixture.keeper.pubkey(), 0, None),
    );
    set_user_stats_account(&mut fixture.svm, filler_stats, &fixture.keeper.pubkey());

    // Stats for the clob maker (loaded as part of the maker map).
    let clob_maker_stats = Pubkey::new_unique();
    set_user_stats_account(
        &mut fixture.svm,
        clob_maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    // Fresh oracle + a couple of slots for the CLOB order to be past
    // placement.
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    // Maps: oracle, spot, perp.
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // Maker section: (user, stats) pairs.
    accounts.push(AccountMeta::new(dlob_maker_user, false));
    accounts.push(AccountMeta::new(dlob_maker_stats, false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(clob_maker_stats, false));
    // Quoter section: registry entry + its CPI accounts + program.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
        }
        .data(),
    };
    let meta = send(&mut fixture.svm, &fixture.keeper, ix, &[]).unwrap();

    // Taker fully filled: 0.5 @ 99 from the CLOB, 0.5 @ 100 from the DLOB
    // maker (the vAMM ask sits above 100 and gets nothing).
    let taker: User = read_zero_copy(&fixture.svm, &taker_user);
    assert_eq!(taker.perp_positions[0].base_asset_amount, UNIT as i64);
    assert_eq!(taker.perp_positions[0].open_bids, 0);
    assert_eq!(taker.orders[0].status, OrderStatus::Filled);

    // CLOB maker: short 0.5, aggregates fully unwound (fill + completed
    // order reported on the execute response).
    let clob_maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(
        clob_maker.perp_positions[0].base_asset_amount,
        -((UNIT / 2) as i64)
    );
    assert_eq!(clob_maker.perp_positions[0].open_asks, 0);
    assert_eq!(clob_maker.perp_positions[0].open_orders, 0);
    assert_eq!(clob_maker.open_orders, 0);
    assert_eq!(clob_ask_count(&fixture), 0);

    // DLOB maker: short 0.5, order fully filled.
    let dlob_maker: User = read_zero_copy(&fixture.svm, &dlob_maker_user);
    assert_eq!(
        dlob_maker.perp_positions[0].base_asset_amount,
        -((UNIT / 2) as i64)
    );
    assert_eq!(dlob_maker.orders[0].base_asset_amount_filled, UNIT / 2);

    println!(
        "CU — router fill across CLOB + DLOB + vAMM sources: {}",
        meta.compute_units_consumed
    );
}

/// Place a bid through velocity, so a sweep has both sides to take.
fn place_clob_bid(fixture: &mut Fixture, price: u64, size: u64) -> ClobOrderRefV0 {
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter,
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
    let (clob_authority, _) = clob_authority_pda();
    Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelAllClobOrders {
            state: state_pda(),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelAllClobOrders {
            params: CancelAllClobOrdersParams {
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
            fixture.quoter,
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
    assert_eq!(theirs.perp_positions[0].open_asks, -((2 * (UNIT / 5)) as i64));
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
    let (clob_authority, _) = clob_authority_pda();
    let per_order_cu: u64 = refs
        .iter()
        .map(|order_ref| {
            let ix = Instruction {
                program_id: velocity_id(),
                accounts: velocity::accounts::CancelClobOrder {
                    state: state_pda(),
                    perp_market: perp_market_pda(0),
                    user: fixture.clob_maker_user,
                    authority: fixture.clob_maker_authority.pubkey(),
                    quoter: fixture.quoter,
                    clob_market: fixture.clob_market,
                    clob_program: clob_id(),
                    clob_authority,
                }
                .to_account_metas(None),
                data: velocity::instruction::CancelClobOrder {
                    params: CancelClobOrderParams {
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

    let (clob_authority, _) = clob_authority_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelClobOrder {
            params: CancelClobOrderParams {
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
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelClobOrder {
            params: CancelClobOrderParams {
                market_index: 0,
                order_ref,
            },
        }
        .data(),
    };
    assert!(send(&mut fixture.svm, &fixture.clob_maker_authority, ix2, &[]).is_err());
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
        fixture.quoter,
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

    let (clob_authority, _) = clob_authority_pda();
    let cancel = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelClobOrder {
            params: CancelClobOrderParams {
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

/// The mandatory baseline: a router fill that omits the market's named CLOB
/// quoter entry fails, even though the vAMM alone could fill the order.
#[test]
fn router_fill_without_the_markets_clob_quoter_fails() {
    let mut fixture = setup();

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Market;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    taker_order.price = 105 * PRICE;
    taker_order.auction_end_price = (105 * PRICE) as i64;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            100 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
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

    // No quoter section at all: the fill must fail the baseline check.
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
        }
        .data(),
    };
    let err = send(&mut fixture.svm, &fixture.keeper, ix, &[]).expect_err("baseline must fail");
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
        fixture.quoter,
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

    let (clob_authority, _) = clob_authority_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CrankClobOrderRemoval {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            perp_market: perp_market_pda(0),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobRemoveExpired {
            market_index: 0,
            order_ref,
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

    let (clob_authority, _) = clob_authority_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CrankClobOrderRemoval {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            perp_market: perp_market_pda(0),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobEvict {
            market_index: 0,
            side: velocity::state::prop_amm::ClobSide::Ask,
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
/// books for every source — the CLOB (via a real `quote_v0` CPI), the DLOB
/// maker (bridged in-program), and the vAMM (quoted off a copy) — in fill
/// order, so the vAMM's last-look shading is already applied.
#[test]
fn quote_router_returns_verified_books_for_every_source() {
    let mut fixture = setup();

    // CLOB ask 0.5 @ 99 through the adapter.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    // DLOB maker: post-only ask 0.5 @ 100.
    let dlob_maker_authority = Keypair::new();
    let dlob_maker_user = Pubkey::new_unique();
    let dlob_maker_stats = Pubkey::new_unique();
    let mut dlob_order = Order::default();
    dlob_order.order_id = 1;
    dlob_order.status = OrderStatus::Open;
    dlob_order.order_type = OrderType::Limit;
    dlob_order.market_type = MarketType::Perp;
    dlob_order.market_index = 0;
    dlob_order.direction = PositionDirection::Short;
    dlob_order.post_only = true;
    dlob_order.base_asset_amount = UNIT / 2;
    dlob_order.price = 100 * PRICE;
    set_user_account(
        &mut fixture.svm,
        dlob_maker_user,
        &trading_user(
            &dlob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(dlob_order),
        ),
    );
    set_user_stats_account(
        &mut fixture.svm,
        dlob_maker_stats,
        &dlob_maker_authority.pubkey(),
    );

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
        data: velocity::instruction::InitializeRouterQuoteBuffer { market_index: 0 }.data(),
    };
    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    let (clob_authority, _) = clob_authority_pda();
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
    // Maker section: the DLOB maker (read-only is fine for a quote).
    accounts.push(AccountMeta::new_readonly(dlob_maker_user, false));
    accounts.push(AccountMeta::new_readonly(dlob_maker_stats, false));
    // Quoter section: the CLOB entry + its CPI accounts.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                market_index: 0,
                direction: Direction::Long,
                size: 2 * UNIT,
                quoter_count: 1,
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

    // Three sources, in fill order: CLOB quoter, DLOB order, vAMM last.
    assert_eq!(buffer.source_count, 3, "clob + dlob + vamm");
    let sources = &buffer.sources[..3];
    use velocity::state::router_quote::QuotedSourceKind;
    assert_eq!(sources[0].kind, QuotedSourceKind::Quoter);
    assert_eq!(sources[0].key, fixture.quoter);
    assert_eq!(sources[1].kind, QuotedSourceKind::DlobOrder);
    assert_eq!(sources[1].key, dlob_maker_user);
    assert_eq!(sources[2].kind, QuotedSourceKind::Vamm);

    // The CLOB's book came back through a real quote_v0 CPI: 0.5 @ 99.
    let clob_levels = &buffer.levels[0][..sources[0].level_count as usize];
    assert_eq!(clob_levels.len(), 1);
    assert_eq!(clob_levels[0].price, 99 * PRICE);
    assert_eq!(clob_levels[0].size, UNIT / 2);

    // The DLOB maker's resting order: 0.5 @ 100.
    let dlob_levels = &buffer.levels[1][..sources[1].level_count as usize];
    assert_eq!(dlob_levels.len(), 1);
    assert_eq!(dlob_levels[0].price, 100 * PRICE);
    assert_eq!(dlob_levels[0].size, UNIT / 2);

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

    // A DLOB order is one row against its maker, and velocity knows that
    // without asking anyone.
    let dlob_rows = &buffer.rows[sources[1].row_start as usize..][..sources[1].row_len as usize];
    assert_eq!(dlob_rows.len(), 1);
    assert_eq!(dlob_rows[0].authority, dlob_maker_authority.pubkey());
    assert_eq!(dlob_rows[0].order_id, 1);

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

/// The protocol-owned filler: a `User`/`UserStats` pair whose authority is
/// the velocity signer PDA, at the derivation the resolvers stage.
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

/// Cost units to sync with when the test does not care what the sync costs.
const ANY_SYNC_COST_UNITS: u32 = 20_000;

/// Cost units to attach with when the test does not care what a crank costs.
/// Nonzero is all the attach requires; under the flat-per-signature rails the
/// fixture starts on, the figure does not reach the payment.
const ANY_CRANK_COST_UNITS: CrankCostUnitsV0 = CrankCostUnitsV0 {
    removal: 30_000,
    cross: 180_000,
    taker_origin_cross: 190_000,
    trigger: 40_000,
    liquidation: 120_000,
    force_cancel: 60_000,
    refill: 30_000,
};

/// Attach the CLOB to the market through the admin ix — which also stands up
/// the crank conditions account, so no separate init exists to call.
///
/// Takes the lamport payment the test wants to see, and gets it by putting the
/// exchange on the flat-per-signature fee model at that price. Under that model
/// every crank is worth one signature, whatever it requests, which is what lets
/// a test assert one number.
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
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority: clob_authority_pda().0,
            crank_conditions: conditions,
            treasury: crank_treasury_pda(),
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdatePerpMarketClobQuoter {
            crank_cost_units,
            expire_fallback_slots: 100,
            min_cross_surplus,
        }
        .data(),
    };
    let admin = fixture.admin.insecure_clone();
    let meta = send(&mut fixture.svm, &admin, ix, &[]).unwrap();
    // The book reports where its block sits; the registrant does not derive it.
    fixture.crank_block_offset = u32::from_le_bytes(meta.return_data.data[..4].try_into().unwrap());
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
            quoter: fixture.quoter,
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
            quoter: fixture.quoter,
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
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
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
        fixture.quoter,
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
                quoter: fixture.quoter,
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
        fixture.quoter,
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
            fixture.quoter,
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
        data: velocity::instruction::RefillCrankReservoir { market_index: 0 }.data(),
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
        fixture.quoter,
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
    assert_eq!(resolved.accounts.len(), 11);
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
            fixture.quoter,
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

fn trigger_clob_order_ix(
    fixture: &Fixture,
    order_id: u32,
    filler: Pubkey,
    filler_stats: Pubkey,
    maker_stats: Pubkey,
) -> Instruction {
    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::TriggerClobOrder {
        trigger_conditions: None,
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler,
        filler_stats,
        user: fixture.clob_maker_user,
        user_stats: maker_stats,
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::TriggerClobOrder {
            market_index: 0,
            order_id,
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
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
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
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
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

    // A second trigger attempt on the placed slot fails.
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("already placed");
    assert!(format!("{:?}", err.meta.logs).contains("already rests on the CLOB"));

    // Evict (fixture soft cap = 1): the shadow re-arms in the same tx,
    // edge-gated on a recross.
    let (clob_authority, _) = clob_authority_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CrankClobOrderRemoval {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            perp_market: perp_market_pda(0),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobEvict {
            market_index: 0,
            side: velocity::state::prop_amm::ClobSide::Ask,
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
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
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
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
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
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
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
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
            crank_conditions: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::CrankClobRemoveExpired {
            market_index: 0,
            order_ref: velocity::state::prop_amm::ClobOrderRefV0 {
                node_index,
                order_id: clob_order_id,
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
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
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
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
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
    let (clob_authority, _) = clob_authority_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            perp_market: perp_market_pda(0),
            user: fixture.clob_maker_user,
            authority: maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelClobOrder {
            params: CancelClobOrderParams {
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
        fixture.quoter,
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
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
        accounts.push(AccountMeta::new(maker_stats, false));
        accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_authority_pda().0, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::CrankCrossMatch {
                market_index: 0,
                size: UNIT,
                buy_quoter_index: 0,
                sell_quoter_index: 0,
            }
            .data(),
        }
    };
    let ix = cross_ix();
    let keeper = fixture.keeper.insecure_clone();
    let err = send(&mut fixture.svm, &keeper, ix.clone(), &[]).unwrap_err();
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
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
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
        fixture.quoter,
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
        }
        .to_account_metas(None);
        // Maps, then the maker pair, then the quoter section.
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
        accounts.push(AccountMeta::new(maker_stats, false));
        accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_authority_pda().0, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::CrankCrossMatch {
                market_index: 0,
                size: UNIT,
                buy_quoter_index: 0,
                sell_quoter_index: 0,
            }
            .data(),
        }
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send(&mut fixture.svm, &keeper, cross_ix(), &[]).unwrap();
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
    let err = send(&mut fixture.svm, &keeper, cross_ix(), &[]).expect_err("no cross left");
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
        fixture.quoter,
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

/// The authority-wide latch is grounds by itself. Relay proves one
/// subaccount below its floor and trips it; from then on every subaccount is
/// barred from risk-increasing activity, so the orders this one is resting
/// cannot legally fill and anyone may reclaim them — without re-deriving the
/// breach, which is what the latch exists to record.
#[test]
fn a_tripped_equity_breaker_is_grounds_on_its_own() {
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
            maker_stats,
            fixture.keeper.pubkey(),
            vec![ask_ref(order_ref)],
        )
    };

    // Control: latch clear, account healthy, so there is nothing to do.
    let ix = build(&fixture);
    send(&mut fixture.svm, &keeper, ix, &[]).expect("healthy is a no-op");
    assert_eq!(clob_ask_count(&fixture), 1);

    // Latch set: the same call now reclaims the order.
    set_tripped_user_stats(
        &mut fixture.svm,
        maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );
    let ix = build(&fixture);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    assert_eq!(clob_ask_count(&fixture), 0);
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // The book's maker, so its fill can be settled.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    // The route: the market's CLOB and the accounts its CPI resolves against.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // The latched maker's pair: the clamp reads it, and the clear writes it.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
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

/// A partially-filled place-and-take limit rests its remainder on the CLOB
/// when the caller passes the CLOB accounts: the taker fills half against a
/// DLOB maker, the leftover half leaves `User.orders` and becomes a resting
/// book bid, aggregates reserved.
#[test]
fn place_and_take_rests_the_remainder_on_the_clob() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();

    // A DLOB maker with a post-only ask 0.5 @ 99 for the take leg.
    let dlob_maker_authority = Keypair::new();
    let dlob_maker_user = Pubkey::new_unique();
    let dlob_maker_stats = Pubkey::new_unique();
    let mut dlob_order = Order::default();
    dlob_order.order_id = 1;
    dlob_order.status = OrderStatus::Open;
    dlob_order.order_type = OrderType::Limit;
    dlob_order.market_type = MarketType::Perp;
    dlob_order.market_index = 0;
    dlob_order.direction = PositionDirection::Short;
    dlob_order.post_only = true;
    dlob_order.base_asset_amount = UNIT / 2;
    dlob_order.price = 99 * PRICE;
    set_user_account(
        &mut fixture.svm,
        dlob_maker_user,
        &trading_user(
            &dlob_maker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(dlob_order),
        ),
    );
    set_user_stats_account(
        &mut fixture.svm,
        dlob_maker_stats,
        &dlob_maker_authority.pubkey(),
    );

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

    let (clob_authority, _) = clob_authority_pda();
    // The V1 route: v0's account list is frozen at its pre-CLOB shape, so
    // resting the remainder on the book is a distinct endpoint whose CLOB
    // accounts are required.
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(dlob_maker_user, false));
    accounts.push(AccountMeta::new(dlob_maker_stats, false));
    // The quoter section: v1 routes, so the taker names the entries it wants
    // consulted. The market's canonical CLOB is mandatory, and its registered
    // CPI accounts have to be resolvable from this list even though the
    // remainder-placement leg names them too — same locks, one index byte.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
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
            params,
            success_condition: Some(0),
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
        "took the maker's half"
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
/// It used to keep DLOB behaviour on this route while the keeper route rested
/// it, so the same order ended up in a different place depending on which one
/// reached it. That is not a fallback once the DLOB is gone — it is an order
/// nothing will fill. A market order's own `price` is zero, so it rests at
/// `auction_end_price`, which is safe because a migrated remainder is
/// taker-origin: nobody can take it at that bound while a counterparty
/// crosses it, and the cross settles at the counterparty's price.
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            params: OrderParams {
                order_type: OrderType::Market,
                market_type: MarketType::Perp,
                direction: PositionDirection::Long,
                base_asset_amount: UNIT,
                // A market order's bound: the worst fill it agreed to, and
                // the only price its remainder can rest at.
                auction_end_price: Some((101 * PRICE) as i64),
                market_index: 0,
                post_only: PostOnlyParam::None,
                ..OrderParams::default()
            },
            success_condition: None,
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
}

// ---------------------------------------------------------------------------
// Midpoint spline quoter: a maker's PDA instance of the midpoint program,
// registered as a Custom entry, quoting offsets around a maker-fed mid.
// ---------------------------------------------------------------------------

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
/// instance PDA (velocity signer as execute authority), spline levels around
/// a $100 mid, and the approved Custom registry entry whose CPI legs carry
/// the instance + instructions sysvar (+ the signer slot on execute).
fn setup_midpoint_maker(fixture: &mut Fixture, deposit: u64, side_size: u64) -> MidpointMaker {
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
    // The instance's execute authority is the signer velocity CPIs *this
    // entry* as, so the entry key has to be known before the instance is
    // created. It is a PDA, so it is derivable ahead of registration.
    let entry = quoter_pda(0, &midpoint_id(), &user);
    let (quoter_signer, _) = quoter_signer_pda(&entry);
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
    // u64, staleness u64, tick u64, step u64, min u64, attested bool.
    let instance = midpoint_instance_pda(&authority.pubkey());
    let mut data = ix_discriminator("initialize_quoter_v0").to_vec();
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&UNIT.to_le_bytes());
    data.extend_from_slice(&1_000u64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.extend_from_slice(&1u64.to_le_bytes());
    data.push(0);
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
            AccountMeta::new_readonly(quoter_signer, false),
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

    for (leg, metas) in [
        (
            QuoterCpiLeg::Quote,
            vec![
                QuoterAccountMetaArg {
                    pubkey: instance,
                    is_writable: true,
                },
                QuoterAccountMetaArg {
                    pubkey: instructions_sysvar(),
                    is_writable: false,
                },
                // The midpoint reads the live flow authority out of
                // velocity's State rather than trusting a local copy.
                QuoterAccountMetaArg {
                    pubkey: state_pda(),
                    is_writable: false,
                },
            ],
        ),
        (
            QuoterCpiLeg::Execute,
            vec![
                QuoterAccountMetaArg {
                    pubkey: instance,
                    is_writable: true,
                },
                QuoterAccountMetaArg {
                    pubkey: quoter_signer,
                    is_writable: false,
                },
                QuoterAccountMetaArg {
                    pubkey: instructions_sysvar(),
                    is_writable: false,
                },
                QuoterAccountMetaArg {
                    pubkey: state_pda(),
                    is_writable: false,
                },
            ],
        ),
    ] {
        let ix = Instruction {
            program_id: velocity_id(),
            accounts: velocity::accounts::UpdateQuoterAccounts {
                authority: authority.pubkey(),
                quoter: entry,
            }
            .to_account_metas(None),
            data: velocity::instruction::UpdateQuoterAccounts {
                args: UpdateQuoterAccountsArgs {
                    leg,
                    index: 0,
                    metas,
                },
            }
            .data(),
        };
        send(&mut fixture.svm, &fixture.keeper, ix, &[&authority]).unwrap();
    }
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: fixture.admin.pubkey(),
            state: state_pda(),
            quoter: entry,
            quoter_program: midpoint_id(),
            quoter_program_data: Some(program_data_pda(&midpoint_id())),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved { approved: true }.data(),
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
    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Market;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = size;
    taker_order.price = 105 * PRICE;
    taker_order.auction_end_price = (105 * PRICE) as i64;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // Maker section: the midpoint's quoted user.
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    // Quoter section: baseline CLOB entry + midpoint entry, then the union
    // of their CPI accounts and programs.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new_readonly(maker.entry, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(maker.instance, false));
    // The midpoint entry's own CPI signer, which its execute leg registered and
    // its `execute_authority` is set to. Each entry has its own, so the fill has
    // to carry the one belonging to the entry it routes through.
    accounts.push(AccountMeta::new_readonly(
        quoter_signer_pda(&maker.entry).0,
        false,
    ));
    accounts.push(AccountMeta::new_readonly(instructions_sysvar(), false));
    // Velocity resolves each quoter's CPI metas from this trailing map, and
    // the midpoint's quote/execute legs name velocity's State (they read the
    // live flow authority from it); the fill's own named `state` account is
    // not part of the map, so it has to ride here too.
    accounts.push(AccountMeta::new_readonly(state_pda(), false));
    accounts.push(AccountMeta::new_readonly(midpoint_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
        }
        .data(),
    };
    // Three quoter CPI legs + the vAMM outgrow the 200k default.
    let meta = send_with_ixs(
        &mut fixture.svm,
        &fixture.keeper,
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
    assert_eq!(taker.orders[0].status, OrderStatus::Filled);

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
    let clob_maker_stats = Pubkey::new_unique();
    set_user_stats_account(
        &mut fixture.svm,
        clob_maker_stats,
        &fixture.clob_maker_authority.pubkey(),
    );

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Market;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT + UNIT / 2;
    taker_order.price = 105 * PRICE;
    taker_order.auction_end_price = (105 * PRICE) as i64;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        instructions_sysvar: Some(instructions_sysvar()),
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
    // Quoter section: both entries + the union of their CPI accounts.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new_readonly(maker.entry, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(maker.instance, false));
    // The midpoint entry's own CPI signer, which its execute leg registered and
    // its `execute_authority` is set to. Each entry has its own, so the fill has
    // to carry the one belonging to the entry it routes through.
    accounts.push(AccountMeta::new_readonly(
        quoter_signer_pda(&maker.entry).0,
        false,
    ));
    accounts.push(AccountMeta::new_readonly(instructions_sysvar(), false));
    // Velocity resolves each quoter's CPI metas from this trailing map, and
    // the midpoint's quote/execute legs name velocity's State (they read the
    // live flow authority from it); the fill's own named `state` account is
    // not part of the map, so it has to ride here too.
    accounts.push(AccountMeta::new_readonly(state_pda(), false));
    accounts.push(AccountMeta::new_readonly(midpoint_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
        }
        .data(),
    };
    let meta = send_with_ixs(
        &mut fixture.svm,
        &fixture.keeper,
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
    assert_eq!(taker.orders[0].status, OrderStatus::Filled);

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
    // The declaration is a config change: approval resets, admin re-vets.
    let entry: velocity::state::prop_amm::QuoterV0 = read_zero_copy(&fixture.svm, &maker.entry);
    assert!(!entry.is_approved);
    assert_eq!(entry.watch_account.to_bytes(), maker.instance.to_bytes());
    assert_eq!(entry.watch_offset, 136);
    assert_eq!(entry.watch_len, 16);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: fixture.admin.pubkey(),
            state: state_pda(),
            quoter: maker.entry,
            quoter_program: midpoint_id(),
            quoter_program_data: Some(program_data_pda(&midpoint_id())),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved { approved: true }.data(),
    };
    let admin = fixture.admin.insecure_clone();
    send(&mut fixture.svm, &admin, ix, &[]).unwrap();
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
            clob_quoter: fixture.quoter,
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
            expire_fallback_slots: 100,
        }
        .data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    cross_conditions
}

/// Run the generic resolver the way a turner does. Its account list is
/// exactly what the attach registered: conditions, book, state, entry, user,
/// the CLOB's own entry and program, then the entry's registered quote
/// surface + program.
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
        quoter: maker.entry,
        user: maker.user,
        clob_quoter: fixture.quoter,
        clob_program: clob_id(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new(maker.instance, false));
    // The midpoint entry's own CPI signer, which its execute leg registered and
    // its `execute_authority` is set to. Each entry has its own, so the fill has
    // to carry the one belonging to the entry it routes through.
    accounts.push(AccountMeta::new_readonly(
        quoter_signer_pda(&maker.entry).0,
        false,
    ));
    accounts.push(AccountMeta::new_readonly(instructions_sysvar(), false));
    // Velocity resolves each quoter's CPI metas from this trailing map, and
    // the midpoint's quote/execute legs name velocity's State (they read the
    // live flow authority from it); the fill's own named `state` account is
    // not part of the map, so it has to ride here too.
    accounts.push(AccountMeta::new_readonly(state_pda(), false));
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

    // Midpoint asks 2.0 @ 100.1 (mid 100 + 10bps).
    let maker = setup_midpoint_maker(&mut fixture, 10_000 * SPOT_BALANCE_PRECISION_U64, 2 * UNIT);
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
    // The resolver list (shared scratch, the named accounts, the CLOB's own
    // entry and program, then the entry's registered quote surface) is stored
    // once in the relay block's built-in region; every condition points at it
    // indirectly.
    //
    // The midpoint's quote leg names velocity's State, which it reads the live
    // flow authority from. The CLOB entry and program are there because the
    // resolver quotes the book through the registry too, rather than reading
    // its account.
    assert_eq!(conditions[QUOTER_CROSS_WATCH].resolvers().count, 12);
    assert_eq!(acct.relay.resolver_refs().len(), 12);

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
        fixture.quoter,
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

    let resolved = run_quoter_cross_resolver(&mut fixture, &maker, cross_conditions)
        .expect("crossed books stage a crank");
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let payout_before = fixture.svm.get_balance(&payout).unwrap();
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

// ---------------------------------------------------------------------------
// Trigger orders as relay conditions: per-user OnValueCross watches synced
// from live orders, resolvers staging the dual-mode trigger cranks.
// ---------------------------------------------------------------------------

fn user_conditions_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_conditions", user.as_ref()], &velocity_id()).0
}

fn sync_trigger_conditions(fixture: &mut Fixture, user: Pubkey, market_conditions: Pubkey) {
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
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::SyncTriggerConditions {}.data(),
    };
    let keeper = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
}

fn run_trigger_resolver(
    fixture: &mut Fixture,
    user: Pubkey,
    clob_path: bool,
) -> Option<velocity::relay_spec::ResolvedCrankV0> {
    let conditions = user_conditions_pda(&user);
    let accounts = velocity::accounts::ResolveTriggerOrder {
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
        data: if clob_path {
            velocity::instruction::ResolveTriggerClobOrder {}.data()
        } else {
            velocity::instruction::ResolveTriggerOrder {}.data()
        },
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

/// The whole loop, turner-shaped: sync writes an OnValueCross condition at
/// the trigger's raw-oracle threshold, the resolver reports no work while
/// the price sits short, stages the dual-mode `trigger_order` once it
/// crosses, and the staged executor lands unsigned — order triggered,
/// keeper paid from the market reservoir, the fired slot released so the
/// level-triggered wake goes quiet.
#[test]
fn trigger_relay_conditions_fire_an_armed_trigger_unsigned() {
    use velocity::state::user_conditions::{UserConditionsV0, TRIGGER_SLOT_BASE, USER_CONDITIONS};

    let mut fixture = setup();
    const PAYMENT: u64 = 25_000;
    let market_conditions = init_crank_conditions(&mut fixture, PAYMENT);
    set_protocol_user(&mut fixture.svm);
    fixture
        .svm
        .airdrop(&market_conditions, 1_000_000_000)
        .unwrap();

    // A user with an armed stop: trigger-market sell 1.0 when the oracle
    // climbs to 105.
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

    sync_trigger_conditions(&mut fixture, user, market_conditions);

    // The sync wrote a value watch at the trigger threshold (lazer exponent
    // 6 = PRICE_PRECISION, so raw == trigger) with the plain trigger
    // executor, and captured the margin-map section.
    let conditions = user_conditions_pda(&user);
    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let (header, block) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    // One block per user: liquidation slots first, then the trigger slots.
    assert_eq!(header.num_conditions as usize, USER_CONDITIONS);
    let trig = &block[TRIGGER_SLOT_BASE..];
    assert_eq!(
        value_cross(&trig[0]),
        (fixture.oracle.to_bytes(), 8, 8, (105 * PRICE) as i64, 0)
    );
    assert_eq!(trig[0].min_payment(), PAYMENT);
    assert!(!trig[1].is_active(), "one armed trigger, one live slot");
    assert_eq!(acct.trigger_slots[0].order_id, 7);
    // The shared list: the resolver's four named accounts (scratch first),
    // then the map.
    assert_eq!(acct.relay.resolver_refs().len(), 4 + 3);

    // Below the trigger: the resolver reports no work.
    fixture.svm.warp_to_slot(12);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (100 * PRICE_PRECISION) as i64,
        12,
    );
    assert!(run_trigger_resolver(&mut fixture, user, false).is_none());

    // Crossed: the resolver stages the executor; a turner-shaped unsigned
    // submission triggers the order and pays the keeper from the reservoir.
    fixture.svm.warp_to_slot(13);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (106 * PRICE_PRECISION) as i64,
        13,
    );
    let resolved =
        run_trigger_resolver(&mut fixture, user, false).expect("crossed threshold stages");
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    let payout_before = fixture.svm.get_balance(&payout).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::TriggerOrder::DISCRIMINATOR,
        payout,
    );

    let triggered: User = read_zero_copy(&fixture.svm, &user);
    assert!(triggered.orders[0].triggered());
    assert_eq!(
        fixture.svm.get_balance(&payout).unwrap(),
        payout_before + PAYMENT
    );
    // The fired slot went quiet — the level-triggered wake must not spin.
    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let (_, block) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    assert!(!block[0].is_active());
    assert_eq!(acct.trigger_slots[0].order_id, 0);
}

/// A trigger-limit on a market with a vetted CLOB syncs to the
/// `trigger_clob_order` executor path.
#[test]
fn trigger_limit_sync_targets_the_clob_executor() {
    use velocity::state::user_conditions::{UserConditionsV0, TRIGGER_SLOT_BASE, USER_CONDITIONS};

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

    sync_trigger_conditions(&mut fixture, user, market_conditions);

    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &user_conditions_pda(&user));
    let (_, block) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    let trig = &block[TRIGGER_SLOT_BASE..];
    assert_eq!(value_cross(&trig[0]).4, 1, "Below trigger watches downward");
    // The CLOB path shows up as the resolver the condition names — the
    // executor it stages (`trigger_clob_order`) is that resolver's answer,
    // asserted where the payload is read.
    assert_eq!(
        trig[0].crank_spec().resolver_disc,
        velocity::instruction::ResolveTriggerClobOrder::DISCRIMINATOR
    );
    assert_eq!(
        acct.trigger_slots[0].quoter.to_bytes(),
        fixture.quoter.to_bytes()
    );
    assert_eq!(
        acct.trigger_slots[0].clob_market.to_bytes(),
        fixture.clob_market.to_bytes()
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
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
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
    assert_eq!(map.len(), 5, "oracle, spot, perp, conditions, quoter");
    assert_eq!(map[0].address, fixture.oracle.to_bytes());
    assert_eq!(map[1].address, spot_market_pda(0).to_bytes());
    assert_eq!(map[2].address, perp_market_pda(0).to_bytes());
    assert_eq!(map[3].address, market_conditions.to_bytes());
    assert_eq!(map[4].address, fixture.quoter.to_bytes());

    // And the proof it parses: the staged trigger executor lands.
    fixture.svm.warp_to_slot(13);
    set_oracle(
        &mut fixture.svm,
        fixture.oracle,
        (106 * PRICE_PRECISION) as i64,
        13,
    );
    let resolved =
        run_trigger_resolver(&mut fixture, user, false).expect("crossed threshold stages");
    let payout = Pubkey::new_unique();
    fixture.svm.airdrop(&payout, 1_000_000_000).unwrap();
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::TriggerOrder::DISCRIMINATOR,
        payout,
    );
    let triggered: User = read_zero_copy(&fixture.svm, &user);
    assert!(triggered.orders[0].triggered());
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
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
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
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
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
    // Args: the target perp market.
    assert_eq!(resolved.data, 0u16.to_le_bytes().to_vec());
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
    use velocity::state::user_conditions::{UserConditionsV0, TRIGGER_SLOT_BASE, USER_CONDITIONS};

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

/// A filler cannot quietly drop a quoter the taker signed for.
///
/// The order carries a digest of the route its signer chose, so the route a
/// filler claims is pinned to that one, and every entry in it has to be
/// carried by the fill. That is what makes a signed route a constraint on the
/// filler rather than a suggestion — a keeper has no reason to prefer the
/// taker's sources over its own, so the chain holds it to them.
#[test]
fn a_fill_must_carry_every_quoter_the_taker_signed_for() {
    use velocity::state::order_params::route_digest;

    let mut fixture = setup();
    let maker = setup_midpoint_maker(&mut fixture, 10_000 * SPOT_BALANCE_PRECISION_U64, UNIT);

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Market;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    taker_order.price = 105 * PRICE;
    taker_order.auction_end_price = (105 * PRICE) as i64;
    // Signed with a route naming the midpoint entry — what a swift message's
    // `route` becomes once `place_signed_msg_taker_order` stamps it.
    let route = vec![maker.entry];
    taker_order.route_digest = route_digest(&route);
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
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

    let (clob_authority, _) = clob_authority_pda();
    // A fill that carries only the mandatory CLOB baseline: the signed
    // midpoint entry is nowhere in the transaction.
    let fill_ix = |claimed: Vec<Pubkey>| {
        let mut accounts = velocity::accounts::FillOrder {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: taker_user,
            user_stats: taker_stats,
            instructions_sysvar: Some(instructions_sysvar()),
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_authority, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::FillPerpOrder {
                order_id: Some(1),
                _maker_order_id: None,
                signed_route: claimed,
            }
            .data(),
        }
    };

    // Claiming the real route while omitting its quoter: refused.
    let err = send_with_ixs(
        &mut fixture.svm,
        &fixture.keeper,
        &[compute_unit_limit_ix(400_000), fill_ix(route.clone())],
        &[],
    )
    .expect_err("the signed quoter is absent from the transaction");
    assert!(
        format!("{:?}", err.meta.logs).contains("SignedRouteEntryMissing"),
        "unexpected: {:?}",
        err.meta.logs
    );

    // Claiming no route at all, to dodge the presence check: also refused —
    // the claim no longer digests to what the order was signed with.
    let err = send_with_ixs(
        &mut fixture.svm,
        &fixture.keeper,
        &[compute_unit_limit_ix(400_000), fill_ix(vec![])],
        &[],
    )
    .expect_err("an empty claim does not match the order's digest");
    assert!(
        format!("{:?}", err.meta.logs).contains("SignedRouteMismatch"),
        "unexpected: {:?}",
        err.meta.logs
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
fn place_and_make_v1_rests_the_unmatched_remainder_on_the_book() {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let mut fixture = setup();

    // A taker resting a long for half a unit at $100 — the order the maker
    // will be matched against.
    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Market;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT / 2;
    taker_order.price = 101 * PRICE;
    taker_order.auction_end_price = (101 * PRICE) as i64;
    set_user_account(
        &mut fixture.svm,
        taker_user,
        &trading_user(
            &taker_authority.pubkey(),
            10_000 * SPOT_BALANCE_PRECISION_U64,
            Some(taker_order),
        ),
    );
    set_user_stats_account(&mut fixture.svm, taker_stats, &taker_authority.pubkey());

    // The maker: quotes a full unit, so half is left after the taker's half.
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::PlaceAndMakeV1 {
        state: state_pda(),
        user: maker_user,
        user_stats: maker_stats,
        taker: taker_user,
        taker_stats,
        authority: maker_authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndMakePerpOrderV1 {
            params: OrderParams {
                order_type: OrderType::Limit,
                market_type: MarketType::Perp,
                direction: PositionDirection::Short,
                base_asset_amount: UNIT,
                price: 100 * PRICE,
                market_index: 0,
                post_only: PostOnlyParam::MustPostOnly,
                // IOC is a bit flag on the params, not a field.
                bit_flags: velocity::state::order_params::OrderParamsBitFlag::ImmediateOrCancel
                    as u8,
                ..OrderParams::default()
            },
            taker_order_id: 1,
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

    // The maker sold the taker's half, and the other half is resting as a
    // CLOB ask rather than having been cancelled.
    let maker: User = read_zero_copy(&fixture.svm, &maker_user);
    assert_eq!(
        maker.perp_positions[0].base_asset_amount,
        -((UNIT / 2) as i64),
        "matched the taker's half"
    );
    assert!(
        maker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "nothing rests in User.orders — IOC still holds there"
    );
    assert_eq!(
        maker.perp_positions[0].open_asks,
        -((UNIT / 2) as i64),
        "the remainder is reserved against the book"
    );
    assert_eq!(maker.open_orders, 1);
    assert_eq!(clob_ask_count(&fixture), 1);
}

/// A keeper fill's restable remainder migrates to the book.
///
/// This is the case `place_and_take_v1` could not reach: a signed-message
/// taker order cannot be IOC, so its leftover rests — and a keeper-driven fill
/// held no CLOB accounts, so it rested on the DLOB. v1 gives the fill those
/// accounts and the remainder lands on the book, where the activation window
/// and the cross give it counterparties.
#[test]
fn fill_v1_migrates_a_restable_remainder_to_the_book() {
    use velocity::state::order_params::PostOnlyParam;

    let mut fixture = setup();
    let _ = PostOnlyParam::None;

    // A taker resting a limit long for a full unit at $100, with only half a
    // unit of CLOB liquidity to take.
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
    place_clob_ask(&mut fixture, 100 * PRICE, UNIT / 2);

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Limit;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    taker_order.price = 100 * PRICE;
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        Some(taker_order),
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrderV1 {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    // The quoter section: the mandatory CLOB baseline and its CPI accounts.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrderV1 {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
            market_index: 0,
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
        "took the book's half"
    );
    assert!(
        taker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "no remainder rests on the DLOB"
    );
    assert_eq!(
        taker.perp_positions[0].open_bids,
        (UNIT / 2) as i64,
        "the remainder is reserved against the book"
    );
    assert_eq!(clob_bid_count(&fixture), 1, "and rests there as a bid");
}

// ---------------------------------------------------------------------------
// Taker-origin crosses: a migrated taker remainder, and the crank that hands
// it the improvement its auction window earned.
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
        fixture.quoter,
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
) {
    use velocity::state::order_params::{OrderParams, PostOnlyParam};

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: party.user,
        user_stats: party.stats,
        authority: party.authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // No makers: the remainder is the whole order.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::PlaceAndTakePerpOrderV1 {
            params: OrderParams {
                order_type: OrderType::Limit,
                market_type: MarketType::Perp,
                direction,
                base_asset_amount: size,
                price,
                market_index: 0,
                post_only: PostOnlyParam::None,
                ..OrderParams::default()
            },
            success_condition: None,
        }
        .data(),
    };
    let authority = party.authority.insecure_clone();
    send_with_ixs(
        &mut fixture.svm,
        &authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
}

/// The crank, signed-keeper mode: `filler` is the caller's own `User` and the
/// crank reward lands there as quote.
fn crank_taker_origin_cross_ix(
    fixture: &Fixture,
    keeper: &Party,
    taker: &Party,
    counterparty: &Party,
) -> Instruction {
    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::CrankTakerOriginCross {
        state: state_pda(),
        authority: keeper.authority.pubkey(),
        filler: keeper.user,
        filler_stats: keeper.stats,
        taker: taker.user,
        taker_stats: taker.stats,
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
    }
    .to_account_metas(None);
    // Maps, then the counterparty's (User, UserStats) pair.
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(counterparty.user, false));
    accounts.push(AccountMeta::new(counterparty.stats, false));
    Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::CrankTakerOriginCross { market_index: 0 }.data(),
    }
}

fn perp_position(svm: &litesvm::LiteSVM, user: &Pubkey) -> velocity::state::user::PerpPosition {
    let user: User = read_zero_copy(svm, user);
    user.perp_positions[0]
}

/// The mechanism, end to end. A taker's unfilled limit bid at 101 migrates to
/// the book flagged taker-origin; two makers then line up asks at 100 and 99
/// inside its window. The crank resolves against the **99** — the best price,
/// not the taker's own — so the improvement goes to the taker and not to
/// whoever could have taken the remainder at 101. The maker who quoted 100
/// loses on price rather than on latency, and is untouched until the next
/// crank walks down to it.
#[test]
fn taker_origin_cross_settles_at_the_best_counterpartys_price() {
    let mut fixture = setup();
    pause_amm_fill(&mut fixture.svm);

    let taker = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let best = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let worse = party(&mut fixture.svm, 10_000 * SPOT_BALANCE_PRECISION_U64);
    let keeper = party(&mut fixture.svm, 0);

    // The remainder: a whole unfilled unit resting at its limit of 101.
    rest_taker_origin_order(
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

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &best);
    let keeper_authority = keeper.authority.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    let logs = meta.logs.join(" ");
    // Two CLOB CPIs (cancel + execute), the settlement, and the re-placement.
    // A ceiling rather than an equality, but a tight one: this runs inside a
    // relay executor's budget alongside its own accounting.
    assert!(
        meta.compute_units_consumed < 80_000,
        "crank cost {} CU",
        meta.compute_units_consumed
    );
    assert!(
        logs.contains("taker-origin cross: 500000000 base at 99000000 instead of 101000000"),
        "settled at the counterparty's price, not the resting one: {logs}"
    );

    // The taker is long half a unit at 99, not at 101. Its quote is the 49.5
    // notional plus the taker fee plus the cranker's cut — all of which fits
    // well inside the $1 the 101 rest price would have cost it.
    let taker_position = perp_position(&fixture.svm, &taker.user);
    assert_eq!(taker_position.base_asset_amount, (UNIT / 2) as i64);
    let paid = -taker_position.quote_asset_amount;
    assert!(
        (49_500_000..49_600_000).contains(&paid),
        "paid {paid}: 99 plus fees, where 101 would have been 50_500_000"
    );

    // The cranker is paid out of the improvement, in quote, on its own `User`.
    let reward = perp_position(&fixture.svm, &keeper.user).quote_asset_amount;
    assert_eq!(
        reward, 1_980,
        "the ordinary filler reward: 10% of the taker fee on the 49.5 notional"
    );
    let improvement = 1_000_000; // (101 - 99) * 0.5 units
    assert!(
        reward < improvement,
        "reward {reward} must fit inside the {improvement} improvement"
    );
    // The invariant, measured: the taker's all-in cost beats what being taken
    // at its own resting price would have been, fee included.
    let cost_if_taken = 50_500_000 + 50_500; // 101 * 0.5 plus 10bps
    assert!(
        paid < cost_if_taken,
        "crossing cost {paid}, resting would have cost {cost_if_taken}"
    );

    // The best-priced maker filled at its own price; the one that quoted 100
    // is untouched, order and reservation intact.
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
    assert_eq!(worse_position.base_asset_amount, 0, "not filled");
    assert_eq!(worse_position.open_asks, -((UNIT / 2) as i64));
    assert_eq!(worse_position.open_orders, 1);

    // The half the counterparty was too small to take went back on the book,
    // still taker-origin — cancelling it would let a cranker delete a taker's
    // whole order by crossing one unit of it.
    assert_eq!(clob_bid_count(&fixture), 1);
    assert_eq!(clob_ask_count(&fixture), 1);
    assert_eq!(
        perp_position(&fixture.svm, &taker.user).open_bids,
        (UNIT / 2) as i64,
        "the re-placed remainder keeps its reservation"
    );

    // ---- The second crank walks down to the 100: same order, next best
    // counterparty, and the taker still beats its 101.
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &worse);
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    assert!(
        meta.logs
            .join(" ")
            .contains("taker-origin cross: 500000000 base at 100000000"),
        "the next-best counterparty prices the second half"
    );
    let taker_position = perp_position(&fixture.svm, &taker.user);
    assert_eq!(taker_position.base_asset_amount, UNIT as i64);
    assert_eq!(taker_position.open_bids, 0, "nothing left resting");
    assert_eq!(taker_position.open_orders, 0);
    assert_eq!(clob_bid_count(&fixture), 0);
    assert_eq!(clob_ask_count(&fixture), 0);

    // Nothing crossed anymore: the crank declines rather than doing something
    // arbitrary.
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &worse);
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

    rest_taker_origin_order(
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

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &taker, &maker);
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
    let (clob_authority, _) = clob_authority_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            perp_market: perp_market_pda(0),
            user: party.user,
            authority: party.authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            clob_authority,
        }
        .to_account_metas(None),
        data: velocity::instruction::CancelClobOrder {
            params: CancelClobOrderParams {
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
) {
    let (early_party, early_direction, early_price, early_size) = early;
    let (late_party, late_direction, late_price, late_size) = late;
    rest_taker_origin_order(
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
    rest_taker_origin_order(fixture, late_party, late_direction, late_price, late_size);
    cancel_clob_order_for(fixture, blocker, blocker_ref);
    assert_eq!(clob_bid_count(&fixture), 1);
    assert_eq!(clob_ask_count(&fixture), 1);
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

    rest_crossing_remainders(
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
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &late, &early);
    let keeper_authority = keeper.authority.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    // Two cancels, the settlement and the re-placement — no `execute_v0`, which
    // is why this is cheaper than the maker path rather than more expensive.
    assert!(
        meta.compute_units_consumed < 70_000,
        "crank cost {} CU",
        meta.compute_units_consumed
    );
    assert!(
        meta.logs
            .join(" ")
            .contains("taker-origin cross: 500000000 base at 101000000 instead of 99000000"),
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
    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &late, &early);
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
    rest_crossing_remainders(
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

    let ix = crank_taker_origin_cross_ix(&fixture, &keeper, &late, &early);
    let keeper_authority = keeper.authority.insecure_clone();
    let meta = send_with_ixs(
        &mut fixture.svm,
        &keeper_authority,
        &[compute_unit_limit_ix(400_000), ix],
        &[],
    )
    .unwrap();
    assert!(
        meta.logs
            .join(" ")
            .contains("taker-origin cross: 500000000 base at 99000000 instead of 101000000"),
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
    rest_taker_origin_order(
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
    let (clob_authority, _) = clob_authority_pda();
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
        (fixture.quoter, false),
        (fixture.clob_market, true),
        (clob_id(), false),
        (clob_authority, false),
        (conditions, true),
        (fixture.oracle, false),
        (spot_market_pda(0), true),
        (perp_market_pda(0), true),
        (maker.user, true),
        (maker.stats, true),
    ];
    assert_eq!(
        resolved
            .accounts
            .iter()
            .map(|a| (Pubkey::new_from_array(a.address), a.is_writable()))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        resolved.data,
        0u16.to_le_bytes(),
        "market_index is the only argument"
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
    rest_crossing_remainders(
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
        resolved.accounts[14].address,
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
fn the_arb_crank_clears_the_front_of_book_before_the_remainders_own_cross() {
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
    rest_taker_origin_order(
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

/// A market order's remainder rests at the bound it already accepted.
///
/// Its own `price` is zero, so `auction_end_price` — the worst fill it agreed
/// to — is the only price it can rest at. Resting there is safe only because
/// the migrated order is taker-origin: a maker arriving during the activation
/// window has to beat it on price, and the cross pays the taker the
/// difference, rather than the order being a free option for whoever lands
/// first.
#[test]
fn fill_v1_rests_a_market_remainder_at_its_auction_bound() {
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
    // The vAMM is in every fill's mandatory baseline and quotes deep enough to
    // absorb a market order outright, so there is no remainder to migrate
    // unless it is out of the picture — which is the real-world case too: a
    // market remainder survives only when the taker's bound is tighter than
    // the curve.
    pause_amm_fill(&mut fixture.svm);
    // Half a unit of book liquidity against a one-unit market order.
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let taker_authority = Keypair::new();
    let taker_user = Pubkey::new_unique();
    let taker_stats = Pubkey::new_unique();
    let mut taker_order = Order::default();
    taker_order.order_id = 1;
    taker_order.status = OrderStatus::Open;
    taker_order.order_type = OrderType::Market;
    taker_order.market_type = MarketType::Perp;
    taker_order.market_index = 0;
    taker_order.direction = PositionDirection::Long;
    taker_order.base_asset_amount = UNIT;
    // A market order carries no price of its own; the auction end is its bound.
    taker_order.price = 0;
    taker_order.auction_start_price = (99 * PRICE) as i64;
    // Under the oracle, so the vAMM cannot fill and the remainder survives.
    taker_order.auction_end_price = (99 * PRICE + PRICE / 2) as i64;
    let mut taker_state = trading_user(
        &taker_authority.pubkey(),
        10_000 * SPOT_BALANCE_PRECISION_U64,
        Some(taker_order),
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

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::FillOrderV1 {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        clob_authority,
        crank_conditions: None,
        instructions_sysvar: Some(instructions_sysvar()),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(maker_stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrderV1 {
            order_id: Some(1),
            _maker_order_id: None,
            signed_route: vec![],
            market_index: 0,
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
        "took the book's half"
    );
    assert!(
        taker
            .orders
            .iter()
            .all(|order| order.status != OrderStatus::Open),
        "nothing left on the DLOB"
    );
    assert_eq!(
        taker.perp_positions[0].open_bids,
        (UNIT / 2) as i64,
        "the remainder is reserved against the book"
    );
    let (bid_count, best_bid) = (clob_bid_count(&fixture), clob_best_bid_price(&fixture));
    assert_eq!(bid_count, 1, "and rests there");
    assert_eq!(
        best_bid,
        Some(99 * PRICE + PRICE / 2),
        "at the auction bound, not at a zero price"
    );
}

/// The arbitrage crank refuses a book holding a crossed taker remainder.
///
/// The book's own gate cannot cover this: the arb crank's first leg can
/// consume the whole opposite side, after which nothing crosses the remainder
/// and taking it is legitimate as far as the CLOB can tell — so the second leg
/// would fill it at its own resting price and the improvement would land with
/// the protocol, which is the outcome the taker-origin path exists to prevent.
/// Relay never stages that, but the instruction is permissionless, so a
/// hand-built one has to be refused.
#[test]
fn the_arb_crank_refuses_a_book_holding_a_crossed_taker_remainder() {
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
    rest_taker_origin_order(
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
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // Both owners loaded, which is what lets a hand-built call settle against
    // the remainder at all.
    accounts.push(AccountMeta::new(taker.user, false));
    accounts.push(AccountMeta::new(taker.stats, false));
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority_pda().0, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::CrankCrossMatch {
            market_index: 0,
            size: UNIT / 2,
            buy_quoter_index: 0,
            sell_quoter_index: 0,
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
        format!("{:?}", err.meta.logs).contains("CrossedTakerRemainderPending"),
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
        data: velocity::instruction::InitializeRouterQuoteBuffer { market_index: 0 }.data(),
    };
    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    // Break the entry's quote-leg discriminator. The CPI now names an
    // instruction the CLOB does not have, so the callee errors and velocity
    // reports the entry it could not use.
    let mut entry = fixture.svm.get_account(&fixture.quoter).unwrap();
    let offset =
        8 + core::mem::offset_of!(velocity::state::prop_amm::QuoterV0, quote_v0_discriminator);
    entry.data[offset..offset + 8].copy_from_slice(&[0xAA; 8]);
    fixture.svm.set_account(fixture.quoter, entry).unwrap();

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::QuoteRouter {
        state: state_pda(),
        authority: router.pubkey(),
        quote_buffer,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                market_index: 0,
                direction: Direction::Long,
                size: 2 * UNIT,
                quoter_count: 1,
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
        data: velocity::instruction::InitializeRouterQuoteBuffer { market_index: 0 }.data(),
    };
    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    // Claim one more registered account than the entry holds. The extra slot
    // is zeroed, so velocity cannot resolve it and refuses before any CPI.
    let mut entry = fixture.svm.get_account(&fixture.quoter).unwrap();
    let offset =
        8 + core::mem::offset_of!(velocity::state::prop_amm::QuoterV0, quote_accounts_count);
    entry.data[offset] += 1;
    fixture.svm.set_account(fixture.quoter, entry).unwrap();

    let (clob_authority, _) = clob_authority_pda();
    let mut accounts = velocity::accounts::QuoteRouter {
        state: state_pda(),
        authority: router.pubkey(),
        quote_buffer,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                market_index: 0,
                direction: Direction::Long,
                size: 2 * UNIT,
                quoter_count: 1,
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
        data: velocity::instruction::InitializeRouterQuoteBuffer { market_index: 0 }.data(),
    };
    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    let (clob_authority, _) = clob_authority_pda();
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
        accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_authority, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        let ix = Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::QuoteRouter {
                args: velocity::instructions::QuoteRouterArgs {
                    market_index: 0,
                    direction: Direction::Long,
                    size: 2 * UNIT,
                    quoter_count: 1,
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
        data: velocity::instruction::InitializeRouterQuoteBuffer { market_index: 0 }.data(),
    };
    send(&mut fixture.svm, &router, ix, &[]).unwrap();

    let (clob_authority, _) = clob_authority_pda();
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
    // Quoter section: the CLOB entry, then each midpoint entry. A later pass
    // of the publisher's plan carries neither the CLOB nor the vAMM.
    if !quoters_only {
        accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    }
    for maker in &makers {
        accounts.push(AccountMeta::new_readonly(maker.entry, false));
    }
    // CPI union.
    if !quoters_only {
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
    }
    accounts.push(AccountMeta::new_readonly(clob_authority, false));
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
                market_index: 0,
                direction: Direction::Long,
                size: 1_000 * UNIT,
                quoter_count: makers.len() as u8 + u8::from(!quoters_only),
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
    // Eight quoters is what this fixture's transaction holds, not what the
    // program can carry: a midpoint quoter costs four account slots (its
    // entry, its instance, and its user pair), and a legacy message addresses
    // at most `PACKET_DATA_SIZE / 32` static keys. Past that the runtime
    // rejects the transaction before velocity runs.
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
