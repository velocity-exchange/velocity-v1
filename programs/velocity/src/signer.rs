//! Velocity's program-signing identities, deliberately distinct keys.
//!
//! `velocity_signer` (`State::signer`) is a *privileged* key: it is the SPL
//! token authority on every `spot_market_vault` and `insurance_fund_vault`,
//! and the `User`/`UserStats` authority of the protocol account. It signs
//! token-program CPIs and nothing else.
//!
//! The other two exist so an external program velocity CPIs into can tell that
//! velocity — not an arbitrary caller — invoked it. Signer privilege is
//! inherited by a callee, so a callee handed `velocity_signer` could forward it
//! to the token program and move vault funds; that is why external CPIs sign as
//! one of these instead.
//!
//! They are two keys rather than one, and the split is load-bearing:
//!
//! - `clob_authority` is every book's `place_authority`, so it *is* an
//!   authority over an account: whoever holds it may place, cancel, evict,
//!   expire and execute on any market, for any user (`place_order_v0` takes its
//!   `user` as an argument and trusts the authority to have checked it). It is
//!   therefore never placed in a third party's account list — only velocity's
//!   own CLOB CPI paths sign as it, plus the `execute_v0`/`quote_v0` leg of a
//!   registry entry that velocity has confirmed *is* the CLOB.
//! - `quoter_signer` is per registry entry and is the authority on nothing. A
//!   third-party quoter receives its own entry's key and no other, which is
//!   what stops one quoter authenticating as velocity to a second quoter it
//!   also controls, and what stops any of them reaching a book.
//!
//! A single global key for both roles collapses that: every quoter's account
//! list has to carry the key its `execute_v0` authenticates against, so a
//! quoter whose approved list also named a book would hold the book's
//! `place_authority` as a live signature inside its own CPI.

use anchor_lang::prelude::Pubkey;

pub const VELOCITY_SIGNER_SEED: &[u8] = b"velocity_signer";
pub const QUOTER_SIGNER_SEED: &[u8] = b"quoter_signer";
pub const CLOB_AUTHORITY_SEED: &[u8] = b"clob_authority";

pub fn get_signer_seeds(nonce: &u8) -> [&[u8]; 2] {
    [VELOCITY_SIGNER_SEED, bytemuck::bytes_of(nonce)]
}

/// Signing seeds for one registry entry's quoter CPI signer.
pub fn get_quoter_signer_seeds<'a>(entry: &'a Pubkey, nonce: &'a u8) -> [&'a [u8]; 3] {
    [
        QUOTER_SIGNER_SEED,
        entry.as_ref(),
        bytemuck::bytes_of(nonce),
    ]
}

/// Signing seeds for the CLOB place authority.
pub fn get_clob_authority_seeds(nonce: &u8) -> [&[u8]; 2] {
    [CLOB_AUTHORITY_SEED, bytemuck::bytes_of(nonce)]
}

/// Derive the quoter CPI signer for one registry entry, and its bump.
///
/// Keyed by the entry rather than global so a quoter only ever receives a
/// signature that authenticates velocity *to itself*. Forwarding it to another
/// quoter proves nothing there, because that quoter authenticates against a key
/// derived from its own entry.
///
/// Not stored on `State` the way `signer`/`signer_nonce` are: there is nothing
/// to configure, and a `State` field left zeroed by an in-place upgrade of an
/// already-deployed account would read as `Pubkey::default()` — a wrong key
/// that fails obscurely rather than loudly. Deriving also means no admin write
/// can ever point this at the vault authority.
pub fn find_quoter_signer(entry: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[QUOTER_SIGNER_SEED, entry.as_ref()], &crate::ID)
}

/// Derive the CLOB place authority and its bump.
///
/// Global rather than per book: one key drives every market's book, and the
/// books are velocity's own program. What matters is that it is not the key any
/// third-party quoter is handed.
pub fn find_clob_authority() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CLOB_AUTHORITY_SEED], &crate::ID)
}
