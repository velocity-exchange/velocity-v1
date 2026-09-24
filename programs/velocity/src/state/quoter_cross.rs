//! Per-quoter relay condition block that finds crosses between a PropAMM
//! quoter and the CLOB.
//!
//! One account holds the conditions for one Custom registry entry. The PDA
//! derives from the entry key, and whoever attaches it pays the rent. That is
//! normally the maker. Discovery needs no per-quoter velocity code. The
//! resolver prices any entry through the `quote_v0` surface the entry
//! registered, by CPI under simulation. Every fill uses that same interface.
//! The wake comes from the reprice region the maker declares in
//! `QuoterConfigV0::watch_*`. Three conditions:
//!
//! - [`QUOTER_CROSS_WATCH`] is an `OnAccountChange` over the declared watch
//!   region. It stays inactive when the maker declares none.
//! - [`QUOTER_CROSS_CLOB`] is an `OnAccountChange` over the CLOB's bests. A
//!   cross can also start on the CLOB side, because a new crossing order is
//!   always a new best.
//! - [`QUOTER_CROSS_FALLBACK`] is an `EverySlots` poll. It is the liveness
//!   floor for both, and the only wake when the maker declares no watch
//!   region.
//!
//! All three run [`ResolveCrankCrossMatchQuoter`] and stage the same
//! `crank_cross_match` executor that the market's own CLOB conditions stage.
//! The market's `ClobCrankConditionsV0` reservoir pays the keeper in both
//! cases.

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
/// Index of the watch on the CLOB's bests, which are the other side of the
/// cross.
pub const QUOTER_CROSS_CLOB: usize = 1;
/// Index of the periodic fallback poll.
pub const QUOTER_CROSS_FALLBACK: usize = 2;
/// Conditions hosted per entry.
pub const QUOTER_CROSS_CONDITIONS: usize = 3;

/// Capacity of the resolver account list. It must hold the resolver's fixed
/// accounts, the entry's full quote surface (`MAX_QUOTER_ACCOUNTS`) and the
/// quoter program. A smaller capacity leaves a maker with a full quote list
/// unable to attach cross discovery. [`RelayBlockV0`] requires a multiple of 8.
pub const QUOTER_CROSS_RESOLVER_CAPACITY: usize = 48;

/// Account-data offset of the relay block (what a `WatchV0` registers at).
pub const QUOTER_CROSS_BLOCK_OFFSET: usize =
    relay_spec::block_offset!(QuoterCrossConditionsV0, relay);

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
pub struct QuoterCrossConditionsV0 {
    /// Everything relay needs, in one field. It holds the `relay-spec`
    /// header, the condition slots, and the resolver account list that every
    /// condition here points at. The attach writes that list. This is the
    /// first field, so its watch offset is 8.
    pub relay: RelayBlock<QUOTER_CROSS_CONDITIONS, QUOTER_CROSS_RESOLVER_CAPACITY>,
    /// The Custom entry these conditions discover crosses for.
    pub quoter: Pubkey,
    /// The market's book and its program, captured at attach time. The
    /// resolver stages the executor's CLOB leg from here without holding those
    /// accounts. Attach again after a CLOB rotation.
    pub clob_market: Pubkey,
    pub clob_program: Pubkey,
    /// The market's oracle, captured at attach time. It fills the map section
    /// of the staged executor.
    pub oracle: Pubkey,
    pub market_index: u16,
    pub quote_spot_market_index: u16,
    /// Tail reserve. A resolver that needs another fixed account takes a
    /// pubkey from here. That avoids an `extend_account` migration on every
    /// attached quoter entry. The length also keeps `SIZE - 8` a multiple of
    /// 16.
    pub padding: [u8; 92],
}

// `#[derive(Default)]` covers an array of at most 32 elements, and `padding`
// is longer.
impl Default for QuoterCrossConditionsV0 {
    fn default() -> Self {
        Self {
            relay: RelayBlock::default(),
            quoter: Pubkey::default(),
            clob_market: Pubkey::default(),
            clob_program: Pubkey::default(),
            oracle: Pubkey::default(),
            market_index: 0,
            quote_spot_market_index: 0,
            padding: [0; 92],
        }
    }
}

impl QuoterCrossConditionsV0 {
    pub const SIZE: usize = 8
        + RelayBlockV0::<QUOTER_CROSS_CONDITIONS, QUOTER_CROSS_RESOLVER_CAPACITY>::SIZE
        + 4 * 32
        + 2
        + 2
        + 92;

    /// Write the resolver account list the conditions point at, and
    /// describe where it landed.
    pub fn write_resolver_list(
        &mut self,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        // A block written by an older spec is migrated first, so the slots
        // below are in the shape this program addresses them by.
        self.relay
            .migrate()
            .map_err(|_| error!(ErrorCode::InvalidConditionBlock))?;

        self.relay.write_resolvers(refs).map_err(|_| {
            msg!("resolver list of {} exceeds the region", refs.len());
            error!(ErrorCode::ConditionResolverListTooLarge)
        })
    }

    pub fn block(&self) -> &[u8] {
        ConditionBlock::block(&self.relay)
    }

    /// This method and the four below wrap
    /// [`relay_spec::ConditionBlock`] and return the program's own error
    /// type, so a handler can use `?`.
    pub fn init_block(&mut self) -> Result<()> {
        self.relay
            .init(QUOTER_CROSS_BLOCK_OFFSET as u32)
            .map_err(|_| error!(ErrorCode::InvalidConditionBlock))
    }

    pub fn set_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        ConditionBlock::write_condition(&mut self.relay, index, condition)
            .map_err(|_| error!(ErrorCode::InvalidConditionBlock))
    }

    pub fn get_condition(&self, index: usize) -> Result<relay_spec::ConditionV0> {
        ConditionBlock::read_condition(&self.relay, index)
            .map_err(|_| error!(ErrorCode::InvalidConditionBlock))
    }

    pub fn edit_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut relay_spec::ConditionV0),
    ) -> Result<()> {
        ConditionBlock::update_condition(&mut self.relay, index, f)
            .map_err(|_| error!(ErrorCode::InvalidConditionBlock))
    }

    pub fn clear_condition(&mut self, index: usize) -> Result<()> {
        ConditionBlock::deactivate_condition(&mut self.relay, index)
            .map_err(|_| error!(ErrorCode::InvalidConditionBlock))
    }
}

const _: () = assert!(QUOTER_CROSS_BLOCK_OFFSET.is_multiple_of(8));
const _: () = assert!((QuoterCrossConditionsV0::SIZE - 8).is_multiple_of(16));
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
