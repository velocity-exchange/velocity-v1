//! P7b `e2e-svm-signedmsg` — signed-message ("swift") regression harnesses for
//! OtterSec F10 (PR #271).
//!
//! Two standalone Crucible regression fixtures, each feature-gated so their
//! per-feature `main` / `__CRUCIBLE_ALLOC` symbols never collide:
//!
//!  * `regr_271_replay_resize_authority` — `resize_signed_msg_user_orders` let a
//!    per-sub-account **delegate** shrink the authority-scoped `SignedMsgUserOrders`
//!    replay account, evicting other sub-accounts' active replay UUIDs. The fix
//!    restricts shrinking to the `authority`. Asserts the FIXED behavior (delegate
//!    shrink rejected with `InvalidSignedMsgUserOrdersResize` = 6313); FAILS on
//!    pre-fix master (shrink allowed → success). No signing required.
//!
//!  * `regr_271_signed_msg_taker_pause` — `place_signed_msg_taker_order` lacked
//!    the global `exchange_not_paused` guard, so a signed-message taker order
//!    could be placed while the exchange is fully paused. The fix adds the guard
//!    as an `#[access_control]` that runs *before* the handler body. Submits a
//!    signed-message taker order (ed25519 pre-ix + `SignedMsgUserOrders` account)
//!    while the exchange is fully paused (`exchange_status = 0xFF`) and asserts
//!    the FIXED behavior (rejected with `ExchangePaused` = 6024); FAILS on pre-fix
//!    master (the access-control guard is absent, so the ix proceeds past it).
//!
//! Shared SVM integration path mirrors P7 `e2e-svm`:
//!  * `crucible_idl_gen::declare_fuzz_program!` ingests the velocity IDL and gives
//!    `register_schemas()` for field-level crash diffs.
//!  * Instructions are built with `ctx.raw_call(Instruction{..})` + IDL anchor
//!    discriminators + hand-encoded args.
//!  * Zero-copy account reads use `bytemuck::pod_read_unaligned` (`read_zc`), NOT
//!    `read_zero_copy_account` (which panics on unaligned u128 fields).
//!  * Injection of anchor zero-copy accounts uses
//!    `velocity::test_utils::get_anchor_account_bytes`.

#![allow(dead_code)]

use {
    crucible_fuzzer::*,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::rc::Rc,
    velocity::state::user::User,
};

// Generated types/schemas from the canonical velocity IDL. We only use
// `register_schemas()`; instruction building goes through `raw_call`.
crucible_idl_gen::declare_fuzz_program!(velocity_idl = "idls/velocity.json");

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VELOCITY_SO: &str = "../../target/deploy/velocity.so";

/// Anchor instruction discriminators (from the canonical IDL).
const D_RESIZE_SIGNED_MSG: [u8; 8] = [137, 10, 87, 150, 18, 115, 79, 168];
const D_PLACE_SIGNED_MSG_TAKER: [u8; 8] = [32, 79, 101, 139, 25, 6, 98, 15];
const D_INITIALIZE_USER_STATS: [u8; 8] = [254, 243, 72, 98, 251, 130, 168, 213];
const D_INITIALIZE_USER: [u8; 8] = [111, 17, 185, 250, 60, 122, 38, 254];

/// Anchor account discriminator for `SignedMsgUserOrders` (from the IDL).
const A_SIGNED_MSG_USER_ORDERS: [u8; 8] = [70, 6, 50, 248, 222, 1, 143, 49];

/// SIGNED_MSG PDA seed.
const SIGNED_MSG_PDA_SEED: &[u8] = b"SIGNED_MSG";

/// Anchor custom error codes (6000 + enum index).
const E_EXCHANGE_PAUSED: u32 = 6024;
const E_INVALID_SIGNED_MSG_USER_ORDERS_RESIZE: u32 = 6313;

