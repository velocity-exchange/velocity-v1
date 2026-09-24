//! Velocity's program-derived addresses, for a program id read at runtime.
//!
//! The program derives the same set in `velocity::state::pdas`, closed over
//! its own compile-time id. A service takes the id from configuration, so it
//! cannot call those. The `pin` tests below hold this module to that one: each
//! derivation is asserted equal at the program's own id, so the two sets
//! cannot drift.

use {
    program::state::{
        clob_crank::CLOB_CRANK_CONDITIONS_PDA_SEED,
        prop_amm::{ClobUserRefV0, QUOTER_SLAB_PDA_SEED},
    },
    solana_sdk::pubkey::Pubkey,
};

/// A user's two accounts. The CLOB stores `(authority, sub_account_id)` on its
/// nodes, so both derive from a node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserPair {
    pub user: Pubkey,
    pub stats: Pubkey,
}

pub fn state(velocity: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"velocity_state"], velocity).0
}

/// The vault authority: SPL token authority on every `spot_market_vault` and
/// `insurance_fund_vault`, and the protocol `User`'s authority.
pub fn velocity_signer(velocity: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"velocity_signer"], velocity).0
}

pub fn user(velocity: &Pubkey, authority: &Pubkey, sub_account_id: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            authority.as_ref(),
            sub_account_id.to_le_bytes().as_ref(),
        ],
        velocity,
    )
    .0
}

/// The `User` a wire identity derives to. The CLOB and the quote view both
/// name a maker this way.
pub fn user_of(velocity: &Pubkey, identity: &ClobUserRefV0) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            identity.authority.as_ref(),
            identity.sub_account_id.to_le_bytes().as_ref(),
        ],
        velocity,
    )
    .0
}

pub fn user_stats(velocity: &Pubkey, authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_stats", authority.as_ref()], velocity).0
}

pub fn user_pair(velocity: &Pubkey, authority: &Pubkey, sub_account_id: u16) -> UserPair {
    UserPair {
        user: user(velocity, authority, sub_account_id),
        stats: user_stats(velocity, authority),
    }
}

/// The protocol-owned `User` and its stats. The `User` is the signer
/// authority's first sub-account. It receives the crank incentives and acts as
/// the pass-through taker.
pub fn protocol_user_pair(velocity: &Pubkey) -> UserPair {
    user_pair(velocity, &velocity_signer(velocity), 0)
}

pub fn perp_market(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        velocity,
    )
    .0
}

pub fn spot_market(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"spot_market", market_index.to_le_bytes().as_ref()],
        velocity,
    )
    .0
}

/// A market's `QuoterSlabV0`. There is one per market, and it holds every
/// approved quoter config. Fills and quote views read quoters from it.
pub fn quoter_slab(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[QUOTER_SLAB_PDA_SEED, market_index.to_le_bytes().as_ref()],
        velocity,
    )
    .0
}

/// A market's CLOB crank conditions. It hosts the wakes and holds the keeper
/// reservoir.
pub fn clob_crank_conditions(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        velocity,
    )
    .0
}

#[cfg(test)]
mod pin {
    use super::*;

    fn authority() -> Pubkey {
        Pubkey::new_from_array([7; 32])
    }

    #[test]
    fn state_matches_the_program() {
        assert_eq!(state(&program::ID), program::state::pdas::state());
    }

    #[test]
    fn velocity_signer_matches_the_program() {
        assert_eq!(
            velocity_signer(&program::ID),
            program::state::pdas::velocity_signer()
        );
    }

    #[test]
    fn user_matches_the_program() {
        assert_eq!(
            user(&program::ID, &authority(), 3),
            program::state::pdas::user(&authority(), 3)
        );
    }

    #[test]
    fn user_of_matches_the_program() {
        let identity = ClobUserRefV0 {
            authority: authority(),
            sub_account_id: 3,
        };

        assert_eq!(
            user_of(&program::ID, &identity),
            program::state::pdas::user(&authority(), 3)
        );
    }

    #[test]
    fn user_stats_matches_the_program() {
        assert_eq!(
            user_stats(&program::ID, &authority()),
            program::state::pdas::user_stats(&authority())
        );
    }

    #[test]
    fn user_pair_matches_the_program() {
        let (expected_user, expected_stats) = program::state::pdas::user_pair(&authority(), 3);
        let pair = user_pair(&program::ID, &authority(), 3);

        assert_eq!(pair.user, expected_user);
        assert_eq!(pair.stats, expected_stats);
    }

    #[test]
    fn protocol_user_pair_matches_the_program() {
        let (expected_user, expected_stats) = program::state::pdas::protocol_user_pair();
        let pair = protocol_user_pair(&program::ID);

        assert_eq!(pair.user, expected_user);
        assert_eq!(pair.stats, expected_stats);
    }

    #[test]
    fn perp_market_matches_the_program() {
        assert_eq!(
            perp_market(&program::ID, 5),
            program::state::pdas::perp_market(5)
        );
    }

    #[test]
    fn spot_market_matches_the_program() {
        assert_eq!(
            spot_market(&program::ID, 5),
            program::state::pdas::spot_market(5)
        );
    }

    #[test]
    fn quoter_slab_matches_the_program() {
        assert_eq!(
            quoter_slab(&program::ID, 5),
            program::state::pdas::quoter_slab(5)
        );
    }

    #[test]
    fn clob_crank_conditions_matches_the_program() {
        assert_eq!(
            clob_crank_conditions(&program::ID, 5),
            program::state::pdas::clob_crank_conditions(5)
        );
    }
}
