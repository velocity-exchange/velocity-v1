//! Output buffer for [`crate::instructions::router::quote_router`], the
//! router's quote view.
//!
//! The books cannot come back through return data. The return-data cap is
//! 1024 bytes and one book exceeds it, which is why the quoter wire puts each
//! payload in the quoter's own response account. A batch adds a second
//! problem. Return data is last-writer-wins per transaction, so after several
//! CPIs only the final quoter's pointer survives, and the caller cannot
//! recover the rest.
//!
//! The view instead writes every source's book into this account in one
//! encoding, and the caller reads it out of post-simulation account state.
//! One encoding is needed because the sources have nothing else in common.
//! The vAMM has no response account, and a book that verification clamped no
//! longer matches what sits in its response account.
//!
//! This account is only ever written under simulation. Landing the
//! instruction changes nothing else, so it is harmless and useless.

use {
    crate::{
        error::{ErrorCode, VelocityResult},
        msg,
        state::{
            prop_amm::{L3RowV0, PriceLevel},
            traits::Size,
        },
        validate,
    },
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
};

/// Sources quotable in one view call. A transaction's account list bounds
/// this anyway, because each external quoter brings its own CPI accounts. A
/// market with more quoters needs several calls.
pub const MAX_QUOTED_SOURCES: usize = 16;

/// Levels kept per source. Each source has its own fixed slot, so no code
/// computes an offset into a shared region. The value matches
/// [`crate::math::router::MAX_LEVELS_PER_BOOK`], the most the split ever
/// consumes; a book with more distinct prices truncates, understating depth but never overstating it.
pub const MAX_LEVELS_PER_SOURCE: usize = crate::math::router::MAX_LEVELS_PER_BOOK;

/// Which kind of liquidity a quoted book came from. The router needs it to
/// execute the allocation as a CPI leg or as the vAMM. A user interface needs
/// it to label depth, because a PropAMM's levels are a quote at a size rather
/// than resting orders.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum QuotedSourceKind {
    /// The in-program constant-product AMM.
    #[default]
    Vamm,
    /// A resting `User` order, bridged as a single level.
    /// @deprecated No source publishes this. The discriminant stays so the
    /// wire layout of `QuotedSourceV0.kind` does not shift.
    DlobOrder,
    /// A registered external quoter: the CLOB or a PropAMM.
    Quoter,
}

/// One source's slice of the buffer's level region.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuotedSourceV0 {
    /// The `QuoterV0` entry for a `Quoter`, the maker's `User` for a
    /// `DlobOrder`, the perp market for `Vamm`.
    pub key: Pubkey,
    /// Live levels in this source's slot of `levels`.
    pub level_count: u16,
    /// Routing tier the split applies. It is `QuoterV0::priority`, or the
    /// type default for the in-program sources.
    pub priority: u8,
    pub kind: QuotedSourceKind,
    /// Set when verification cut this book. A Custom quoter can advertise more
    /// depth than its `User`'s margin supports, and the cut removes the
    /// excess. The caller then never routes against or displays depth that no
    /// fill can take.
    pub clamped: bool,
    /// Where this source's slice of `rows` starts and how long it is. A source
    /// with no per-order detail has no slice.
    pub row_start: u8,
    pub row_len: u8,
    pub padding: [u8; 1],
}

const_assert_eq!(std::mem::size_of::<QuotedSourceV0>(), 40);

/// A quoted level in the buffer's Pod form. The wire `PriceLevel` is borsh.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuotedLevelV0 {
    pub price: u64,
    pub size: u64,
}

const_assert_eq!(std::mem::size_of::<QuotedLevelV0>(), 16);

// `zero_copy(unsafe)` emits no bytemuck derives, so the quote view casts a
// stored book straight to the wire's level type instead of copying it. The
// cast is sound because both types are `#[repr(C)]` over the same two `u64`
// fields with no padding, and the asserts below keep it true: a widened or reordered field stops the compile.
const_assert_eq!(
    std::mem::size_of::<QuotedLevelV0>(),
    std::mem::size_of::<PriceLevel>()
);
const_assert_eq!(
    std::mem::align_of::<QuotedLevelV0>(),
    std::mem::align_of::<PriceLevel>()
);