// System / builtin program ids.
fn system_program_id() -> Pubkey {
    Pubkey::new_from_array([0u8; 32])
}
fn rent_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111")
}
fn ix_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("Sysvar1nstructions1111111111111111111111111")
}
fn ed25519_program_id() -> Pubkey {
    Pubkey::from_str_const("Ed25519SigVerify111111111111111111111111111")
}
fn native_loader_id() -> Pubkey {
    Pubkey::from_str_const("NativeLoader1111111111111111111111111111111")
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
/// velocity structs (byte-identical).
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

/// Build the on-chain bytes for a `SignedMsgUserOrders` account holding
/// `num_orders` (all-zero / default) slots, owned by `authority`.
///
/// The byte layout is identical for the borsh `Account` path (resize) and the
/// custom zero-copy loader (place):
///   disc[8] | authority_pubkey[32] | padding: u32 | len: u32 | num_orders * 24
/// where each slot is a 24-byte `SignedMsgOrderId` (uuid[8] | max_slot u64 |
/// order_id u32 | padding u32). All-zero slots read as `max_slot == 0` (empty).
fn signed_msg_user_orders_bytes(authority: Pubkey, num_orders: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&A_SIGNED_MSG_USER_ORDERS);
    v.extend_from_slice(&authority.to_bytes()); // authority_pubkey / fixed.user_pubkey
    v.extend_from_slice(&0u32.to_le_bytes()); // padding
    v.extend_from_slice(&num_orders.to_le_bytes()); // vec len / fixed.len
    v.extend_from_slice(&vec![0u8; num_orders as usize * 24]); // slots
    v
}

/// `SignedMsgUserOrders::space(num_orders)` — mirror of the program helper:
/// 8 (disc) + 32 (authority) + 4 (padding) + 32 + num_orders * 24.
fn signed_msg_space(num_orders: usize) -> usize {
    8 + 32 + 4 + 32 + num_orders * 24
}

/// Create a program-owned `SignedMsgUserOrders` PDA account with `num_orders`
/// slots, sized to `space(num_orders)`.
fn create_signed_msg_account(
    ctx: &mut TestContext,
    pda: Pubkey,
    authority: Pubkey,
    num_orders: u32,
) {
    let mut data = signed_msg_user_orders_bytes(authority, num_orders);
    // Pad to space(num_orders) so realloc math matches the anchor helper.
    data.resize(signed_msg_space(num_orders as usize), 0);
    ctx.create_account()
        .pubkey(pda)
        .owner(velocity_program_id())
        .lamports(1_000_000_000)
        .data(&data)
        .create()
        .expect("create signed_msg_user_orders");
}

/// Read an anchor zero-copy account by unaligned copy (see e2e-svm rationale).
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

// ===========================================================================
// Harness 2 (implemented first — no signing): PR #271 replay-resize authority
// ===========================================================================
//
// `resize_signed_msg_user_orders` (pre-fix) permits `payer == user.delegate` to
// shrink the authority-scoped SignedMsgUserOrders replay account. This evicts
// other sub-accounts' active replay UUIDs, re-enabling replay of their signed
// orders. The fix restricts shrinking to `authority`.
//
// Setup: inject a `SignedMsgUserOrders` with 8 slots + a `User` whose
// `delegate` is a keypair we control; then call `resize_signed_msg_user_orders`
// with `num_orders = 1` (shrink), signed by the delegate as `payer`.
//
// FIXED  => rejected with InvalidSignedMsgUserOrdersResize (6313).
// MASTER => shrink allowed (tx succeeds) — assertion fails, reproducing the bug.
#[cfg(feature = "regr_271_replay_resize_authority")]
mod regr_271_resize {
    use super::*;

    const INITIAL_ORDERS: u32 = 8;
    const SHRINK_TO_ORDERS: u16 = 1;

    #[derive(Clone)]
    pub struct Regr271Resize {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub signed_msg_pda: Pubkey,
        pub authority: Pubkey,
        pub user_pda: Pubkey,
        pub delegate: Rc<Keypair>,
    }

    #[fuzz_fixture]
    impl Regr271Resize {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();

            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO)
                .expect("add velocity.so");

            // Authority (owner of the replay account) — a plain pubkey; it is an
            // UncheckedAccount in the resize context and does NOT sign.
            let authority = Keypair::new().pubkey();

