//! P8 `e2e-svm-liq` — the liquidation / bankruptcy stateful Crucible harness.
//!
//! Sibling of P7 `e2e-svm`; copies its proven SVM-integration patterns
//! (`declare_fuzz_program!` IDL ingest, `raw_call` instruction building with
//! anchor discriminators + borsh args, `get_anchor_account_bytes` account
//! injection, `read_zero_copy_account` host-struct reads) and specializes them
//! for the liquidation / insurance-fund / bankruptcy surface.
//!
//! ## Focus: families III (loss-tranche waterfall), V (margin monotonicity),
//! ## VII (IF-share math), checked end-to-end after every action.
//!
//! Setup injects a coherent protocol state — State + a USDC quote SpotMarket
//! (deposits + a small borrow, vault funded) + a PerpMarket carrying standing
//! `pending_if_fee` / `pending_protocol_fee` and a floored bankruptcy tranche +
//! three users, one of them injected *underwater and cross-margin bankrupt*
//! (a negative-quote perp position with no base + a spot borrow), so the
//! liquidation and bankruptcy code paths are reachable.
//!
//! ## Regression harnesses (assert the FIXED behavior ⇒ FAIL on current master)
//!  * `regr_267_perp_before_spot` — PR #267: `resolve_spot_bankruptcy` must
//!    revert (`PerpBankruptcyMustPrecedeSpot`, 6361) while the user still has a
//!    pending cross-margin perp bankruptcy. On master the guard is absent, so
//!    the call succeeds — the harness fires.
//!  * `regr_255_floored_if_tranche` — PR #255: a permissionless fee sweep can
//!    never drain the token backing of the floored `pending_if_fee` tranche.
//!    Driven at the state level through the real `sweep_market_fees` controller
//!    (same fn the `sweep_perp_market_fees` ix calls) to sidestep the ix's
//!    oracle/price-band gates and reproduce deterministically.
//!  * `regr_275_equity_floor_liq` — PR #275: a liquidator whose authority-wide
//!    equity breaker is tripped is barred (`EquityBelowFloor`) from the
//!    position-acquiring `liquidate_perp`. Scaffolded; see the header note on
//!    the file for the reproduction caveat.
//!  * `regr_306_amm_phantom_funding` — PR #306: `resolve_perp_bankruptcy` must
//!    resync the AMM's funding stamp past the socialization bump. On master it
//!    doesn't, so the next funding settlement credits the net-flat AMM phantom
//!    `total_fee_minus_distributions` (≈ the socialized loss). Forces a
//!    socializing bankruptcy, then computes the AMM's next funding payment from
//!    the post-resolve state and asserts it is zero.

use {
    crucible_fuzzer::*,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::rc::Rc,
    velocity::{
        math::{
            constants::{
                BASE_PRECISION, PEG_PRECISION, PRICE_PRECISION, QUOTE_PRECISION,
                SPOT_BALANCE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
            },
            time::legacy_slot_duration_u8,
        },
        state::{
            market_status::MarketStatus,
            oracle::OracleSource,
            perp_market::PerpMarket,
            spot_market::{SpotBalanceType, SpotMarket},
            state::State,
            user::{User, UserStatus},
        },
    },
};

// Generated types/schemas, read straight from the canonical IDL that the SDK
// also consumes. We only use `register_schemas()` for richer crash output;
// instruction building goes through `raw_call`.
crucible_idl_gen::declare_fuzz_program!(velocity_idl = "../../packages/sdk/src/idl/velocity.json");

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VELOCITY_SO: &str = "../../target/deploy/velocity.so";

/// user 0 = liquidator / IF staker, user 1 = healthy bystander,
/// user 2 = the injected underwater + cross-margin-bankrupt victim.
const NUM_USERS: usize = 3;
const VICTIM_IDX: usize = 2;
const LIQUIDATOR_IDX: usize = 0;
/// A second victim: LEVERED but NOT bankrupt (user index 1 was unused).
///
/// The fixture's original victim is injected already `Bankrupt`, and every
/// ordinary liquidation handler refuses a bankrupt user
/// (`ErrorCode::UserBankrupt`, e.g. controller/liquidation.rs:116). So the
/// harness could only ever reach `resolve_*_bankruptcy` — the entire
/// liquidation pipeline that PRODUCES a bankruptcy was unreachable. This user
/// starts solvent but thinly collateralised, so compounding sub-1% oracle moves
/// can walk them under maintenance the way the market really does it.
const LEVERED_IDX: usize = 1;

const INITIAL_USDC: u64 = 1_000_000 * QUOTE_PRECISION as u64; // 1,000,000 USDC

/// pnl-pool token backing injected into the perp market (1,000 USDC).
const PNL_POOL_TOKENS: u64 = 1_000 * QUOTE_PRECISION as u64;
/// standing `pending_protocol_fee` — large enough that the master fee sweep
/// drains the whole pnl pool (and thus the floored IF tranche's backing).
const PENDING_PROTOCOL_FEE: u128 = 5_000 * QUOTE_PRECISION;
/// standing `pending_if_fee` (500 USDC) — the first-loss bankruptcy tranche.
const PENDING_IF_FEE: u128 = 500 * QUOTE_PRECISION;
/// bankruptcy IF floor: 30% of OI notional (PERCENTAGE_PRECISION = 100%).
const BANKRUPTCY_IF_FLOOR_PCT: u32 = 300_000;
/// injected open interest: 10 base units long, matched short.
const OI_BASE: i128 = 10 * BASE_PRECISION as i128;

/// the victim's spot borrow (1 USDC) and negative perp pnl (-750 USDC).
const VICTIM_BORROW_TOKENS: u64 = QUOTE_PRECISION as u64;
/// The perp debt must EXCEED `PENDING_IF_FEE` (500 USDC), or the first
/// bankruptcy tranche — `min(|loss|, pending_if_fee)`, controller/
/// liquidation.rs:4356-4370, with no floor cap — absorbs the whole loss,
/// `loss_after_pending` is 0, and tranches 2/3/4 stay dead no matter what
/// `quote_max_insurance` says. It was -1 USDC, which is why the entire IF
/// waterfall below tranche 1 was unreachable.
///
/// 750 USDC leaves a 250 USDC residual: tranche 2 draws it from the IF vault,
/// and if the vault is already drawn down, tranche 3 (the AMM clawback, capped
/// at `amm_protocol_fees_received` = 0 here) falls through to tranche 4
/// socialization. The dedicated `regr_306` harness makes the same move by hand
/// for exactly this reason.
const VICTIM_PERP_QUOTE: i64 = -(750 * QUOTE_PRECISION as i64);
/// IF vault seed funding.
const IF_VAULT_FUNDING: u64 = 1_000 * QUOTE_PRECISION as u64;

// Anchor instruction discriminators (from the canonical IDL).
const D_INITIALIZE_USER_STATS: [u8; 8] = [254, 243, 72, 98, 251, 130, 168, 213];
const D_INITIALIZE_USER: [u8; 8] = [111, 17, 185, 250, 60, 122, 38, 254];
const D_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const D_SET_USER_STATUS_BEING_LIQUIDATED: [u8; 8] = [106, 133, 160, 206, 193, 171, 192, 194];
const D_LIQUIDATE_PERP: [u8; 8] = [75, 35, 119, 247, 191, 18, 139, 2];
const D_LIQUIDATE_SPOT: [u8; 8] = [107, 0, 128, 41, 35, 229, 251, 18];
const D_LIQUIDATE_PERP_PNL_FOR_DEPOSIT: [u8; 8] = [237, 75, 198, 235, 233, 186, 75, 35];
const D_LIQUIDATE_PERP_WITH_FILL: [u8; 8] = [95, 111, 124, 105, 86, 169, 187, 34];
const D_RESOLVE_PERP_BANKRUPTCY: [u8; 8] = [224, 16, 176, 214, 162, 213, 183, 222];
const D_RESOLVE_SPOT_BANKRUPTCY: [u8; 8] = [124, 194, 240, 254, 198, 213, 52, 122];
const D_RESOLVE_PERP_PNL_DEFICIT: [u8; 8] = [168, 204, 68, 150, 159, 126, 95, 148];
const D_INITIALIZE_IF_STAKE: [u8; 8] = [187, 179, 243, 70, 248, 90, 92, 147];
const D_ADD_IF_STAKE: [u8; 8] = [251, 144, 115, 11, 222, 47, 62, 236];
const D_REQUEST_REMOVE_IF_STAKE: [u8; 8] = [142, 70, 204, 92, 73, 106, 180, 52];
const D_REMOVE_IF_STAKE: [u8; 8] = [128, 166, 142, 9, 254, 187, 143, 174];

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

