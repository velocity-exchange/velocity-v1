//! P7b `e2e-svm-pause` — pause-enforcement regression harnesses for the two
//! deferred OtterSec audit-fix PRs (#272 and #276).
//!
//! Each test injects a coherent-enough protocol state with the relevant pause
//! bit(s) **pre-set** (the cheapest way to reach a paused state — no admin ix
//! needed), drives the real target instruction via `ctx.raw_call(..)`, and
//! asserts the *fixed* behavior. So every harness **FAILS on current (pre-fix)
//! master** — proving the fuzzer catches the bug — and flips to passing once the
//! PR merges. The loaded `.so` at `../../target/deploy/velocity.so` is the
//! pre-fix build, so `crucible run` reproduces the bug.
//!
//! ## Harness set A — PR #272 (deposit/withdraw/revenue pause on bypassed paths)
//!  * `regr_272_revenue_pool_deposit` — `deposit_into_spot_market_revenue_pool`
//!    credits the spot vault while ignoring the market-scoped
//!    `SpotOperation::Deposit` pause. Fix rejects with `MarketActionPaused`.
//!  * `regr_272_settle_revenue_if` — direct `settle_revenue_to_insurance_fund`
//!    ignores the market-scoped `SpotOperation::Withdraw` pause (only the
//!    exchange-wide withdraw pause was ever gated). Fix rejects with
//!    `MarketWithdrawPaused`.
//!
//! ## Harness set B — PR #276 (funding pause on bypassed paths)
//!  * `regr_276_spot_interest` — the shared `update_spot_market_cumulative_interest`
//!    keeps accruing interest during an exchange-wide `FundingPaused` on every
//!    caller but the dedicated crank (here driven via `deposit`). Fix freezes it.
//!  * `regr_276_bid_ask_twap` — `update_perp_bid_ask_twap` keeps advancing a
//!    market's funding-input TWAP state when that market's
//!    `PerpOperation::UpdateFunding` is paused. Fix early-returns `Ok` before it
//!    touches oracle / keeper / TWAP state.
//!
//! ## DEFERRED (documented with evidence in the module-level note near the
//! bottom of this file): `regr_272_transfer_pools_pause`,
//! `regr_272_swap_begin_pause`, `regr_272_opportunistic_revenue`,
//! `regr_276_funding_settle` — each genuinely needs trade / fill / liquidation
//! machinery this injection tier cannot stand up cheaply.

#![allow(dead_code, unused_imports)]

use crucible_fuzzer::*;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::rc::Rc;

use anchor_lang::AnchorSerialize;
use velocity::math::constants::{
    ONE_YEAR, QUOTE_PRECISION, SPOT_BALANCE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION,
    SPOT_RATE_PRECISION, SPOT_UTILIZATION_PRECISION, SPOT_WEIGHT_PRECISION,
};
use velocity::state::market_status::MarketStatus;
use velocity::state::oracle::OracleSource;
use velocity::state::paused_operations::{PerpOperation, SpotOperation};
use velocity::state::perp_market::PerpMarket;
use velocity::state::spot_market::SpotBalanceType;
use velocity::state::spot_market::SpotMarket;
use velocity::state::state::State;
use velocity::state::user::{User, UserStats};

// Generated types/schemas from the canonical velocity IDL. We only use
// `register_schemas()`; instruction building goes through `raw_call`.
crucible_idl_gen::declare_fuzz_program!(velocity_idl = "idls/velocity.json");

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VELOCITY_SO: &str = "../../target/deploy/velocity.so";

const INITIAL_USDC: u64 = 1_000_000 * QUOTE_PRECISION as u64;

// Anchor instruction discriminators (from the canonical IDL).
const D_INITIALIZE_USER_STATS: [u8; 8] = [254, 243, 72, 98, 251, 130, 168, 213];
const D_INITIALIZE_USER: [u8; 8] = [111, 17, 185, 250, 60, 122, 38, 254];
const D_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const D_DEPOSIT_INTO_SPOT_MARKET_REVENUE_POOL: [u8; 8] = [92, 40, 151, 42, 122, 254, 139, 246];
const D_SETTLE_REVENUE_TO_INSURANCE_FUND: [u8; 8] = [200, 120, 93, 136, 69, 38, 199, 159];
const D_UPDATE_PERP_BID_ASK_TWAP: [u8; 8] = [247, 23, 255, 65, 212, 90, 221, 194];