            // Delegate — the per-sub-account delegate that (on master) is allowed
            // to shrink. Funded so it can be the resize `payer` + tx fee payer.
            let delegate = Rc::new(Keypair::new());
            ctx.create_account()
                .pubkey(delegate.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();

            // SignedMsgUserOrders PDA = [SIGNED_MSG, authority].
            let (signed_msg_pda, _) = Pubkey::find_program_address(
                &[SIGNED_MSG_PDA_SEED, authority.as_ref()],
                &program_id,
            );
            create_signed_msg_account(&mut ctx, signed_msg_pda, authority, INITIAL_ORDERS);

            // User (sub-account 0) with authority = authority and delegate = delegate.
            let sub0 = 0u16.to_le_bytes();
            let (user_pda, _) =
                Pubkey::find_program_address(&[b"user", authority.as_ref(), &sub0], &program_id);
            let mut user = User::default();
            user.authority = anchor_pk(authority);
            user.delegate = anchor_pk(delegate.pubkey());
            user.sub_account_id = 0;
            inject(&mut ctx, user_pda, &mut user);

            Regr271Resize {
                ctx,
                program_id,
                signed_msg_pda,
                authority,
                user_pda,
                delegate,
            }
        }

        /// Attempt the delegate-driven shrink. Returns the program error code, if
        /// any (None on success).
        pub fn resize_shrink_as_delegate(&mut self) -> Option<u32> {
            let args = SHRINK_TO_ORDERS.to_le_bytes();
            let outcome = self
                .ctx
                .raw_call(Instruction {
                    program_id: self.program_id,
                    // Pre-fix account order: signed_msg_user_orders, authority,
                    // user, payer, system_program.
                    accounts: vec![
                        AccountMeta::new(self.signed_msg_pda, false),
                        AccountMeta::new_readonly(self.authority, false),
                        AccountMeta::new_readonly(self.user_pda, false),
                        AccountMeta::new(self.delegate.pubkey(), true),
                        AccountMeta::new_readonly(system_program_id(), false),
                    ],
                    data: ix_data(D_RESIZE_SIGNED_MSG, &args),
                })
                .signers(&[&self.delegate])
                .send();
            outcome.ok().and_then(|o| o.error_code())
        }

        // Fuzz-fixture requires >= 1 action_*.
        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }
    }

    #[cfg(test)]
    mod smoke {
        use super::*;

        #[test]
        fn delegate_shrink_reproduces() {
            let mut f = Regr271Resize::setup();
            // Account exists with INITIAL_ORDERS slots before resize.
            let before = f.ctx.get_account(&f.signed_msg_pda).unwrap();
            assert_eq!(before.data.len(), signed_msg_space(INITIAL_ORDERS as usize));
            let code = f.resize_shrink_as_delegate();
            eprintln!("[SMOKE] delegate shrink error_code = {:?}", code);
            let after = f.ctx.get_account(&f.signed_msg_pda).unwrap();
            eprintln!(
                "[SMOKE] account data len before={} after={}",
                before.data.len(),
                after.data.len()
            );
            // On master this is None (shrink succeeded) — documents the bug.
        }
    }
}

#[cfg(feature = "regr_271_replay_resize_authority")]
use regr_271_resize::Regr271Resize;

#[cfg(feature = "regr_271_replay_resize_authority")]
#[crucible_fuzz]
fn regr_271_replay_resize_authority(fixture: &mut Regr271Resize, #[range(0..1u8)] _unused: u8) {
    let code = fixture.resize_shrink_as_delegate();
    // FIXED behavior: a delegate (payer != authority) may not shrink the
    // authority-scoped replay account. On pre-fix master the shrink is allowed,
    // so `code` is None (success) — this assertion then fails, reproducing the
    // bug.
    fuzz_assert_eq!(
        code,
        Some(E_INVALID_SIGNED_MSG_USER_ORDERS_RESIZE),
        "PR #271: delegate was allowed to shrink the authority-scoped SignedMsgUserOrders replay account (got error_code={:?})",
        code
    );
}

