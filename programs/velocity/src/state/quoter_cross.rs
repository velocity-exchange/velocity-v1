//! Per-quoter relay condition block for PropAMM×CLOB cross discovery.
//!
//! One account per Custom registry entry (PDA off the entry key, rent on
//! whoever attaches it — normally the maker), so discovery scales to any
//! number of PropAMMs with zero per-quoter velocity code: the resolver
//! prices the quoter *generically*, by CPI-ing its registered `quote_v0`
//! surface under simulation — the same interface every fill uses — and the
//! wake comes from the entry's maker-declared reprice region
//! (`QuoterConfigV0::watch_*`). Three conditions:
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
    relay_anchor::RelayBlock,
    relay_spec::{ConditionBlock, RelayBlockV0},
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

/// The resolver's account list capacity: 5 named accounts + the entry's
/// registered quote surface (≤32) + its program, rounded to
/// [`RelayBlockV0`]'s granularity of 8.
// 8 fixed accounts + the entry's full quote surface (`MAX_QUOTER_ACCOUNTS` = 32)
// + the quoter program once more = 41 accounts. A capacity below that leaves a
// maker with a full quote list unable to attach cross discovery. RelayBlockV0
// requires a multiple of 8, so round up to 48.
pub const QUOTER_CROSS_RESOLVER_CAPACITY: usize = 48;

/// Account-data offset of the relay block (what a `WatchV0` registers at).
pub const QUOTER_CROSS_BLOCK_OFFSET: usize =
    relay_spec::block_offset!(QuoterCrossConditionsV0, relay);

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct QuoterCrossConditionsV0 {
    /// Everything relay needs hosted, in one field: the `relay-spec` header,
    /// the condition slots, and the resolver account list (written at attach)
    /// every condition here points at. First field, so its watch offset
    /// is 8.
    pub relay: RelayBlock<QUOTER_CROSS_CONDITIONS, QUOTER_CROSS_RESOLVER_CAPACITY>,
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
    /// Tail reserve: 3 bytes of alignment slack plus room for two more
    /// captured pubkeys, so a resolver that needs another fixed account can
    /// take it from here instead of forcing an `extend_account` migration on
    /// every attached quoter entry.
    pub padding: [u8; 60],
}

// `padding` is longer than 32 bytes, which `#[derive(Default)]` does not
// cover (arrays only derive it up to 32).
impl Default for QuoterCrossConditionsV0 {
    fn default() -> Self {
        Self {
            relay: RelayBlock::default(),
            quoter: Pubkey::default(),
            clob_quoter: Pubkey::default(),
            clob_market: Pubkey::default(),
            clob_program: Pubkey::default(),
            oracle: Pubkey::default(),
            market_index: 0,
            quote_spot_market_index: 0,
            padding: [0; 60],
        }
    }
}

impl QuoterCrossConditionsV0 {
    pub const SIZE: usize = 8
        + RelayBlockV0::<QUOTER_CROSS_CONDITIONS, QUOTER_CROSS_RESOLVER_CAPACITY>::SIZE
        + 5 * 32
        + 2
        + 2
        + 60;

    /// Write the resolver account list the conditions point at, and
    /// describe where it landed.
    pub fn write_resolver_list(
        &mut self,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        self.relay.write_resolvers(refs).map_err(|_| {
            msg!("resolver list of {} exceeds the region", refs.len());
            error!(ErrorCode::DefaultError)
        })
    }

    pub fn block(&self) -> &[u8] {
        ConditionBlock::block(&self.relay)
    }

    /// Anchor-flavoured wrappers over [`relay_spec::ConditionBlock`]'s
    /// provided methods, so handlers keep using `?` with the program's own
    /// error type.
    pub fn init_block(&mut self) -> Result<()> {
        self.relay
            .init(QUOTER_CROSS_BLOCK_OFFSET as u32)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn set_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        ConditionBlock::write_condition(&mut self.relay, index, condition)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn get_condition(&self, index: usize) -> Result<relay_spec::ConditionV0> {
        ConditionBlock::read_condition(&self.relay, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn edit_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut relay_spec::ConditionV0),
    ) -> Result<()> {
        ConditionBlock::update_condition(&mut self.relay, index, f)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn clear_condition(&mut self, index: usize) -> Result<()> {
        ConditionBlock::deactivate_condition(&mut self.relay, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }
}

const _: () = assert!(QUOTER_CROSS_BLOCK_OFFSET % 8 == 0);
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
