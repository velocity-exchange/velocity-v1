//! `e2e-svm-revshare` — two previously-deferred Crucible regression harnesses
//! for the velocity revenue-share (builder-code) subsystem.
//!
//! Both load the compiled velocity `.so` into LiteSVM, inject a coherent
//! protocol state (State + USDC quote SpotMarket + PerpMarket + the
//! RevenueShareEscrow / RevenueShareOrder / RevenueShare / User accounts the
//! revenue-share paths read), and then drive the *real* instruction under test
//! (`place_and_take_perp_order_v1`, `settle_multiple_pnls`) with `ctx.raw_call`.
//! A v1 take routes through the market's CLOB book, so `regr_256` installs one.
//! Account reads use the velocity host-library zero-copy structs via
//! `pod_read_unaligned` (see `read_zc`), never `read_zero_copy_account` (which
//! panics on velocity's u128-aligned structs on unaligned LiteSVM data).
//!
//! Each harness encodes the FIXED invariant, so it FAILS on current (pre-fix)
//! master — reproducing the audit bug — and flips to passing once the PR merges.
//!
//!  * `regr_273_revenue_share_subaccount_redirect` (PR #273 / OtterSec #45):
//!    `load_revenue_share_map` keyed the recipient `User` by stored authority
//!    only, so a permissionless settle caller could supply a *sibling*
//!    subaccount (`sub_account_id != 0`) of the builder authority and redirect
//!    the accrued builder reward. The fix requires `sub_account_id == 0`.
//!
//!  * `regr_256_reused_order_id_stale_builder_fee` (PR #256 / OtterSec #49):
//!    `add_builder_order` writes a `RevenueShareOrder` keyed to
//!    `user.next_order_id` *before* the order is built; when placement
//!    soft-skips on an expired `max_ts` it returns before consuming
//!    `next_order_id` or setting `HasBuilder`, so the stale row persists for an
//!    id the next placement reuses. The fix clears the row when placement bails
//!    and gates the fill-time lookup on the order's live `is_has_builder()`.

#![allow(dead_code)]

