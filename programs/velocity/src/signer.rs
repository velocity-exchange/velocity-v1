//! Velocity's program-signing identities, deliberately distinct keys.
//!
//! `velocity_signer` (`State::signer`) is a *privileged* key: it is the SPL
//! token authority on every `spot_market_vault` and `insurance_fund_vault`,
//! and the `User`/`UserStats` authority of the protocol account. It signs
//! token-program CPIs and nothing else.
//!
//! Every external quoter CPI — the book's place/cancel/crank surface and the
//! registry's quote/execute legs alike — signs as the market's
//! [`crate::state::prop_amm::QuoterSlabV0`] PDA instead. The slab exists in
//! every instruction that CPIs a quoter, so one shared identity costs zero
//! extra accounts, where a per-quoter signer cost one account per quoter per
//! transaction. It is a separate key from `velocity_signer` because signer
//! privilege is inherited by a CPI callee: a callee handed `velocity_signer`
//! could forward it to the token program and move vault funds.
//!
//! One key for every quoter on a market means a quoter's `execute_v0` holds,
//! live inside its own CPI, the same signature that authenticates velocity at
//! every other quoter on that market — including the book, whose
//! `place_authority` it is. Two facts keep that forwarding worthless:
//!
//! - A CPI can only name accounts the caller received, and a quoter receives
//!   exactly its registered leg accounts plus the slab. Approval refuses a
//!   registered list that names any other approved quoter's response account
//!   (`update_quoter_approved`, both directions), and it refuses one that
//!   names the market's book. Every authority-trusting instruction on the
//!   book and on the midpoint requires its response account, so the
//!   forwarded signature has no instruction it can complete.
//! - The slab is per market, so the signature authenticates nothing on any
//!   other market's quoters or book. A quoter binds the key at its own
//!   registration and compares against the stored copy, so a signature from
//!   another market's slab fails the comparison.
//!
//! Two instructions enforce this, and between them they cover both orders of
//! events. A slot's registered list and its response account change nowhere
//! else: `update_quoter_approved` writes the whole config, and every other
//! writer sets one scalar (`is_active`, `priority`,
//! `max_oracle_deviation_bps`, the book's tick and minimum size).
//!
//! - Approval holds the config it copies in apart from every slot already on
//!   the slab, and apart from the account the market names as its book. The
//!   book needs naming separately because a market designates it at
//!   registration and it reaches slot 0 only at its own approval. A list
//!   approved during that window would otherwise pass a sweep of the slots.
//! - Registration holds a book designation apart from every slot already on
//!   the slab (`initialize_quoter`). That is the same rule read the other way
//!   round: an entry approved before the designation was never held to it.
//!
//! The exclusion covers every approved slot on the slab, where one fill sees
//! only the slots it consults — and the attack does not need the victim in
//! the transaction, so a per-fill re-check could not establish the property
//! even if the slab were small enough to sweep.
//!
//! For the book and the midpoint the second fact is structural rather than
//! reviewed: each stores its authority **on** its response account — the
//! book's `place_authority` on its market account, the midpoint's
//! `execute_authority` on its quoter account — and both write their responses
//! there too. An instruction cannot read the authority without taking the
//! account, so it cannot be gated on the slab signer and skip the response
//! account.
//!
//! That is the rule a third-party quoter program should follow: keep the
//! execute authority on the account the response is written to. An author who
//! puts it elsewhere can gate an instruction on the slab signer without
//! naming the response account, and a quoter on the same market could then
//! complete that call with a forwarded signature. Approving such a program
//! carries the matching obligation on the admin: check that every instruction
//! gated on the slab signer also requires the program's response account.
//!
//! The slab must never be made an authority over anything of value — not a
//! token authority, not a `User` authority. The test below pins it apart from
//! `velocity_signer`.
//!
//! Signing seeds live beside the PDA seed:
//! [`crate::state::prop_amm::get_quoter_slab_signer_seeds`]; the bump is
//! stored in the slab header at creation.

pub const VELOCITY_SIGNER_SEED: &[u8] = b"velocity_signer";

pub fn get_signer_seeds(nonce: &u8) -> [&[u8]; 2] {
    [VELOCITY_SIGNER_SEED, bytemuck::bytes_of(nonce)]
}

#[cfg(test)]
mod tests {
    use {crate::state::pdas, anchor_lang::prelude::Pubkey};

    /// The quoter CPI signer must never be the vault authority.
    ///
    /// `velocity_signer` is the SPL token authority on every spot and IF
    /// vault. Signer privilege is inherited by a CPI callee, so a quoter
    /// handed `velocity_signer` could forward it to the token program and
    /// drain a vault. Every quoter CPI instead signs as the market's slab.
    /// This pins the separation the whole quoter safety model rests on: a
    /// seed change that collided the two fails here.
    #[test]
    fn slab_signers_are_never_the_vault_authority() {
        let (velocity_signer, _) =
            Pubkey::find_program_address(&[super::VELOCITY_SIGNER_SEED], &crate::ID);
        for market in 0u16..64 {
            assert_ne!(
                pdas::quoter_slab(market),
                velocity_signer,
                "a quoter slab collided with the vault authority"
            );
        }
    }
}
