//! P7 `e2e-svm` — the crown-jewel stateful / coverage-guided Crucible harness.
//!
//! Loads the compiled velocity `.so` into LiteSVM, injects a coherent protocol
//! state (State + a USDC quote SpotMarket + a PerpMarket + funded users) via
//! account injection, then drives *real* instructions and checks the protocol's
//! global solvency / conservation invariants after every action.
//!
//! ## SVM integration path (both proven working; see the crate README/report)
//!  * `crucible_idl_gen::declare_fuzz_program!` ingests the full ~500 KB velocity
//!    IDL cleanly and gives us `register_schemas()` for semantic field-level diffs
//!    in crash output.
//!  * Instructions are built with `ctx.raw_call(Instruction{..})` using the IDL
//!    anchor discriminators + hand-encoded borsh args. This is uniform and avoids
//!    solana-version friction between the generated account structs and our deps.
//!  * Account reads / invariants use the velocity **host library** zero-copy
//!    structs via `ctx.read_zero_copy_account::<velocity::state::…>()` (velocity
//!    zero-copy structs are `bytemuck::Pod`), and the program's own authoritative
//!    helpers (`validate_spot_market_vault_amount`, `get_token_amount`).
//!  * Account *injection* uses `velocity::test_utils::get_anchor_account_bytes`
//!    (8-byte anchor discriminator + struct, alignment-correct) written straight
//!    into a program-owned account via the generic account builder.
//!
//! ## Setup coherence (verified against the deposit/withdraw handlers)
//!  * A SpotMarket with `oracle == Pubkey::default()` takes the hard-coded $1
//!    quote-asset price path — no oracle account required in `remaining_accounts`.
//!  * `status = Active`, `cumulative_{deposit,borrow}_interest = 1e10` (nonzero,
//!    else div-by-zero), `withdraw_guard_threshold = u64::MAX` (disables the twap
//!    withdraw limiter). Vault token authority = the `velocity_signer` PDA.

use {
    anchor_lang::AnchorSerialize,
    crucible_fuzzer::*,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::rc::Rc,
    velocity::{
        math::constants::{
            QUOTE_PRECISION, SPOT_BALANCE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION,
            SPOT_WEIGHT_PRECISION,
        },
        state::{
            market_status::MarketStatus,
            oracle::OracleSource,
            perp_market::PerpMarket,
            spot_market::{SpotBalanceType, SpotMarket},
            state::State,
            user::User,
        },
    },
};

// Generated types/schemas, read straight from the canonical IDL that the SDK
// also consumes. We only use `register_schemas()`; instruction building goes
// through `raw_call`.
crucible_idl_gen::declare_fuzz_program!(velocity_idl = "../../packages/sdk/src/idl/velocity.json");

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VELOCITY_SO: &str = "../../target/deploy/velocity.so";

/// Number of funded users the fixture creates.
const NUM_USERS: usize = 2;
/// Starting USDC (6 decimals) in each user's token account and the injected
/// budget we reconcile against.
const INITIAL_USDC: u64 = 1_000_000 * QUOTE_PRECISION as u64; // 1,000,000 USDC

// Anchor instruction discriminators (from the canonical IDL).
const D_INITIALIZE_USER_STATS: [u8; 8] = [254, 243, 72, 98, 251, 130, 168, 213];
const D_INITIALIZE_USER: [u8; 8] = [111, 17, 185, 250, 60, 122, 38, 254];
const D_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const D_WITHDRAW: [u8; 8] = [183, 18, 70, 156, 148, 109, 161, 34];
const D_PLACE_PERP_ORDER: [u8; 8] = [69, 161, 93, 202, 120, 126, 76, 185];
const D_CANCEL_ORDER: [u8; 8] = [95, 129, 237, 240, 8, 49, 223, 132];
const D_SETTLE_PNL: [u8; 8] = [43, 61, 234, 45, 15, 95, 152, 153];

// System / builtin program ids.
fn system_program_id() -> Pubkey {
    Pubkey::new_from_array([0u8; 32])
}
fn rent_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111")
}
fn clock_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111")
}
fn token_program_id() -> Pubkey {
    Pubkey::new_from_array(anchor_spl::token::ID.to_bytes())
}

fn velocity_program_id() -> Pubkey {
    Pubkey::new_from_array(velocity::ID.to_bytes())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ix_data(disc: [u8; 8], args: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + args.len());
    v.extend_from_slice(&disc);
    v.extend_from_slice(args);
    v
}

