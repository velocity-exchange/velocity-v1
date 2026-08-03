//! Velocity's two program-signing identities, deliberately distinct keys.
//!
//! `velocity_signer` (`State::signer`) is a *privileged* key: it is the SPL
//! token authority on every `spot_market_vault` and `insurance_fund_vault`,
//! and the `User`/`UserStats` authority of the protocol account. It signs
//! token-program CPIs and nothing else.
//!
//! `quoter_signer` is the authority on nothing. It exists only so an external
//! program velocity CPIs into (a registered quoter, the CLOB) can tell that
//! velocity — not an arbitrary caller — invoked it. Signer privilege is
//! inherited by a callee, so a callee handed `velocity_signer` could forward
//! it to the token program and move vault funds; that is why external CPIs
//! sign as this key instead. Nothing may ever make `quoter_signer` an
//! authority over an account.

use anchor_lang::prelude::Pubkey;

pub const VELOCITY_SIGNER_SEED: &[u8] = b"velocity_signer";
pub const QUOTER_SIGNER_SEED: &[u8] = b"quoter_signer";

pub fn get_signer_seeds(nonce: &u8) -> [&[u8]; 2] {
    [VELOCITY_SIGNER_SEED, bytemuck::bytes_of(nonce)]
}

pub fn get_quoter_signer_seeds(nonce: &u8) -> [&[u8]; 2] {
    [QUOTER_SIGNER_SEED, bytemuck::bytes_of(nonce)]
}

/// Derive the quoter CPI signer and its bump.
///
/// Not stored on `State` the way `signer`/`signer_nonce` are: there is nothing
/// to configure, and a `State` field left zeroed by an in-place upgrade of an
/// already-deployed account would read as `Pubkey::default()` — a wrong key
/// that fails obscurely rather than loudly. Deriving also means no admin write
/// can ever point this at the vault authority.
pub fn find_quoter_signer() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[QUOTER_SIGNER_SEED], &crate::ID)
}
