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
    anchor_lang::{Discriminator, InstructionData, ToAccountMetas},
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
            market_status::MarketStatus,
            oracle::OracleSource,
            perp_market::PerpMarket,
            prop_amm::{ClobCancelSides, ClobOrderRefV0, QuoterCpiLeg, QuoterType},
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            state::{FeeStructure, OracleGuardRails, State},
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

/// `[disc][ClobHeaderV0 8480][len u32][pad][OrderNodeV0 x cap]`. The header
/// is the CLOB's `size_of::<ClobHeaderV0>()` — a 128-byte reserve for future
/// fields sits between the config and the 8192-byte response region.
fn clob_market_space(capacity: usize) -> usize {
    let orders_offset = (8 + 8480 + 4usize).next_multiple_of(8);
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

fn clob_bid_count(svm: &litesvm::LiteSVM, market: &Pubkey) -> u32 {
    let data = svm.get_account(market).unwrap().data;
    u32::from_le_bytes(data[136..140].try_into().unwrap())
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
    let (quoter_signer, _) = quoter_signer_pda();
    let ix = clob_ix(
        "initialize_market_v0",
        clob_market_config(0),
        vec![
            AccountMeta::new_readonly(clob_admin.pubkey(), true),
            AccountMeta::new_readonly(quoter_signer, false),
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
    let (quoter_signer, _) = quoter_signer_pda();
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
                    pubkey: quoter_signer,
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
    let (quoter_signer, _) = quoter_signer_pda();
    let mut accounts = velocity::accounts::PlaceClobOrder {
        state: state_pda(),
        user,
        authority: authority.pubkey(),
        quoter,
        clob_market,
        clob_program: clob_id(),
        quoter_signer,
        crank_conditions,
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

/// The speed bump replaced JIT; skipping it is attested flow only. A
/// below-default activation delay must fail without the flow authority
/// co-signing the transaction, and pass with it — while at-or-above the
/// default stays permissionless.
#[test]
fn fast_activation_requires_the_flow_authority_attestation() {
    use velocity::state::state::HotRole;

    let mut fixture = setup();
    // The fixture's book has a zero default (every test placement is
    // "fast"); raise it so below-default is expressible.
    let mut book = fixture.svm.get_account(&fixture.clob_market).unwrap();
    let at = velocity::state::prop_amm::CLOB_DEFAULT_ACTIVATION_DELAY_OFFSET;
    book.data[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    fixture.svm.set_account(fixture.clob_market, book).unwrap();

    let place = |fixture: &Fixture, delay: Option<u32>, with_sysvar: bool| {
        let mut ix = place_clob_order_ix(
            fixture.clob_maker_user,
            &fixture.clob_maker_authority,
            fixture.quoter,
            fixture.clob_market,
            fixture.oracle,
            None,
            PlaceClobOrderParams {
                market_index: 0,
                direction: PositionDirection::Short,
                price: 105 * PRICE,
                base_asset_amount: UNIT,
                max_ts: 0,
                activation_delay_slots: delay,
            },
        );
        if with_sysvar {
            // The optional slot is encoded as a program-id placeholder;
            // swap the real sysvar in.
            // Two optionals are encoded as placeholders (crank_conditions,
            // then instructions_sysvar) — the sysvar is the LAST one.
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
        format!("{:?}", err.err).contains("6375"),
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
    assert!(format!("{:?}", err.err).contains("6375"));

    // Co-signed by the flow authority (a signer meta anywhere in the
    // transaction — here, appended to the placement's own accounts): the
    // fast activation is attested and lands.
    let mut ix = fast_ix;
    ix.accounts
        .push(AccountMeta::new_readonly(flow.pubkey(), true));
    send(&mut fixture.svm, &keeper, ix, &[&flow]).unwrap();
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

    let (quoter_signer, _) = quoter_signer_pda();
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
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
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

/// Place a bid through velocity, so a sweep has both sides to take.
fn place_clob_bid(fixture: &mut Fixture, price: u64, size: u64) -> ClobOrderRefV0 {
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

fn cancel_all_clob_ix(fixture: &Fixture, sides: ClobCancelSides) -> Instruction {
    let (quoter_signer, _) = quoter_signer_pda();
    Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelAllClobOrders {
            state: state_pda(),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            quoter_signer,
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
        bid_base += UNIT / 2 + i;
        ask_base += UNIT / 4 + i;
        place_clob_bid(&mut fixture, (98 - i) * PRICE, UNIT / 2 + i);
        place_clob_ask(&mut fixture, (99 + i) * PRICE, UNIT / 4 + i);
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
    assert_eq!(clob_bid_count(&fixture.svm, &fixture.clob_market), 0);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 5);

    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Both);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();
    let after: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(after.perp_positions[0].open_bids, 0);
    assert_eq!(after.perp_positions[0].open_asks, 0);
    assert_eq!(after.perp_positions[0].open_orders, 0);
    assert_eq!(after.open_orders, 0);
    assert!(!after.has_open_order);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

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
            None,
            PlaceClobOrderParams {
                market_index: 0,
                direction: PositionDirection::Short,
                price: 99 * PRICE,
                base_asset_amount: UNIT / 3,
                max_ts: 0,
                activation_delay_slots: Some(0),
            },
        );
        send(&mut fixture.svm, &other_authority, ix, &[]).unwrap();
    }
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 4);

    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Both);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();

    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 2);
    let mine: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(mine.perp_positions[0].open_asks, 0);
    assert_eq!(mine.perp_positions[0].open_orders, 0);
    let theirs: User = read_zero_copy(&fixture.svm, &other_user);
    assert_eq!(theirs.perp_positions[0].open_asks, -((2 * UNIT / 3) as i64));
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
    let (quoter_signer, _) = quoter_signer_pda();
    let per_order_cu: u64 = refs
        .iter()
        .map(|order_ref| {
            let ix = Instruction {
                program_id: velocity_id(),
                accounts: velocity::accounts::CancelClobOrder {
                    state: state_pda(),
                    user: fixture.clob_maker_user,
                    authority: fixture.clob_maker_authority.pubkey(),
                    quoter: fixture.quoter,
                    clob_market: fixture.clob_market,
                    clob_program: clob_id(),
                    quoter_signer,
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

    let (quoter_signer, _) = quoter_signer_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            user: fixture.clob_maker_user,
            authority: fixture.clob_maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            quoter_signer,
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
            quoter_signer,
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

    let (quoter_signer, _) = quoter_signer_pda();
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
            quoter_signer,
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

    let (quoter_signer, _) = quoter_signer_pda();
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
            quoter_signer,
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

    let (quoter_signer, _) = quoter_signer_pda();
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
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
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
    let stats = Pubkey::find_program_address(&[b"user_stats", signer.as_ref()], &velocity_id()).0;
    set_user_account(svm, user, &trading_user(&signer, 0, None));
    set_user_stats_account(svm, stats, &signer);
    user
}

/// Attach the CLOB to the market through the admin ix — which also stands up
/// the crank conditions account, so no separate init exists to call.
fn init_crank_conditions(fixture: &mut Fixture, keeper_payment_lamports: u64) -> Pubkey {
    init_crank_conditions_with_floor(fixture, keeper_payment_lamports, 0)
}

/// `min_cross_surplus` is the floor on what the protocol must net from a
/// cross-match crank; 0 is the bare "strictly profitable" rule.
fn init_crank_conditions_with_floor(
    fixture: &mut Fixture,
    keeper_payment_lamports: u64,
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
            min_cross_surplus,
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
            scratch: relay_scratch_pda(),
            crank_conditions: conditions,
            clob_market: fixture.clob_market,
            quoter: fixture.quoter,
            state: state_pda(),
        }
        .to_account_metas(None),
        data: if expire {
            velocity::instruction::ResolveCrankClobRemoveExpired {}.data()
        } else {
            velocity::instruction::ResolveCrankClobEvict {}.data()
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
    let evict = acct.get_condition(CLOB_CRANK_EVICT).unwrap();
    assert_eq!(evict.min_payment(), PAYMENT);
    assert_eq!(
        account_change(&evict).0,
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

fn trigger_clob_order_ix(
    fixture: &Fixture,
    order_id: u32,
    filler: Pubkey,
    filler_stats: Pubkey,
    maker_stats: Pubkey,
) -> Instruction {
    let (quoter_signer, _) = quoter_signer_pda();
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
        quoter_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 1);
    let _ = node_index;

    // A second trigger attempt on the placed slot fails.
    let ix = trigger_clob_order_ix(&fixture, 1, filler_user, filler_stats, maker_stats);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("already placed");
    assert!(format!("{:?}", err.meta.logs).contains("already rests on the CLOB"));

    // Evict (fixture soft cap = 1): the shadow re-arms in the same tx,
    // edge-gated on a recross.
    let (quoter_signer, _) = quoter_signer_pda();
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
            quoter_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

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
            quoter_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 2);

    let ix = cancel_all_clob_ix(&fixture, ClobCancelSides::Asks);
    send(&mut fixture.svm, &fixture.clob_maker_authority, ix, &[]).unwrap();

    let after: User = read_zero_copy(&fixture.svm, &fixture.clob_maker_user);
    assert_eq!(after.orders[0].status, OrderStatus::Canceled);
    assert_eq!(after.perp_positions[0].open_orders, 0);
    assert_eq!(after.open_orders, 0);
    assert_eq!(after.perp_positions[0].open_asks, 0);
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
    let (quoter_signer, _) = quoter_signer_pda();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::CancelClobOrder {
            state: state_pda(),
            user: fixture.clob_maker_user,
            authority: maker_authority.pubkey(),
            quoter: fixture.quoter,
            clob_market: fixture.clob_market,
            clob_program: clob_id(),
            quoter_signer,
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
        accounts.push(AccountMeta::new_readonly(quoter_signer_pda().0, false));
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
    init_crank_conditions_with_floor(&mut fixture, PAYMENT, 0);
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
        accounts.push(AccountMeta::new_readonly(quoter_signer_pda().0, false));
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
    let resolve_cross = |fixture: &mut Fixture| -> Option<velocity::relay_spec::ResolvedCrankV0> {
        let ix = Instruction {
            program_id: velocity_id(),
            accounts: velocity::accounts::ResolveClobCrank {
                scratch: relay_scratch_pda(),
                crank_conditions: conditions,
                clob_market: fixture.clob_market,
                quoter: fixture.quoter,
                state: state_pda(),
            }
            .to_account_metas(None),
            data: velocity::instruction::ResolveCrankCrossMatch {}.data(),
        };
        let keeper = fixture.keeper.insecure_clone();
        let meta = send(&mut fixture.svm, &keeper, ix, &[]).unwrap();
        let pointer =
            velocity::relay_spec::ResponsePointerV0::read(&meta.return_data.data).unwrap();
        if !pointer.has_work() {
            return None;
        }
        let data = fixture.svm.get_account(&relay_scratch_pda()).unwrap().data;
        let staged = &data[pointer.offset() as usize..(pointer.offset() + pointer.len()) as usize];
        Some(velocity::relay_spec::ResolvedCrankV0::read(staged).unwrap())
    };
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

    let (quoter_signer, _) = quoter_signer_pda();
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
            quoter_signer,
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
    let err = {
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
    assert_eq!(
        maker.spot_positions[0].balance_type,
        SpotBalanceType::Borrow
    );

    // Nothing left: the same ref is now stale and the call fails loudly.
    let ix = force_cancel_ix(&fixture);
    let err = send(&mut fixture.svm, &keeper, ix, &[]).expect_err("stale ref");
    assert!(format!("{:?}", err.meta.logs).contains("no passed refs are live orders"));
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

    let (quoter_signer, _) = quoter_signer_pda();
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: taker_user,
        user_stats: taker_stats,
        authority: taker_authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        quoter_signer,
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
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);
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

    let (quoter_signer, _) = quoter_signer_pda();
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
        quoter_signer,
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
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
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
    let (quoter_signer, _) = quoter_signer_pda();

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
    let entry = quoter_pda(0, &midpoint_id(), &user);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
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

    let (quoter_signer, _) = quoter_signer_pda();
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
    // Maker section: the midpoint's quoted user.
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    // Quoter section: baseline CLOB entry + midpoint entry, then the union
    // of their CPI accounts and programs.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new_readonly(maker.entry, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(maker.instance, false));
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

    let (quoter_signer, _) = quoter_signer_pda();
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
    // Maker section: both quoted users.
    accounts.push(AccountMeta::new(fixture.clob_maker_user, false));
    accounts.push(AccountMeta::new(clob_maker_stats, false));
    accounts.push(AccountMeta::new(maker.user, false));
    accounts.push(AccountMeta::new(maker.stats, false));
    // Quoter section: both entries + the union of their CPI accounts.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new_readonly(maker.entry, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
    accounts.push(AccountMeta::new_readonly(clob_id(), false));
    accounts.push(AccountMeta::new(maker.instance, false));
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

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
/// exactly what the attach registered: conditions, book, state, entry,
/// user, then the entry's registered quote surface + program.
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
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new(maker.instance, false));
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
    // The resolver list (shared scratch, then the entry's registered quote
    // surface) is stored once in the relay block's built-in region; every
    // condition points at it indirectly.
    // Ten, not nine: the midpoint's quote leg now names velocity's State,
    // which it reads the live flow authority from.
    assert_eq!(conditions[QUOTER_CROSS_WATCH].resolvers().count, 10);
    assert_eq!(acct.relay.resolver_refs().len(), 10);

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
        None,
        PlaceClobOrderParams {
            market_index: 0,
            direction: PositionDirection::Long,
            price: 101 * PRICE,
            base_asset_amount: UNIT,
            max_ts: 0,
            activation_delay_slots: Some(0),
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
    assert_eq!(clob_bid_count(&fixture.svm, &fixture.clob_market), 0);
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
                sync_payment_lamports: 20_000,
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
    sync_payment_lamports: u64,
) -> Pubkey {
    let conditions = user_conditions_pda(&user);
    let mut accounts = velocity::accounts::SyncLiqConditions {
        payer: fixture.keeper.pubkey(),
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
    let ix = Instruction {
        program_id: velocity_id(),
        accounts,
        data: velocity::instruction::SyncLiqConditions {
            args: velocity::instructions::SyncLiqConditionsArgs {
                sync_payment_lamports,
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

/// A leveraged long's liquidation threshold: the sync solves the
/// ceteris-paribus price where free collateral runs out, haircuts it, and
/// writes a downward OnValueCross — the "high-risk bucket boundary",
/// precomputed. The resolver reports no work while the account is healthy
/// (the level wake costs a turner nothing until the price is near), and
/// the self-sync watch covers the user's own position bytes.
#[test]
fn liq_conditions_write_a_conservative_downward_threshold() {
    use velocity::state::user_conditions::{UserConditionsV0, LIQ_SYNC_FALLBACK, LIQ_SYNC_WATCH};

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
    let conditions = sync_liq_conditions(&mut fixture, user, market_conditions, 5_000);
    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    let (header, block) = velocity::relay_spec::read_block(acct.block(), 0).unwrap();
    assert_eq!(header.num_conditions as usize, USER_CONDITIONS);

    // Slot 0: the perp exposure's threshold — a downward value cross on the
    // market oracle, strictly below spot and above zero.
    assert_eq!(value_cross(&block[0]).0, fixture.oracle.to_bytes());
    assert_eq!(
        value_cross(&block[0]).4,
        1,
        "a long is liquidated as price falls"
    );
    // The distance is real, not epsilon: ~$50 free collateral against a
    // 9.5-unit-equivalent slope puts the ceteris-paribus boundary ~$5.26
    // below spot, and the 20% haircut arms the watch ~$4.21 below. A
    // threshold a whisker under spot means the slope/distance scales
    // diverged again (the localnet harness caught exactly that: every
    // perp watch due on any tick, every spot watch never armed).
    let threshold = value_cross(&block[0]).3;
    assert!(
        threshold > (94 * PRICE) as i64 && threshold < (97 * PRICE) as i64,
        "threshold {threshold} should sit ~4 dollars below spot"
    );
    assert_eq!(block[0].min_payment(), PAYMENT);
    assert_eq!(acct.slots[0].target_market_index, 0);
    assert_eq!(acct.slots[0].active, 1);

    // The self-maintenance pair: a watch over the user's own position bytes
    // whose executor is the sync, plus the coarse poll.
    assert_eq!(account_change(&block[LIQ_SYNC_WATCH]).0, user.to_bytes());
    assert_eq!(
        block[LIQ_SYNC_WATCH].crank_spec().resolver_disc,
        velocity::instruction::ResolveResyncLiqConditions::DISCRIMINATOR
    );
    assert_eq!(block[LIQ_SYNC_WATCH].min_payment(), 5_000);
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
fn liq_self_sync_stages_an_unsigned_executor_and_pays_from_its_own_lamports() {
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

    const SYNC_FEE: u64 = 5_000;
    let conditions = sync_liq_conditions(&mut fixture, user, market_conditions, SYNC_FEE);
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
    run_staged_executor(
        &mut fixture,
        &resolved,
        velocity::instruction::ResyncLiqConditions::DISCRIMINATOR,
        payout,
    );

    // Keeper paid from the conditions account, and the stale threshold is
    // gone (no live exposures left to watch).
    assert_eq!(
        fixture.svm.get_balance(&payout).unwrap(),
        payout_before + SYNC_FEE
    );
    assert_eq!(
        fixture.svm.get_balance(&conditions).unwrap(),
        conditions_before - SYNC_FEE
    );
    let acct: UserConditionsV0 = read_zero_copy(&fixture.svm, &conditions);
    assert_eq!(acct.slots[0].active, 0);
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

    let (quoter_signer, _) = quoter_signer_pda();
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
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
        accounts.push(AccountMeta::new(spot_market_pda(0), false));
        accounts.push(AccountMeta::new(perp_market_pda(0), false));
        accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
        accounts.push(AccountMeta::new(fixture.clob_market, false));
        accounts.push(AccountMeta::new_readonly(quoter_signer, false));
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

    let (quoter_signer, _) = quoter_signer_pda();
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
        quoter_signer,
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
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 1);
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

    let (quoter_signer, _) = quoter_signer_pda();
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
        quoter_signer,
        crank_conditions: None,
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
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
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
    assert_eq!(
        clob_bid_count(&fixture.svm, &fixture.clob_market),
        1,
        "and rests there as a bid"
    );
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
        None,
        PlaceClobOrderParams {
            market_index: 0,
            direction,
            price,
            base_asset_amount: size,
            max_ts: 0,
            activation_delay_slots: Some(0),
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

    let (quoter_signer, _) = quoter_signer_pda();
    let mut accounts = velocity::accounts::PlaceAndTakeV1 {
        state: state_pda(),
        user: party.user,
        user_stats: party.stats,
        authority: party.authority.pubkey(),
        quoter: fixture.quoter,
        clob_market: fixture.clob_market,
        clob_program: clob_id(),
        quoter_signer,
        crank_conditions: None,
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(fixture.oracle, false));
    accounts.push(AccountMeta::new(spot_market_pda(0), false));
    accounts.push(AccountMeta::new(perp_market_pda(0), false));
    // No makers: the remainder is the whole order.
    accounts.push(AccountMeta::new_readonly(fixture.quoter, false));
    accounts.push(AccountMeta::new(fixture.clob_market, false));
    accounts.push(AccountMeta::new_readonly(quoter_signer, false));
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
    let (quoter_signer, _) = quoter_signer_pda();
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
        quoter_signer,
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
    assert_eq!(clob_bid_count(&fixture.svm, &fixture.clob_market), 1);
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
        reward, 4_950,
        "the ordinary filler reward: 10% of the taker fee on the 99 notional"
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
    assert_eq!(clob_bid_count(&fixture.svm, &fixture.clob_market), 1);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 1);
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
    assert_eq!(clob_bid_count(&fixture.svm, &fixture.clob_market), 0);
    assert_eq!(clob_ask_count(&fixture.svm, &fixture.clob_market), 0);

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
    assert_eq!(paid, 49_999_000 + 49_999);
    assert!(
        paid < 50_000_000 + 50_000,
        "still cheaper than being taken at the 100 it rested at"
    );
}
