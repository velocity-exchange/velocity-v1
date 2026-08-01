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
    crate::error::ErrorCode,
    anchor_lang::prelude::*,
    relay_spec::{ConditionBlock, BLOCK_HEADER_LEN, CONDITION_LEN},
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

/// Account-data offset of the staging region (what a `ResponsePointerV0`'s
/// `offset` is relative to): discriminator + the block.
pub const QUOTER_CROSS_TAIL_OFFSET: usize = 8 + QUOTER_CROSS_BLOCK_LEN;

/// The resolver's account list, stored ONCE next to the block and pointed
/// at by every condition's `resolver_list_offset` (relay's indirection for
/// lists that outgrow the inline slots): 5 named accounts + the entry's
/// registered quote surface (≤32) + its program.
pub const QUOTER_CROSS_RESOLVER_LIST_MAX: usize = 38;
pub const QUOTER_CROSS_RESOLVER_LIST_LEN: usize =
    QUOTER_CROSS_RESOLVER_LIST_MAX * relay_spec::ACCOUNT_REF_LEN;
/// Account-data offset of the resolver list region.
pub const QUOTER_CROSS_RESOLVER_LIST_OFFSET: usize =
    relay_spec::block_offset!(QuoterCrossConditionsV0, resolver_list);

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct QuoterCrossConditionsV0 {
    /// The relay condition block; first field, so it sits at the 8-aligned
    /// offset 8 `read_block` requires.
    pub block: [u8; QUOTER_CROSS_BLOCK_LEN],
    /// Scratch the resolver stages its `ResolvedCrankV0` into. Only ever
    /// written under simulation.
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
    pub const SIZE: usize =
        8 + QUOTER_CROSS_BLOCK_LEN + QUOTER_CROSS_RESOLVER_LIST_LEN + 5 * 32 + 2 + 2 + 1 + 5;

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

    /// Anchor-flavoured wrappers over the spec trait's provided methods,
    /// so handlers keep using `?` with the program's own error type.
    pub fn init_block(&mut self) -> Result<()> {
        ConditionBlock::init_header(self).map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn set_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        ConditionBlock::write_condition(self, index, condition)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn get_condition(&self, index: usize) -> Result<relay_spec::ConditionV0> {
        ConditionBlock::read_condition(self, index).map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn edit_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut relay_spec::ConditionV0),
    ) -> Result<()> {
        ConditionBlock::update_condition(self, index, f)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn clear_condition(&mut self, index: usize) -> Result<()> {
        ConditionBlock::deactivate_condition(self, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }
}

const _: () = assert!((QuoterCrossConditionsV0::SIZE - 8) % 16 == 0);
const _: () = assert!(QuoterCrossConditionsV0::SIZE <= 10_240);

/// Block hosting + staging, from the spec (see
/// [`relay_spec::ConditionBlock`]): `init_header`, `write_condition`,
/// `read_condition`, `update_condition`, `deactivate_condition`, and
/// `stage` are all provided.
relay_spec::condition_block!(QuoterCrossConditionsV0, block, QUOTER_CROSS_CONDITIONS);

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
            QUOTER_CROSS_RESOLVER_LIST_OFFSET,
            8 + core::mem::offset_of!(QuoterCrossConditionsV0, resolver_list)
        );
    }

    #[test]
    fn header_and_conditions_round_trip_through_the_spec() {
        let mut acct = QuoterCrossConditionsV0::default();
        acct.init_block().unwrap();
        let mut condition = relay_spec::ConditionV0::zeroed();
        condition.set_wake(relay_spec::WakeView::AtSlot { slot: 77 });
        acct.set_condition(QUOTER_CROSS_FALLBACK, &condition)
            .unwrap();
        let (header, conditions) = relay_spec::read_block(acct.block(), 0).unwrap();
        assert_eq!(header.num_conditions, QUOTER_CROSS_CONDITIONS as u8);
        assert_eq!(
            conditions[QUOTER_CROSS_FALLBACK].wake(),
            Ok(relay_spec::WakeView::AtSlot { slot: 77 })
        );
        assert!(!conditions[QUOTER_CROSS_WATCH].is_active());
    }
}