unsafe impl bytemuck::Pod for QuotedLevelV0 {}
unsafe impl bytemuck::Zeroable for QuotedLevelV0 {}

/// One resting order behind a quoted book, in the buffer's Pod form. The wire
/// `L3RowV0` holds the same bytes.
///
/// A book's ladder can aggregate orders that belong to different people. A
/// caller that must carry those accounts, or draw the book, needs them apart.
/// Every other quoter fills from the one account its registry entry names, so
/// its rows report that account. A consumer reads one shape either way.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuotedRowV0 {
    pub price: u64,
    pub size: u64,
    /// The quoter's own handle for the order. Zero when the row is not an
    /// order but a rung attributed to the quoter's user.
    pub order_id: u64,
    /// The other half of the handle, for a quoter that keeps an arena. Zero
    /// when it does not.
    pub node_index: u32,
    /// Authority of the `User` this row settles against.
    pub authority: Pubkey,
    pub sub_account_id: u16,
    /// `L3_ROW_FLAG_*`, as the quoter reported them.
    pub flags: u8,
    pub padding: [u8; 1],
    /// Slot the order was placed in. Zero when the quoter keeps no such
    /// record.
    pub placed_slot: u64,
}

const_assert_eq!(std::mem::size_of::<QuotedRowV0>(), 72);
const_assert_eq!(
    std::mem::size_of::<QuotedRowV0>(),
    std::mem::size_of::<L3RowV0>()
);

unsafe impl bytemuck::Pod for QuotedRowV0 {}
unsafe impl bytemuck::Zeroable for QuotedRowV0 {}

/// Rows one call may report, across every source it carried. A depth-100
/// book of one side fits, which is what a display asks for. The account is
/// sized once for every market; a pass that fills the region sets
/// `rows_truncated` rather than dropping rows without a record.
pub const MAX_QUOTED_ROWS: usize = 128;

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct RouterQuoteBufferV0 {
    /// Only this signer may quote into the buffer, so two routers that share a
    /// market do not overwrite each other's reads.
    pub authority: Pubkey,
    /// Taker size the books were quoted at. The size changes the answer rather
    /// than echoing the request. It only truncates a resting book on the CLOB,
    /// but the vAMM's levels and a PropAMM's levels depend on it.
    pub quoted_size: u64,
    /// Slot the quote ran at, so a reader can tell how stale a cached book is.
    pub slot: u64,
    pub market: u16,
    /// Live entries in `sources`.
    pub source_count: u8,
    /// Live entries in `rows`.
    pub row_count: u8,
    /// The rows region filled before every source was described, so the last
    /// sources carry fewer rows than their books hold. The ladders keep every
    /// level. A row is detail about a level, never the level itself.
    pub rows_truncated: bool,
    /// Taker direction quoted, as a `Direction` cast to u8. 0 is long and 1 is
    /// short.
    pub direction: u8,
    /// Pads the header to 128 bytes. The reserve holds two more pubkeys, so
    /// naming another account later does not move `sources` or `levels` and
    /// break every off-chain decoder. The length also keeps
    /// `(SIZE - 8) % 16 == 0`. See docs/alignment-and-native-offsets.md.
    pub padding: [u8; 74],
    pub sources: [QuotedSourceV0; MAX_QUOTED_SOURCES],
    /// One slot per source, parallel to `sources`.
    pub levels: [[QuotedLevelV0; MAX_LEVELS_PER_SOURCE]; MAX_QUOTED_SOURCES],
    /// The orders behind the ladders, in the order the sources were quoted.
    /// Each source names its own run through `row_start`/`row_len`.
    pub rows: [QuotedRowV0; MAX_QUOTED_ROWS],
}

// Zero-copy layout invariant. The struct holds no u128 field, and its size
// with the 8-byte discriminator is congruent to 8 modulo 16. See
// docs/alignment-and-native-offsets.md.
const_assert_eq!(std::mem::size_of::<RouterQuoteBufferV0>(), 42752);
const_assert_eq!((RouterQuoteBufferV0::SIZE - 8) % 16, 0);

impl Size for RouterQuoteBufferV0 {
    // The size is derived from the struct's own width plus the discriminator.
    // A hand-written number goes stale the moment a row grows, and a caller
    // then allocates the buffer too small. The overrun shows up at the far end
    // of a write rather than at the change that caused it.
    const SIZE: usize = 8 + std::mem::size_of::<RouterQuoteBufferV0>();
}