// Error codes (from the canonical IDL `errors` list).
const E_MARKET_ACTION_PAUSED: u32 = 6146;
const E_MARKET_WITHDRAW_PAUSED: u32 = 6149;

// System / builtin program ids.
fn system_program_id() -> Pubkey {
    Pubkey::new_from_array([0u8; 32])
}
fn rent_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111")
}
fn token_program_id() -> Pubkey {
    Pubkey::new_from_array(anchor_spl::token::ID.to_bytes())
}
fn velocity_program_id() -> Pubkey {
    Pubkey::new_from_array(velocity::ID.to_bytes())
}

// ---------------------------------------------------------------------------
// Shared helpers (mirrors fuzz/e2e-svm/src/main.rs)
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

/// Convert a solana-3.0 Pubkey into the anchor-lang Pubkey type used by the
/// velocity structs (byte-identical; go through bytes to be version-agnostic).
fn anchor_pk(p: Pubkey) -> anchor_lang::prelude::Pubkey {
    anchor_lang::prelude::Pubkey::new_from_array(p.to_bytes())
}

fn build_state(signer: Pubkey, signer_nonce: u8, exchange_status: u8) -> State {
    let mut s = State::default();
    s.signer = anchor_pk(signer);
    s.signer_nonce = signer_nonce;
    s.exchange_status = exchange_status;
    s.number_of_spot_markets = 1;
    s.number_of_markets = 1;
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
    m.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();
    m
}

fn build_perp_market(pubkey: Pubkey, quote_spot_index: u16) -> PerpMarket {
    let mut m = PerpMarket::default();
    m.pubkey = anchor_pk(pubkey);
    m.oracle = anchor_pk(Pubkey::new_from_array([0u8; 32]));
    m.market_index = 0;
    m.quote_spot_market_index = quote_spot_index;
    m.status = MarketStatus::Active;
    m.oracle_source = OracleSource::QuoteAsset;
    m.margin_ratio_initial = 1_000;
    m.margin_ratio_maintenance = 500;
    m.order_step_size = 1_000_000;
    m.order_tick_size = 1;
    m.market_stats.min_order_size = 1_000_000;
    m.market_stats.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();
    m
}

/// Read an anchor zero-copy account by **unaligned** copy (see fuzz/e2e-svm for
/// the alignment rationale; velocity zero-copy structs hold u128/i128 so a
/// reference cast at `data[8..]` panics on the x86_64 host).
fn read_zc<T: bytemuck::Pod>(ctx: &TestContext, pk: &Pubkey) -> Option<T> {
    let acct = ctx.get_account(pk).ok()?;
    let size = std::mem::size_of::<T>();
    // An account that EXISTS but is too small is a host/on-chain LAYOUT DRIFT
    // (host `size_of::<T>` diverged from the deployed .so, e.g. a stale vendored
    // IDL or an un-rebuilt .so). Fail LOUDLY: silently returning None here would
    // skip every invariant that reads through this helper and turn the whole
    // harness green with zero checks executed. Genuinely-absent accounts still
    // return None via the `.ok()?` above.
    assert!(
        acct.data.len() >= 8 + size,
        "layout drift: account {pk} has {} data bytes, need >= {} (8 + size_of::<{}>); \
         re-run `bash fuzz/sync-idls.sh` and rebuild target/deploy/velocity.so",
        acct.data.len(),
        8 + size,
        std::any::type_name::<T>(),
    );
    Some(bytemuck::pod_read_unaligned::<T>(&acct.data[8..8 + size]))
}

/// Set a deterministic clock (litesvm's default `unix_timestamp` can be 0,
/// which would make interest-elapsed math a no-op). Uses the same `Clock` type
/// crucible's context uses.
fn set_clock(ctx: &mut TestContext, unix_timestamp: i64, slot: u64) {
    use anchor_lang::prelude::Clock;
    let clock = Clock {
        slot,
        epoch_start_timestamp: unix_timestamp,
        epoch: 0,
        leader_schedule_epoch: 0,
        unix_timestamp,
    };
    ctx.set_sysvar::<Clock>(&clock);
}