/// Inject an anchor zero-copy account (discriminator + struct) at `pda`, owned
/// by the velocity program (copied verbatim from P7).
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

/// Convert a token amount (mint precision, 6 decimals) into a spot scaled
/// balance at the injected cumulative interest = SPOT_CUMULATIVE_INTEREST_PRECISION.
/// scaled = tokens * SPOT_BALANCE_PRECISION / 10^6.
fn tokens_to_scaled(tokens: u128) -> u128 {
    tokens
        .saturating_mul(SPOT_BALANCE_PRECISION)
        .saturating_div(QUOTE_PRECISION)
}

fn anchor_pk(p: Pubkey) -> anchor_lang::prelude::Pubkey {
    anchor_lang::prelude::Pubkey::new_from_array(p.to_bytes())
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
    /// The perp market's oracle: a velocity-owned `PythLazerOracle` account.
    ///
    /// This harness previously used `oracle == Pubkey::default()` +
    /// `OracleSource::QuoteAsset` — the hard-coded $1 path — and reached
    /// liquidations only by INJECTING already-liquidatable state. With a real,
    /// movable oracle the fuzzer can drive an account underwater the way the
    /// market actually does it, which is the difference between exercising the
    /// liquidation handlers and exercising the conditions that lead to them.
    perp_oracle_pda: Pubkey,
    users: Vec<UserAcct>,
    if_stake_pda: Pubkey,
    /// Monotonic publish_time for injected oracle updates.
    oracle_seq: u64,
}

// ---------------------------------------------------------------------------
// Injected-state builders
// ---------------------------------------------------------------------------

/// A `PythLazerOracle` at `price` with exponent -6 (so the raw field IS the
/// PRICE_PRECISION-scaled price). `posted_slot` is the only staleness source the
/// program reads: `oracle_delay = clock_slot - posted_slot`.
fn build_pyth_lazer_oracle(
    price: i64,
    conf: u64,
    posted_slot: u64,
    publish_time: u64,
) -> velocity::state::pyth_lazer_oracle::PythLazerOracle {
    velocity::state::pyth_lazer_oracle::PythLazerOracle {
        price,
        publish_time,
        posted_slot,
        exponent: -6,
        _padding: [0u8; 4],
        conf,
    }
}

fn build_state(signer: Pubkey, signer_nonce: u8) -> State {
    let mut s = State::default();
    s.signer = anchor_pk(signer);
    s.signer_nonce = signer_nonce;
    s.exchange_status = 0; // Active
    s.number_of_spot_markets = 1;
    s.number_of_markets = 1;
    // Liquidation config used by liquidate_perp_pnl_for_deposit.
    s.liquidation_margin_buffer_ratio = 50; // 0.5%
                                            // 10% of LIQUIDATION_PCT_PRECISION (= 10_000, math/constants.rs:46).
                                            //
                                            // WAS `10_000` with the comment "10% (PERCENTAGE-ish)" — that is 100%, not
                                            // 10%. `calculate_max_pct_to_liquidate` (math/liquidation.rs:453-489) then
                                            // pinned `pct_freeable` at 10_000 via the `.min()` on the next line and the
                                            // `liquidation_margin_freed` ramp could never bind, so every liquidation was
                                            // one full-size step and the partial path was dead.
                                            //
                                            // Note this only bites when `margin_shortage >= 50 * QUOTE_PRECISION`:
                                            // liquidation.rs:461-463 short-circuits smaller shortages to full precision,
                                            // so both the ramped and the accelerated branch stay reachable.
    s.initial_pct_to_liquidate = 1_000;
    s.liquidation_duration = legacy_slot_duration_u8(150);
    s
}

fn build_spot_market_usdc(f: &Fixture) -> SpotMarket {
    let mut m = SpotMarket::default();
    m.pubkey = anchor_pk(f.spot_market_pda);
    m.oracle = anchor_pk(Pubkey::new_from_array([0u8; 32])); // $1 quote path
    m.oracle_source = OracleSource::QuoteAsset;
    m.mint = anchor_pk(f.usdc_mint);
    m.vault = anchor_pk(f.spot_vault_pda);
    m.insurance_fund.vault = anchor_pk(f.if_vault_pda);
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
    // Aggregate balances: the perp pnl-pool deposit lives inside deposit_balance,
    // plus the victim's small borrow. depositors_claim = deposit - borrow.
    m.deposit_balance = tokens_to_scaled(PNL_POOL_TOKENS as u128);
    m.borrow_balance = tokens_to_scaled(VICTIM_BORROW_TOKENS as u128);
    // IF share bookkeeping (family VII invariant reads these).
    m.insurance_fund.total_shares = 0;
    m.insurance_fund.user_shares = 0;
    m.insurance_fund.if_fee_factor = 10_000;
    m
}

fn build_perp_market(f: &Fixture) -> PerpMarket {
    let mut m = PerpMarket::default();
    m.pubkey = anchor_pk(f.perp_market_pda);
    m.oracle = anchor_pk(f.perp_oracle_pda);
    m.market_index = 0;
    m.quote_spot_market_index = 0;
    m.status = MarketStatus::Active;
    m.oracle_source = OracleSource::PythLazer;
    m.margin_ratio_initial = 1_000; // 10%
    m.margin_ratio_maintenance = 500; // 5%
    m.order_step_size = 1_000_000;
    m.order_tick_size = 1;
    m.market_stats.min_order_size = 1_000_000;

    // Coherent AMM pricing at ~$1 so the sweep instruction's reserve_price /
    // price-band gate can pass (curve_update_intensity = 0 keeps the AMM
    // freshness gate off).
    m.amm.base_asset_reserve = 10_000 * BASE_PRECISION;
    m.amm.quote_asset_reserve = 10_000 * BASE_PRECISION;
    m.amm.sqrt_k = 10_000 * BASE_PRECISION;
    // Must match quote_asset_reserve for a balanced AMM (net position 0), or the
    // program's own AMM validation rejects the market with InvalidAmmDetected
    // ("terminal_quote_asset_reserve out of wack 0 != ...") and every
    // AMM-validating instruction fails before reaching its logic.
    m.amm.terminal_quote_asset_reserve = 10_000 * BASE_PRECISION;
    // Reserve bounds. `validate_for_fill` checks base_asset_reserve against
    // these per direction, and the defaults (both 0) fail every short-side fill
    // with InvalidAmmForFillDetected.
    m.amm.min_base_asset_reserve = 0;
    m.amm.max_base_asset_reserve = u64::MAX as u128;
    m.amm.concentration_coef = velocity::math::constants::MAX_CONCENTRATION_COEFFICIENT;
    m.amm.max_fill_reserve_fraction = 100;
    m.amm.peg_multiplier = PEG_PRECISION;
    m.amm.base_asset_amount_with_amm = 0;
    m.amm.curve_update_intensity = 0;

    // Standing fee tranches + floored bankruptcy tranche.
    m.fee_ledger.pending_protocol_fee = PENDING_PROTOCOL_FEE;
    m.fee_ledger.pending_if_fee = PENDING_IF_FEE;
    m.fee_ledger.pending_amm_provision = 0;
    m.fee_ledger.amm_protocol_fees_received = 0;
    m.bankruptcy_if_floor_pct = BANKRUPTCY_IF_FLOOR_PCT;

    // INSURANCE CLAIM. `PerpMarket::default()` zeroes all of these
    // (state/perp_market.rs:1591-1608, all QUOTE_PRECISION), which made the
    // ENTIRE second bankruptcy tranche dead: `if_payment` is
    // `min(loss_after_pending, if_vault_balance - 1, quote_max_insurance -
    // quote_settled_insurance)` (controller/liquidation.rs:4374-4386) and the
    // last term was always 0, so the shared IF vault was never drawn on — which
    // is the family this harness exists for.
    //
    // 2000 USDC is deliberately ABOVE the IF vault seed (IF_VAULT_FUNDING =
    // 1000 USDC): that makes `insurance_fund_vault_balance - 1` the BINDING
    // term, which is the guard `check_if_vault_not_overspent` exists to test. A
    // value below the vault would leave that invariant vacuous, because the
    // lifetime cap rather than the vault would always be the min.
    m.insurance_claim.quote_max_insurance = 2_000 * QUOTE_PRECISION as u64;
    m.insurance_claim.quote_settled_insurance = 0;
    // Per-period revenue cap for `resolve_perp_pnl_deficit`
    // (controller/insurance.rs:839-854). 250 USDC is deliberately SMALL relative
    // to `quote_max_insurance`, so a second draw in the same period hits
    // `MaxRevenueWithdrawPerPeriodReached` and warping past
    // `insurance_fund.revenue_settle_period` exercises the period reset. Eight
    // successful draws then exhaust the 2000 lifetime cap and cover
    // `MaxIFWithdrawReached` (insurance.rs:861-867). Both sides of both caps.
    m.insurance_claim.max_revenue_withdraw_per_period = 250 * QUOTE_PRECISION as u64;

    // Open interest so get_bankruptcy_if_floor() > 0 (OI conserved: long+short = 0).
    m.base_asset_amount_long = OI_BASE;
    m.base_asset_amount_short = -OI_BASE;
    m.net_unsettled_funding_pnl = 0;

    // `resolve_perp_pnl_deficit` (controller/insurance.rs:759-930) was 100%
    // dead, and NOT for the reason the field name suggests. Its gate chain, in
    // the order the handler evaluates it:
    //   1. `amm.is_underwater()` -> total_fee_minus_distributions < 0  (:768-773)
    //   2. pnl_pool_token_amount < net_user_pnl                        (:799-804)
    //   3. unrealized_pnl_max_imbalance > 0 && excess > 0              (:808-825)
    //   4. max_revenue_withdraw_per_period > 0                         (:849-854)
    //   5. quote_max_insurance - quote_settled_insurance > 0           (:861-867)
    // 4 and 5 are set above; without 1-3 the handler never reaches them, so
    // setting the insurance_claim fields alone would have changed nothing.
    //
    // `net_user_pnl` is `base_asset_amount_with_amm * price + quote_asset_amount
    // + net_unsettled_funding_pnl`. The AMM is net flat here (OI conservation
    // above), so `quote_asset_amount` IS the whole figure: 3000 USDC of user
    // claims against a 1000 USDC pnl pool is a genuine 2000 USDC deficit — the
    // exact state this instruction exists to resolve.
    //
    // `curve_update_intensity = 0` above additionally keeps `is_curve_update_enabled()`
    // false, so the handler skips the oracle-freshness block — one fewer blocker.
    m.amm.total_fee_minus_distributions = -(1_000 * QUOTE_PRECISION as i128);
    m.quote_asset_amount = 3_000 * QUOTE_PRECISION as i128;
    m.unrealized_pnl_max_imbalance = 1_000 * QUOTE_PRECISION as u64;

    // pnl-pool backing that the floored tranche relies on.
    m.pnl_pool.scaled_balance = tokens_to_scaled(PNL_POOL_TOKENS as u128);
    m.pnl_pool.market_index = 0;
    m.amm.fee_pool.scaled_balance = 0;
    m.amm.fee_pool.market_index = 0;

    // $1 oracle TWAP so the floor (OI × TWAP × pct) is nonzero.
    m.market_stats.historical_oracle_data.last_oracle_price = PRICE_PRECISION as i64;
    m.market_stats.historical_oracle_data.last_oracle_price_twap = PRICE_PRECISION as i64;
    m.market_stats
        .historical_oracle_data
        .last_oracle_price_twap_5min = PRICE_PRECISION as i64;
    m.market_stats.last_mark_price_twap = PRICE_PRECISION as u64;
    m.market_stats.last_mark_price_twap_5min = PRICE_PRECISION as u64;
    m
}