impl RouterQuoteBufferV0 {
    /// Reset the buffer for a fresh quote round.
    pub fn begin(&mut self, direction: u8, quoted_size: u64, slot: u64) {
        self.source_count = 0;
        self.row_count = 0;
        self.rows_truncated = false;
        self.direction = direction;
        self.quoted_size = quoted_size;
        self.slot = slot;
    }

    /// Append one source's book. A book that does not fit fails the call. A
    /// truncated book reads as thin liquidity, and the router then routes
    /// around depth that exists.
    pub fn push(
        &mut self,
        kind: QuotedSourceKind,
        key: Pubkey,
        priority: u8,
        levels: &[PriceLevel],
    ) -> VelocityResult {
        self.push_capped(kind, key, priority, levels, u64::MAX)
            .map(|_| ())
    }

    /// Append one source's book, cut to `cap` total base, best levels first.
    ///
    /// The cut is the verification a Custom quoter's book needs. Its depth is
    /// never margin-reserved, so whatever it advertises past its `User`'s
    /// margin is depth no fill can take. The cut runs while the levels are
    /// copied, which keeps the book off the heap. The caller reads it straight
    /// from the quoter's response account and never holds a second copy.
    ///
    /// Returns the `clamped` bit, which a consumer reads to tell a thin quoter
    /// from a cut one.
    pub fn push_capped(
        &mut self,
        kind: QuotedSourceKind,
        key: Pubkey,
        priority: u8,
        levels: &[PriceLevel],
        cap: u64,
    ) -> VelocityResult<bool> {
        let index = self.source_count as usize;
        validate!(
            index < MAX_QUOTED_SOURCES,
            ErrorCode::RouterQuoteSourcesFull,
            "router quote buffer holds at most {} sources",
            MAX_QUOTED_SOURCES
        )?;

        let mut remaining = cap;
        let mut written = 0usize;
        for level in levels {
            if remaining == 0 {
                break;
            }

            // The check counts what the cap admits, not what the quoter
            // offered. A book deeper than the slot is a problem only when the
            // cap lets that much of it through.
            validate!(
                written < MAX_LEVELS_PER_SOURCE,
                ErrorCode::RouterQuoteLevelsFull,
                "a source's book holds at most {} levels, got {}",
                MAX_LEVELS_PER_SOURCE,
                levels.len()
            )?;

            let size = level.size.min(remaining);
            self.levels[index][written] = QuotedLevelV0 {
                price: level.price,
                size,
            };

            remaining -= size;
            written += 1;
        }

        let quoted: u64 = levels
            .iter()
            .map(|level| level.size)
            .fold(0, u64::saturating_add);
        let clamped = quoted > cap;
        self.sources[index] = QuotedSourceV0 {
            row_start: 0,
            row_len: 0,
            key,
            level_count: written as u16,
            priority,
            kind,
            clamped,
            padding: [0; 1],
        };

        self.source_count += 1;
        Ok(clamped)
    }

    /// Attach one row to the source most recently pushed.
    ///
    /// Returns `false` when the region is full. A row is detail about a ladder
    /// that stands on its own, so a full region shortens the detail instead of
    /// failing the view. The buffer records that it happened in
    /// `rows_truncated`.
    pub fn push_row(&mut self, row: QuotedRowV0) -> VelocityResult<bool> {
        let index = (self.source_count as usize).checked_sub(1).ok_or_else(|| {
            msg!("a row needs a source to belong to");
            ErrorCode::RouterQuoteRowWithoutSource
        })?;

        if self.row_count as usize >= MAX_QUOTED_ROWS {
            self.rows_truncated = true;
            return Ok(false);
        }
        if self.sources[index].row_len == 0 {
            self.sources[index].row_start = self.row_count;
        }

        self.rows[self.row_count as usize] = row;
        self.row_count += 1;
        self.sources[index].row_len += 1;
        Ok(true)
    }

    /// Rows the region can still take.
    pub fn rows_remaining(&self) -> usize {
        MAX_QUOTED_ROWS.saturating_sub(self.row_count as usize)
    }

