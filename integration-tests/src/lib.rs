//! Cross-program litesvm harness for the PropAMM/router work: loads the
//! compiled velocity + CLOB `.so` fixtures and drives real instructions.
//! Heavy protocol state (State, PerpMarket, User) is synthesized directly via
//! `set_account` where a test only needs account identity, not the init flow.

use {
    anchor_lang::Discriminator,
    bytemuck::Zeroable,
    litesvm::{
        types::{FailedTransactionMetadata, TransactionMetadata},
        LiteSVM,
    },
    solana_account::Account,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    velocity::state::{perp_market::PerpMarket, state::State, traits::Size, user::User},
};

pub const VELOCITY_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/velocity.so");
pub const CLOB_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../anchor-v2/target/deploy/clob.so"
);
pub const MIDPOINT_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../anchor-v2/target/deploy/midpoint.so"
);

pub fn velocity_id() -> Pubkey {
    Pubkey::new_from_array(velocity::ID.to_bytes())
}

pub fn clob_id() -> Pubkey {
    "BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU"
        .parse()
        .unwrap()
}

pub fn midpoint_id() -> Pubkey {
    "eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D"
        .parse()
        .unwrap()
}

/// Fresh SVM with all program fixtures loaded.
pub fn svm() -> LiteSVM {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(velocity_id(), VELOCITY_SO)
        .expect("velocity.so missing — run `bun run program:build` first");
    svm.add_program_from_file(clob_id(), CLOB_SO)
        .expect("clob.so missing — run `bun run program:build:clob` first");
    svm.add_program_from_file(midpoint_id(), MIDPOINT_SO)
        .expect("midpoint.so missing — run `bun run program:build:midpoint` first");
    // Approval reads the deploy slot out of a program's program-data account,
    // and litesvm loads a program under the upgradeable loader without writing
    // one. Every fixture program gets one.
    for program in [clob_id(), midpoint_id()] {
        set_program_data(&mut svm, &program);
    }
    // Every resolver names the program-wide staging account, so it exists
    // from the start rather than each fixture remembering to create it.
    set_relay_scratch(&mut svm);
    set_crank_treasury(&mut svm);
    svm
}

/// `BPFLoaderUpgradeab1e11111111111111111111111`, which owns a program's data
/// account.
pub fn bpf_loader_upgradeable_id() -> Pubkey {
    "BPFLoaderUpgradeab1e11111111111111111111111"
        .parse()
        .unwrap()
}

/// The program-data account for `program`, where the upgradeable loader keeps
/// the upgrade authority.
pub fn program_data_pda(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program.as_ref()], &bpf_loader_upgradeable_id()).0
}

/// Write a `ProgramData` account for `program`, which approval reads to record
/// the slot the program was last deployed at.
///
/// Layout: a four-byte enum tag (3 = `ProgramData`), the deploy slot, then the
/// upgrade authority behind an option tag. Both are zero here, so an approval
/// records slot zero.
pub fn set_program_data(svm: &mut LiteSVM, program: &Pubkey) -> Pubkey {
    let address = program_data_pda(program);
    let mut data = vec![0u8; 45];
    data[0] = 3;
    svm.set_account(
        address,
        Account {
            lamports: 100_000_000_000,
            data,
            owner: bpf_loader_upgradeable_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    address
}

pub fn state_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"velocity_state"], &velocity_id()).0
}

pub fn velocity_signer_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"velocity_signer"], &velocity_id())
}

/// Anchor default instruction discriminator: sha256("global:<name>")[..8].
pub fn ix_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(format!("global:{name}").as_bytes());
    hash[..8].try_into().unwrap()
}

pub fn perp_market_pda(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        &velocity_id(),
    )
    .0
}

pub fn quoter_pda(market_index: u16, quoter_program: &Pubkey, user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"quoter",
            market_index.to_le_bytes().as_ref(),
            quoter_program.as_ref(),
            user.as_ref(),
        ],
        &velocity_id(),
    )
    .0
}

/// The market's quoter slab: one account per market, holding every approved
/// quoter config. Fills read only this account, never the staging entries.
/// It is also the one identity velocity signs every external quoter CPI as —
/// the book's `place_authority` and each quoter's `execute_authority`.
/// Deliberately a different PDA from [`velocity_signer_pda`], which is the
/// token authority on every vault; response-account exclusion at approval and
/// the per-market seed keep the shared signature harmless (see
/// `programs/velocity/src/signer.rs`).
pub fn quoter_slab_pda(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"quoter_slab", market_index.to_le_bytes().as_ref()],
        &velocity_id(),
    )
    .0
}