/// Craft the levered-but-solvent victim: a real long perp position with base
/// exposure, collateralised just above maintenance margin.
///
/// Maintenance is 5% of notional (`margin_ratio_maintenance = 500`). With 10
/// units of base at $1 the requirement is ~$0.50, so ~$0.62 of collateral sits
/// just above the line and a modest adverse move breaches it.
fn build_levered_user(authority: Pubkey, base_seed: &User) -> User {
    let mut u = *base_seed;
    u.authority = anchor_pk(authority);
    u.sub_account_id = 0;
    u.status = 0; // solvent, not flagged
    u.next_liquidation_id = 1;

    // Long 10 base opened at ~$1 (quote paid out, hence negative).
    u.perp_positions[0].market_index = 0;
    u.perp_positions[0].base_asset_amount = (10 * BASE_PRECISION) as i64;
    u.perp_positions[0].quote_asset_amount = -(10 * QUOTE_PRECISION as i64);
    u.perp_positions[0].open_orders = 0;
    u.perp_positions[0].open_bids = 0;
    u.perp_positions[0].open_asks = 0;

    // Thin collateral: enough to clear maintenance at $1, not much more.
    u.spot_positions[0].market_index = 0;
    u.spot_positions[0].balance_type = SpotBalanceType::Deposit;
    u.spot_positions[0].scaled_balance = tokens_to_scaled(620_000u128) as u64; // $0.62
    u.spot_positions[0].open_orders = 0;
    u
}