/// Inject an anchor zero-copy account (discriminator + struct) at `pda`, owned
/// by the velocity program, with plenty of lamports.
fn inject<T>(ctx: &mut TestContext, pda: Pubkey, acct: &mut T)
where
    T: bytemuck::Pod + anchor_lang::ZeroCopy + anchor_lang::Owner,
{
    let bytes = velocity::test_utils::get_anchor_account_bytes(acct);
    ctx.create_account()
        .pubkey(pda)
        .owner(velocity_program_id())
        .lamports(1_000_000_000)
        .data(&bytes)
        .create()
        .expect("inject account");
}

#[derive(Clone)]
struct UserAcct {
    keypair: Rc<Keypair>,
    user_pda: Pubkey,
    stats_pda: Pubkey,
    token_account: Pubkey,
}

#[derive(Clone)]
struct Fixture {
    ctx: TestContext,
    program_id: Pubkey,
    signer_pda: Pubkey,
    usdc_mint: Pubkey,
    spot_market_pda: Pubkey,
    spot_vault_pda: Pubkey,
    if_vault_pda: Pubkey,
    perp_market_pda: Pubkey,
    crank: Rc<Keypair>,
    users: Vec<UserAcct>,
    /// Monotonic mm-oracle sequence id for the native update path.
    mm_seq: u64,
}

// ---------------------------------------------------------------------------
// Injected-state builders (coherent USDC quote market + perp market + State)
// ---------------------------------------------------------------------------

fn build_state(signer: Pubkey, signer_nonce: u8, crank: Pubkey) -> State {
    let mut s = State::default();
    s.signer = anchor_pk(signer);
    s.signer_nonce = signer_nonce;
    s.exchange_status = 0; // Active
    s.number_of_spot_markets = 1;
    s.number_of_markets = 1;
    // Enable the native MM-oracle update path (feature_bit_flags bit 0).
    s.feature_bit_flags = 1;
    // Route the native hot-key roles to our crank keypair so the native
    // entrypoint's signer check passes regardless of the .so's anchor-test flag.
    s.hot_mm_oracle_crank = anchor_pk(crank);
    s.hot_amm_spread_adjust = anchor_pk(crank);
    s
}

fn build_spot_market_usdc(
    pubkey: Pubkey,
    mint: Pubkey,
    vault: Pubkey,
    if_vault: Pubkey,
) -> SpotMarket {
    let mut m = SpotMarket::default();
    m.pubkey = anchor_pk(pubkey);
    m.oracle = anchor_pk(Pubkey::new_from_array([0u8; 32])); // $1 quote path
    m.oracle_source = OracleSource::QuoteAsset;
    m.mint = anchor_pk(mint);
    m.vault = anchor_pk(vault);
    m.insurance_fund.vault = anchor_pk(if_vault);
    m.market_index = 0;
    m.decimals = 6;
    m.status = MarketStatus::Active;
    m.cumulative_deposit_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    m.cumulative_borrow_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    m.initial_asset_weight = SPOT_WEIGHT_PRECISION;
    m.maintenance_asset_weight = SPOT_WEIGHT_PRECISION;
    m.initial_liability_weight = SPOT_WEIGHT_PRECISION;
    m.maintenance_liability_weight = SPOT_WEIGHT_PRECISION;
    m.withdraw_guard_threshold = u64::MAX; // disable the twap withdraw limiter
    m.order_step_size = 1;
    m.order_tick_size = 1;
    // Nonzero oracle twap so the withdraw-path margin/oracle validity check
    // (`HistoricalOracleData::validate`) passes for the $1 quote asset.
    m.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();
    m
}

fn build_perp_market(pubkey: Pubkey, quote_spot_index: u16) -> PerpMarket {
    let mut m = PerpMarket::default();
    m.pubkey = anchor_pk(pubkey);
    // $1 quote-asset oracle path (no oracle account needed).
    m.oracle = anchor_pk(Pubkey::new_from_array([0u8; 32]));
    m.market_index = 0;
    m.quote_spot_market_index = quote_spot_index;
    m.status = MarketStatus::Active;
    m.oracle_source = OracleSource::QuoteAsset;
    m.margin_ratio_initial = 1_000; // 10%
    m.margin_ratio_maintenance = 500; // 5%
    m.order_step_size = 1_000_000; // 0.001 base units
    m.order_tick_size = 1;
    m.market_stats.min_order_size = 1_000_000;
    m.market_stats.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();
    m
}