// ===========================================================================
// Harness 1: PR #271 signed-message taker-order pause guard
// ===========================================================================
//
// `place_signed_msg_taker_order` (pre-fix) lacks the `exchange_not_paused`
// access-control guard that normal placement carries. With the exchange fully
// paused (`exchange_status = 0xFF`, i.e. ExchangeStatus::is_all()) a signed
// message taker order can still be submitted.
//
// The signed-message flow requires an ed25519 pre-instruction (native
// Ed25519 program) whose offsets reference the signature/pubkey/message region
// embedded in the *velocity* instruction's own data, plus the
// `SignedMsgUserOrders` account. The velocity handler re-derives that region
// and validates the offset layout via `verify_and_decode_ed25519_msg`.
//
// FIXED  => `exchange_not_paused` (runs before the handler body) rejects with
//           ExchangePaused (6024), regardless of the rest of the flow.
// MASTER => the guard is absent, so the ix proceeds past it (into sig
//           verification / order placement) — the error code is never 6024,
//           so the assertion fails, reproducing the bug.
#[cfg(feature = "regr_271_signed_msg_taker_pause")]
mod regr_271_pause {
    use {
        super::*,
        anchor_lang::AnchorSerialize,
        solana_program_runtime::{
            invoke_context::InvokeContext,
            solana_sbpf::{declare_builtin_function, memory_region::MemoryMapping},
        },
        velocity::{
            controller::position::PositionDirection,
            state::{
                order_params::{OrderParams, PostOnlyParam, SignedMsgOrderParamsMessage},
                state::State,
                user::{MarketType, OrderType},
            },
        },
    };

    // A no-op builtin registered at the native Ed25519 program id. Crucible builds
    // litesvm WITHOUT the `precompiles` feature, so `is_precompile()` returns false
    // and the ed25519 pre-ix would otherwise fail with UnsupportedProgramId before
    // the velocity ix runs. The velocity handler re-reads the ed25519 ix data from
    // the instructions sysvar and validates the offset *layout* + embedded pubkey
    // itself (`verify_and_decode_ed25519_msg`); the precompile's cryptographic
    // check is bypassed in this SVM anyway (sigverify off, dummy tx signatures), so
    // a no-op stub here faithfully lets the full signed-message flow reach velocity.
    declare_builtin_function!(
        Ed25519NoopBuiltin,
        fn rust(
            invoke_context: &mut InvokeContext,
            _arg0: u64,
            _arg1: u64,
            _arg2: u64,
            _arg3: u64,
            _arg4: u64,
            _memory_mapping: &mut MemoryMapping,
        ) -> std::result::Result<u64, Box<dyn std::error::Error>> {
            // Builtins must consume >0 compute units (else the runtime errors with
            // BuiltinProgramsMustConsumeComputeUnits).
            invoke_context.consume_checked(1)?;
            Ok(0)
        }
    );

    /// Build the signed-message envelope carried in the velocity ix `bytes` arg.
    ///
    /// Layout (see `validation/sig_verification.rs`):
    ///   signature[64] | pubkey[32] | message_size: u16 LE | message[...]
    /// where `message` is the ASCII-hex encoding of
    ///   manual_discriminator[8] || borsh(SignedMsgOrderParamsMessage)
    ///
    /// Returns (envelope, message_size).
    fn build_envelope(
        signer_kp: &Keypair,
        sub_account_id: u16,
        current_slot: u64,
    ) -> (Vec<u8>, u16) {
        let signer = signer_kp.pubkey();
        // A valid perp taker order with well-formed auction params.
        let order = OrderParams {
            order_type: OrderType::Market,
            market_type: MarketType::Perp,
            direction: PositionDirection::Long,
            base_asset_amount: 1_000_000_000,
            price: 0,
            market_index: 0,
            post_only: PostOnlyParam::None,
            auction_duration: Some(10),
            auction_start_price: Some(1_000_000),
            auction_end_price: Some(1_010_000),
            ..Default::default()
        };
        let msg_struct = SignedMsgOrderParamsMessage {
            signed_msg_order_params: order,
            sub_account_id,
            slot: current_slot,
            uuid: *b"regr0271",
            take_profit_order_params: None,
            stop_loss_order_params: None,
            max_margin_ratio: None,
            builder_idx: None,
            builder_fee_tenth_bps: None,
            isolated_position_deposit: None,
        };

        // manual 8-byte discriminator (not validated by the program) + borsh body.
        let mut borsh_body = vec![0u8; 8];
        msg_struct.serialize(&mut borsh_body).unwrap();

        // The signed message is the ASCII-hex of (disc || borsh).
        let hex_msg = hex_encode(&borsh_body);
        let message_size = hex_msg.len() as u16;

        // Envelope: signature[64] | pubkey[32] | size u16 | message.
        // The native Ed25519 program is registered (executable, native-loader
        // owned) and the SVM runs FeatureSet::all_enabled(), so precompile
        // verification runs inside the SVM — the signature must be a REAL
        // ed25519 signature by `signer` over the message region (the ASCII-hex
        // bytes).
        let signature = signer_kp.sign_message(hex_msg.as_bytes());
        let mut envelope = Vec::new();
        envelope.extend_from_slice(signature.as_ref()); // signature (64 bytes)
        envelope.extend_from_slice(&signer.to_bytes()); // pubkey
        envelope.extend_from_slice(&message_size.to_le_bytes());
        envelope.extend_from_slice(hex_msg.as_bytes());
        (envelope, message_size)
    }