/// Craft the injected victim: cross-margin bankrupt, a negative-quote perp
/// position with no base (a "pending cross-margin perp bankruptcy" per
/// `has_pending_cross_margin_perp_bankruptcy`), and a spot borrow.
fn build_victim_user(authority: Pubkey, base_seed: &User) -> User {
    let mut u = *base_seed; // preserve authority/sub_account/discriminator-adjacent fields
    u.authority = anchor_pk(authority);
    u.sub_account_id = 0;
    // Bankrupt bit implies is_being_liquidated() too.
    u.status = UserStatus::BeingLiquidated as u8 | UserStatus::Bankrupt as u8;
    u.next_liquidation_id = 1;

    // Perp position 0: base 0, quote < 0, no open orders, not isolated.
    u.perp_positions[0].market_index = 0;
    u.perp_positions[0].base_asset_amount = 0;
    u.perp_positions[0].quote_asset_amount = VICTIM_PERP_QUOTE;
    u.perp_positions[0].open_orders = 0;
    u.perp_positions[0].open_bids = 0;
    u.perp_positions[0].open_asks = 0;

    // Spot position 0: borrow.
    u.spot_positions[0].market_index = 0;
    u.spot_positions[0].balance_type = SpotBalanceType::Borrow;
    u.spot_positions[0].scaled_balance = tokens_to_scaled(VICTIM_BORROW_TOKENS as u128) as u64;
    u.spot_positions[0].open_orders = 0;
    u.spot_positions[0].open_bids = 0;
    u.spot_positions[0].open_asks = 0;
    u
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

        let (signer_pda, signer_nonce) =
            Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
        let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

        let mi0 = 0u16.to_le_bytes();
        let (spot_market_pda, _) =
            Pubkey::find_program_address(&[b"spot_market", &mi0], &program_id);
        let (spot_vault_pda, _) =
            Pubkey::find_program_address(&[b"spot_market_vault", &mi0], &program_id);
        let (if_vault_pda, _) =
            Pubkey::find_program_address(&[b"insurance_fund_vault", &mi0], &program_id);
        let (perp_market_pda, _) =
            Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);

        // State singleton.
        let mut state = build_state(signer_pda, signer_nonce);
        inject(&mut ctx, state_pda, &mut state);

        // USDC mint (6 decimals).
        let usdc_mint = Keypair::new().pubkey();
        ctx.create_mint()
            .pubkey(usdc_mint)
            .mint_authority(signer_pda)
            .decimals(6)
            .create()
            .unwrap();

        // Vaults. Spot vault backs depositor claims (deposit - borrow) with
        // headroom; IF vault seeded so resolutions can draw it.
        ctx.create_token_account()
            .pubkey(spot_vault_pda)
            .mint(usdc_mint)
            .token_owner(signer_pda)
            .amount(PNL_POOL_TOKENS)
            .create()
            .unwrap();
        ctx.create_token_account()
            .pubkey(if_vault_pda)
            .mint(usdc_mint)
            .token_owner(signer_pda)
            .amount(IF_VAULT_FUNDING)
            .create()
            .unwrap();

        // Perp oracle: injected before the markets so `build_perp_market` can
        // point at it. `posted_slot` must track the clock or every read is stale.
        let (perp_oracle_pda, _) = Pubkey::find_program_address(
            &[
                velocity::state::pyth_lazer_oracle::PYTH_LAZER_ORACLE_SEED,
                &mi0,
            ],
            &program_id,
        );
        let slot0 = ctx.slot();
        let mut oracle = build_pyth_lazer_oracle(PRICE_PRECISION as i64, 0, slot0, 1);
        inject(&mut ctx, perp_oracle_pda, &mut oracle);

        // A partially-initialized fixture so builders can read the PDAs.
        let mut f = Fixture {
            ctx,
            program_id,
            signer_pda,
            usdc_mint,
            spot_market_pda,
            spot_vault_pda,
            if_vault_pda,
            perp_market_pda,
            perp_oracle_pda,
            users: Vec::new(),
            if_stake_pda: Pubkey::default(),
            oracle_seq: 2,
        };

        let mut spot_market = build_spot_market_usdc(&f);
        inject(&mut f.ctx, spot_market_pda, &mut spot_market);
        let mut perp_market = build_perp_market(&f);
        inject(&mut f.ctx, perp_market_pda, &mut perp_market);

        // Users: create keypair + token account, init user_stats + user.
        let mut users = Vec::new();
        for _ in 0..NUM_USERS {
            let kp = Rc::new(Keypair::new());
            f.ctx
                .create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();

            let token_account = Keypair::new().pubkey();
            f.ctx
                .create_token_account()
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

            let _ = f
                .ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new(stats_pda, false),
                        AccountMeta::new(state_pda, false),
                        AccountMeta::new_readonly(kp.pubkey(), false),
                        AccountMeta::new(kp.pubkey(), true),
                        AccountMeta::new_readonly(rent_sysvar_id(), false),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_INITIALIZE_USER_STATS, &[]),
                })
                .signers(&[&kp])
                .send();

            let mut init_args = Vec::new();
            init_args.extend_from_slice(&0u16.to_le_bytes());
            init_args.extend_from_slice(&[0u8; 32]);
            let _ = f
                .ctx
                .raw_call(Instruction {
                    program_id,
                    accounts: vec![
                        AccountMeta::new(user_pda, false),
                        AccountMeta::new(stats_pda, false),
                        AccountMeta::new(state_pda, false),
                        AccountMeta::new_readonly(kp.pubkey(), false),
                        AccountMeta::new(kp.pubkey(), true),
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
        f.users = users;

        // Overwrite the victim's User account body with the crafted bankrupt
        // state. `write_zero_copy_account` preserves the 8-byte anchor
        // discriminator that `initialize_user` wrote, so a default-seeded User
        // (authority set explicitly) is a coherent fresh account.
        let victim = f.users[VICTIM_IDX].clone();
        let crafted = build_victim_user(victim.keypair.pubkey(), &User::default());
        f.ctx
            .write_zero_copy_account(&victim.user_pda, &crafted)
            .expect("overwrite victim user");

        // The levered-but-solvent second victim (see LEVERED_IDX).
        let levered = f.users[LEVERED_IDX].clone();
        let levered_state = build_levered_user(levered.keypair.pubkey(), &User::default());
        f.ctx
            .write_zero_copy_account(&levered.user_pda, &levered_state)
            .expect("overwrite levered user");

        // Initialize an IF-stake account for the liquidator so IF stake actions
        // are reachable.
        let staker = f.users[LIQUIDATOR_IDX].clone();
        let (if_stake_pda, _) = Pubkey::find_program_address(
            &[
                b"insurance_fund_stake",
                staker.keypair.pubkey().as_ref(),
                &0u16.to_le_bytes(),
            ],
            &program_id,
        );
        let _ = f
            .ctx
            .raw_call(Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new_readonly(spot_market_pda, false),
                    AccountMeta::new(if_stake_pda, false),
                    AccountMeta::new(staker.stats_pda, false),
                    AccountMeta::new_readonly(state_pda, false),
                    AccountMeta::new_readonly(staker.keypair.pubkey(), true),
                    AccountMeta::new(staker.keypair.pubkey(), true),
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INITIALIZE_IF_STAKE, &0u16.to_le_bytes()),
            })
            .signers(&[&staker.keypair])
            .send();
        f.if_stake_pda = if_stake_pda;

        f
    }

    // ---- helpers -------------------------------------------------------

    fn state_pda(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"velocity_state"], &self.program_id).0
    }

    fn velocity_signer(&self) -> Pubkey {
        self.signer_pda
    }

    // ---- actions -------------------------------------------------------

    /// Deposit USDC into spot market 0 (funds a liquidator / bystander).
    pub fn action_deposit(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..500_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let bal = self.ctx.token_balance(&user.token_account);
        let amount = amount.min(bal);
        if amount == 0 {
            return false;
        }
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        args.push(0u8);
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
                    AccountMeta::new(self.spot_market_pda, false),
                ],
                data: ix_data(D_DEPOSIT, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Flag a user as being-liquidated (permissionless keeper action).
    pub fn action_set_being_liquidated(&mut self, #[range(0..NUM_USERS)] user_idx: usize) -> bool {
        let user = self.users[user_idx].clone();
        let liquidator = self.users[LIQUIDATOR_IDX].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new_readonly(liquidator.keypair.pubkey(), true),
                    // remaining: perp oracle + spot market (r) + perp market (r)
                    AccountMeta::new(self.perp_oracle_pda, false),
                    AccountMeta::new_readonly(self.spot_market_pda, false),
                    AccountMeta::new_readonly(self.perp_market_pda, false),
                ],
                data: ix_data(D_SET_USER_STATUS_BEING_LIQUIDATED, &[]),
            })
            .signers(&[&liquidator.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    fn liquidator_meta(&self) -> (UserAcct, UserAcct) {
        (
            self.users[LIQUIDATOR_IDX].clone(),
            self.users[VICTIM_IDX].clone(),
        )
    }

    /// Liquidator plus a fuzzer-chosen victim.
    ///
    /// The liquidator can never be its own victim, and targeting either the
    /// injected-bankrupt user or the levered-but-solvent one matters: the
    /// bankrupt user only admits `resolve_*`, the levered one only admits the
    /// ordinary liquidation handlers.
    fn liquidator_meta_for(&self, victim_idx: usize) -> (UserAcct, UserAcct) {
        // Map the liquidator's own index onto the LEVERED user, not the
        // bankrupt one. Every ordinary liquidation handler refuses a bankrupt
        // user, so with the old mapping 2 of the 3 selectable targets were
        // guaranteed failures and the fuzzer burned most of its liquidation
        // budget on `UserBankrupt`. Index VICTIM_IDX still reaches the bankrupt
        // user, which keeps that rejection branch covered.
        let v = if victim_idx == LIQUIDATOR_IDX {
            LEVERED_IDX
        } else {
            victim_idx
        };
        (self.users[LIQUIDATOR_IDX].clone(), self.users[v].clone())
    }

    /// liquidate_perp against the victim.
    pub fn action_liquidate_perp(
        &mut self,
        #[range(1..1_000_000_000u64)] max_base: u64,
        #[range(0..NUM_USERS)] victim_idx: usize,
    ) -> bool {
        let (liq, victim) = self.liquidator_meta_for(victim_idx);
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes()); // market_index
        args.extend_from_slice(&max_base.to_le_bytes());
        args.push(0u8); // limit_price None
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(liq.keypair.pubkey(), true),
                    AccountMeta::new(liq.user_pda, false),
                    AccountMeta::new(liq.stats_pda, false),
                    AccountMeta::new(victim.user_pda, false),
                    AccountMeta::new(victim.stats_pda, false),
                    // remaining: perp oracle + spot market (r) + perp market (w)
                    AccountMeta::new(self.perp_oracle_pda, false),
                    AccountMeta::new_readonly(self.spot_market_pda, false),
                    AccountMeta::new(self.perp_market_pda, false),
                ],
                data: ix_data(D_LIQUIDATE_PERP, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `liquidate_spot` (asset market 0, liability market 0).
    ///
    /// PRE-EXISTING BUG, now fixed: this account list omitted `liquidator_stats`
    /// (`LiquidateSpot` is `[state, authority, liquidator, liquidator_stats,
    /// user]`, instructions/keeper.rs:4089-4103). Every meta after `liquidator`
    /// shifted up one, so `victim.user_pda` landed in the `liquidator_stats`
    /// slot and Anchor failed to deserialize a `User` as `AccountLoader<UserStats>`.
    /// The handler was never entered AT ALL — the old comment's claim that this
    /// covered "the self-market reject path" was wrong; the reject path it
    /// actually covered was Anchor's. `fuzz/e2e-svm/src/main.rs` builds the same
    /// call with the correct list.
    ///
    /// With a single spot market this still cannot SUCCEED (a `SpotPosition`
    /// holds one balance type, so asset == liability == 0 always hits
    /// `WrongSpotBalanceType` at controller/liquidation.rs:2062-2066, or
    /// `UserBankrupt` for the seed victim). But the handler prologue now runs:
    /// account loading, both `SpotOperation::Liquidation` pause checks, the
    /// pool-id check, `check_spot_oracle_validity`, and the asset-side validates.
    pub fn action_liquidate_spot(
        &mut self,
        #[range(1..1_000_000u64)] max_liab: u64,
        #[range(0..NUM_USERS)] victim_idx: usize,
    ) -> bool {
        let (liq, victim) = self.liquidator_meta_for(victim_idx);
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes()); // asset market
        args.extend_from_slice(&0u16.to_le_bytes()); // liability market
        args.extend_from_slice(&(max_liab as u128).to_le_bytes());
        args.push(0u8); // limit_price None
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(liq.keypair.pubkey(), true),
                    AccountMeta::new(liq.user_pda, false),
                    // `liquidator_stats` — readonly here (unlike
                    // `LiquidateBorrowForPerpPnl`, which marks it `mut`).
                    AccountMeta::new_readonly(liq.stats_pda, false),
                    AccountMeta::new(victim.user_pda, false),
                    AccountMeta::new(self.perp_oracle_pda, false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.perp_market_pda, false),
                ],
                data: ix_data(D_LIQUIDATE_SPOT, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// liquidate_perp_pnl_for_deposit (pnl-settlement liquidation).
    pub fn action_liquidate_pnl_for_deposit(
        &mut self,
        #[range(1..1_000_000u64)] max_pnl: u64,
        #[range(0..NUM_USERS)] victim_idx: usize,
    ) -> bool {
        let (liq, victim) = self.liquidator_meta_for(victim_idx);
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes()); // perp market
        args.extend_from_slice(&0u16.to_le_bytes()); // spot market
        args.extend_from_slice(&(max_pnl as u128).to_le_bytes());
        args.push(0u8); // limit_price None
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(liq.keypair.pubkey(), true),
                    AccountMeta::new(liq.user_pda, false),
                    AccountMeta::new(liq.stats_pda, false),
                    AccountMeta::new(victim.user_pda, false),
                    AccountMeta::new(victim.stats_pda, false),
                    // remaining: perp oracle + spot market (w) + perp market (r)
                    AccountMeta::new(self.perp_oracle_pda, false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new_readonly(self.perp_market_pda, false),
                ],
                data: ix_data(D_LIQUIDATE_PERP_PNL_FOR_DEPOSIT, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// resolve_perp_bankruptcy against the victim.
    /// `liquidate_perp_with_fill(market_index)` — close a liquidatable perp
    /// position by filling it against the book rather than transferring it to
    /// the liquidator. Same account shape as `liquidate_perp` plus the maker
    /// side in remaining_accounts.
    pub fn action_liquidate_perp_with_fill(
        &mut self,
        #[range(0..NUM_USERS)] victim_idx: usize,
    ) -> bool {
        let (liq, victim) = self.liquidator_meta_for(victim_idx);
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(liq.keypair.pubkey(), true),
            AccountMeta::new(liq.user_pda, false),
            AccountMeta::new(liq.stats_pda, false),
            AccountMeta::new(victim.user_pda, false),
            AccountMeta::new(victim.stats_pda, false),
        ];
        accounts.push(AccountMeta::new(self.perp_oracle_pda, false));
        accounts.push(AccountMeta::new(self.spot_market_pda, false));
        accounts.push(AccountMeta::new(self.perp_market_pda, false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_LIQUIDATE_PERP_WITH_FILL, &0u16.to_le_bytes()),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    pub fn action_resolve_perp_bankruptcy(&mut self) -> bool {
        let (liq, victim) = self.liquidator_meta();
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes()); // quote_spot_market_index
        args.extend_from_slice(&0u16.to_le_bytes()); // market_index
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: self.resolve_accounts(&liq, &victim),
                data: ix_data(D_RESOLVE_PERP_BANKRUPTCY, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// resolve_spot_bankruptcy against the victim.
    pub fn action_resolve_spot_bankruptcy(&mut self) -> bool {
        let (liq, victim) = self.liquidator_meta();
        let args = 0u16.to_le_bytes(); // market_index
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: self.resolve_accounts(&liq, &victim),
                data: ix_data(D_RESOLVE_SPOT_BANKRUPTCY, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// resolve_perp_pnl_deficit.
    pub fn action_resolve_perp_pnl_deficit(&mut self) -> bool {
        let (liq, _) = self.liquidator_meta();
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes()); // spot market
        args.extend_from_slice(&0u16.to_le_bytes()); // perp market
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(liq.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(self.velocity_signer(), false),
                    AccountMeta::new_readonly(token_program_id(), false),
                    // remaining: perp oracle + perp market (w) + spot market (w) + mint
                    AccountMeta::new(self.perp_oracle_pda, false),
                    AccountMeta::new(self.perp_market_pda, false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new_readonly(self.usdc_mint, false),
                ],
                data: ix_data(D_RESOLVE_PERP_PNL_DEFICIT, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Shared account list for the two resolve-bankruptcy instructions
    /// (ResolveBankruptcy accounts struct + remaining: spot market, perp
    /// market, mint).
    fn resolve_accounts(&self, liq: &UserAcct, victim: &UserAcct) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(liq.keypair.pubkey(), true),
            AccountMeta::new(liq.user_pda, false),
            AccountMeta::new(liq.stats_pda, false),
            AccountMeta::new(victim.user_pda, false),
            AccountMeta::new(victim.stats_pda, false),
            AccountMeta::new(self.spot_vault_pda, false),
            AccountMeta::new(self.if_vault_pda, false),
            AccountMeta::new_readonly(self.velocity_signer(), false),
            AccountMeta::new_readonly(token_program_id(), false),
            // remaining accounts (perp oracle must lead)
            AccountMeta::new(self.perp_oracle_pda, false),
            AccountMeta::new(self.spot_market_pda, false),
            AccountMeta::new(self.perp_market_pda, false),
            AccountMeta::new_readonly(self.usdc_mint, false),
        ]
    }

    /// add_insurance_fund_stake (liquidator stakes into the IF).
    pub fn action_add_if_stake(&mut self, #[range(1..100_000_000u64)] amount: u64) -> bool {
        let staker = self.users[LIQUIDATOR_IDX].clone();
        let bal = self.ctx.token_balance(&staker.token_account);
        let amount = amount.min(bal);
        if amount == 0 {
            return false;
        }
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.if_stake_pda, false),
                    AccountMeta::new(staker.stats_pda, false),
                    AccountMeta::new_readonly(staker.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(self.velocity_signer(), false),
                    AccountMeta::new(staker.token_account, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_ADD_IF_STAKE, &args),
            })
            .signers(&[&staker.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// request_remove_insurance_fund_stake.
    pub fn action_request_remove_if_stake(
        &mut self,
        #[range(1..100_000_000u64)] amount: u64,
    ) -> bool {
        let staker = self.users[LIQUIDATOR_IDX].clone();
        let mut args = Vec::new();
        args.extend_from_slice(&0u16.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                // PRE-EXISTING BUG: this list omitted `state`, `spot_market_vault`,
                // `velocity_signer` and `token_program`, so anchor read
                // `spot_market` into the `state` slot and every call died with
                // AccountDiscriminatorMismatch. The action could never succeed.
                // Order per the IDL: state, spot_market, insurance_fund_stake,
                // user_stats, authority, spot_market_vault, insurance_fund_vault,
                // velocity_signer, token_program.
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.if_stake_pda, false),
                    AccountMeta::new(staker.stats_pda, false),
                    AccountMeta::new_readonly(staker.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(self.velocity_signer(), false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_REQUEST_REMOVE_IF_STAKE, &args),
            })
            .signers(&[&staker.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// remove_insurance_fund_stake.
    pub fn action_remove_if_stake(&mut self) -> bool {
        let staker = self.users[LIQUIDATOR_IDX].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.if_stake_pda, false),
                    AccountMeta::new(staker.stats_pda, false),
                    AccountMeta::new_readonly(staker.keypair.pubkey(), true),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(self.velocity_signer(), false),
                    AccountMeta::new(staker.token_account, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_REMOVE_IF_STAKE, &0u16.to_le_bytes()),
            })
            .signers(&[&staker.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Advance the clock (past the IF unstaking period etc.).
    pub fn action_warp(
        &mut self,
        #[range(1..500_000u64)] slots: u64,
        #[range(0..4u8)] repost: u8,
    ) -> bool {
        let target = self.ctx.slot() + slots;
        self.ctx.warp_to_slot(target);

        // ADVANCE WALL-CLOCK TIME.
        //
        // `LiteSVM::warp_to_slot` sets `clock.slot` and nothing else, so
        // `unix_timestamp` never moves. Every time-gated path reads the
        // timestamp, not the slot: the IF unstaking cooldown that
        // `action_remove_if_stake` waits on, interest accrual, the funding
        // cadence, and the revenue settle period. Warping slots alone left all
        // of them permanently un-triggerable while appearing to advance time.
        // ~400ms per slot keeps slot-based staleness and time-based cadences
        // coherent with each other.
        {
            use anchor_lang::prelude::Clock;
            let mut clock: Clock = self.ctx.svm.get_sysvar();
            clock.slot = target;
            clock.unix_timestamp = clock
                .unix_timestamp
                .saturating_add((slots as i64).saturating_mul(400) / 1000);
            self.ctx.svm.set_sysvar(&clock);
        }

        // Repost the oracle at the new slot, else the warp leaves it stale by
        // exactly the warp distance and every later margin/liquidation read
        // bails out before reaching the logic under test. `repost == 0` (1 in 4)
        // deliberately leaves it stale so those branches stay reachable.
        if repost != 0 {
            let price = self
                .read_zc::<velocity::state::pyth_lazer_oracle::PythLazerOracle>(
                    &self.perp_oracle_pda,
                )
                .map(|o| o.price)
                .unwrap_or(PRICE_PRECISION as i64);
            let seq = self.oracle_seq;
            self.oracle_seq += 1;
            let mut o = build_pyth_lazer_oracle(price, 0, target, seq);
            let pda = self.perp_oracle_pda;
            inject(&mut self.ctx, pda, &mut o);
        }
        true
    }

    /// Move the perp oracle by a bounded RELATIVE step (<1% per call).
    ///
    /// Capped at 99bps and applied to the current price rather than as an
    /// absolute jump: a large jump mostly trips the oracle guard rails
    /// (`is_oracle_valid_for_action`) and buys a rejection branch instead of
    /// coverage, and real insolvency comes from a levered account meeting a
    /// small move. Compounding steps still walk the price anywhere the fuzzer
    /// needs while every intermediate state stays one the protocol could
    /// actually be in — which matters here, because this harness's whole job is
    /// deciding whether a liquidation was legitimate.
    pub fn action_move_oracle_price(
        &mut self,
        #[range(0..2u8)] up: u8,
        #[range(0..100u64)] bps: u64,
        #[range(0..1_000_000u64)] conf: u64,
        #[range(0..40u64)] slot_lag: u64,
    ) -> bool {
        let current = match self
            .read_zc::<velocity::state::pyth_lazer_oracle::PythLazerOracle>(&self.perp_oracle_pda)
        {
            Some(o) => o.price.max(1),
            None => return false,
        };
        let delta = (current as i128 * bps as i128 / 10_000).max(if bps > 0 { 1 } else { 0 });
        let next = if up == 1 {
            (current as i128).saturating_add(delta)
        } else {
            (current as i128).saturating_sub(delta).max(1)
        };
        let slot = self.ctx.slot();
        let mut o = build_pyth_lazer_oracle(
            next.clamp(1, i64::MAX as i128) as i64,
            conf,
            slot.saturating_sub(slot_lag),
            self.oracle_seq,
        );
        self.oracle_seq += 1;
        let pda = self.perp_oracle_pda;
        inject(&mut self.ctx, pda, &mut o);
        true
    }

    // ---- reads ---------------------------------------------------------

    /// Read an anchor zero-copy account (8-byte discriminator + struct) with an
    /// *unaligned* pod read. velocity zero-copy structs carry `u128` fields, so
    /// on the x86_64 host they require 16-byte alignment; the account's backing
    /// `Vec<u8>` at offset 8 is not guaranteed 16-aligned, which makes the
    /// alignment-checked `read_zero_copy_account`/`bytemuck::from_bytes` panic
    /// intermittently. `pod_read_unaligned` copies into an aligned `T` and is
    /// always safe.
    fn read_zc<T: bytemuck::Pod>(&self, pk: &Pubkey) -> Option<T> {
        let acct = self.ctx.read_account(pk).ok()?;
        let size = std::mem::size_of::<T>();
        // An account that EXISTS but is too small is a host/on-chain LAYOUT DRIFT
        // (host `size_of::<T>` diverged from the deployed .so). Fail LOUDLY:
        // silently returning None would skip every invariant reading through this
        // helper and turn the harness green with zero checks. Genuinely-absent
        // accounts still return None via the `.ok()?` above.
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

    fn read_spot_market(&self) -> Option<SpotMarket> {
        self.read_zc::<SpotMarket>(&self.spot_market_pda)
    }
    fn read_perp_market(&self) -> Option<PerpMarket> {
        self.read_zc::<PerpMarket>(&self.perp_market_pda)
    }
    fn read_user(&self, pk: &Pubkey) -> Option<User> {
        self.read_zc::<User>(pk)
    }
}

// ---------------------------------------------------------------------------
// Shared invariant reconciliation (copied from P7 e2e-svm, not imported —
// crates are independent workspaces) + P8-specific families III / V / VII.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Family I + II: per-spot-market solvency and global quote conservation.
    /// Copied from P7's `invariant_solvency`.
    fn check_solvency_and_conservation(&self) {
        let Some(spot_market) = self.read_spot_market() else {
            return;
        };
        let vault_amount = self.ctx.token_balance(&self.spot_vault_pda);

        // Family I — the program's own authoritative vault check.
        if let Err(e) = velocity::math::spot_withdraw::validate_spot_market_vault_amount(
            &spot_market,
            vault_amount,
        ) {
            fuzz_assert!(
                false,
                "family I: spot market 0 insolvent: vault={} fails validate_spot_market_vault_amount ({:?})",
                vault_amount,
                e
            );
        }

        // Family II — quote conservation: vault covers all quote claims backed
        // here (user net balances + market pools).
        let mut net_user_tokens: i128 = 0;
        for u in &self.users {
            if let Some(user) = self.read_user(&u.user_pda) {
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
        if let Some(pm) = self.read_perp_market() {
            backed += pool_tokens(pm.pnl_pool.scaled_balance);
            backed += pool_tokens(pm.amm.fee_pool.scaled_balance);
        }
        fuzz_assert!(
            vault_amount as i128 >= backed,
            "family II: quote conservation broken: vault={} < backed claims={} (net_user={})",
            vault_amount,
            backed,
            net_user_tokens
        );
    }

    /// Family II — perp open-interest conservation.
    fn check_oi_conservation(&self) {
        if let Some(pm) = self.read_perp_market() {
            let net_user = pm.base_asset_amount_long + pm.base_asset_amount_short;
            fuzz_assert_eq!(
                net_user,
                pm.amm.base_asset_amount_with_amm,
                "family II: perp OI: base_long+base_short ({}) != amm.base_asset_amount_with_amm ({})",
                net_user,
                pm.amm.base_asset_amount_with_amm
            );
        }
    }

    /// Family III — the floored IF bankruptcy tranche keeps its token backing:
    /// the perp pnl pool (+ tokenized amm fee pool) must always cover
    /// `min(pending_if_fee, get_bankruptcy_if_floor())`. This is the tranche
    /// `resolve_perp_bankruptcy` consumes counter-only, so its backing must
    /// stay in the pool. Violated by an unreserved fee sweep (#255).
    fn check_if_tranche_backed(&self) {
        let (Some(pm), Some(sm)) = (self.read_perp_market(), self.read_spot_market()) else {
            return;
        };
        let floor = pm.get_bankruptcy_if_floor().unwrap_or(0);
        let tranche = pm.fee_ledger.pending_if_fee.min(floor);
        if tranche == 0 {
            return;
        }
        let pnl_tokens = velocity::math::spot_balance::get_token_amount(
            pm.pnl_pool.scaled_balance,
            &sm,
            &SpotBalanceType::Deposit,
        )
        .unwrap_or(0);
        let fee_tokens = velocity::math::spot_balance::get_token_amount(
            pm.amm.fee_pool.scaled_balance,
            &sm,
            &SpotBalanceType::Deposit,
        )
        .unwrap_or(0);
        let backing = pnl_tokens.saturating_add(fee_tokens);
        fuzz_assert!(
            backing >= tranche,
            "family III: floored IF tranche unbacked: pool backing={} < tranche={} (pending_if_fee={}, floor={})",
            backing,
            tranche,
            pm.fee_ledger.pending_if_fee,
            floor
        );
    }

    /// Family III — the shared IF vault is never fully drained (the program
    /// enforces `> 0` after each resolve; check it holds after every action).
    fn check_if_vault_not_overspent(&self) {
        let if_bal = self.ctx.token_balance(&self.if_vault_pda);
        // The vault was seeded above 0; a resolution that drove it to 0 would be
        // a double-spend of the shared quote IF vault.
        fuzz_assert!(
            if_bal > 0,
            "family III: shared IF vault fully drained (balance = 0) — double-spend"
        );
    }

    /// Family V — no user carries a borrow with no collateral without being
    /// flagged liquidatable, and any bankrupt user is flagged being-liquidated.
    fn check_no_unflagged_underwater(&self) {
        for u in &self.users {
            if let Some(user) = self.read_user(&u.user_pda) {
                let mut has_borrow = false;
                let mut has_deposit = false;
                for sp in user.spot_positions.iter() {
                    if sp.scaled_balance == 0 {
                        continue;
                    }
                    match sp.balance_type {
                        SpotBalanceType::Borrow => has_borrow = true,
                        SpotBalanceType::Deposit => has_deposit = true,
                    }
                }
                let flagged = user.is_being_liquidated();
                fuzz_assert!(
                    !(has_borrow && !has_deposit && !flagged),
                    "family V: user {} has a borrow with no collateral and is not flagged liquidatable",
                    u.user_pda
                );
                // Bankrupt ⇒ being-liquidated.
                fuzz_assert!(
                    !(user.is_bankrupt() && !user.is_being_liquidated()),
                    "family V: user {} is bankrupt but not flagged being-liquidated",
                    u.user_pda
                );
            }
        }
    }

    /// Family VII — IF share bookkeeping never goes incoherent: staker shares
    /// never exceed total, and total == 0 implies no staker shares.
    fn check_if_share_math(&self) {
        if let Some(sm) = self.read_spot_market() {
            let f = &sm.insurance_fund;
            fuzz_assert!(
                f.user_shares <= f.total_shares,
                "family VII: IF user_shares ({}) > total_shares ({})",
                f.user_shares,
                f.total_shares
            );
            fuzz_assert!(
                !(f.total_shares == 0 && f.user_shares > 0),
                "family VII: IF user_shares ({}) > 0 while total_shares == 0",
                f.user_shares
            );
        }
    }

    /// The full P8 invariant sweep, run after every action.
    fn check_all_invariants(&self) {
        self.check_solvency_and_conservation();
        self.check_oi_conservation();
        self.check_if_tranche_backed();
        self.check_if_vault_not_overspent();
        self.check_no_unflagged_underwater();
        self.check_if_share_math();
    }
}

// ---------------------------------------------------------------------------
// Always-on invariant harness (families III + V + VII end-to-end)
// ---------------------------------------------------------------------------

#[cfg(feature = "invariant_liq")]
#[invariant_test]
fn invariant_liq(fixture: &mut Fixture) {
    fixture.check_all_invariants();
}

// ---------------------------------------------------------------------------
// Regression harnesses (pending audit-fix PRs). Assert the FIXED behavior, so
// they FAIL (report a violation) on current pre-fix master.
// ---------------------------------------------------------------------------

// PENDING PR #267 (OtterSec F5 / finding #52): `resolve_spot_bankruptcy` must
// revert with `PerpBankruptcyMustPrecedeSpot` (6361) while the user still has a
// pending cross-margin perp bankruptcy, so the shared quote IF-vault draw order
// can't be gamed. On current master the ordering guard is absent, so the call
// succeeds against our injected victim (bankrupt + negative-quote perp position
// with no base + a spot borrow) — this harness fires.
#[cfg(feature = "regr_267_perp_before_spot")]
#[invariant_test]
fn regr_267_perp_before_spot(fixture: &mut Fixture) {
    // The victim is injected in setup() with both a pending cross-margin perp
    // bankruptcy and a spot borrow. Fixed behavior: the call must revert.
    let succeeded = fixture.action_resolve_spot_bankruptcy();
    fuzz_assert!(
        !succeeded,
        "regr #267: resolve_spot_bankruptcy succeeded while a pending cross-margin \
         perp bankruptcy exists — perp-before-spot ordering not enforced (expected \
         PerpBankruptcyMustPrecedeSpot / 6361)"
    );
}

// PENDING PR #255 (OtterSec High / finding #53, campaign regression #2): a
// permissionless perp fee sweep can never drain the token backing of the
// floored `pending_if_fee` tranche ahead of `resolve_perp_bankruptcy`. Driven
// at the state level through the real `sweep_market_fees` controller (identical
// to the `sweep_perp_market_fees` ix's core) so it reproduces deterministically
// without the ix's oracle/price-band gating. On master the protocol-fee drain
// runs first and is exempt from the tranche reservation, so it sweeps the
// pnl-pool tokens backing the floor into `protocol_fee_pool`, leaving the
// counter standing but unbacked — this harness fires.
#[cfg(feature = "regr_255_floored_if_tranche")]
#[invariant_test]
fn regr_255_floored_if_tranche(fixture: &mut Fixture) {
    let (Some(mut pm), Some(mut sm)) = (fixture.read_perp_market(), fixture.read_spot_market())
    else {
        return;
    };

    let floor = pm.get_bankruptcy_if_floor().unwrap_or(0);
    let tranche_before = pm.fee_ledger.pending_if_fee.min(floor);
    if tranche_before == 0 {
        return;
    }

    // Run the real fee sweep on host copies of the injected markets. net_user_pnl
    // is 0 (no live user PnL in this market), so the whole pnl pool is
    // "available" to the protocol-fee drain.
    let now = 1_000i64;
    let _ = velocity::controller::perp_pools::sweep_market_fees(&mut pm, &mut sm, 0, now, false);

    // Post-sweep: the floored tranche must still be backed by pnl-pool tokens.
    let floor_after = pm.get_bankruptcy_if_floor().unwrap_or(0);
    let tranche_after = pm.fee_ledger.pending_if_fee.min(floor_after);
    let pnl_tokens = velocity::math::spot_balance::get_token_amount(
        pm.pnl_pool.scaled_balance,
        &sm,
        &SpotBalanceType::Deposit,
    )
    .unwrap_or(0);
    let fee_tokens = velocity::math::spot_balance::get_token_amount(
        pm.amm.fee_pool.scaled_balance,
        &sm,
        &SpotBalanceType::Deposit,
    )
    .unwrap_or(0);
    let backing = pnl_tokens.saturating_add(fee_tokens);
    fuzz_assert!(
        backing >= tranche_after,
        "regr #255: fee sweep drained the floored IF tranche backing: pool backing={} \
         < floored tranche={} (pending_if_fee={}, floor={}) — the standing first-loss \
         tranche resolve_perp_bankruptcy relies on is no longer backed",
        backing,
        tranche_after,
        pm.fee_ledger.pending_if_fee,
        floor_after
    );
}

// PENDING PR #275 (OtterSec Medium / finding #82): a liquidator whose
// authority-wide equity breaker (`UserStats.equity_breaker_tripped`) is set is
// barred (`EquityBelowFloor`) from position-acquiring liquidations
// (`liquidate_perp`, `liquidate_spot`). This harness trips the liquidator's
// breaker and asserts `liquidate_perp` reverts.
//
// REPRODUCTION CAVEAT (documented in the report): fully reproducing this on
// master needs a liquidatee that is genuinely below maintenance on a *base*
// perp position so `liquidate_perp` proceeds past its pre-checks to where the
// (missing) breaker guard would apply — which requires a coherent AMM and an
// adverse mark/oracle move the QuoteAsset ($1 fixed) oracle path cannot
// produce via pure injection. As injected here the victim's perp position has
// no base, so master rejects the liquidation earlier for an unrelated reason
// and the harness may not fire. Also, #275 changes `liquidate_spot`'s ABI (adds
// `liquidator_stats`), which is not present on master, so only the
// `liquidate_perp` arm is exercised. See the report's per-regression status.
#[cfg(feature = "regr_275_equity_floor_liq")]
#[invariant_test]
fn regr_275_equity_floor_liq(fixture: &mut Fixture) {
    // Trip the liquidator's authority-wide equity breaker in-place.
    let liq = fixture.users[LIQUIDATOR_IDX].clone();
    if let Some(mut stats) = fixture.read_zc::<velocity::state::user::UserStats>(&liq.stats_pda) {
        stats.set_equity_breaker_tripped(true);
        fixture
            .ctx
            .write_zero_copy_account(&liq.stats_pda, &stats)
            .expect("overwrite liquidator stats");
    }

    let succeeded = fixture.action_liquidate_perp(1_000_000, VICTIM_IDX);
    fuzz_assert!(
        !succeeded,
        "regr #275: liquidate_perp succeeded while the liquidator's authority-wide \
         equity breaker is tripped — position-acquiring liquidation not barred \
         (expected EquityBelowFloor)"
    );
}

// PENDING PR #306 (OtterSec High / finding #89): `resolve_perp_bankruptcy`
// socializes residual bad debt by bumping `cumulative_funding_rate_long` up and
// `_short` down (so surviving longs AND shorts both owe funding covering the
// loss). It must also resync the AMM's own funding stamp
// (`amm.last_cumulative_funding_rate_long`/`_short`) past that bump. On master
// it does not, so the next `update_funding_rate` settles the AMM against the
// stale stamp: because the socialization bump is ASYMMETRIC, the AMM (the
// zero-sum counterparty to both the gross long and gross short book) "receives"
// on both legs — pocketing ~the socialized loss as phantom
// `total_fee_minus_distributions`, even though its net position is flat.
//
// The fixture's AMM is net-flat (`base_asset_amount_with_amm == 0`), so a
// correct implementation leaves it with ZERO funding to settle after the
// bankruptcy. This harness forces a socializing bankruptcy (the seed victim's
// -$1 quote is fully covered and never socializes, so we inject bad debt well
// beyond all coverage), then computes exactly what the next `FundingUpdated`
// settlement would pay the AMM from the post-resolve on-chain state. Pre-#306
// that is nonzero (phantom); with #306 (settle-then-resync) it is zero.
#[cfg(feature = "regr_306_amm_phantom_funding")]
#[invariant_test]
fn regr_306_amm_phantom_funding(fixture: &mut Fixture) {
    // Force the victim's perp bad debt far beyond bankruptcy coverage (IF vault
    // + pending IF-fee tranche) so `resolve_perp_bankruptcy` must socialize a
    // residual via the cum-rate bump rather than fully absorbing it.
    let victim = fixture.users[VICTIM_IDX].clone();
    if let Some(mut u) = fixture.read_user(&victim.user_pda) {
        u.perp_positions[0].quote_asset_amount = -(50_000i64 * QUOTE_PRECISION as i64);
        fixture
            .ctx
            .write_zero_copy_account(&victim.user_pda, &u)
            .expect("overwrite victim perp quote");
    }

    let cum_long_before = fixture
        .read_perp_market()
        .expect("perp market")
        .cumulative_funding_rate_long;

    let succeeded = fixture.action_resolve_perp_bankruptcy();
    fuzz_assert!(
        succeeded,
        "regr #306: resolve_perp_bankruptcy did not execute; scenario broken"
    );

    let pm = fixture.read_perp_market().expect("perp market");

    // The residual must actually have socialized (cum rate bumped); otherwise
    // the path under test was never exercised and the regression is vacuous.
    fuzz_assert!(
        pm.cumulative_funding_rate_long != cum_long_before,
        "regr #306: bankruptcy did not socialize (cum rate unchanged); scenario broken"
    );

    // What the next FundingUpdated settlement would pay this (net-flat) AMM,
    // computed from the post-resolve on-chain state exactly as the quoter does.
    let phantom = velocity::math::funding::calculate_amm_funding_payment(
        pm.base_asset_amount_long,
        pm.base_asset_amount_short,
        pm.cumulative_funding_rate_long,
        pm.cumulative_funding_rate_short,
        pm.amm.last_cumulative_funding_rate_long,
        pm.amm.last_cumulative_funding_rate_short,
    )
    .unwrap_or(0);
    fuzz_assert!(
        phantom == 0,
        "regr #306: net-flat AMM would receive {} phantom funding on the next update — \
         resolve_perp_bankruptcy left amm.last_cumulative_funding_rate behind the socialization \
         bump (phantom total_fee_minus_distributions ≈ the socialized loss)",
        phantom
    );
}

// ---------------------------------------------------------------------------
// Host smoke test (does not require the fuzzer runtime).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod smoke {
    use super::*;

    /// ACTION CENSUS — every action must be able to succeed at least once.
    ///
    /// A single-core probe reported `discovered: 6/13 actions`, which says how
    /// many fire but never WHICH. An action that can never succeed is worse than
    /// no action: the fuzzer still spends mutation budget picking it, and the
    /// failure is invisible because every action returns a bare bool. This drives
    /// each one and prints a table, so a silently-dead action fails the build.
    ///
    /// Actions that legitimately cannot succeed from this fixture's state are
    /// listed in EXPECTED_CONDITIONAL with the reason.
    #[test]
    fn action_census() {
        let mut f = Fixture::setup();
        let mut results: Vec<(&str, bool)> = Vec::new();
        macro_rules! run {
            ($n:expr, $e:expr) => {
                results.push(($n, $e));
            };
        }

        run!("deposit", f.action_deposit(LIQUIDATOR_IDX, 100_000_000_000));
        run!("move_oracle_price", f.action_move_oracle_price(1, 50, 0, 0));
        run!("warp", f.action_warp(1_000, 1));
        // Walk the price DOWN in sub-1% steps until the levered long is under
        // maintenance; this is the pipeline the injected-bankrupt victim skips.
        // The levered long is under maintenance at roughly -1.3% (10 base at
        // $1, $0.62 collateral, 5% maintenance). Four sub-1% steps is enough.
        // Walking much further would decouple the oracle from the AMM's mark
        // and trip the price bands instead, which blocks the fill-based
        // liquidation for an unrelated reason.
        for _ in 0..4 {
            let _ = f.action_move_oracle_price(0, 99, 0, 0);
        }
        run!(
            "set_being_liquidated",
            f.action_set_being_liquidated(LEVERED_IDX)
        );
        run!(
            "liquidate_perp",
            f.action_liquidate_perp(1_000_000, LEVERED_IDX)
        );
        run!(
            "liquidate_perp_with_fill",
            f.action_liquidate_perp_with_fill(LEVERED_IDX)
        );
        run!("liquidate_spot", f.action_liquidate_spot(1_000, VICTIM_IDX));
        run!(
            "liquidate_pnl_for_deposit",
            f.action_liquidate_pnl_for_deposit(1_000, VICTIM_IDX)
        );
        run!(
            "resolve_perp_bankruptcy",
            f.action_resolve_perp_bankruptcy()
        );
        run!(
            "resolve_spot_bankruptcy",
            f.action_resolve_spot_bankruptcy()
        );
        run!(
            "resolve_perp_pnl_deficit",
            f.action_resolve_perp_pnl_deficit()
        );
        run!("add_if_stake", f.action_add_if_stake(10_000_000));
        run!(
            "request_remove_if_stake",
            f.action_request_remove_if_stake(1_000_000)
        );
        run!("warp(cooldown)", f.action_warp(400_000, 1));
        run!("remove_if_stake", f.action_remove_if_stake());

        println!("\n==== e2e-svm-liq ACTION CENSUS ====");
        let mut failed = Vec::new();
        for (n, ok) in &results {
            println!("  {:<30} {}", n, if *ok { "ok" } else { "FAIL" });
            if !*ok {
                failed.push(*n);
            }
        }
        println!(
            "  {}/{} succeeded",
            results.len() - failed.len(),
            results.len()
        );

        const EXPECTED_CONDITIONAL: &[&str] = &[
            // Ordering: the victim is already flagged by the fixture, and each
            // resolve/liquidate consumes the state the next one needs.
            //
            // `liquidate_spot` now REACHES the handler (the account list was
            // missing `liquidator_stats`, so it used to die in Anchor), but it
            // still cannot succeed with one spot market: asset == liability == 0
            // and a `SpotPosition` holds a single balance type, so it lands on
            // `WrongSpotBalanceType` (controller/liquidation.rs:2062-2066).
            "liquidate_spot",
            "liquidate_pnl_for_deposit",
            "resolve_spot_bankruptcy",
            "resolve_perp_pnl_deficit",
            // Needs a settled stake and the escrow period to have elapsed.
            "remove_if_stake",
        ];
        let unexpected: Vec<_> = failed
            .iter()
            .filter(|n| !EXPECTED_CONDITIONAL.contains(n))
            .collect();
        assert!(
            unexpected.is_empty(),
            "actions that should succeed but did not: {:?}",
            unexpected
        );
    }

    #[test]
    fn setup_is_coherent() {
        let f = Fixture::setup();

        // Users initialized.
        for u in &f.users {
            assert!(
                f.read_user(&u.user_pda).is_some(),
                "user account should exist after initialize_user"
            );
        }

        // Victim carries the injected bankrupt state.
        let victim = f.read_user(&f.users[VICTIM_IDX].user_pda).unwrap();
        assert!(
            victim.is_cross_margin_bankrupt(),
            "victim should be bankrupt"
        );
        assert!(
            victim.is_being_liquidated(),
            "victim flagged being-liquidated"
        );

        // Setup solvency invariants hold before any action.
        f.check_all_invariants();

        // Floored tranche is backed at rest.
        let pm = f.read_perp_market().unwrap();
        let floor = pm.get_bankruptcy_if_floor().unwrap();
        assert!(floor > 0, "floor should be > 0 given OI + TWAP + pct");
    }
}