/// Convert a solana-3.0 Pubkey into the anchor-lang Pubkey type used by the
/// velocity structs (byte-identical; go through bytes to be version-agnostic).
fn anchor_pk(p: Pubkey) -> anchor_lang::prelude::Pubkey {
    anchor_lang::prelude::Pubkey::new_from_array(p.to_bytes())
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

#[fuzz_fixture]
impl Fixture {
    pub fn setup() -> Self {
        velocity_idl::register_schemas();

        let mut ctx = TestContext::new();
        let program_id = velocity_program_id();
        ctx.add_program(&program_id, VELOCITY_SO)
            .expect("add velocity.so");

        // velocity_signer PDA (vault authority).
        let (signer_pda, signer_nonce) =
            Pubkey::find_program_address(&[b"velocity_signer"], &program_id);

        // State singleton.
        let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

        // Crank keypair used for the native hot-key roles.
        let crank = Rc::new(Keypair::new());
        ctx.create_account()
            .pubkey(crank.pubkey())
            .lamports(10_000_000_000)
            .owner(system_program_id())
            .create()
            .unwrap();

        let mut state = build_state(signer_pda, signer_nonce, crank.pubkey());
        inject(&mut ctx, state_pda, &mut state);

        // USDC mint (6 decimals).
        let usdc_mint = Keypair::new().pubkey();
        ctx.create_mint()
            .pubkey(usdc_mint)
            .mint_authority(signer_pda)
            .decimals(6)
            .create()
            .unwrap();

        // Spot market 0 (USDC quote) + vault + IF vault.
        let mi0 = 0u16.to_le_bytes();
        let (spot_market_pda, _) =
            Pubkey::find_program_address(&[b"spot_market", &mi0], &program_id);
        let (spot_vault_pda, _) =
            Pubkey::find_program_address(&[b"spot_market_vault", &mi0], &program_id);
        let (if_vault_pda, _) =
            Pubkey::find_program_address(&[b"insurance_fund_vault", &mi0], &program_id);

        let mut spot_market =
            build_spot_market_usdc(spot_market_pda, usdc_mint, spot_vault_pda, if_vault_pda);
        inject(&mut ctx, spot_market_pda, &mut spot_market);

        ctx.create_token_account()
            .pubkey(spot_vault_pda)
            .mint(usdc_mint)
            .token_owner(signer_pda)
            .amount(0)
            .create()
            .unwrap();
        ctx.create_token_account()
            .pubkey(if_vault_pda)
            .mint(usdc_mint)
            .token_owner(signer_pda)
            .amount(0)
            .create()
            .unwrap();

        // Perp market 0.
        let (perp_market_pda, _) =
            Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);
        let mut perp_market = build_perp_market(perp_market_pda, 0);
        inject(&mut ctx, perp_market_pda, &mut perp_market);

        // Users: create keypair + token account, then init user_stats + user.
        let mut users = Vec::new();
        for _ in 0..NUM_USERS {
            let kp = Rc::new(Keypair::new());
            ctx.create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();

            // User's USDC token account.
            let token_account = Keypair::new().pubkey();
            ctx.create_token_account()
                .pubkey(token_account)
                .mint(usdc_mint)
                .token_owner(kp.pubkey())
                .amount(INITIAL_USDC)
                .create()
                .unwrap();

            let sub0 = 0u16.to_le_bytes();
            let (stats_pda, _) =
                Pubkey::find_program_address(&[b"user_stats", kp.pubkey().as_ref()], &program_id);
            let (user_pda, _) =
                Pubkey::find_program_address(&[b"user", kp.pubkey().as_ref(), &sub0], &program_id);

            // initialize_user_stats
            let _ = ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new(stats_pda, false),
                        AccountMeta::new(state_pda, false),
                        AccountMeta::new_readonly(kp.pubkey(), false), // authority
                        AccountMeta::new(kp.pubkey(), true),           // payer (signer)
                        AccountMeta::new_readonly(rent_sysvar_id(), false),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_INITIALIZE_USER_STATS, &[]),
                })
                .signers(&[&kp])
                .send();

            // initialize_user(sub_account_id=0, name=[0;32])
            let mut init_args = Vec::new();
            init_args.extend_from_slice(&0u16.to_le_bytes());
            init_args.extend_from_slice(&[0u8; 32]);
            let _ = ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new(user_pda, false),
                        AccountMeta::new(stats_pda, false),
                        AccountMeta::new(state_pda, false),
                        AccountMeta::new_readonly(kp.pubkey(), false), // authority
                        AccountMeta::new(kp.pubkey(), true),           // payer
                        AccountMeta::new_readonly(rent_sysvar_id(), false),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_INITIALIZE_USER, &init_args),
                })
                .signers(&[&kp])
                .send();

            users.push(UserAcct {
                keypair: kp,
                user_pda,
                stats_pda,
                token_account,
            });
        }

        Fixture {
            ctx,
            program_id,
            signer_pda,
            usdc_mint,
            spot_market_pda,
            spot_vault_pda,
            if_vault_pda,
            perp_market_pda,
            crank,
            users,
            mm_seq: 1,
        }
    }

    // ---- helpers -------------------------------------------------------

    fn state_pda(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"velocity_state"], &self.program_id).0
    }

    // ---- actions -------------------------------------------------------

    /// Deposit USDC into spot market 0.
    pub fn action_deposit(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..1_000_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let bal = self.ctx.token_balance(&user.token_account);
        let amount = amount.min(bal);
        if amount == 0 {
            return false;
        }
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes()); // market_index
        args.extend_from_slice(&amount.to_le_bytes());
        args.push(0u8); // reduce_only
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new(user.token_account, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                    // remaining_accounts: spot market 0 (writable)
                    AccountMeta::new(self.spot_market_pda, false),
                ],
                data: ix_data(D_DEPOSIT, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Withdraw USDC from spot market 0.
    pub fn action_withdraw(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..1_000_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        args.push(0u8); // reduce_only
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new_readonly(self.signer_pda, false),
                    AccountMeta::new(user.token_account, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                ],
                data: ix_data(D_WITHDRAW, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Place a resting (post-only) perp limit order, away from the $1 oracle so
    /// it does not cross. Exercises order placement + the initial-margin gate.
    pub fn action_place_perp_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] dir: u8,
        #[range(1..1_000_000_000u64)] base: u64,
    ) -> bool {
        use velocity::{
            controller::position::PositionDirection,
            state::{
                order_params::{OrderParams, PostOnlyParam},
                user::{MarketType, OrderType},
            },
        };

        let user = self.users[user_idx].clone();
        let (direction, price) = if dir == 0 {
            (PositionDirection::Long, 900_000u64) // $0.90 bid < $1
        } else {
            (PositionDirection::Short, 1_100_000u64) // $1.10 ask > $1
        };
        let params = OrderParams {
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction,
            base_asset_amount: base,
            price,
            market_index: 0,
            post_only: PostOnlyParam::MustPostOnly,
            ..Default::default()
        };
        let mut buf = Vec::new();
        params.serialize(&mut buf).unwrap();
        let data = ix_data(D_PLACE_PERP_ORDER, &buf);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    // remaining: quote spot market (r) + perp market (w)
                    AccountMeta::new_readonly(self.spot_market_pda, false),
                    AccountMeta::new(self.perp_market_pda, false),
                ],
                data,
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Cancel a specific order id (best-effort; None cancels all).
    pub fn action_cancel_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..40u32)] order_id: u32,
    ) -> bool {
        let user = self.users[user_idx].clone();
        // Option<u32>::Some(order_id)
        let mut args = vec![1u8];
        args.extend_from_slice(&order_id.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(self.spot_market_pda, false),
                    AccountMeta::new(self.perp_market_pda, false),
                ],
                data: ix_data(D_CANCEL_ORDER, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Settle perp PnL for market 0 (best-effort).
    pub fn action_settle_pnl(&mut self, #[range(0..NUM_USERS)] user_idx: usize) -> bool {
        let user = self.users[user_idx].clone();
        let args = 0u16.to_le_bytes(); // market_index
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(self.spot_vault_pda, false),
                    // remaining: spot market (w) + perp market (w)
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.perp_market_pda, false),
                ],
                data: ix_data(D_SETTLE_PNL, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Advance the clock.
    pub fn action_warp(&mut self, #[range(1..5_000u64)] slots: u64) -> bool {
        let target = self.ctx.slot() + slots;
        self.ctx.warp_to_slot(target);
        true
    }

    /// Native entrypoint opcode 0 — update_mm_oracle (`[0xFF×4, 0, price, seq]`).
    pub fn action_native_mm_oracle(&mut self, #[range(1..10_000_000i64)] price: i64) -> bool {
        let mut data = vec![0xFF, 0xFF, 0xFF, 0xFF, 0u8];
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&self.mm_seq.to_le_bytes());
        self.mm_seq += 1;
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(self.perp_market_pda, false),
                    AccountMeta::new_readonly(self.crank.pubkey(), true),
                    AccountMeta::new_readonly(clock_sysvar_id(), false),
                    AccountMeta::new_readonly(self.state_pda(), false),
                ],
                data,
            })
            .signers(&[&self.crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Native entrypoint opcode 1 — update_amm_spread_adjustment
    /// (`[0xFF×4, 1, adj_i8]`).
    pub fn action_native_spread_adjust(&mut self, #[range(-100..100i32)] adj: i32) -> bool {
        let data = vec![0xFF, 0xFF, 0xFF, 0xFF, 1u8, adj as i8 as u8];
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(self.perp_market_pda, false),
                    AccountMeta::new_readonly(self.crank.pubkey(), true),
                    AccountMeta::new_readonly(self.state_pda(), false),
                ],
                data,
            })
            .signers(&[&self.crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Invariants (families I, II, V) — checked after every action.
// ---------------------------------------------------------------------------

/// Read an anchor zero-copy account by **unaligned** copy.
///
/// `TestContext::read_zero_copy_account` uses `bytemuck::from_bytes` (a *reference*
/// cast), which requires the `data[8..]` slice to meet `align_of::<T>()`. Velocity's
/// zero-copy structs contain `u128`/`i128` fields, so on the x86_64 host they have
/// alignment 16, while LiteSVM stores account data in a `Vec<u8>` whose `+8` offset
/// is essentially never 16-aligned — the reference cast then panics
/// (`TargetAlignmentGreaterAndInputNotAligned`). `pod_read_unaligned` copies the
/// bytes out instead, which is correct and alignment-agnostic.
fn read_zc<T: bytemuck::Pod>(ctx: &TestContext, pk: &Pubkey) -> Option<T> {
    let acct = ctx.get_account(pk).ok()?;
    let size = std::mem::size_of::<T>();
    // An account that EXISTS but is too small is a host/on-chain LAYOUT DRIFT
    // (host `size_of::<T>` diverged from the deployed .so). Fail LOUDLY:
    // silently returning None here would skip every invariant that reads through
    // this helper and turn the whole harness green with zero checks executed.
    // Genuinely-absent accounts still return None via the `.ok()?` above.
    assert!(
        acct.data.len() >= 8 + size,
        "layout drift: account {pk} has {} data bytes, need >= {} (8 + size_of::<{}>); \
         rebuild target/deploy/velocity.so from the current program source",
        acct.data.len(),
        8 + size,
        std::any::type_name::<T>(),
    );
    Some(bytemuck::pod_read_unaligned::<T>(&acct.data[8..8 + size]))
}

impl Fixture {
    fn read_spot_market(&self) -> Option<SpotMarket> {
        read_zc::<SpotMarket>(&self.ctx, &self.spot_market_pda)
    }
    fn read_perp_market(&self) -> Option<PerpMarket> {
        read_zc::<PerpMarket>(&self.ctx, &self.perp_market_pda)
    }
    fn read_user(&self, pk: &Pubkey) -> Option<User> {
        read_zc::<User>(&self.ctx, pk)
    }
}

#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    fn setup_and_deposit_withdraw() {
        let mut f = Fixture::setup();

        // Users initialized?
        for u in &f.users {
            assert!(
                f.read_user(&u.user_pda).is_some(),
                "user account should exist after initialize_user"
            );
        }

        // Deposit must actually succeed against the injected coherent market.
        let vault_before = f.ctx.token_balance(&f.spot_vault_pda);
        let ok = f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64);
        assert!(ok, "deposit should succeed");
        let vault_after = f.ctx.token_balance(&f.spot_vault_pda);
        assert!(
            vault_after > vault_before,
            "vault should grow after deposit: {} -> {}",
            vault_before,
            vault_after
        );

        // Solvency invariant must hold after a real deposit.
        let sm = f.read_spot_market().unwrap();
        velocity::math::spot_withdraw::validate_spot_market_vault_amount(&sm, vault_after)
            .expect("vault solvent after deposit");

        // Withdraw a portion back out.
        let ok = f.action_withdraw(0, 100_000 * QUOTE_PRECISION as u64);
        assert!(ok, "withdraw should succeed");

        // Best-effort perp/native actions must not panic (may soft-fail).
        let _ = f.action_place_perp_order(0, 0, 10_000_000);
        let _ = f.action_native_mm_oracle(1_000_000);
        let _ = f.action_native_spread_adjust(5);
        let _ = f.action_settle_pnl(0);
        let _ = f.action_warp(100);
    }
}