    fn hex_encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    /// Build the native Ed25519 verify instruction whose offsets reference the
    /// signature/pubkey/message embedded in the velocity instruction at
    /// `velocity_ix_index`. The velocity ix data lays the envelope out starting
    /// at offset 12 (8 disc + 4 borsh-vec length prefix):
    ///   [12..76] signature, [76..108] pubkey, [108..110] size, [110..] message.
    fn build_ed25519_ix(velocity_ix_index: u16, message_size: u16) -> Instruction {
        // Ed25519SignatureOffsets, packed as the native program expects:
        //   signature_offset u16, signature_instruction_index u16,
        //   public_key_offset u16, public_key_instruction_index u16,
        //   message_data_offset u16, message_data_size u16,
        //   message_instruction_index u16
        const SIG_OFFSET: u16 = 12;
        const PUBKEY_OFFSET: u16 = SIG_OFFSET + 64; // 76
        const MSG_DATA_OFFSET: u16 = PUBKEY_OFFSET + 32 + 2; // 110

        let mut data = Vec::new();
        data.push(1u8); // num_signatures
        data.push(0u8); // padding
        data.extend_from_slice(&SIG_OFFSET.to_le_bytes());
        data.extend_from_slice(&velocity_ix_index.to_le_bytes());
        data.extend_from_slice(&PUBKEY_OFFSET.to_le_bytes());
        data.extend_from_slice(&velocity_ix_index.to_le_bytes());
        data.extend_from_slice(&MSG_DATA_OFFSET.to_le_bytes());
        data.extend_from_slice(&message_size.to_le_bytes());
        data.extend_from_slice(&velocity_ix_index.to_le_bytes());

        Instruction {
            program_id: ed25519_program_id(),
            accounts: vec![],
            data,
        }
    }

    #[derive(Clone)]
    pub struct Regr271Pause {
        pub ctx: TestContext,
        pub program_id: Pubkey,
        pub state_pda: Pubkey,
        pub spot_market_pda: Pubkey,
        pub perp_market_pda: Pubkey,
        pub signed_msg_pda: Pubkey,
        pub user_pda: Pubkey,
        pub user_stats_pda: Pubkey,
        pub authority: Rc<Keypair>,
    }

