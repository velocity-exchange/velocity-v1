//! Per-quoter relay condition block for PropAMM×CLOB cross discovery.
//!
//! One account per Custom registry entry (PDA off the entry key, rent on
//! whoever attaches it — normally the maker), so discovery scales to any
//! number of PropAMMs with zero per-quoter velocity code: the resolver
//! prices the quoter *generically*, by CPI-ing its registered `quote_v0`
//! surface under simulation — the same interface every fill uses — and the
//! wake comes from the entry's maker-declared reprice region
//! (`QuoterV0::watch_*`). Three conditions:
//!
//! - [`QUOTER_CROSS_WATCH`] — `OnAccountChange` over the declared watch
//!   region (inactive when the maker declared none).
//! - [`QUOTER_CROSS_CLOB`] — `OnAccountChange` over the CLOB's bests: a
//!   cross can be created from the CLOB side too (a new crossing order is
//!   always a new best).
//! - [`QUOTER_CROSS_FALLBACK`] — `EverySlots`, the liveness floor for both
//!   (and the only wake when no watch region is declared).
//!
//! All three run [`ResolveCrankCrossMatchQuoter`] and stage the same
//! `crank_cross_match` executor the CLOB×CLOB conditions do; the market's
//! `ClobCrankConditionsV0` reservoir pays the keeper either way.

use {
    crate::{error::ErrorCode, msg},
    anchor_lang::prelude::*,
    relay_spec::{
        ConditionBlockHeaderV0, ResolvedCrankV0, ResponsePointerV0, BLOCK_HEADER_LEN, CONDITION_LEN,
    },
};

/// PDA seed: `["quoter_cross_conditions", quoter entry key]`.
pub const QUOTER_CROSS_CONDITIONS_PDA_SEED: &[u8] = b"quoter_cross_conditions";

/// Index of the maker-declared reprice watch.
pub const QUOTER_CROSS_WATCH: usize = 0;
/// Index of the CLOB-bests watch (the other side of the cross).
pub const QUOTER_CROSS_CLOB: usize = 1;
/// Index of the periodic fallback poll.
pub const QUOTER_CROSS_FALLBACK: usize = 2;
/// Conditions hosted per entry.
pub const QUOTER_CROSS_CONDITIONS: usize = 3;

/// Bytes the condition block occupies: header + the fixed condition array.
pub const QUOTER_CROSS_BLOCK_LEN: usize =
    BLOCK_HEADER_LEN + QUOTER_CROSS_CONDITIONS * CONDITION_LEN;

/// Resolver staging bytes. The staged cross executor carries the named
/// accounts, the map section, up to `MAX_CROSS_MAKERS` maker pairs, both
/// entries, and the union of both entries' execute surfaces — a quoter
/// registering an unusually long execute list can exceed this, in which
/// case staging fails and the turner sees no work (self-limiting, and the
/// publisher fast path still covers the cross).
pub const QUOTER_CROSS_STAGING_LEN: usize = 2048;

/// Account-data offset of the staging region (what a `ResponsePointerV0`'s
/// `offset` is relative to): discriminator + the block.
pub const QUOTER_CROSS_STAGING_OFFSET: usize = 8 + QUOTER_CROSS_BLOCK_LEN;

/// The resolver's account list, stored ONCE next to the block and pointed
/// at by every condition's `resolver_list_offset` (relay's indirection for
/// lists that outgrow the inline slots): 5 named accounts + the entry's
/// registered quote surface (≤32) + its program.
pub const QUOTER_CROSS_RESOLVER_LIST_MAX: usize = 38;
pub const QUOTER_CROSS_RESOLVER_LIST_LEN: usize =
    QUOTER_CROSS_RESOLVER_LIST_MAX * relay_spec::ACCOUNT_REF_LEN;
/// Account-data offset of the resolver list region.
pub const QUOTER_CROSS_RESOLVER_LIST_OFFSET: usize =
    QUOTER_CROSS_STAGING_OFFSET + QUOTER_CROSS_STAGING_LEN;

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct QuoterCrossConditionsV0 {
    /// The relay condition block; first field, so it sits at the 8-aligned
    /// offset 8 `read_block` requires.
    pub block: [u8; QUOTER_CROSS_BLOCK_LEN],
    /// Scratch the resolver stages its `ResolvedCrankV0` into. Only ever
    /// written under simulation.
    pub staging: [u8; QUOTER_CROSS_STAGING_LEN],
    /// The resolver's account list ([`relay_spec::AccountRefV0`] wire
    /// bytes), written at attach; the conditions reference it indirectly.
    pub resolver_list: [u8; QUOTER_CROSS_RESOLVER_LIST_LEN],
    /// The Custom entry these conditions discover crosses for.
    pub quoter: Pubkey,
    /// The market's canonical CLOB entry / book / program, captured at
    /// attach time (the resolver stages the executor's CLOB leg from here
    /// without holding those accounts). Re-attach after a CLOB rotation.
    pub clob_quoter: Pubkey,
    pub clob_market: Pubkey,
    pub clob_program: Pubkey,
    /// The market's oracle, captured at attach time (the staged executor's
    /// map section).
    pub oracle: Pubkey,
    pub market_index: u16,
    pub quote_spot_market_index: u16,
    /// Live entries in `resolver_list`.
    pub resolver_list_count: u8,
    pub padding: [u8; 5],
}

