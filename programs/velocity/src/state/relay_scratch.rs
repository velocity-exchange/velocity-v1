//! The one scratch account every relay resolver stages into.
//!
//! A resolver's output — the executor's account list and args — is written
//! somewhere a turner can read it back out of the simulation, and until
//! now that somewhere was a region on each conditions account. That is
//! kilobytes per account of space whose contents are meaningless on chain:
//! resolvers run *only* under simulation, so a staged payload is never
//! committed, and two turners simulating at the same time cannot collide
//! because neither one's writes exist outside its own simulation.
//!
//! So it does not need to be per account, and per account is expensive —
//! 2KB on every user's conditions account is rent every user pays for
//! scratch space. One account, shared by every block in the program,
//! serves the same purpose.
//!
//! It is a normal writable account in each resolver's account list, always
//! at index 0, which is what a [`relay_spec::ResponsePointerV0`] means by
//! `account_index`.

use {
    crate::error::ErrorCode,
    anchor_lang::prelude::*,
    relay_spec::{ResolvedCrankV0, RESPONSE_POINTER_LEN},
};

/// PDA seed: `["relay_scratch"]`. One per program.
pub const RELAY_SCRATCH_PDA_SEED: &[u8] = b"relay_scratch";

/// Big enough for the largest executor any resolver stages: the
/// liquidation fill's account list runs to the user's full margin map
/// plus the maker side.
pub const RELAY_SCRATCH_LEN: usize = 4096;

/// Index of the scratch account in every resolver's account list. Fixed
/// by convention rather than per resolver, so the pointer a resolver
/// returns means the same thing everywhere.
pub const RELAY_SCRATCH_ACCOUNT_INDEX: u8 = 0;

/// Account-data offset of the scratch region (past anchor's discriminator).
pub const RELAY_SCRATCH_OFFSET: u32 = 8;

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct RelayScratchV0 {
    pub scratch: [u8; RELAY_SCRATCH_LEN],
}

impl Default for RelayScratchV0 {
    fn default() -> Self {
        Self {
            scratch: [0; RELAY_SCRATCH_LEN],
        }
    }
}

impl RelayScratchV0 {
    pub const SIZE: usize = 8 + RELAY_SCRATCH_LEN;

    /// Write a resolved crank here and return the pointer the turner
    /// follows to find it.
    pub fn stage(&mut self, resolved: &ResolvedCrankV0) -> Result<[u8; RESPONSE_POINTER_LEN]> {
        relay_spec::stage_into(
            &mut self.scratch,
            RELAY_SCRATCH_ACCOUNT_INDEX,
            RELAY_SCRATCH_OFFSET,
            resolved,
        )
        .map_err(|e| {
            msg!("staging a resolved crank failed: {:?}", e);
            error!(ErrorCode::DefaultError)
        })
    }
}

const _: () = assert!((RelayScratchV0::SIZE - 8).is_multiple_of(16));
const _: () = assert!(RelayScratchV0::SIZE <= 10_240);

#[cfg(test)]
mod tests {
    use super::*;

    /// Deploy-time sizes live in `deploy-scripts/migrate.ts` too, so they
    /// are printed here rather than looked up by hand.
    #[test]
    fn sizes_for_the_migration_script() {
        println!(
            "RelayScratchV0={} ClobCrankConditionsV0={} QuoterCrossConditionsV0={} UserConditionsV0={}",
            RelayScratchV0::SIZE,
            crate::state::clob_crank::ClobCrankConditionsV0::SIZE,
            crate::state::quoter_cross::QuoterCrossConditionsV0::SIZE,
            crate::state::user_conditions::UserConditionsV0::SIZE,
        );
    }
}