#[cfg(feature = "invariant_solvency")]
#[invariant_test]
fn invariant_solvency(fixture: &mut Fixture) {
    // --- Family I: per-spot-market solvency (the vault covers all claims). ---
    // Uses the program's OWN authoritative check; an Err is a solvency
    // violation (vault holds less than depositor claims).
    if let Some(spot_market) = fixture.read_spot_market() {
        let vault_amount = fixture.ctx.token_balance(&fixture.spot_vault_pda);
        match velocity::math::spot_withdraw::validate_spot_market_vault_amount(
            &spot_market,
            vault_amount,
        ) {
            Ok(_claim) => {}
            Err(e) => {
                fuzz_assert!(
                    false,
                    "spot market 0 insolvent: vault={} fails validate_spot_market_vault_amount ({:?})",
                    vault_amount,
                    e
                );
            }
        }

        // --- Family II: global quote conservation. ---
        // No quote unit is minted: the vault balance must cover the net token
        // amount owed to depositors. We reconcile the vault against the sum of
        // every user's spot-market-0 balance (deposits positive, borrows
        // negative) plus the market's own pool balances, using the program's
        // exact token-amount conversion.
        let mut net_user_tokens: i128 = 0;
        for u in &fixture.users {
            if let Some(user) = fixture.read_user(&u.user_pda) {
                for sp in user.spot_positions.iter() {
                    if sp.market_index != 0 || sp.scaled_balance == 0 {
                        continue;
                    }
                    let tok = velocity::math::spot_balance::get_token_amount(
                        sp.scaled_balance as u128,
                        &spot_market,
                        &sp.balance_type,
                    )
                    .unwrap_or(0) as i128;
                    match sp.balance_type {
                        SpotBalanceType::Deposit => net_user_tokens += tok,
                        SpotBalanceType::Borrow => net_user_tokens -= tok,
                    }
                }
            }
        }
        // Pool balances denominated in this (quote) market.
        let pool_tokens = |bal: u128| -> i128 {
            velocity::math::spot_balance::get_token_amount(
                bal,
                &spot_market,
                &SpotBalanceType::Deposit,
            )
            .unwrap_or(0) as i128
        };
        let mut backed: i128 = net_user_tokens;
        backed += pool_tokens(spot_market.revenue_pool.scaled_balance);
        backed += pool_tokens(spot_market.insurance_fund_revenue_receivable.scaled_balance);
        if let Some(perp_market) = fixture.read_perp_market() {
            backed += pool_tokens(perp_market.pnl_pool.scaled_balance);
            backed += pool_tokens(perp_market.amm.fee_pool.scaled_balance);
        }

        // The vault must be able to cover all quote claims backed here. Rounding
        // (deposits floor, borrows ceil) and external SPL donations only ever
        // make the vault >= claims; vault < claims means quote was created.
        let vault_i = vault_amount as i128;
        fuzz_assert!(
            vault_i >= backed,
            "quote conservation: vault={} < backed claims={} (net_user={})",
            vault_i,
            backed,
            net_user_tokens
        );
    }

    // --- Family II: perp open-interest conservation. ---
    // The program's own invariant: base_long + base_short == amm net position.
    if let Some(pm) = fixture.read_perp_market() {
        let net_user = pm.base_asset_amount_long + pm.base_asset_amount_short;
        fuzz_assert_eq!(
            net_user,
            pm.amm.base_asset_amount_with_amm,
            "perp OI: base_long+base_short ({}) != amm.base_asset_amount_with_amm ({})",
            net_user,
            pm.amm.base_asset_amount_with_amm
        );
    }

    // --- Family V (partial): no user carries a spot borrow in the quote
    // market with zero collateral anywhere (a trivially-underwater account that
    // was never flagged). Full maintenance-margin valuation requires the whole
    // oracle/market map and is covered end-to-end in P8; here we pin the cheap
    // structural case. ---
    for u in &fixture.users {
        if let Some(user) = fixture.read_user(&u.user_pda) {
            let mut has_borrow = false;
            let mut has_any_deposit = false;
            for sp in user.spot_positions.iter() {
                if sp.scaled_balance == 0 {
                    continue;
                }
                match sp.balance_type {
                    SpotBalanceType::Borrow => has_borrow = true,
                    SpotBalanceType::Deposit => has_any_deposit = true,
                }
            }
            let flagged = user.is_being_liquidated();
            fuzz_assert!(
                !(has_borrow && !has_any_deposit && !flagged),
                "family V: user {} has a borrow with no collateral and is not flagged liquidatable",
                u.user_pda
            );
        }
    }
}