impl Default for QuoterCrossConditionsV0 {
    fn default() -> Self {
        Self {
            block: [0; QUOTER_CROSS_BLOCK_LEN],
            staging: [0; QUOTER_CROSS_STAGING_LEN],
            resolver_list: [0; QUOTER_CROSS_RESOLVER_LIST_LEN],
            quoter: Pubkey::default(),
            clob_quoter: Pubkey::default(),
            clob_market: Pubkey::default(),
            clob_program: Pubkey::default(),
            oracle: Pubkey::default(),
            market_index: 0,
            quote_spot_market_index: 0,
            resolver_list_count: 0,
            padding: [0; 5],
        }
    }
}

impl QuoterCrossConditionsV0 {
    pub const SIZE: usize = 8
        + QUOTER_CROSS_BLOCK_LEN
        + QUOTER_CROSS_STAGING_LEN
        + QUOTER_CROSS_RESOLVER_LIST_LEN
        + 5 * 32
        + 2
        + 2
        + 1
        + 5;

    /// Write the resolver account list the conditions point at.
    pub fn write_resolver_list(&mut self, refs: &[relay_spec::AccountRefV0]) -> Result<()> {
        if refs.len() > QUOTER_CROSS_RESOLVER_LIST_MAX {
            msg!("resolver list of {} exceeds the region", refs.len());
            return Err(ErrorCode::DefaultError.into());
        }
        for (i, r) in refs.iter().enumerate() {
            let start = i * relay_spec::ACCOUNT_REF_LEN;
            self.resolver_list[start..start + 32].copy_from_slice(&r.address);
            self.resolver_list[start + 32] = r.writable;
        }
        self.resolver_list_count = refs.len() as u8;
        Ok(())
    }

    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// Stamp the spec header; conditions are written by index.
    pub fn init_header(&mut self) -> Result<()> {
        let header = ConditionBlockHeaderV0::new(QUOTER_CROSS_CONDITIONS as u8);
        self.block[..BLOCK_HEADER_LEN].copy_from_slice(bytemuck::bytes_of(&header));
        Ok(())
    }

    pub fn write_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        if index >= QUOTER_CROSS_CONDITIONS {
            msg!("quoter cross condition index {} out of range", index);
            return Err(ErrorCode::DefaultError.into());
        }
        let start = BLOCK_HEADER_LEN + index * CONDITION_LEN;
        self.block[start..start + CONDITION_LEN].copy_from_slice(bytemuck::bytes_of(condition));
        Ok(())
    }

    /// Stage a resolver's payload and return the pointer bytes to set as
    /// return data.
    pub fn stage(
        &mut self,
        resolved: &ResolvedCrankV0,
    ) -> Result<[u8; relay_spec::RESPONSE_POINTER_LEN]> {
        let len = resolved.write_into(&mut self.staging).map_err(|e| {
            msg!(
                "staging a {}-byte resolved crank failed: {:?}",
                resolved.encoded_len(),
                e
            );
            error!(ErrorCode::DefaultError)
        })?;
        Ok(ResponsePointerV0::new(0, QUOTER_CROSS_STAGING_OFFSET as u32, len as u32).to_bytes())
    }
}

const _: () = assert!((QuoterCrossConditionsV0::SIZE - 8) % 16 == 0);
const _: () = assert!(QuoterCrossConditionsV0::SIZE <= 10_240);

#[cfg(test)]
mod tests {
    use {super::*, relay_spec::bytemuck::Zeroable};

    #[test]
    fn size_matches_the_layout() {
        assert_eq!(
            std::mem::size_of::<QuoterCrossConditionsV0>(),
            QuoterCrossConditionsV0::SIZE - 8
        );
        assert_eq!(
            QUOTER_CROSS_STAGING_OFFSET,
            8 + core::mem::offset_of!(QuoterCrossConditionsV0, staging)
        );
    }

    #[test]
    fn header_and_conditions_round_trip_through_the_spec() {
        let mut acct = QuoterCrossConditionsV0::default();
        acct.init_header().unwrap();
        let mut condition = relay_spec::ConditionV0::zeroed();
        condition.wake_slot = 77;
        condition.active = 1;
        acct.write_condition(QUOTER_CROSS_FALLBACK, &condition)
            .unwrap();
        let (header, conditions) = relay_spec::read_block(acct.block(), 0).unwrap();
        assert_eq!(header.num_conditions, QUOTER_CROSS_CONDITIONS as u8);
        assert_eq!(conditions[QUOTER_CROSS_FALLBACK].wake_slot, 77);
        assert_eq!(conditions[QUOTER_CROSS_WATCH].active, 0);
    }
}