// Create a funded system account + return a signer keypair.
fn funded_signer(ctx: &mut TestContext) -> Rc<Keypair> {
    let kp = Rc::new(Keypair::new());
    ctx.create_account()
        .pubkey(kp.pubkey())
        .lamports(10_000_000_000)
        .owner(system_program_id())
        .create()
        .unwrap();
    kp
}

// ===========================================================================
// HARNESS A1 — PR #272: deposit_into_spot_market_revenue_pool ignores the
// market-scoped SpotOperation::Deposit pause.
//
// Inject a spot market with the Deposit bit set, then credit its revenue pool.
// FIXED => rejected with MarketActionPaused (6146). MASTER => the credit lands
// (no such gate on this path), so error_code is None (or anything != 6146).
// ===========================================================================
#[cfg(feature = "regr_272_revenue_pool_deposit")]
mod regr_272_rev_deposit {
    use super::*;

    #[derive(Clone)]
    pub struct Fx {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub spot_market_pda: Pubkey,
        pub spot_vault_pda: Pubkey,
        pub authority: Rc<Keypair>,
        pub user_token_account: Pubkey,
    }

    #[fuzz_fixture]
    impl Fx {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();
            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO).unwrap();
            set_clock(&mut ctx, 1_700_000_000, 1000);

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // Exchange fully Active — the bug is a *market-scoped* gate, not the
            // exchange-wide one.
            let mut state = build_state(signer_pda, signer_nonce, 0);
            inject(&mut ctx, state_pda, &mut state);

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
            // Pause DEPOSIT for this market.
            spot_market.paused_operations = SpotOperation::Deposit as u8;
            inject(&mut ctx, spot_market_pda, &mut spot_market);

            ctx.create_token_account()
                .pubkey(spot_vault_pda)
                .mint(usdc_mint)
                .token_owner(signer_pda)
                .amount(0)
                .create()
                .unwrap();

            let authority = funded_signer(&mut ctx);
            let user_token_account = Keypair::new().pubkey();
            ctx.create_token_account()
                .pubkey(user_token_account)
                .mint(usdc_mint)
                .token_owner(authority.pubkey())
                .amount(INITIAL_USDC)
                .create()
                .unwrap();

            Fx {
                ctx,
                program_id,
                state_pda,
                spot_market_pda,
                spot_vault_pda,
                authority,
                user_token_account,
            }
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }
    }
}

#[cfg(feature = "regr_272_revenue_pool_deposit")]
use regr_272_rev_deposit::Fx as Fx272Rev;

#[cfg(feature = "regr_272_revenue_pool_deposit")]
#[crucible_fuzz]
fn regr_272_revenue_pool_deposit(fixture: &mut Fx272Rev, #[range(0..1u8)] _unused: u8) {
    let amount = 1_000 * QUOTE_PRECISION as u64;
    let outcome = fixture
        .ctx
        .raw_call(Instruction {
            program_id: fixture.program_id,
            accounts: vec![
                AccountMeta::new_readonly(fixture.state_pda, false),
                AccountMeta::new(fixture.spot_market_pda, false),
                AccountMeta::new(fixture.authority.pubkey(), true),
                AccountMeta::new(fixture.spot_vault_pda, false),
                AccountMeta::new(fixture.user_token_account, false),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data: ix_data(
                D_DEPOSIT_INTO_SPOT_MARKET_REVENUE_POOL,
                &amount.to_le_bytes(),
            ),
        })
        .signers(&[&fixture.authority])
        .send();

    let code = outcome.ok().and_then(|o| o.error_code());
    fuzz_assert_eq!(
        code,
        Some(E_MARKET_ACTION_PAUSED),
        "PR #272: deposit_into_spot_market_revenue_pool credited a Deposit-paused \
         market (expected MarketActionPaused=6146, got error_code={:?})",
        code
    );
}

// ===========================================================================
// HARNESS A2 — PR #272: direct settle_revenue_to_insurance_fund ignores the
// market-scoped SpotOperation::Withdraw pause (only the exchange-wide withdraw
// pause was gated). FIXED => rejected with MarketWithdrawPaused (6149) right
// after the market-index check. MASTER => proceeds past the (missing) gate into
// the revenue-settle timing/transfer logic, returning a different code.
// ===========================================================================
#[cfg(feature = "regr_272_settle_revenue_if")]
mod regr_272_settle_rev {
    use super::*;

    #[derive(Clone)]
    pub struct Fx {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub signer_pda: Pubkey,
        pub spot_market_pda: Pubkey,
        pub spot_vault_pda: Pubkey,
        pub if_vault_pda: Pubkey,
    }

    #[fuzz_fixture]
    impl Fx {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();
            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO).unwrap();
            set_clock(&mut ctx, 1_700_000_000, 1000);

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // Exchange withdraw NOT paused — the bug is the market-scoped bit.
            let mut state = build_state(signer_pda, signer_nonce, 0);
            inject(&mut ctx, state_pda, &mut state);

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
            // Pause WITHDRAW for this market; configure a revenue-settle period so
            // master gets past the `revenue_settle_period > 0` guard.
            spot_market.paused_operations = SpotOperation::Withdraw as u8;
            spot_market.insurance_fund.revenue_settle_period = 3600;
            inject(&mut ctx, spot_market_pda, &mut spot_market);

            ctx.create_token_account()
                .pubkey(spot_vault_pda)
                .mint(usdc_mint)
                .token_owner(signer_pda)
                .amount(1_000_000_000)
                .create()
                .unwrap();
            ctx.create_token_account()
                .pubkey(if_vault_pda)
                .mint(usdc_mint)
                .token_owner(signer_pda)
                .amount(0)
                .create()
                .unwrap();

            Fx {
                ctx,
                program_id,
                state_pda,
                signer_pda,
                spot_market_pda,
                spot_vault_pda,
                if_vault_pda,
            }
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }
    }
}