/// Create the market's quoter slab through the real (permissionless)
/// instruction. A slab is born at capacity 1 (slot 0, the book's); approval
/// grows it by exactly the slot it needs. Returns the slab address.
pub fn create_quoter_slab(svm: &mut LiteSVM, payer: &Keypair, market_index: u16) -> Pubkey {
    use anchor_lang::{InstructionData, ToAccountMetas};
    let slab = quoter_slab_pda(market_index);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoterSlab {
            payer: payer.pubkey(),
            perp_market: perp_market_pda(market_index),
            quoter_slab: slab,
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoterSlab {
            args: velocity::instructions::InitializeQuoterSlabArgs { market_index },
        }
        .data(),
    };
    send(svm, payer, ix, &[]).unwrap();
    slab
}

/// One slot of a market's slab, read out of the account bytes the way the
/// program reads them.
pub fn read_slab_slot(
    svm: &LiteSVM,
    market_index: u16,
    index: usize,
) -> velocity::state::prop_amm::QuoterSlotV0 {
    use velocity::state::prop_amm::{QuoterSlabV0, QuoterSlotV0};
    let account = svm
        .get_account(&quoter_slab_pda(market_index))
        .expect("quoter slab missing");
    let at = QuoterSlabV0::SLOT_REGION_OFFSET + index * core::mem::size_of::<QuoterSlotV0>();
    bytemuck::pod_read_unaligned(&account.data[at..at + core::mem::size_of::<QuoterSlotV0>()])
}

/// The slot holding `entry`'s approved copy, with its index.
pub fn find_slab_slot(
    svm: &LiteSVM,
    market_index: u16,
    entry: &Pubkey,
) -> Option<(usize, velocity::state::prop_amm::QuoterSlotV0)> {
    use velocity::state::prop_amm::QuoterSlabV0;
    let slab: QuoterSlabV0 = read_zero_copy(svm, &quoter_slab_pda(market_index));
    (0..slab.capacity as usize)
        .map(|index| (index, read_slab_slot(svm, market_index, index)))
        .find(|(_, slot)| slot.entry.to_bytes() == entry.to_bytes())
}

/// Overwrite one slot of a market's slab in place. Fixture-only: production
/// writes slots through the approval flow.
pub fn write_slab_slot(
    svm: &mut LiteSVM,
    market_index: u16,
    index: usize,
    slot: &velocity::state::prop_amm::QuoterSlotV0,
) {
    use velocity::state::prop_amm::{QuoterSlabV0, QuoterSlotV0};
    let address = quoter_slab_pda(market_index);
    let mut account = svm.get_account(&address).expect("quoter slab missing");
    let at = QuoterSlabV0::SLOT_REGION_OFFSET + index * core::mem::size_of::<QuoterSlotV0>();
    account.data[at..at + core::mem::size_of::<QuoterSlotV0>()]
        .copy_from_slice(bytemuck::bytes_of(slot));
    svm.set_account(address, account).unwrap();
}

