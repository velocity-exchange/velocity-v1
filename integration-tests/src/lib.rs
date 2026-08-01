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
    // Every resolver names the program-wide staging account, so it exists
    // from the start rather than each fixture remembering to create it.
    set_relay_scratch(&mut svm);
    svm
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
pub fn set_perp_market(svm: &mut LiteSVM, market_index: u16) {
    let mut market: PerpMarket = Zeroable::zeroed();
    market.market_index = market_index;
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

/// Read a zero-copy account body (past the discriminator), unaligned.
pub fn read_zero_copy<T: bytemuck::Pod>(svm: &LiteSVM, address: &Pubkey) -> T {
    let account = svm.get_account(address).expect("account missing");
    bytemuck::pod_read_unaligned(&account.data[8..8 + core::mem::size_of::<T>()])
}