// ===========================================================================
// Phase 3 — regression harnesses (PENDING audit-fix PRs). Off by default; each
// has its own `[features]` entry. Each asserts the FIXED behavior, so it FAILS
// on current (pre-fix) master — proving the fuzzer catches the bug — and flips
// to passing once the PR merges.
//
// IMPLEMENTED:
//   * #270 (F1) — below. Reproduces on master: the market-scoped SettlePnl
//     pause is not enforced on `settle_expired_position`, so a paused market's
//     expired position still settles (master runs deep into settlement logic —
//     error 6135 InvalidSpotMarketState — instead of rejecting early with 6158
//     InvalidMarketStatusToSettlePnl as the fix does).
//
// DEFERRED (documented, not stubbed — each needs state this injection-based
// tier cannot cheaply build; better homed in P8 or a dedicated escrow harness):
//   * #256 builder fee — needs a RevenueShareEscrow with an approved-builder row
//     written by `add_builder_order`, a place with expired `max_ts` that
//     soft-skips, then a fill that reuses the id. Requires the full builder-code
//     escrow + fill/matching engine.
//   * #273 revenue-share sub_account — needs `builder_codes_enabled`, a
//     RevenueShareEscrow with accrued fees, and a sibling recipient User at
//     sub_account_id != 0 driven through `load_revenue_share_map`.
//   * #271 signed-msg pause / replay resize — needs signed-message ("swift")
//     envelopes (ed25519 pre-ix + SignedMsgUserOrders account) and the
//     authority/delegate resize path; no signing infra in this harness.
//   * #272 deposit/withdraw/revenue pause on bypassed paths — the bugs live on
//     `transfer_pools`, `liquidate_spot_with_swap_begin`, and the opportunistic
//     revenue-to-IF settle, none of which the direct deposit/withdraw actions
//     here reach.
//   * #276 funding pause — spot interest only diverges under nonzero
//     borrow/utilization (needs a borrowing counterparty); the bid/ask-TWAP and
//     funding-settlement side-effect cases need the funding crank + fills.
// ===========================================================================