#[cfg(feature = "regr_272_settle_revenue_if")]
use regr_272_settle_rev::Fx as Fx272Settle;

#[cfg(feature = "regr_272_settle_revenue_if")]
#[crucible_fuzz]
fn regr_272_settle_revenue_if(fixture: &mut Fx272Settle, #[range(0..1u8)] _unused: u8) {
    let args = 0u16.to_le_bytes(); // spot_market_index
    let outcome = fixture
        .ctx
        .raw_call(Instruction {
            program_id: fixture.program_id,
            accounts: vec![
                AccountMeta::new_readonly(fixture.state_pda, false),
                AccountMeta::new(fixture.spot_market_pda, false),
                AccountMeta::new(fixture.spot_vault_pda, false),
                AccountMeta::new_readonly(fixture.signer_pda, false), // velocity_signer
                AccountMeta::new(fixture.if_vault_pda, false),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data: ix_data(D_SETTLE_REVENUE_TO_INSURANCE_FUND, &args),
        })
        .send();

    let code = outcome.ok().and_then(|o| o.error_code());
    fuzz_assert_eq!(
        code,
        Some(E_MARKET_WITHDRAW_PAUSED),
        "PR #272: settle_revenue_to_insurance_fund ran on a Withdraw-paused market \
         (expected MarketWithdrawPaused=6149, got error_code={:?})",
        code
    );
}

// ===========================================================================
// HARNESS B1 — PR #276: the shared update_spot_market_cumulative_interest keeps
// accruing interest under an exchange-wide FundingPaused on every caller but the
// dedicated crank. Driven here through `deposit` (which calls the shared helper
// early, before crediting). Requires nonzero borrow utilization + elapsed time
// so interest would accrue.
//
// FIXED => the helper freezes accrual under FundingPaused: cumulative_*_interest
// is unchanged (only TWAP stats advance). MASTER => interest accrues, so
// cumulative_borrow_interest grows past its injected seed.
// ===========================================================================
#[cfg(feature = "regr_276_spot_interest")]
mod regr_276_interest {
    use super::*;

    // FundingPaused = 0b00100000.
    const EXCHANGE_FUNDING_PAUSED: u8 = 0b0010_0000;
    const CLOCK_TS: i64 = 1_700_000_000;
    // 1 day of elapsed interest.
    pub const LAST_INTEREST_TS: u64 = (CLOCK_TS as u64) - 86_400;

    #[derive(Clone)]
    pub struct Fx {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub spot_market_pda: Pubkey,
        pub spot_vault_pda: Pubkey,
        pub user_pda: Pubkey,
        pub stats_pda: Pubkey,
        pub authority: Rc<Keypair>,
        pub user_token_account: Pubkey,
    }

    #[fuzz_fixture]
    impl Fx {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();
            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO).unwrap();
            set_clock(&mut ctx, CLOCK_TS, 1000);

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // Exchange-wide FundingPaused set.
            let mut state = build_state(signer_pda, signer_nonce, EXCHANGE_FUNDING_PAUSED);
            inject(&mut ctx, state_pda, &mut state);

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
            // Nonzero utilization (~50%) + a live borrow rate so interest accrues.
            spot_market.deposit_balance = 1_000_000 * SPOT_BALANCE_PRECISION;
            spot_market.borrow_balance = 500_000 * SPOT_BALANCE_PRECISION;
            spot_market.optimal_utilization = SPOT_UTILIZATION_PRECISION as u32;
            spot_market.optimal_borrow_rate = (SPOT_RATE_PRECISION / 10) as u32; // 10%
            spot_market.max_borrow_rate = SPOT_RATE_PRECISION as u32; // 100%
            spot_market.last_interest_ts = LAST_INTEREST_TS;
            inject(&mut ctx, spot_market_pda, &mut spot_market);

            // Over-fund the vault so the deposit's solvency check passes despite
            // the large injected balances.
            ctx.create_token_account()
                .pubkey(spot_vault_pda)
                .mint(usdc_mint)
                .token_owner(signer_pda)
                .amount(2_000_000 * QUOTE_PRECISION as u64)
                .create()
                .unwrap();

            let authority = funded_signer(&mut ctx);
            let user_token_account = Keypair::new().pubkey();
            ctx.create_token_account()
                .pubkey(user_token_account)
                .mint(usdc_mint)
                .token_owner(authority.pubkey())
                .amount(INITIAL_USDC)
                .create()
                .unwrap();

            let sub0 = 0u16.to_le_bytes();
            let (stats_pda, _) = Pubkey::find_program_address(
                &[b"user_stats", authority.pubkey().as_ref()],
                &program_id,
            );
            let (user_pda, _) = Pubkey::find_program_address(
                &[b"user", authority.pubkey().as_ref(), &sub0],
                &program_id,
            );

            // initialize_user_stats
            let _ = ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new(stats_pda, false),
                        AccountMeta::new(state_pda, false),
                        AccountMeta::new_readonly(authority.pubkey(), false),
                        AccountMeta::new(authority.pubkey(), true),
                        AccountMeta::new_readonly(rent_sysvar_id(), false),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_INITIALIZE_USER_STATS, &[]),
                })
                .signers(&[&authority])
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
                        AccountMeta::new_readonly(authority.pubkey(), false),
                        AccountMeta::new(authority.pubkey(), true),
                        AccountMeta::new_readonly(rent_sysvar_id(), false),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_INITIALIZE_USER, &init_args),
                })
                .signers(&[&authority])
                .send();

            Fx {
                ctx,
                program_id,
                state_pda,
                spot_market_pda,
                spot_vault_pda,
                user_pda,
                stats_pda,
                authority,
                user_token_account,
            }
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }
    }
}

