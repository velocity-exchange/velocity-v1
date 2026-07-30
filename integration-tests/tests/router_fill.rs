//! End-to-end router fill: a taker's market order routed across a real CLOB
//! book (quote + execute CPIs through its `QuoterV0` entry), a DLOB maker,
//! and the vAMM — through the real `fill_perp_order_router` instruction.
//! CLOB placement runs through the velocity `place_clob_order` adapter, so
//! the maker's open-order aggregates are reserved by the margin gate and
//! unwound by the fill, exactly as the margin model requires.
//!
//! Protocol state (State, markets, users, oracle) is synthesized via
//! `set_account` with the same values the controller unit fixtures use.

use anchor_lang::{Discriminator, InstructionData, ToAccountMetas};
use bytemuck::Zeroable;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use velocity::controller::position::PositionDirection;
use velocity::instructions::{
    CancelClobOrderParams, InitializeQuoterArgs, PlaceClobOrderParams, QuoterAccountMetaArg,
    UpdateQuoterAccountsArgs,
};
use velocity::math::constants::{
    AMM_RESERVE_PRECISION, PEG_PRECISION, PRICE_PRECISION, QUOTE_PRECISION_I64,
    SPOT_BALANCE_PRECISION_U64, SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
};
use velocity::state::market_status::MarketStatus;
use velocity::state::oracle::OracleSource;
use velocity::state::perp_market::PerpMarket;
use velocity::state::prop_amm::{ClobOrderRefV0, QuoterCpiLeg, QuoterType};
use velocity::state::pyth_lazer_oracle::PythLazerOracle;
use velocity::state::spot_market::{SpotBalanceType, SpotMarket};
use velocity::state::state::{FeeStructure, OracleGuardRails, State};
use velocity::state::traits::Size;
use velocity::state::user::{MarketType, Order, OrderStatus, OrderType, User, UserStats};
use velocity_integration_tests::*;

const UNIT: u64 = 1_000_000_000;
const PRICE: u64 = 1_000_000; // PRICE_PRECISION as u64

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

/// `[disc][ClobHeaderV0 8352][len u32][pad][OrderNodeV0 x cap]`.
fn clob_market_space(capacity: usize) -> usize {
    let orders_offset = (8 + 8352 + 4usize).next_multiple_of(8);
    orders_offset + capacity * 96
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
    v.extend_from_slice(&1u64.to_le_bytes()); // order_step_size
    v.extend_from_slice(&1u64.to_le_bytes()); // min_order_size
    v.extend_from_slice(&0u32.to_le_bytes()); // default_activation_delay
    v.extend_from_slice(&20u32.to_le_bytes()); // max_activation_delay
    v.extend_from_slice(&2u32.to_le_bytes()); // unknown_user_grace_slots
    v.extend_from_slice(&threshold.to_le_bytes()); // evict_threshold_per_side
    v.extend_from_slice(&128u16.to_le_bytes()); // max_quote_levels
    v.extend_from_slice(&64u16.to_le_bytes()); // max_execute_fills
    v.extend_from_slice(&32u16.to_le_bytes()); // max_execute_users
    v
}