// PENDING PR #270 (OtterSec F1): perp-pause enforcement. `settle_expired_position`
// (the Settlement-market path of `settle_pnl`) never checked the market-scoped
// `PerpOperation::SettlePnl` pause bit, so a paused market's expired positions
// could still be settled permissionlessly. The fix makes it revert
// `InvalidMarketStatusToSettlePnl` (6158). Here we inject a Settlement-status
// perp market with the SettlePnl bit set and a user holding a pure-PnL expired
// position (base==0, quote>0, so the margin calc is skipped), then call
// `settle_pnl`. FIXED => error 6158; MASTER => the settle proceeds (no 6158).
#[cfg(feature = "regr_270_perp_pause")]
mod regr_270 {
    use super::*;

    pub const E_INVALID_MARKET_STATUS_TO_SETTLE_PNL: u32 = 6158;

    #[derive(Clone)]
    pub struct Regr270 {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub spot_market_pda: Pubkey,
        pub spot_vault_pda: Pubkey,
        pub perp_market_pda: Pubkey,
        pub user_pda: Pubkey,
        pub user_kp: Rc<Keypair>,
    }

    #[fuzz_fixture]
    impl Regr270 {
        pub fn setup() -> Self {
            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO).unwrap();

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // State: exchange fully Active (so `settle_pnl_not_paused` and
            // `amm_not_paused` pass); NO feature_bit_flags (builder codes off).
            let mut state = State::default();
            state.signer = anchor_pk(signer_pda);
            state.signer_nonce = signer_nonce;
            state.exchange_status = 0;
            state.number_of_spot_markets = 1;
            state.number_of_markets = 1;
            inject(&mut ctx, state_pda, &mut state);