use {
    anchor_lang::{AnchorSerialize, Discriminator},
    crucible_fuzzer::*,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::rc::Rc,
    velocity::{
        math::constants::{
            AMM_RESERVE_PRECISION, PEG_PRECISION, QUOTE_PRECISION, SPOT_BALANCE_PRECISION,
            SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
        },
        state::{
            market_status::MarketStatus, oracle::OracleSource, perp_market::PerpMarket,
            spot_market::SpotMarket, state::State, user::User,
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

// Anchor instruction discriminators (from the canonical IDL).
const D_INITIALIZE_USER_STATS: [u8; 8] = [254, 243, 72, 98, 251, 130, 168, 213];
const D_INITIALIZE_USER: [u8; 8] = [111, 17, 185, 250, 60, 122, 38, 254];
const D_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const D_SETTLE_MULTIPLE_PNLS: [u8; 8] = [127, 66, 117, 57, 40, 50, 152, 127];

// System / builtin program ids.
fn system_program_id() -> Pubkey {
    Pubkey::new_from_array([0u8; 32])
}
fn rent_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111")
}
fn token_program_id() -> Pubkey {
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
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

/// Convert a solana-3.0 Pubkey into the anchor-lang Pubkey type used by the
/// velocity structs (byte-identical; go through bytes to be version-agnostic).
fn anchor_pk(p: Pubkey) -> anchor_lang::prelude::Pubkey {
    anchor_lang::prelude::Pubkey::new_from_array(p.to_bytes())
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

/// Inject a raw velocity-owned account with the given bytes (used for
/// non-zero-copy Anchor `Account`s such as `RevenueShareEscrow`, whose Vec
/// fields are serialized with borsh, not bytemuck).
fn inject_raw(ctx: &mut TestContext, pda: Pubkey, data: &[u8]) {
    ctx.create_account()
        .pubkey(pda)
        .owner(velocity_program_id())
        .lamports(1_000_000_000)
        .data(data)
        .create()
        .expect("inject raw account");
}

/// Read an anchor zero-copy account by **unaligned** copy.
///
/// `TestContext::read_zero_copy_account` uses `bytemuck::from_bytes` (a *reference*
/// cast), which requires the `data[8..]` slice to meet `align_of::<T>()`. Velocity's
/// zero-copy structs contain `u128`/`i128` fields, so on the x86_64 host they have
/// alignment 16, while LiteSVM stores account data in a `Vec<u8>` whose `+8` offset
/// is essentially never 16-aligned — the reference cast then panics. `pod_read_unaligned`
/// copies the bytes out instead, which is correct and alignment-agnostic.
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

// ---------------------------------------------------------------------------
// Injected-state builders (coherent USDC quote market + perp market + State)
// ---------------------------------------------------------------------------

fn build_state(signer: Pubkey, signer_nonce: u8) -> State {
    let mut s = State::default();
    s.signer = anchor_pk(signer);
    s.signer_nonce = signer_nonce;
    s.exchange_status = 0; // Active
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
    m.withdraw_guard_threshold = u64::MAX;
    m.order_step_size = 1;
    m.order_tick_size = 1;
    m.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();
    m
}

fn build_perp_market(pubkey: Pubkey, quote_spot_index: u16) -> PerpMarket {
    let mut m = PerpMarket::default();
    m.pubkey = anchor_pk(pubkey);
    m.oracle = anchor_pk(Pubkey::new_from_array([0u8; 32])); // $1 quote-asset path
    m.market_index = 0;
    m.quote_spot_market_index = quote_spot_index;
    m.status = MarketStatus::Active;
    m.oracle_source = OracleSource::QuoteAsset;
    m.margin_ratio_initial = 1_000; // 10%
    m.margin_ratio_maintenance = 500; // 5%
    m.order_step_size = 1_000_000;
    m.order_tick_size = 1;
    m.market_stats.min_order_size = 1_000_000;
    m.market_stats.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();
    m
}

// ===========================================================================
// HARNESS 2 — regr_273_revenue_share_subaccount_redirect (PR #273 / OtterSec #45)
//
// `load_revenue_share_map` keyed the builder/referrer recipient `User` account by
// its stored authority only, never checking `sub_account_id`. Because the escrow
// records only the authority, a permissionless `settle_pnl`/`settle_multiple_pnls`
// caller could supply any *sibling* subaccount (`sub_account_id != 0`) of that
// authority as the recipient and redirect the accrued builder reward to it. The
// fix requires the recipient to be `sub_account_id == 0` (new error
// `InvalidRevenueShareRecipient`, 6360); both call sites wrap the map load in
// `.ok()`, so a rejected map means the sweep is skipped and fees stay accrued.
//
// Setup (all via injection): builder codes enabled; a Settlement-status perp
// market with a funded pnl_pool; a trader `User` (authority T, sub 0) that holds
// a pure-PnL expired position (base == 0, quote > 0); a `RevenueShareEscrow`
// (authority T) holding one Completed builder order with `fees_accrued > 0` and
// one approved builder B; and — passed to the revenue-share map — a *sibling*
// builder `User` at `sub_account_id = 1` (authority B) plus B's `RevenueShare`
// account. The position makes `settle_expired_position` settle for real, which
// is what lets the sweep run: a caller with no position settles nothing, and
// the handler skips the sweep on that signal.
//
// The harness drives a REAL `settle_multiple_pnls([0], MustSettle)` and asserts
// the sibling recipient's quote spot balance is UNCHANGED (== 0). On fixed
// master the map load rejects the sibling and the sweep is skipped, so it stays
// 0. On pre-fix master the sweep pays the sibling, so its balance becomes
// `fees_accrued` worth — the assertion fails, reproducing the bug.
// ===========================================================================

#[cfg(feature = "regr_273_revenue_share_subaccount_redirect")]
mod regr_273 {
    use {
        super::*,
        velocity::state::{
            revenue_share::{
                BuilderInfo, RevenueShare, RevenueShareEscrow, RevenueShareOrder,
                RevenueShareOrderBitFlag,
            },
            spot_market::SpotBalanceType,
            user::MarketType,
        },
    };

    /// Builder fee accrued in the escrow's completed order (tiny; just needs > 0).
    const FEES_ACCRUED: u64 = 5_000;

    #[derive(Clone)]
    pub struct Regr273 {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub spot_market_pda: Pubkey,
        pub spot_vault_pda: Pubkey,
        pub perp_market_pda: Pubkey,
        pub trader_pda: Pubkey,
        pub trader_kp: Rc<Keypair>,
        pub escrow_pda: Pubkey,
        /// The non-canonical sibling recipient (sub_account_id = 1) that a
        /// pre-fix master pays; the fix must leave it untouched.
        pub sibling_builder_pda: Pubkey,
        pub builder_rev_share_pda: Pubkey,
    }

    #[fuzz_fixture]
    impl Regr273 {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();

            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO)
                .expect("add velocity.so");

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // State: exchange Active, builder codes enabled.
            let mut state = build_state(signer_pda, signer_nonce);
            state.feature_bit_flags = velocity::state::state::FeatureBitFlags::BuilderCodes as u8; // 0b100
            inject(&mut ctx, state_pda, &mut state);

            // USDC mint + quote spot market + vault (SettlePNL requires the vault PDA).
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
            // The pnl_pool is a Deposit-type claim against this market, so
            // `transfer_spot_balances` (the sweep) requires
            // `deposit_balance >= pnl_pool.scaled_balance`. Keep them coherent.
            let pnl_pool_scaled = 1_000_000 * SPOT_BALANCE_PRECISION;
            spot_market.deposit_balance = pnl_pool_scaled;
            inject(&mut ctx, spot_market_pda, &mut spot_market);
            // Vault must cover the depositors' claim (`validate_spot_market_vault_amount`);
            // fund it well above the pnl_pool token value.
            ctx.create_token_account()
                .pubkey(spot_vault_pda)
                .mint(usdc_mint)
                .token_owner(signer_pda)
                .amount(1_000_000_000_000_000)
                .create()
                .unwrap();

            // Perp market 0: Settlement status with a deep-past expiry so the
            // settle path is reached, and a well-funded pnl_pool to back the sweep.
            let (perp_market_pda, _) =
                Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);
            let mut perp_market = build_perp_market(perp_market_pda, 0);
            perp_market.status = MarketStatus::Settlement;
            perp_market.expiry_ts = -100;
            perp_market.expiry_price = 1_000_000; // $1
            perp_market.pnl_pool.scaled_balance = pnl_pool_scaled;
            inject(&mut ctx, perp_market_pda, &mut perp_market);

            // Trader (settle caller): authority T, sub 0, pool_id 0, holding a
            // pure-PnL expired position (base == 0, quote > 0). The zero base
            // skips the margin calc, and the claim is small next to the
            // pnl_pool, so the settle succeeds and the sweep is reached.
            let trader_kp = Rc::new(Keypair::new());
            ctx.create_account()
                .pubkey(trader_kp.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();
            let trader_pda = Keypair::new().pubkey();
            let mut trader = User::default();
            trader.authority = anchor_pk(trader_kp.pubkey());
            trader.sub_account_id = 0;
            trader.pool_id = 0;
            trader.perp_positions[0].market_index = 0;
            trader.perp_positions[0].base_asset_amount = 0;
            trader.perp_positions[0].quote_asset_amount = 1_000_000; // +$1 claim
            inject(&mut ctx, trader_pda, &mut trader);

            // Builder authority B and its NON-canonical sibling recipient (sub 1).
            let builder_authority = Keypair::new().pubkey();
            let sibling_builder_pda = Keypair::new().pubkey();
            let mut sibling = User::default();
            sibling.authority = anchor_pk(builder_authority);
            sibling.sub_account_id = 1; // <-- the redirect target; fix must reject this
            sibling.pool_id = 0;
            inject(&mut ctx, sibling_builder_pda, &mut sibling);

            // Builder's RevenueShare account (authority B).
            let builder_rev_share_pda = Keypair::new().pubkey();
            let mut rev_share = RevenueShare::default();
            rev_share.authority = anchor_pk(builder_authority);
            inject(&mut ctx, builder_rev_share_pda, &mut rev_share);

            // RevenueShareEscrow (authority T): one Completed builder order with
            // fees_accrued > 0 keyed to builder_idx 0, and one approved builder B.
            let mut completed_order = RevenueShareOrder::new(
                0,   // builder_idx
                0,   // sub_account_id (trader's)
                1,   // order_id (irrelevant once completed)
                100, // fee_tenth_bps
                MarketType::Perp,
                0, // market_index
                RevenueShareOrderBitFlag::Completed as u8,
                0, // user_order_index
            );
            completed_order.fees_accrued = FEES_ACCRUED;

            let escrow = RevenueShareEscrow {
                authority: anchor_pk(trader_kp.pubkey()),
                referrer: anchor_pk(Pubkey::default()),
                reserved_fixed: [0u8; 24],
                padding0: 0,
                orders: vec![completed_order],
                padding1: 0,
                approved_builders: vec![BuilderInfo {
                    authority: anchor_pk(builder_authority),
                    max_fee_tenth_bps: 1_000,
                    padding: [0u8; 6],
                }],
            };
            let mut escrow_bytes = RevenueShareEscrow::DISCRIMINATOR.to_vec();
            escrow.serialize(&mut escrow_bytes).unwrap();
            let escrow_pda = Keypair::new().pubkey();
            inject_raw(&mut ctx, escrow_pda, &escrow_bytes);

            let _ = SpotBalanceType::Deposit; // (imported for the assertion below)

            Regr273 {
                ctx,
                program_id,
                state_pda,
                spot_market_pda,
                spot_vault_pda,
                perp_market_pda,
                trader_pda,
                trader_kp,
                escrow_pda,
                sibling_builder_pda,
                builder_rev_share_pda,
            }
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }

        /// Quote (market 0) spot balance of the sibling recipient User.
        pub fn sibling_quote_balance(&self) -> u64 {
            match read_zc::<User>(&self.ctx, &self.sibling_builder_pda) {
                Some(u) => u
                    .spot_positions
                    .iter()
                    .find(|sp| sp.market_index == 0)
                    .map(|sp| sp.scaled_balance)
                    .unwrap_or(0),
                None => 0,
            }
        }
    }
}

#[cfg(feature = "regr_273_revenue_share_subaccount_redirect")]
use regr_273::Regr273;

#[cfg(feature = "regr_273_revenue_share_subaccount_redirect")]
#[crucible_fuzz]
fn regr_273_revenue_share_subaccount_redirect(fixture: &mut Regr273, #[range(0..1u8)] _unused: u8) {
    // Baseline: the sibling recipient has no balance before the sweep.
    let before = fixture.sibling_quote_balance();

    // settle_multiple_pnls(market_indexes = [0], mode = MustSettle).
    let mut args = Vec::new();
    args.extend_from_slice(&1u32.to_le_bytes()); // Vec<u16> length = 1
    args.extend_from_slice(&0u16.to_le_bytes()); // market_index 0
    args.push(0u8); // SettlePnlMode::MustSettle

    let outcome = fixture
        .ctx
        .raw_call(Instruction {
            program_id: fixture.program_id,
            accounts: vec![
                AccountMeta::new_readonly(fixture.state_pda, false),
                AccountMeta::new(fixture.trader_pda, false),
                AccountMeta::new_readonly(fixture.trader_kp.pubkey(), true),
                AccountMeta::new_readonly(fixture.spot_vault_pda, false),
                // remaining: quote spot market (w), perp market (w), escrow (w),
                // then the revenue-share map (sibling builder User + RevenueShare, w).
                AccountMeta::new(fixture.spot_market_pda, false),
                AccountMeta::new(fixture.perp_market_pda, false),
                AccountMeta::new(fixture.escrow_pda, false),
                AccountMeta::new(fixture.sibling_builder_pda, false),
                AccountMeta::new(fixture.builder_rev_share_pda, false),
            ],
            data: ix_data(D_SETTLE_MULTIPLE_PNLS, &args),
        })
        .signers(&[&fixture.trader_kp])
        .send();

    // The instruction itself must succeed in both worlds: on master the sweep
    // pays; on the fix the map load fails safe (`.ok()` -> None) and the sweep
    // is skipped — either way settle_multiple_pnls returns Ok.
    let code = outcome.as_ref().ok().and_then(|o| o.error_code());
    fuzz_assert!(
        outcome.is_ok() && code.is_none(),
        "settle_multiple_pnls unexpectedly failed (error_code={:?})",
        code
    );

    let after = fixture.sibling_quote_balance();

    // FIXED invariant: a non-canonical sibling subaccount (sub_account_id != 0)
    // must NEVER receive the swept builder reward. On pre-fix master the sweep
    // credits it, so `after > before` — this assertion fails, reproducing the
    // redirect bug.
    fuzz_assert_eq!(
        after,
        before,
        "PR #273: builder reward was redirected to a non-canonical sibling subaccount \
         (sub_account_id=1): quote balance {} -> {}",
        before,
        after
    );
}

// ===========================================================================
// HARNESS 1 — regr_256_reused_order_id_stale_builder_fee (PR #256 / OtterSec #49)
//
// See the crate-level doc. Two real `place_and_take_perp_order_v1` calls:
//   Part A: a take carrying a builder code with an expired `max_ts`. It runs the
//     real `add_builder_order`, which writes a RevenueShareOrder keyed to
//     `next_order_id`, and then soft-skips. The fix clears the row, so no Open
//     builder row lingers for the order id `next_order_id` still names.
//   Part B: a NON-builder take that reuses that order id and fills against the
//     $1 vAMM. The fix gates the fill-time lookup on `is_has_builder()`, so the
//     reused id is never charged a builder fee.
//
// A clock bump to a positive `unix_timestamp` makes the `max_ts < now` skip
// reachable, because LiteSVM's genesis clock is 0.
// ===========================================================================

#[cfg(feature = "regr_256_reused_order_id_stale_builder_fee")]
mod regr_256 {
    use {
        super::*,
        velocity::state::revenue_share::{
            BuilderInfo, RevenueShare, RevenueShareEscrow, RevenueShareOrder,
        },
    };

    #[derive(Clone)]
    pub struct Regr256 {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub spot_market_pda: Pubkey,
        pub spot_vault_pda: Pubkey,
        pub perp_market_pda: Pubkey,
        pub usdc_mint: Pubkey,
        pub signer_pda: Pubkey,
        pub trader_pda: Pubkey,
        pub trader_stats_pda: Pubkey,
        pub trader_kp: Rc<Keypair>,
        pub trader_token: Pubkey,
        pub escrow_pda: Pubkey,
        pub builder_authority: Pubkey,
        pub clob: velocity_fuzz_common::clob::ClobAccounts,
    }

    #[fuzz_fixture]
    impl Regr256 {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();

            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO)
                .expect("add velocity.so");

            // LiteSVM's genesis clock has unix_timestamp == 0, which would make
            // the `max_ts < now` soft-skip in `place_perp_order` unreachable
            // (any positive max_ts is >= 0). Push the clock to a large positive
            // timestamp so an expired `max_ts = 1` reliably triggers the skip.
            {
                use anchor_lang::prelude::Clock;
                let cur_slot = ctx.slot();
                ctx.set_sysvar(&Clock {
                    slot: cur_slot,
                    epoch_start_timestamp: 1_000_000_000,
                    epoch: 0,
                    leader_schedule_epoch: 0,
                    unix_timestamp: 1_000_000_000,
                });
            }

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // The warm admin installs the market's CLOB book.
            let admin = Keypair::new();
            ctx.create_account()
                .pubkey(admin.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();

            let mut state = build_state(signer_pda, signer_nonce);
            state.feature_bit_flags = velocity::state::state::FeatureBitFlags::BuilderCodes as u8;
            state.warm_admin = anchor_pk(admin.pubkey());
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
            inject(&mut ctx, spot_market_pda, &mut spot_market);
            ctx.create_token_account()
                .pubkey(spot_vault_pda)
                .mint(usdc_mint)
                .token_owner(signer_pda)
                .amount(0)
                .create()
                .unwrap();

            let (perp_market_pda, _) =
                Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);
            let mut perp_market = build_perp_market(perp_market_pda, 0);
            // Give the AMM coherent $1 reserves (peg 1x, equal base/quote) so the
            // Part-B `place_and_take` fill has real liquidity to fill against — the
            // reserve price ($1) matches the QuoteAsset oracle, so no oracle
            // account is needed. `build_perp_market`'s zero-reserve AMM would
            // MathError in `reserve_price`.
            perp_market.amm.base_asset_reserve = 100 * AMM_RESERVE_PRECISION;
            perp_market.amm.quote_asset_reserve = 100 * AMM_RESERVE_PRECISION;
            // net position is 0, so terminal reserves must equal spot reserves
            // (AMM::validate).
            perp_market.amm.terminal_quote_asset_reserve = 100 * AMM_RESERVE_PRECISION;
            perp_market.amm.sqrt_k = 100 * AMM_RESERVE_PRECISION;
            perp_market.amm.peg_multiplier = PEG_PRECISION; // $1
            perp_market.amm.max_base_asset_reserve = 1_000 * AMM_RESERVE_PRECISION;
            perp_market.amm.min_base_asset_reserve = AMM_RESERVE_PRECISION / 10;
            perp_market.amm.max_slippage_ratio = 50;
            perp_market.amm.max_fill_reserve_fraction = 100;
            perp_market.amm.base_spread = 1_000;
            // max_spread must satisfy base_spread < max_spread < margin_ratio_initial*100.
            perp_market.amm.max_spread = 50_000;
            // A funded pnl_pool so builder-fee bookkeeping during the fill has room.
            perp_market.amm.fee_pool.scaled_balance = 1_000 * SPOT_BALANCE_PRECISION;
            perp_market.pnl_pool.scaled_balance = 1_000 * SPOT_BALANCE_PRECISION;
            perp_market.quoter_slab = anchor_pk(velocity_fuzz_common::clob::quoter_slab_pda());
            inject(&mut ctx, perp_market_pda, &mut perp_market);

            // Trader: real user + user_stats via real init instructions so the
            // place/place_and_take paths operate on genuine accounts.
            let trader_kp = Rc::new(Keypair::new());
            ctx.create_account()
                .pubkey(trader_kp.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();
            let trader_token = Keypair::new().pubkey();
            ctx.create_token_account()
                .pubkey(trader_token)
                .mint(usdc_mint)
                .token_owner(trader_kp.pubkey())
                .amount(1_000_000 * QUOTE_PRECISION as u64)
                .create()
                .unwrap();

            let (trader_stats_pda, _) = Pubkey::find_program_address(
                &[b"user_stats", trader_kp.pubkey().as_ref()],
                &program_id,
            );
            let (trader_pda, _) = Pubkey::find_program_address(
                &[b"user", trader_kp.pubkey().as_ref(), &mi0],
                &program_id,
            );

            // `initialize_user` creates the relay liquidation-coverage account
            // alongside the user, so its list carries the PDA.
            let (trader_pda_conditions, _) = Pubkey::find_program_address(
                &[b"user_conditions", trader_pda.as_ref()],
                &program_id,
            );

            let _ = ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new(trader_stats_pda, false),
                        AccountMeta::new(state_pda, false),
                        AccountMeta::new_readonly(trader_kp.pubkey(), false),
                        AccountMeta::new(trader_kp.pubkey(), true),
                        AccountMeta::new_readonly(rent_sysvar_id(), false),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_INITIALIZE_USER_STATS, &[]),
                })
                .signers(&[&trader_kp])
                .send();

            let mut init_args = Vec::new();
            init_args.extend_from_slice(&0u16.to_le_bytes());
            init_args.extend_from_slice(&[0u8; 32]);
            let _ = ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new(trader_pda, false),
                        AccountMeta::new(trader_pda_conditions, false),
                        AccountMeta::new(trader_stats_pda, false),
                        AccountMeta::new(state_pda, false),
                        AccountMeta::new_readonly(trader_kp.pubkey(), false),
                        AccountMeta::new(trader_kp.pubkey(), true),
                        AccountMeta::new_readonly(rent_sysvar_id(), false),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_INITIALIZE_USER, &init_args),
                })
                .signers(&[&trader_kp])
                .send();

            // Deposit collateral so orders can pass the margin gate.
            let mut dep_args = Vec::new();
            dep_args.extend_from_slice(&0u16.to_le_bytes());
            dep_args.extend_from_slice(&(500_000 * QUOTE_PRECISION as u64).to_le_bytes());
            dep_args.push(0u8);
            let _ = ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new_readonly(state_pda, false),
                        AccountMeta::new(trader_pda, false),
                        AccountMeta::new(trader_stats_pda, false),
                        AccountMeta::new_readonly(trader_kp.pubkey(), true),
                        AccountMeta::new(spot_vault_pda, false),
                        AccountMeta::new(trader_token, false),
                        AccountMeta::new_readonly(token_program_id(), false),
                        AccountMeta::new(spot_market_pda, false),
                    ],
                    data: ix_data(D_DEPOSIT, &dep_args),
                })
                .signers(&[&trader_kp])
                .send();

            // RevenueShareEscrow (authority = trader) with one approved builder B
            // and an empty order list (add_builder_order will populate it live).
            let builder_authority = Keypair::new().pubkey();
            let escrow = RevenueShareEscrow {
                authority: anchor_pk(trader_kp.pubkey()),
                referrer: anchor_pk(Pubkey::default()),
                reserved_fixed: [0u8; 24],
                padding0: 0,
                orders: vec![RevenueShareOrder::default(); 8],
                padding1: 0,
                approved_builders: vec![BuilderInfo {
                    authority: anchor_pk(builder_authority),
                    max_fee_tenth_bps: 1_000,
                    padding: [0u8; 6],
                }],
            };
            let mut escrow_bytes = RevenueShareEscrow::DISCRIMINATOR.to_vec();
            escrow.serialize(&mut escrow_bytes).unwrap();
            let escrow_pda = Keypair::new().pubkey();
            inject_raw(&mut ctx, escrow_pda, &escrow_bytes);

            let _ = RevenueShare::default();

            let clob = velocity_fuzz_common::clob::install(
                &mut ctx,
                &admin,
                state_pda,
                perp_market_pda,
                perp_market.order_step_size,
                perp_market.market_stats.min_order_size,
                trader_pda,
            );

            Regr256 {
                ctx,
                program_id,
                state_pda,
                spot_market_pda,
                spot_vault_pda,
                perp_market_pda,
                usdc_mint,
                signer_pda,
                trader_pda,
                trader_stats_pda,
                trader_kp,
                trader_token,
                escrow_pda,
                builder_authority,
                clob,
            }
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }

        /// `place_and_take_perp_order_v1` for the trader. The remaining accounts
        /// are the markets, no makers, the trader's escrow, then the market's
        /// book entry.
        pub fn take_ix(&self, params: velocity::state::order_params::OrderParams) -> Instruction {
            use anchor_lang::{InstructionData, ToAccountMetas};

            let mut accounts = velocity::accounts::PlaceAndTakeV1 {
                state: self.state_pda,
                user: self.trader_pda,
                user_stats: self.trader_stats_pda,
                authority: self.trader_kp.pubkey(),
                quoter_slab: self.clob.quoter_slab,
                clob_market: self.clob.book,
                clob_program: self.clob.program,
            }
            .to_account_metas(None);
            accounts.push(AccountMeta::new(self.spot_market_pda, false));
            accounts.push(AccountMeta::new(self.perp_market_pda, false));
            accounts.push(AccountMeta::new(self.escrow_pda, false));
            accounts.extend(self.clob.route_metas());
            Instruction {
                program_id: self.program_id,
                accounts,
                data: velocity::instruction::PlaceAndTakePerpOrderV1 {
                    args: velocity::instructions::PlaceAndTakePerpOrderV1Args {
                        params,
                        success_condition: None,
                    },
                }
                .data(),
            }
        }

        /// Parse the escrow account bytes and return, for each order slot, a
        /// tuple `(order_id, is_open, is_completed, fees_accrued)`.
        pub fn escrow_orders(&self) -> Vec<(u32, bool, bool, u64)> {
            let acct = match self.ctx.get_account(&self.escrow_pda) {
                Ok(a) => a,
                Err(_) => return vec![],
            };
            let data = &acct.data;
            // Layout after the 8-byte discriminator:
            //   fixed: authority(32) + referrer(32) + reserved_fixed(24) = 88
            //   padding0(4), orders_len(4), then orders[..]
            let base = 8 + 88;
            if data.len() < base + 8 {
                return vec![];
            }
            let orders_len = u32::from_le_bytes([
                data[base + 4],
                data[base + 5],
                data[base + 6],
                data[base + 7],
            ]) as usize;
            let order_size = std::mem::size_of::<RevenueShareOrder>();
            let start = base + 8;
            let mut out = Vec::new();
            for i in 0..orders_len {
                let s = start + i * order_size;
                if data.len() < s + order_size {
                    break;
                }
                let order: RevenueShareOrder =
                    bytemuck::pod_read_unaligned(&data[s..s + order_size]);
                let is_open = (order.bit_flags & 0b0000_0001) != 0;
                let is_completed = (order.bit_flags & 0b0000_0010) != 0;
                out.push((order.order_id, is_open, is_completed, order.fees_accrued));
            }
            out
        }
    }
}