/// Write a velocity-owned zero-copy account: discriminator + Pod bytes.
pub fn set_zero_copy_account<T: bytemuck::Pod>(
    svm: &mut LiteSVM,
    address: Pubkey,
    discriminator: &[u8],
    value: &T,
    size: usize,
) {
    let mut data = vec![0u8; size];
    data[..discriminator.len()].copy_from_slice(discriminator);
    data[discriminator.len()..discriminator.len() + core::mem::size_of::<T>()]
        .copy_from_slice(bytemuck::bytes_of(value));
    svm.set_account(
        address,
        Account {
            lamports: 100_000_000_000,
            data,
            owner: velocity_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// Minimal `State` at the canonical PDA: admin gating (`warm_admin`) plus the
/// velocity signer PDA (quoter CPIs `invoke_signed` as it) — tests that need
/// real protocol config should run the init flow instead.
pub fn set_state(svm: &mut LiteSVM, warm_admin: &Pubkey) {
    let mut state: State = Zeroable::zeroed();
    state.warm_admin = anchor_lang::prelude::Pubkey::new_from_array(warm_admin.to_bytes());
    let (signer, nonce) = velocity_signer_pda();
    state.signer = anchor_lang::prelude::Pubkey::new_from_array(signer.to_bytes());
    state.signer_nonce = nonce;
    // What `initialize` writes, so a fixture prices a crank the way a fresh
    // exchange does. A zeroed rails prices every crank at nothing, which the
    // attach refuses.
    state.transaction_fee_rails = velocity::state::state::TransactionFeeRails::FLAT_PER_SIGNATURE;
    set_zero_copy_account(svm, state_pda(), State::DISCRIMINATOR, &state, State::SIZE);
}

/// The program-wide resolver staging account. Every resolver names it, so
/// any fixture that resolves anything needs it to exist.
pub fn set_relay_scratch(svm: &mut LiteSVM) {
    let scratch: velocity::state::relay_scratch::RelayScratchV0 = Zeroable::zeroed();
    set_zero_copy_account(
        svm,
        relay_scratch_pda(),
        velocity::state::relay_scratch::RelayScratchV0::DISCRIMINATOR,
        &scratch,
        velocity::state::relay_scratch::RelayScratchV0::SIZE,
    );
}

/// The protocol crank treasury. The CLOB crank resolver reads the levels a
/// reservoir is held between from it, and the liquidation-conditions resync is
/// paid out of it, so any fixture that resolves or resyncs needs it to exist.
///
/// Funded well past its rent so a refill staged in a fixture has something to
/// move, and priced the way a live deployment is.
pub fn set_crank_treasury(svm: &mut LiteSVM) {
    let mut treasury: velocity::state::crank_treasury::CrankTreasuryV0 = Zeroable::zeroed();
    treasury.refill_target_cranks = 1_000;
    treasury.refill_watermark_cranks = 100;
    set_zero_copy_account(
        svm,
        crank_treasury_pda(),
        velocity::state::crank_treasury::CrankTreasuryV0::DISCRIMINATOR,
        &treasury,
        velocity::state::crank_treasury::CrankTreasuryV0::SIZE,
    );
    let mut account = svm.get_account(&crank_treasury_pda()).unwrap();
    account.lamports = account.lamports.saturating_add(5_000_000_000);
    svm.set_account(crank_treasury_pda(), account).unwrap();
}

pub fn crank_treasury_pda() -> Pubkey {
    Pubkey::new_from_array(
        anchor_lang::prelude::Pubkey::find_program_address(
            &[velocity::state::crank_treasury::CRANK_TREASURY_PDA_SEED],
            &velocity::ID,
        )
        .0
        .to_bytes(),
    )
}

pub fn relay_scratch_pda() -> Pubkey {
    Pubkey::new_from_array(
        anchor_lang::prelude::Pubkey::find_program_address(
            &[velocity::state::relay_scratch::RELAY_SCRATCH_PDA_SEED],
            &velocity::ID,
        )
        .0
        .to_bytes(),
    )
}

/// Zeroed `PerpMarket` at the index-derived PDA — account identity only.
/// `quoter_slab` carries the slab PDA, which `initialize_perp_market` writes,
/// so the `has_one = quoter_slab` contexts accept the fixture. `clob_market`
/// stays default: the fixture has no book, and a Clob registration writes it.
pub fn set_perp_market(svm: &mut LiteSVM, market_index: u16) {
    let mut market: PerpMarket = Zeroable::zeroed();
    market.market_index = market_index;
    market.quoter_slab =
        anchor_lang::prelude::Pubkey::new_from_array(quoter_slab_pda(market_index).to_bytes());
    set_zero_copy_account(
        svm,
        perp_market_pda(market_index),
        PerpMarket::DISCRIMINATOR,
        &market,
        PerpMarket::SIZE,
    );
}

/// Zeroed `User` owned by `authority` at an arbitrary address.
pub fn set_user(svm: &mut LiteSVM, address: Pubkey, authority: &Pubkey) {
    let mut user: User = Zeroable::zeroed();
    user.authority = anchor_lang::prelude::Pubkey::new_from_array(authority.to_bytes());
    set_zero_copy_account(svm, address, User::DISCRIMINATOR, &user, User::SIZE);
}

/// A raw `SetComputeUnitLimit` instruction, for transactions that outgrow
/// the 200k default (e.g. a router fill spanning several CPI quoters).
pub fn compute_unit_limit_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: "ComputeBudget111111111111111111111111111111"
            .parse()
            .unwrap(),
        accounts: vec![],
        data,
    }
}

/// Send one instruction signed by `payer` plus any of `extra_signers` the
/// metas mark as signers.
pub fn send(
    svm: &mut LiteSVM,
    payer: &Keypair,
    ix: Instruction,
    extra_signers: &[&Keypair],
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    send_with_ixs(svm, payer, &[ix], extra_signers)
}

/// `send`, but with the caller controlling the full instruction list.
pub fn send_with_ixs(
    svm: &mut LiteSVM,
    payer: &Keypair,
    ixs: &[Instruction],
    extra_signers: &[&Keypair],
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &blockhash);
    let signers: Vec<&Keypair> = std::iter::once(payer)
        .chain(extra_signers.iter().copied().filter(|kp| {
            kp.pubkey() != payer.pubkey()
                && ixs.iter().any(|ix| {
                    ix.accounts
                        .iter()
                        .any(|m| m.is_signer && m.pubkey == kp.pubkey())
                })
        }))
        .collect();
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    svm.send_transaction(tx)
}