            // USDC mint + quote spot market + vault.
            let usdc_mint = Keypair::new().pubkey();
            ctx.create_mint()
                .pubkey(usdc_mint)
                .mint_authority(signer_pda)
                .decimals(6)
                .create()
                .unwrap();

            let mi0 = 0u16.to_le_bytes();
            let (spot_market_pda, _) =
                Pubkey::find_program_address(&[b"spot_market", &mi0], &program_id);
            let (spot_vault_pda, _) =
                Pubkey::find_program_address(&[b"spot_market_vault", &mi0], &program_id);
            let (if_vault_pda, _) =
                Pubkey::find_program_address(&[b"insurance_fund_vault", &mi0], &program_id);
            let mut spot_market =
                build_spot_market_usdc(spot_market_pda, usdc_mint, spot_vault_pda, if_vault_pda);
            inject(&mut ctx, spot_market_pda, &mut spot_market);
            ctx.create_token_account()
                .pubkey(spot_vault_pda)
                .mint(usdc_mint)
                .token_owner(signer_pda)
                .amount(1_000_000_000)
                .create()
                .unwrap();

            // Perp market: Settlement status, SettlePnl operation paused, expiry set.
            let (perp_market_pda, _) =
                Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);
            let mut perp_market = build_perp_market(perp_market_pda, 0);
            perp_market.status = MarketStatus::Settlement;
            perp_market.expiry_ts = -100; // deep past, so the settlement buffer is reached
            perp_market.expiry_price = 1_000_000; // $1
            perp_market.paused_operations = 0b0000_1000; // PerpOperation::SettlePnl
            perp_market.pnl_pool.scaled_balance = 100 * SPOT_BALANCE_PRECISION; // backing for the claim
            inject(&mut ctx, perp_market_pda, &mut perp_market);