fn clob_ask_count(svm: &litesvm::LiteSVM, market: &Pubkey) -> u32 {
    let data = svm.get_account(market).unwrap().data;
    u32::from_le_bytes(data[140..144].try_into().unwrap())
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
    let (velocity_signer, _) = velocity_signer_pda();
    let ix = clob_ix(
        "initialize_market_v0",
        clob_market_config(0),
        vec![
            AccountMeta::new_readonly(clob_admin.pubkey(), true),
            AccountMeta::new_readonly(velocity_signer, false),
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
    let (velocity_signer, _) = velocity_signer_pda();
    let quoter = quoter_pda(0, &clob_id(), &user);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
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
                    pubkey: velocity_signer,
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
    crank_conditions: Option<Pubkey>,
    params: PlaceClobOrderParams,
) -> Instruction {
    let (velocity_signer, _) = velocity_signer_pda();
    let mut accounts = velocity::accounts::PlaceClobOrder {
        state: state_pda(),
        user,
        authority: authority.pubkey(),
        quoter,
        clob_market,
        clob_program: clob_id(),
        velocity_signer,
        crank_conditions,
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
    }
}

/// Place a 0.5-unit CLOB ask at `price` through the velocity adapter and
/// return the order ref from the tx return data.
fn place_clob_ask(fixture: &mut Fixture, price: u64, size: u64) -> ClobOrderRefV0 {
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter,
        fixture.clob_market,
        fixture.oracle,
        None,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price,
            base_asset_amount: size,
            max_ts: 0,
            activation_delay_slots: Some(0),
        },
    );
    let meta = send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let data = &meta.return_data.data;
    ClobOrderRefV0 {
        node_index: u32::from_le_bytes(data[..4].try_into().unwrap()),
        order_id: u64::from_le_bytes(data[4..12].try_into().unwrap()),
    }
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 1);

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

    let (velocity_signer, _) = velocity_signer_pda();
    let mut accounts = velocity::accounts::FillOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler: filler_user,
        filler_stats,
        user: taker_user,
        user_stats: taker_stats,
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
    accounts.push(AccountMeta::new_readonly(velocity_signer, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::FillPerpOrder {
            order_id: Some(1),
            _maker_order_id: None,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

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

#[test]
fn cancel_clob_order_unwinds_the_reserved_aggregates() {
    let mut fixture = setup();
    let order_ref = place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);

    let (velocity_signer, _) = velocity_signer_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            velocity_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

    // The stale ref fails closed on a second cancel.
    let ix2 = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            velocity_signer,
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
        None,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 99 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: clock.unix_timestamp + 10,
            activation_delay_slots: Some(0),
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

    let (velocity_signer, _) = velocity_signer_pda();
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
            velocity_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
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

    let (velocity_signer, _) = velocity_signer_pda();
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
            velocity_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
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
    fixture.svm.airdrop(&router.pubkey(), 10_000_000_000).unwrap();
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

    let (velocity_signer, _) = velocity_signer_pda();
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
    accounts.push(AccountMeta::new_readonly(velocity_signer, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));

    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::QuoteRouter {
            args: velocity::instructions::QuoteRouterArgs {
                market_index: 0,
                direction: velocity::state::prop_amm::Direction::Long,
                size: 2 * UNIT,
                quoter_count: 1,
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
    let stats =
        Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;
    set_user_account(svm, user, &trading_user(&signer, 0, None));
    set_user_stats_account(svm, stats, &signer);
    user
}

/// Attach the CLOB to the market through the admin ix — which also stands up
/// the crank conditions account, so no separate init exists to call.
fn init_crank_conditions(fixture: &mut Fixture, keeper_payment_lamports: u64) -> Pubkey {
    let conditions = crank_conditions_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::AdminUpdatePerpMarketClobQuoter {
            admin: fixture.admin.pubkey(),
            state: state_pda(),
            perp_market: perp_market_pda(0),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            crank_conditions: conditions,
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdatePerpMarketClobQuoter {
            keeper_payment_lamports,
            expire_fallback_slots: 100,
        }
        .data(),
    };
    let admin = fixture.admin.insecure_clone();
    send(&mut fixture.svm, &admin, ix, &[]).unwrap();
    conditions
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
            crank_conditions: conditions,
            clob_market: fixture.clob_market,
            quoter: fixture.quoter,
            state: state_pda(),
        }
        .to_account_metas(None),
        data: if expire {
            velocity::instruction::ResolveClobCrankRemoveExpired {}.data()
        } else {
            velocity::instruction::ResolveClobCrankEvict {}.data()
        },
    };
    let keeper = fixture.keeper.insecure_clone();
    let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let pointer =
        velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
    if !pointer.has_work() {
        return None;
    }
    let data = fixture.svm.get_account(&conditions).unwrap().data;
    let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
    Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
}

/// Submit a staged executor the way a turner does: keeper placeholder
/// substituted with the payout account, every meta a non-signer (the fee
/// payer is not in the account list).
fn run_staged_executor(
    fixture: &mut Fixture,
    resolved: &velocity::relay_spec::ResolvedCrankV0,
    executor_disc: &[u8],
    payout: Pubkey,
) {
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
    let mut data = executor_disc.to_vec();
    data.extend_from_slice(&resolved.data);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data,
    };
    let keeper = fixture.keeper.insecure_clone();
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
}

/// The full program-keeper expiry loop: init conditions, place an expiring
/// order through the adapter (min-folding the wake hint), resolve, and
/// submit the staged executor unsigned. The maker's reward accrues to the
/// protocol User, the payout account is paid reservoir lamports, and the
/// hint is repaired.
#[test]
fn program_keeper_expire_crank_pays_reservoir_lamports_to_an_unsigned_keeper() {
    use velocity::state::clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_EVICT};

    let mut fixture = setup();
    const PAYMENT: u64 = 50_000;
    let conditions = init_crank_conditions(&mut fixture, PAYMENT);
    let protocol_user = set_protocol_user(&mut fixture.svm);
    // Top off the reservoir (lamport credits to a program-owned account are
    // unrestricted — this is the hot role's off-chain top-off leg).
    fixture.svm.airdrop(&conditions, 1_000_000_000).unwrap();

    // The init wrote the evict watch over the book's counts and mirrored the
    // payment into min_payment.
    let acct: ClobCrankConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let evict = acct.read_condition(CLOB_CRANK_EVICT).unwrap();
    assert_eq!(evict.min_payment, PAYMENT);
    assert_eq!(
        evict.wake_account,
        fixture.clob_market.to_bytes(),
        "evict watch must point at the CLOB market"
    );
    assert_eq!(acct.expire_wake_ts().unwrap(), i64::MAX);

    // An expiring ask placed WITH the conditions account min-folds the hint.
    let clock: solana_clock::Clock = fixture.svm.get_sysvar();
    let max_ts = clock.unix_timestamp + 10;
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter,
        fixture.clob_market,
        fixture.oracle,
        Some(conditions),
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Short,
            price: 99 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts,
            activation_delay_slots: Some(0),
        },
    );
    let maker = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &maker, ix, &[]).unwrap();
    let acct: ClobCrankConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    assert_eq!(acct.expire_wake_ts().unwrap(), max_ts);

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
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + PAYMENT
    );
    assert_eq!(
        fixture.svm.get_account(&conditions).unwrap().lamports,
        reservoir_before - PAYMENT
    );
    let acct: ClobCrankConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    assert_eq!(acct.expire_wake_ts().unwrap(), i64::MAX);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
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
            Some(conditions),
            PlaceClobOrderParams {
                market_index: 0,
                direction,
                price,
                base_asset_amount: UNIT / 2,
                max_ts: 0,
                activation_delay_slots: Some(0),
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
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

fn trigger_clob_order_ix(fixture: &Fixture, order_id: u32, filler: Pubkey, filler_stats: Pubkey, maker_stats: Pubkey) -> Instruction {
    let (velocity_signer, _) = velocity_signer_pda();
    let mut accounts = velocity::accounts::TriggerClobOrder {
        state: state_pda(),
        authority: fixture.keeper.pubkey(),
        filler,
        filler_stats,
        user: fixture.clob_maker_user,
        user_stats: maker_stats,
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        velocity_signer,
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
    set_oracle(&mut fixture.svm, fixture.oracle, (97 * PRICE_PRECISION) as i64, 12);
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 1);
    let _ = node_index;

    // A second trigger attempt on the placed slot fails.
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("already placed");
    assert!(format!("{:?}", err.meta.logs).contains("already rests on the CLOB"));

    // Evict (fixture soft cap = 1): the shadow re-arms in the same tx,
    // edge-gated on a recross.
    let (velocity_signer, _) = velocity_signer_pda();
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
            velocity_signer,
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
    assert_eq!(maker.perp_positions[0].open_orders, 1, "armed slot counts again");
    assert_eq!(maker.open_orders, 1);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

    // Still through the trigger: the edge gate refuses to re-fire.
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("no recross yet");
    assert!(format!("{:?}", err.meta.logs).contains("never crossed back"));

    // Price back above: the crank observes the recross and clears the gate
    // without placing.
    set_oracle(&mut fixture.svm, fixture.oracle, (100 * PRICE_PRECISION) as i64, 13);
    fixture.svm.warp_to_slot(13);
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(!maker.orders[0].is_bit_flag_set(OrderBitFlag::AwaitingTriggerRecross));
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

    // Crossed again: re-places.
    set_oracle(&mut fixture.svm, fixture.oracle, (97 * PRICE_PRECISION) as i64, 14);
    fixture.svm.warp_to_slot(14);
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert!(maker.orders[0].is_placed_on_clob());
    let (node_index, clob_order_id) = maker.orders[0].clob_order_ref();
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 1);

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
            velocity_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
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

    set_oracle(&mut fixture.svm, fixture.oracle, (97 * PRICE_PRECISION) as i64, 12);
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
    let (velocity_signer, _) = velocity_signer_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            user: fixture.clob_maker_user,
            authority: maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            velocity_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
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
        None,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: 0,
            activation_delay_slots: Some(0),
        },
    );
    let maker_authority = fixture.clob_maker_authority.insecure_clone();
    send(&mut fixture.svm, &maker_authority, ix, &[]).unwrap();
    fixture.svm.warp_to_slot(12);
    set_oracle(&mut fixture.svm, fixture.oracle, (100 * PRICE_PRECISION) as i64, 12);

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
        accounts.push(AccountMeta::new_readonly(velocity_signer_pda().0, false));
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
    send(&mut fixture.svm, &keeper, cross_ix(), &[]).unwrap();

    // The maker round-tripped against themselves: net base zero, they paid
    // the spread; both orders consumed, aggregates unwound.
    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].base_asset_amount, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.perp_positions[0].open_bids, 0);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

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
    assert!(protocol.orders.iter().all(|order| order.status != OrderStatus::Open));
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
    let resolve_cross = |fixture: &mut Fixture| -> Option<velocity::relay_spec::ResolvedCrankV0> {
        let ix = Instruction {
            program_id: velocity_id(),
            accounts: velocity::accounts::ResolveClobCrank {
                crank_conditions: conditions,
                clob_market: fixture.clob_market,
                quoter: fixture.quoter,
                state: state_pda(),
            }
            .to_account_metas(None),
            data: velocity::instruction::ResolveClobCrankCross {}.data(),
        };
        let keeper = fixture.keeper.insecure_clone();
        let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
        let pointer =
            velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
        if !pointer.has_work() {
            return None;
        }
        let data = fixture.svm.get_account(&conditions).unwrap().data;
        let staged =
            &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
        Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
    };
    assert!(resolve_cross(&mut fixture).is_none(), "book is uncrossed");

    // Cross it again — this time behind a speed bump, the makers-line-up
    // scenario: the ask rests immediately, the crossing bid activates three
    // slots out. Placement min-folds the activation slot into the AtSlot
    // wake, the resolver reports no work until the slot arrives, and the
    // cross fires the moment it does.
    fixture.svm.warp_to_slot(20);
    set_oracle(&mut fixture.svm, fixture.oracle, (100 * PRICE_PRECISION) as i64, 20);
    place_clob_ask(&mut fixture, 99 * PRICE, UNIT / 2);
    let ix = place_clob_order_ix(
        fixture.clob_maker_user,
        &fixture.clob_maker_authority,
        fixture.quoter,
        fixture.clob_market,
        fixture.oracle,
        Some(conditions),
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT / 2,
            max_ts: 0,
            activation_delay_slots: Some(3),
        },
    );
    send(&mut fixture.svm, &maker_authority, ix, &[]).unwrap();
    {
        use velocity::state::clob_crank::ClobCrankConditionsV0;
        let acct: ClobCrankConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
        assert_eq!(
            acct.activation_wake_slot().unwrap(),
            23,
            "placement min-folds the activation slot into the AtSlot wake"
        );
    }
    // Before activation the crossing bid isn't matchable: no work.
    assert!(resolve_cross(&mut fixture).is_none(), "speed bump still running");

    fixture.svm.warp_to_slot(23);
    set_oracle(&mut fixture.svm, fixture.oracle, (100 * PRICE_PRECISION) as i64, 23);
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
    assert_eq!(
        fixture.svm.get_account(&payout).unwrap().lamports,
        1_000_000_000 + 2 * PAYMENT
    );
    // The landing executor repaired the activation hint forward: nothing
    // pending, so the AtSlot wake goes quiet.
    {
        use velocity::state::clob_crank::ClobCrankConditionsV0;
        let acct: ClobCrankConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
        assert_eq!(acct.activation_wake_slot().unwrap(), u64::MAX);
    }
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

    let (velocity_signer, _) = velocity_signer_pda();
    let force_cancel_ix = |fixture: &Fixture| {
        let mut accounts = velocity::accounts::ForceCancelClobOrders {
            state: state_pda(),
            authority: fixture.keeper.pubkey(),
            filler: filler_user,
            filler_stats,
            user: fixture.clob_maker_user,
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            velocity_signer,
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
                order_refs: vec![order_ref],
            }
            .data(),
        }
    };

    // Healthy account: refused.
    let keeper = fixture.keeper.insecure_clone();
    let err =
        {
        let ix = force_cancel_ix(&fixture);
        send(&mut fixture.svm, &keeper, ix, &[]).expect_err("still healthy")
    };
    assert!(
        format!("{:?}", err.meta.logs).contains("SufficientCollateral"),
        "unexpected: {:?}",
        err.meta.logs
    );

    // Deteriorated: the order is reclaimed for the flat fee.
    set_user_account(&mut fixture.svm, fixture.clob_maker_user, &broke);
    let ix = force_cancel_ix(&fixture);
    send(&mut fixture.svm, &keeper, ix, &[]).unwrap();

    let maker: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(maker.perp_positions[0].open_asks, 0);
    assert_eq!(maker.perp_positions[0].open_orders, 0);
    assert_eq!(maker.open_orders, 0);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
    // Flat fee moved maker -> filler through the quote spot balances.
    let filler: User = read_zero_copy(&fixture.svm, &filler_user);
    assert!(filler.spot_positions[0].scaled_balance > 0);
    // The maker's dust deposit flipped into a borrow covering the fee.
    assert_eq!(maker.spot_positions[0].balance_type, SpotBalanceType::Borrow);

    // Nothing left: the same ref is now stale and the call fails loudly.
    let ix = force_cancel_ix(&fixture);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("stale ref");
    assert!(format!("{:?}", err.meta.logs).contains("no passed refs are live orders"));
}