/// Run one instruction without landing it, and hand back its return data
/// together with the post-simulation bytes of `observed`.
///
/// What an off-chain reader does, and what a relay turner does with a
/// resolver. The legs that stream their answer into an account write it in the
/// simulated post-state, so a caller reads the pointer out of return data and
/// the payload out of the account it names.
pub fn simulate(
    svm: &LiteSVM,
    payer: &Keypair,
    ix: Instruction,
    observed: &Pubkey,
) -> Result<(Vec<u8>, Vec<u8>), FailedTransactionMetadata> {
    let msg = Message::new_with_blockhash(&[ix], Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer]).unwrap();
    use solana_account::ReadableAccount;
    let info = svm.simulate_transaction(tx)?;
    let data = info
        .post_accounts
        .iter()
        .find(|(key, _)| key.to_bytes() == observed.to_bytes())
        .map(|(_, account)| account.data().to_vec())
        .unwrap_or_default();
    Ok((info.meta.return_data.data, data))
}

/// Read a zero-copy account body (past the discriminator), unaligned.
pub fn read_zero_copy<T: bytemuck::Pod>(svm: &LiteSVM, address: &Pubkey) -> T {
    let account = svm.get_account(address).expect("account missing");
    bytemuck::pod_read_unaligned(&account.data[8..8 + core::mem::size_of::<T>()])
}

/// Decode every anchor event of one type out of a transaction's logs.
///
/// Velocity writes its records base64-encoded through `msg!`, which the
/// runtime renders as a `Program log:` line, while anchor's own `emit!` uses
/// `sol_log_data` and renders as `Program data:`. Both are read here, because
/// this is meant to see what an off-chain subscriber sees rather than what the
/// program happened to hand back.
pub fn events<T: anchor_lang::Discriminator + anchor_lang::AnchorDeserialize>(
    meta: &TransactionMetadata,
) -> Vec<T> {
    meta.logs
        .iter()
        .filter_map(|line| {
            line.strip_prefix("Program data: ")
                .or_else(|| line.strip_prefix("Program log: "))
        })
        .filter_map(|encoded| base64_decode(encoded))
        .filter(|bytes| bytes.len() > 8 && bytes[..8] == T::DISCRIMINATOR[..])
        .filter_map(|bytes| T::try_from_slice(&bytes[8..]).ok())
        .collect()
}

/// Standard base64, enough for the fixed alphabet the runtime emits.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0u32);
    for byte in input.bytes().take_while(|byte| *byte != b'=') {
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Assert a failed send reported `expected`.
///
/// The number a variant reports is its position in `ErrorCode`, so a variant
/// added above it renumbers every variant below. Deriving the number from the
/// enum keeps an assertion pointed at the error it names instead of at a
/// literal that the next insertion silently reassigns to a different error.
pub fn assert_velocity_error(
    err: &FailedTransactionMetadata,
    expected: velocity::error::ErrorCode,
) {
    let code = u32::from(expected);
    let text = format!("{:?}", err.err);
    // The program logs say which check fired. A bare code is not enough to
    // tell a `DefaultError` raised by one `validate!` from another.
    assert!(
        text.contains(&code.to_string()),
        "expected {expected:?} ({code}), got {text}\nlogs: {:#?}",
        err.meta.logs
    );
}