            // User with a pure-PnL expired position (base==0, quote>0).
            let user_kp = Rc::new(Keypair::new());
            ctx.create_account()
                .pubkey(user_kp.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();
            let (user_pda, _) = Pubkey::find_program_address(
                &[b"user", user_kp.pubkey().as_ref(), &mi0],
                &program_id,
            );
            let mut user = User::default();
            user.authority = anchor_pk(user_kp.pubkey());
            user.sub_account_id = 0;
            user.pool_id = 0;
            user.perp_positions[0].market_index = 0;
            user.perp_positions[0].base_asset_amount = 0;
            user.perp_positions[0].quote_asset_amount = 1_000_000; // +$1 claim
            inject(&mut ctx, user_pda, &mut user);

            Regr270 {
                ctx,
                program_id,
                state_pda,
                spot_market_pda,
                spot_vault_pda,
                perp_market_pda,
                user_pda,
                user_kp,
            }
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }
    }
}

#[cfg(feature = "regr_270_perp_pause")]
use regr_270::Regr270;

#[cfg(feature = "regr_270_perp_pause")]
#[crucible_fuzz]
fn regr_270_perp_pause(fixture: &mut Regr270, #[range(0..1u8)] _unused: u8) {
    let args = 0u16.to_le_bytes(); // market_index
    let outcome = fixture
        .ctx
        .raw_call(Instruction {
            program_id: fixture.program_id,
            accounts: vec![
                AccountMeta::new_readonly(fixture.state_pda, false),
                AccountMeta::new(fixture.user_pda, false),
                AccountMeta::new_readonly(fixture.user_kp.pubkey(), true),
                AccountMeta::new_readonly(fixture.spot_vault_pda, false),
                // remaining: quote spot market (w) + perp market (w)
                AccountMeta::new(fixture.spot_market_pda, false),
                AccountMeta::new(fixture.perp_market_pda, false),
            ],
            data: ix_data(D_SETTLE_PNL, &args),
        })
        .signers(&[&fixture.user_kp])
        .send();

    let code = outcome.ok().and_then(|o| o.error_code());
    // FIXED behavior: settling an expired position in a SettlePnl-paused market
    // must be rejected with InvalidMarketStatusToSettlePnl (6158). On pre-fix
    // master the pause is not checked on this path, so `code` will be None
    // (success) or some other error — this assertion then fails, reproducing
    // the bug.
    fuzz_assert_eq!(
        code,
        Some(regr_270::E_INVALID_MARKET_STATUS_TO_SETTLE_PNL),
        "PR #270: expired-position settle in a SettlePnl-paused market was not blocked (got error_code={:?})",
        code
    );
}