    #[fuzz_fixture]
    impl Regr271Pause {
        pub fn setup() -> Self {
            velocity_idl::register_schemas();
            let mut ctx = TestContext::new();
            let program_id = velocity_program_id();
            ctx.add_program(&program_id, VELOCITY_SO).unwrap();

            // Register a no-op builtin at the native Ed25519 program id (see the
            // Ed25519NoopBuiltin doc comment). Without this the ed25519 pre-ix
            // fails with UnsupportedProgramId before the velocity ix runs.
            // `add_builtin` inserts the cache entry but sets the account owner to
            // bpf_loader; the runtime's builtin dispatch only routes to the
            // program's own id when the account is owned by the native loader, so
            // we overwrite the account to be native-loader-owned + executable
            // (the programs-cache entry keyed on the ed25519 id survives).
            ctx.svm
                .add_builtin(ed25519_program_id(), Ed25519NoopBuiltin::vm);
            ctx.create_account()
                .pubkey(ed25519_program_id())
                .owner(native_loader_id())
                .executable(true)
                .lamports(1)
                .create()
                .unwrap();

            let (signer_pda, signer_nonce) =
                Pubkey::find_program_address(&[b"velocity_signer"], &program_id);
            let (state_pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program_id);

            // State: exchange FULLY paused (0xFF == ExchangeStatus::is_all()), so
            // the fix's `exchange_not_paused` guard would reject with ExchangePaused.
            let mut state = State::default();
            state.signer = anchor_pk(signer_pda);
            state.signer_nonce = signer_nonce;
            state.exchange_status = 0xFF;
            state.number_of_spot_markets = 1;
            state.number_of_markets = 1;
            inject(&mut ctx, state_pda, &mut state);

            // Quote USDC spot market (index 0) + vault, $1 QuoteAsset oracle path.
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

            // Perp market 0 ($1 QuoteAsset oracle).
            let (perp_market_pda, _) =
                Pubkey::find_program_address(&[b"perp_market", &mi0], &program_id);
            let mut perp_market = build_perp_market(perp_market_pda);
            inject(&mut ctx, perp_market_pda, &mut perp_market);

            // Authority / taker.
            let authority = Rc::new(Keypair::new());
            ctx.create_account()
                .pubkey(authority.pubkey())
                .lamports(10_000_000_000)
                .owner(system_program_id())
                .create()
                .unwrap();

            // UserStats (authority-scoped) + User (sub 0) with a quote deposit so
            // the margin check has collateral on master's placement path.
            let (user_stats_pda, _) = Pubkey::find_program_address(
                &[b"user_stats", authority.pubkey().as_ref()],
                &program_id,
            );
            let mut user_stats: velocity::state::user::UserStats = bytemuck::Zeroable::zeroed();
            user_stats.authority = anchor_pk(authority.pubkey());
            inject(&mut ctx, user_stats_pda, &mut user_stats);

            let (user_pda, _) = Pubkey::find_program_address(
                &[b"user", authority.pubkey().as_ref(), &mi0],
                &program_id,
            );
            let mut user = User::default();
            user.authority = anchor_pk(authority.pubkey());
            user.sub_account_id = 0;
            user.next_order_id = 1;
            // Quote-market deposit collateral.
            user.spot_positions[0].market_index = 0;
            user.spot_positions[0].scaled_balance =
                1_000_000 * velocity::math::constants::SPOT_BALANCE_PRECISION as u64;
            user.spot_positions[0].balance_type =
                velocity::state::spot_market::SpotBalanceType::Deposit;
            inject(&mut ctx, user_pda, &mut user);

            // SignedMsgUserOrders PDA (8 empty slots) at [SIGNED_MSG, authority].
            let (signed_msg_pda, _) = Pubkey::find_program_address(
                &[SIGNED_MSG_PDA_SEED, authority.pubkey().as_ref()],
                &program_id,
            );
            create_signed_msg_account(&mut ctx, signed_msg_pda, authority.pubkey(), 8);

            Regr271Pause {
                ctx,
                program_id,
                state_pda,
                spot_market_pda,
                perp_market_pda,
                signed_msg_pda,
                user_pda,
                user_stats_pda,
                authority,
            }
        }