#[cfg(feature = "regr_276_spot_interest")]
use regr_276_interest::Fx as Fx276Interest;

#[cfg(feature = "regr_276_spot_interest")]
#[crucible_fuzz]
fn regr_276_spot_interest(fixture: &mut Fx276Interest, #[range(0..1u8)] _unused: u8) {
    // Drive a small deposit — it calls the shared helper before crediting.
    let mut args = Vec::new();
    args.extend_from_slice(&0u16.to_le_bytes()); // market_index
    args.extend_from_slice(&(1_000 * QUOTE_PRECISION as u64).to_le_bytes());
    args.push(0u8); // reduce_only

    let outcome = fixture
        .ctx
        .raw_call(Instruction {
            program_id: fixture.program_id,
            accounts: vec![
                AccountMeta::new_readonly(fixture.state_pda, false),
                AccountMeta::new(fixture.user_pda, false),
                AccountMeta::new(fixture.stats_pda, false),
                AccountMeta::new_readonly(fixture.authority.pubkey(), true),
                AccountMeta::new(fixture.spot_vault_pda, false),
                AccountMeta::new(fixture.user_token_account, false),
                AccountMeta::new_readonly(token_program_id(), false),
                AccountMeta::new(fixture.spot_market_pda, false),
            ],
            data: ix_data(D_DEPOSIT, &args),
        })
        .signers(&[&fixture.authority])
        .send();

    let deposit_ok = outcome.map(|o| o.is_success()).unwrap_or(false);
    // If the deposit itself failed, the harness setup is wrong (interest would
    // never be reached) — surface it loudly rather than silently pass.
    fuzz_assert!(
        deposit_ok,
        "regr_276_spot_interest setup: deposit under FundingPaused must succeed \
         (it is not funding-gated); interest accrual is unreachable otherwise"
    );

    let sm = fixture
        .read_spot_market()
        .expect("spot market readable after deposit");
    // FIXED: FundingPaused freezes accrual -> cumulative_borrow_interest is the
    // injected seed. MASTER: the shared helper accrues interest -> it grows.
    fuzz_assert_eq!(
        sm.cumulative_borrow_interest,
        SPOT_CUMULATIVE_INTEREST_PRECISION,
        "PR #276: spot interest accrued during an exchange FundingPaused \
         (cumulative_borrow_interest={} != frozen seed={})",
        sm.cumulative_borrow_interest,
        SPOT_CUMULATIVE_INTEREST_PRECISION
    );
}