#[cfg(feature = "regr_256_reused_order_id_stale_builder_fee")]
use regr_256::Regr256;

#[cfg(feature = "regr_256_reused_order_id_stale_builder_fee")]
#[crucible_fuzz]
fn regr_256_reused_order_id_stale_builder_fee(fixture: &mut Regr256, #[range(0..1u8)] _unused: u8) {
    use velocity::{
        controller::position::PositionDirection,
        state::{
            order_params::{OrderParams, PostOnlyParam},
            user::{MarketType, OrderType},
        },
    };

    // --- Part A: a BUILDER take with an expired `max_ts`. ---
    // `add_builder_order` writes a RevenueShareOrder keyed to next_order_id (=1),
    // and the take then soft-skips on `max_ts < now` before consuming the id.
    let builder_params = OrderParams {
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: PositionDirection::Long,
        base_asset_amount: 10_000_000,
        price: 1_050_000,
        market_index: 0,
        post_only: PostOnlyParam::None,
        max_ts: Some(1), // expired: 1 << now
        builder_idx: Some(0),
        builder_fee_tenth_bps: Some(1000),
        ..Default::default()
    };
    let place_outcome = fixture
        .ctx
        .raw_call(fixture.take_ix(builder_params))
        .signers(&[&fixture.trader_kp])
        .send();

    let place_code = place_outcome.as_ref().ok().and_then(|o| o.error_code());
    fuzz_assert!(
        place_outcome.is_ok() && place_code.is_none(),
        "builder take (expired max_ts) should soft-skip and return Ok (error_code={:?})",
        place_code
    );

    // The escrow as the soft-skipped take left it.
    let orders_after_place = fixture.escrow_orders();

    // --- Part B: a NON-builder take reuses order id 1 and fills against the
    // vAMM. A fill-time lookup keyed only by (sub_account_id, order_id) would
    // match a stale row and charge the builder fee into it. ---
    let taker_params = OrderParams {
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction: PositionDirection::Long,
        base_asset_amount: 1_000_000_000,
        price: 1_050_000, // $1.05 — crosses the ~$1.0005 AMM ask
        market_index: 0,
        post_only: PostOnlyParam::None,
        ..Default::default()
    };
    let fill_outcome = fixture
        .ctx
        .raw_call(fixture.take_ix(taker_params))
        .signers(&[&fixture.trader_kp])
        .send();

    let filled = read_zc::<User>(&fixture.ctx, &fixture.trader_pda)
        .map(|u| u.perp_positions[0].base_asset_amount > 0)
        .unwrap_or(false);
    fuzz_assert!(
        filled,
        "the non-builder take did not fill, so Part B checks nothing (logs: {:?})",
        fill_outcome.as_ref().map(|o| o.logs().to_vec())
    );

    let orders_after_fill = fixture.escrow_orders();

    // Sanity: the soft-skipped take did NOT consume the order id, so the
    // non-builder take reused id 1 and left next_order_id at 2.
    let taker_reused_id = read_zc::<User>(&fixture.ctx, &fixture.trader_pda)
        .map(|u| u.next_order_id == 2)
        .unwrap_or(false);
    fuzz_assert!(
        taker_reused_id,
        "expected the soft-skip to leave next_order_id unconsumed so the taker reuses order id 1"
    );

    // --- Assertion (Part A): no Open builder row lingers for the un-consumed
    // order id 1 after the soft-skipped take. ---
    let orphan = orders_after_place
        .iter()
        .find(|(order_id, is_open, is_completed, _)| *is_open && !*is_completed && *order_id == 1);
    fuzz_assert!(
        orphan.is_none(),
        "PR #256: a stale Open builder-order row lingers for the un-consumed order id 1 \
         after the take soft-skipped on expired max_ts (escrow orders: {:?})",
        orders_after_place
    );

    // --- Assertion (Part B): the reused non-builder order id 1 is NOT charged a
    // builder fee. ---
    let charged = orders_after_fill
        .iter()
        .find(|(order_id, _, _, fees_accrued)| *order_id == 1 && *fees_accrued > 0);
    fuzz_assert!(
        charged.is_none(),
        "PR #256: a reused non-builder order (id 1) was charged a stale builder fee at fill \
         time (escrow orders after fill: {:?})",
        orders_after_fill
    );
}

#[cfg(test)]
mod smoke {
    #[test]
    fn placeholder() {}
}