        /// Submit the signed-message taker order (ed25519 pre-ix + velocity ix)
        /// while the exchange is fully paused. Returns the program error code.
        pub fn place_while_paused(&mut self) -> Option<u32> {
            let slot = self.ctx.slot();
            let (envelope, message_size) = build_envelope(&self.authority, 0, slot);

            // velocity ix data: disc[8] | borsh-vec len u32 | envelope | bool.
            let mut data = Vec::new();
            data.extend_from_slice(&D_PLACE_SIGNED_MSG_TAKER);
            data.extend_from_slice(&(envelope.len() as u32).to_le_bytes());
            data.extend_from_slice(&envelope);
            data.push(0u8); // is_delegate_signer = false

            let velocity_ix = Instruction {
                program_id: self.program_id,
                accounts: vec![
                    AccountMeta::new_readonly(self.state_pda, false),
                    AccountMeta::new(self.user_pda, false),
                    AccountMeta::new(self.user_stats_pda, false),
                    AccountMeta::new(self.signed_msg_pda, false),
                    AccountMeta::new_readonly(self.authority.pubkey(), true),
                    AccountMeta::new_readonly(ix_sysvar_id(), false),
                    // remaining_accounts, in load_maps order [oracles, spot, perp]:
                    // quote spot market (w) then perp market (w). ($1 QuoteAsset
                    // oracle path needs no oracle account.)
                    AccountMeta::new(self.spot_market_pda, false),
                    AccountMeta::new(self.perp_market_pda, false),
                ],
                data,
            };

            // The velocity ix is at index 1 (ed25519 pre-ix at index 0).
            let ed25519_ix = build_ed25519_ix(1, message_size);

            self.ctx
                .raw_call(ed25519_ix)
                .signers(&[&self.authority])
                .add_transaction()
                .unwrap();
            self.ctx
                .raw_call(velocity_ix)
                .signers(&[&self.authority])
                .add_transaction()
                .unwrap();
            let outcome = self.ctx.send_batch().unwrap();
            outcome.and_then(|o| o.error_code())
        }

        pub fn action_noop(&mut self) {
            let _ = &self.ctx;
        }
    }

    fn build_spot_market_usdc(
        pubkey: Pubkey,
        mint: Pubkey,
        vault: Pubkey,
        if_vault: Pubkey,
    ) -> velocity::state::spot_market::SpotMarket {
        use velocity::{
            math::constants::{SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION},
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                spot_market::SpotMarket,
            },
        };
        let mut m = SpotMarket::default();
        m.pubkey = anchor_pk(pubkey);
        m.oracle = anchor_pk(Pubkey::new_from_array([0u8; 32]));
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
        m.historical_oracle_data = HistoricalOracleData::default_quote_oracle();
        m
    }

    fn build_perp_market(pubkey: Pubkey) -> velocity::state::perp_market::PerpMarket {
        use velocity::state::{
            market_status::MarketStatus,
            oracle::{HistoricalOracleData, OracleSource},
            perp_market::PerpMarket,
        };
        let mut m = PerpMarket::default();
        m.pubkey = anchor_pk(pubkey);
        m.oracle = anchor_pk(Pubkey::new_from_array([0u8; 32]));
        m.market_index = 0;
        m.quote_spot_market_index = 0;
        m.status = MarketStatus::Active;
        m.oracle_source = OracleSource::QuoteAsset;
        m.margin_ratio_initial = 1_000;
        m.margin_ratio_maintenance = 500;
        m.order_step_size = 1_000_000;
        m.order_tick_size = 1;
        m.market_stats.min_order_size = 1_000_000;
        m.market_stats.historical_oracle_data = HistoricalOracleData::default_quote_oracle();
        m
    }

    #[cfg(test)]
    mod smoke {
        use super::*;

        #[test]
        fn place_while_paused_reproduces() {
            let mut f = Regr271Pause::setup();
            let code = f.place_while_paused();
            eprintln!(
                "[SMOKE] place_signed_msg_taker (paused) error_code = {:?}",
                code
            );
            // On master this should NOT be ExchangePaused (6024).
        }
    }
}

#[cfg(feature = "regr_271_signed_msg_taker_pause")]
use regr_271_pause::Regr271Pause;

#[cfg(feature = "regr_271_signed_msg_taker_pause")]
#[crucible_fuzz]
fn regr_271_signed_msg_taker_pause(fixture: &mut Regr271Pause, #[range(0..1u8)] _unused: u8) {
    let code = fixture.place_while_paused();
    // FIXED behavior: `exchange_not_paused` (an access_control that runs before
    // the handler body) rejects with ExchangePaused (6024) when the exchange is
    // fully paused. On pre-fix master the guard is absent, so the ix proceeds
    // past it and `code` is never 6024 — this assertion then fails, reproducing
    // the bug.
    fuzz_assert_eq!(
        code,
        Some(E_EXCHANGE_PAUSED),
        "PR #271: place_signed_msg_taker_order was not blocked while the exchange was fully paused (got error_code={:?})",
        code
    );
}