    /// One source's rows, by its index in `sources`.
    pub fn rows_for(&self, index: usize) -> &[QuotedRowV0] {
        let start = self.sources[index].row_start as usize;
        let end = start + self.sources[index].row_len as usize;
        &self.rows[start..end]
    }

    /// One source's levels, by its index in `sources`.
    pub fn levels_for(&self, index: usize) -> &[QuotedLevelV0] {
        &self.levels[index][..self.sources[index].level_count as usize]
    }
}

#[cfg(test)]
mod tests {
    use {super::*, bytemuck::Zeroable};

    fn buffer() -> Box<RouterQuoteBufferV0> {
        let mut buffer: Box<RouterQuoteBufferV0> = Box::new(RouterQuoteBufferV0::zeroed());
        buffer.begin(0, 1_000, 7);
        buffer
    }

    fn level(price: u64, size: u64) -> PriceLevel {
        PriceLevel { price, size }
    }

    #[test]
    fn a_book_within_its_cap_arrives_whole_and_unclamped() {
        let mut buffer = buffer();
        let clamped = buffer
            .push_capped(
                QuotedSourceKind::Quoter,
                Pubkey::new_unique(),
                20,
                &[level(100, 5), level(101, 7)],
                12,
            )
            .unwrap();
        assert!(!clamped, "the cap is exactly what the book offers");
        assert_eq!(buffer.levels_for(0).len(), 2);
        assert_eq!(buffer.levels_for(0)[1].size, 7);
    }

    /// The cut keeps depth a maker's margin cannot carry off the published
    /// book. It lands inside the rung that crosses the cap instead of dropping
    /// that rung whole.
    #[test]
    fn a_cap_cuts_the_rung_it_lands_in_and_drops_the_rest() {
        let mut buffer = buffer();
        let clamped = buffer
            .push_capped(
                QuotedSourceKind::Quoter,
                Pubkey::new_unique(),
                20,
                &[level(100, 5), level(101, 7), level(102, 9)],
                8,
            )
            .unwrap();
        assert!(clamped);
        let levels = buffer.levels_for(0);
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].size, 5);
        assert_eq!(levels[1].size, 3, "the second rung took what was left");
        assert_eq!(buffer.sources[0].clamped, true);
    }

    #[test]
    fn a_maker_with_no_room_publishes_no_book() {
        let mut buffer = buffer();
        let clamped = buffer
            .push_capped(
                QuotedSourceKind::Quoter,
                Pubkey::new_unique(),
                20,
                &[level(100, 5)],
                0,
            )
            .unwrap();
        assert!(clamped);
        assert!(buffer.levels_for(0).is_empty());
        assert_eq!(buffer.source_count, 1, "the source is still reported");
    }

    /// A book deeper than one slot fails the call instead of publishing a
    /// prefix as the whole book. That only happens when the cap admits that
    /// much of it.
    #[test]
    fn a_book_deeper_than_the_slot_fails_only_when_the_cap_admits_it() {
        let deep: Vec<PriceLevel> = (0..MAX_LEVELS_PER_SOURCE + 1)
            .map(|index| level(100 + index as u64, 1))
            .collect();

        let mut uncapped = buffer();
        assert!(uncapped
            .push_capped(
                QuotedSourceKind::Quoter,
                Pubkey::new_unique(),
                20,
                &deep,
                u64::MAX
            )
            .is_err());

        // The same book, clamped to a maker who can carry three units.
        let mut buffer = buffer();
        let clamped = buffer
            .push_capped(QuotedSourceKind::Quoter, Pubkey::new_unique(), 20, &deep, 3)
            .unwrap();
        assert!(clamped);
        assert_eq!(buffer.levels_for(0).len(), 3);
    }

    /// The router reads the rival books the vAMM is shaded against out of this
    /// buffer, so the two level types must be the same bytes.
    #[test]
    fn a_stored_book_casts_to_the_wire_levels_without_copying() {
        let mut buffer = buffer();
        buffer
            .push(
                QuotedSourceKind::DlobOrder,
                Pubkey::new_unique(),
                10,
                &[level(100, 5), level(101, 7)],
            )
            .unwrap();
        let wire: &[PriceLevel] = bytemuck::cast_slice(buffer.levels_for(0));
        assert_eq!(wire, &[level(100, 5), level(101, 7)]);
    }
}
