//! Program-derived addresses, in one place.
//!
//! Resolvers derive a lot of these: they run under simulation with a
//! four-account list and reconstruct the executor's full account set from
//! seeds. A wrong seed there is a bad failure mode — the resolver succeeds
//! and the *executor* fails on an account mismatch, far from the mistake —
//! so the derivations live here rather than being retyped per call site.

use {crate::state::clob_crank::CLOB_CRANK_CONDITIONS_PDA_SEED, anchor_lang::prelude::*};

/// Relay's payout sentinel, as a `Pubkey` — the slot a turner substitutes
/// its keeper into. Every staged executor must name it exactly once.
pub fn keeper_placeholder() -> Pubkey {
    Pubkey::new_from_array(relay_spec::KEEPER_PLACEHOLDER)
}

/// The shared resolver staging account.
pub fn relay_scratch() -> Pubkey {
    Pubkey::find_program_address(
        &[crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED],
        &crate::ID,
    )
    .0
}

pub fn state() -> Pubkey {
    Pubkey::find_program_address(&[b"velocity_state"], &crate::ID).0
}

/// The vault authority: SPL token authority on every `spot_market_vault` and
/// `insurance_fund_vault`, and the protocol `User`'s authority.
pub fn velocity_signer() -> Pubkey {
    Pubkey::find_program_address(&[crate::signer::VELOCITY_SIGNER_SEED], &crate::ID).0
}

pub fn user(authority: &Pubkey, sub_account_id: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            authority.as_ref(),
            sub_account_id.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    )
    .0
}

pub fn user_stats(authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_stats", authority.as_ref()], &crate::ID).0
}

/// A user's signed-message record, which carries the route any remainder of
/// theirs resting on a book was signed with.
pub fn signed_msg_user_orders(authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            crate::state::signed_msg_user::SIGNED_MSG_PDA_SEED.as_bytes(),
            authority.as_ref(),
        ],
        &crate::ID,
    )
    .0
}

/// The `(User, UserStats)` pair of a derivable identity — the whole point
/// of the CLOB storing `(authority, sub_account_id)` on its nodes.
pub fn user_pair(authority: &Pubkey, sub_account_id: u16) -> (Pubkey, Pubkey) {
    (user(authority, sub_account_id), user_stats(authority))
}

/// The protocol-owned `User` (the signer authority's first sub-account)
/// and its stats — the crank-incentive sink and the pass-through taker.
pub fn protocol_user_pair() -> (Pubkey, Pubkey) {
    let signer = velocity_signer();
    user_pair(&signer, 0)
}

pub fn perp_market(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        &crate::ID,
    )
    .0
}

pub fn spot_market(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"spot_market", market_index.to_le_bytes().as_ref()],
        &crate::ID,
    )
    .0
}

/// A market's CLOB crank conditions — the wake-hint host and the keeper
/// reservoir.
pub fn crank_treasury() -> Pubkey {
    Pubkey::find_program_address(
        &[crate::state::crank_treasury::CRANK_TREASURY_PDA_SEED],
        &crate::ID,
    )
    .0
}

pub fn quoter_slab(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            crate::state::prop_amm::QUOTER_SLAB_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    )
    .0
}

pub fn clob_crank_conditions(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    )
    .0
}