#[cfg(feature = "regr_276_spot_interest")]
impl Fx276Interest {
    fn read_spot_market(&self) -> Option<SpotMarket> {
        read_zc::<SpotMarket>(&self.ctx, &self.spot_market_pda)
    }
}

// ===========================================================================
// HARNESS B2 — PR #276: update_perp_bid_ask_twap keeps running (advancing a
// market's funding-input TWAP state) when that market's PerpOperation::UpdateFunding
// is paused — the crank only ever blocked the exchange-wide FundingPaused via its
// access_control. The fix early-returns Ok(()) right after loading the perp
// market, *before* touching the clock, oracle map, keeper stats, or TWAP state.
//
// Setup: market UpdateFunding paused, exchange NOT funding-paused (so the
// funding_not_paused access_control passes on both builds). keeper_stats has no
// IF stake, so on MASTER the crank runs past the (missing) market gate and fails
// the keeper IF-stake / oracle checks; on the FIXED build it never gets there
// and returns Ok. So: FIXED => success (error_code None); MASTER => error.
// ===========================================================================
#[cfg(feature = "regr_276_bid_ask_twap")]
mod regr_276_twap {
    use super::*;

    #[derive(Clone)]
    pub struct Fx {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub perp_market_pda: Pubkey,
        pub oracle: Pubkey,
        pub keeper_stats_pda: Pubkey,
        pub authority: Rc<Keypair>,
    }

    #[fuzz_fixture]
    impl Fx {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();
            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO).unwrap();
            set_clock(&mut ctx, 1_700_000_000, 1000);

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // Exchange NOT funding-paused — the bug is the market-scoped bit.
            let mut state = build_state(signer_pda, signer_nonce, 0);
            inject(&mut ctx, state_pda, &mut state);

            let mi0 = 0u16.to_le_bytes();
            let (perp_market_pda, _) =
                Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);
            let mut perp_market = build_perp_market(perp_market_pda, 0);
            // Pause UpdateFunding for this market (oracle stays the $1 QuoteAsset
            // path so valid_oracle_for_perp_market matches Pubkey::default()).
            perp_market.paused_operations = PerpOperation::UpdateFunding as u8;
            inject(&mut ctx, perp_market_pda, &mut perp_market);

            // keeper_stats: authority-bound UserStats, no IF stake.
            let authority = funded_signer(&mut ctx);
            let (keeper_stats_pda, _) = Pubkey::find_program_address(
                &[b"user_stats", authority.pubkey().as_ref()],
                &program_id,
            );
            let mut keeper_stats = UserStats::default();
            keeper_stats.authority = anchor_pk(authority.pubkey());
            inject(&mut ctx, keeper_stats_pda, &mut keeper_stats);

            Fx {
                ctx,
                program_id,
                state_pda,
                perp_market_pda,
                oracle: Pubkey::default(), // == perp_market.oracle (QuoteAsset)
                keeper_stats_pda,
                authority,
            }
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }
    }
}

#[cfg(feature = "regr_276_bid_ask_twap")]
use regr_276_twap::Fx as Fx276Twap;

#[cfg(feature = "regr_276_bid_ask_twap")]
#[crucible_fuzz]
fn regr_276_bid_ask_twap(fixture: &mut Fx276Twap, #[range(0..1u8)] _unused: u8) {
    let outcome = fixture
        .ctx
        .raw_call(Instruction {
            program_id: fixture.program_id,
            accounts: vec![
                AccountMeta::new_readonly(fixture.state_pda, false),
                AccountMeta::new(fixture.perp_market_pda, false),
                AccountMeta::new_readonly(fixture.oracle, false),
                AccountMeta::new_readonly(fixture.keeper_stats_pda, false),
                AccountMeta::new_readonly(fixture.authority.pubkey(), true),
            ],
            data: ix_data(D_UPDATE_PERP_BID_ASK_TWAP, &[]),
        })
        .signers(&[&fixture.authority])
        .send();

    let code = outcome.ok().and_then(|o| o.error_code());
    // FIXED: the market-scoped UpdateFunding pause makes the crank early-return
    // Ok before touching oracle/keeper/TWAP state — success, no error. MASTER:
    // the crank runs past the (missing) gate and fails downstream (keeper IF
    // stake / oracle), i.e. it *did* process a funding-paused market.
    fuzz_assert!(
        code.is_none(),
        "PR #276: update_perp_bid_ask_twap processed a market with UpdateFunding \
         paused instead of no-op'ing (fix early-returns Ok; got error_code={:?})",
        code
    );
}

// ===========================================================================
// DEFERRED regressions — reached only through trade / fill / liquidation
// machinery this injection tier cannot cheaply stand up. Documented here with
// the precise blocker (see also the [features] note in Cargo.toml).
//
// PR #272:
//   * regr_272_transfer_pools_pause — `handle_transfer_pools` credits deposit
//     legs via `..._with_limits` (withdraw-side gate only); the fix adds
//     `enforce_transfer_pools_deposit_admission` on the deposit_to / borrow_from
//     credits. Reaching that credit requires FOUR coherent spot markets (four
//     distinct `spot_market_vault` PDAs), two Users with reconcilable balances,
//     and a transfer that first passes the source-side debit/limits gate — i.e.
//     a real multi-market lending state, not a single injected market.
//   * regr_272_swap_begin_pause — `handle_liquidate_spot_with_swap_begin`'s new
//     Deposit/Withdraw-pause checks sit after the two
//     `update_spot_market_cumulative_interest` calls, but the instruction is one
//     half of a flash-loan begin/end pair (`flash_loan_amount` is only set by
//     begin, and begin requires a matching end in the same tx via instruction
//     introspection) driven against a cross-spot *liquidatable* user. Standing up
//     the introspected sibling ix + an underwater two-market user is P8-liq work.
//   * regr_272_opportunistic_revenue — the *skip* (not error) added to
//     `attempt_settle_revenue_to_insurance_fund` only fires when that helper is
//     folded inside IF-add / liquidation / pnl-deficit resolution; there is no
//     standalone entrypoint, so reaching it needs one of those host flows
//     (staked IF + revenue-settle timing, or a full liquidation).
//
// PR #276:
//   * regr_276_funding_settle — `settle_funding_payment` folds a position's
//     accrued funding into its quote balance. The fold is only OBSERVABLE if the
//     host instruction COMMITS (returns Ok): a revert discards the mutation. The
//     side-effect callers are fills (orders.rs), perp-position transfer, PnL
//     settle (pnl.rs), and liquidation (liquidation.rs). The fold requires
//     `base_asset_amount != 0`, which pulls in the full margin / AMM / oracle
//     path (reserve_price, price-band, settlement math) on a default-injected
//     AMM — that reverts, discarding the mutation, so nothing commits without the
//     matching engine + a coherent AMM. NOTE: the sibling freeze mechanism (the
//     shared `update_spot_market_cumulative_interest` gaining the same
//     `funding_paused` freeze) IS reproduced by `regr_276_spot_interest` via a
//     committing `deposit`, so this exact class of divergence is covered.
// ===========================================================================
