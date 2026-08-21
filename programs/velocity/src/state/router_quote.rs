//! Output buffer for [`crate::instructions::router::quote_router`] — the
//! router's quote view.
//!
//! The books can't come back through return data: the cap is 1024 bytes and a
//! single CLOB book's response region is 8192, which is why the quoter wire
//! puts each payload in the quoter's own response account to begin with. A
//! batch makes that worse in a second way — return data is last-writer-wins
//! per transaction, so after N CPIs only the final quoter's pointer survives
//! and a caller could not recover the rest.
//!
//! So the view writes every source's book into this account in one uniform
//! encoding, and the caller reads it out of post-simulation account state
//! (never landing the transaction). Uniform matters: the vAMM has no response
//! account, DLOB orders have no program at all, and an external book that
//! verification clamped no longer matches what sits in its response account.
//! One buffer, one layout, one code path per source.
//!
//! This account is only ever written under simulation. Landing the
//! instruction is harmless — it mutates nothing else — but pointless.

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

/// Sources quotable in one view call. Bounded by what fits in a transaction's
/// account list anyway (each external quoter brings its own CPI accounts), so
/// callers batch across several calls for a market with more quoters.
pub const MAX_QUOTED_SOURCES: usize = 16;

/// Levels kept per source, in its own fixed slot — no shared region, so no
/// offset arithmetic to get wrong.
///
/// Set to [`crate::math::router::MAX_LEVELS_PER_BOOK`] deliberately: that is
/// the most the split will ever consume from one book, so a deeper slot could
/// hold levels no fill could route against. Deep for display too — a book
/// with more than this many distinct prices inside `quoted_size` truncates,
/// which understates depth, never overstates it.
pub const MAX_LEVELS_PER_SOURCE: usize = crate::math::router::MAX_LEVELS_PER_BOOK;

/// Which kind of liquidity a quoted book came from. The router needs this to
/// know how to *execute* the allocation (a CPI leg, an in-program DLOB order,
/// or the vAMM), and a UI needs it to label depth honestly — a PropAMM's
/// levels are a quote at a size, not resting orders.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum QuotedSourceKind {
    /// The in-program constant-product AMM.
    #[default]
    Vamm,
    /// A resting `User` order (the DLOB), bridged as a single level.
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
    /// Routing tier the split will apply (`QuoterV0::priority`, or the
    /// type default for the in-program sources).
    pub priority: u8,
    pub kind: QuotedSourceKind,
    /// Set when verification reduced this book — a Custom quoter advertising
    /// more depth than its `User`'s margin supports gets truncated here, so
    /// the caller never routes against or displays phantom depth. The
    /// difference between what the quoter said and what came back.
    pub clamped: bool,
    /// This source's slice of `rows`: where it starts and how long it is. A
    /// source with no per-order detail has none.
    pub row_start: u8,
    pub row_len: u8,
    pub padding: [u8; 1],
}

const_assert_eq!(std::mem::size_of::<QuotedSourceV0>(), 40);

/// A quoted level, in the buffer's Pod form (the wire `PriceLevel` is borsh).
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuotedLevelV0 {
    pub price: u64,
    pub size: u64,
}

const_assert_eq!(std::mem::size_of::<QuotedLevelV0>(), 16);

// `zero_copy(unsafe)` emits no bytemuck derives, and the quote view casts a
// stored book straight to the wire's level type rather than copying it into
// one. Sound because both are `#[repr(C)]` over the same two `u64`s: no
// padding, and every bit pattern is a valid value. The asserts are what keeps
// that true — widen or reorder either type and this stops compiling.
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

/// One resting order behind a quoted book, in the buffer's Pod form (the wire
/// `L3RowV0` is the same bytes).
///
/// A book's ladder aggregates orders that belong to different people, and a
/// caller that has to carry those accounts — or draw the book — needs them
/// apart. Every other quoter fills from the one account its registry entry
/// names, so its rows say that instead, and a consumer reads one shape either
/// way.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct QuotedRowV0 {
    pub price: u64,
    pub size: u64,
    /// The quoter's own handle for the order. Zero when the row is not an
    /// order but a rung attributed to the quoter's user.
    pub order_id: u64,
    /// Authority of the `User` this row settles against.
    pub authority: Pubkey,
    pub sub_account_id: u16,
    /// `L3_ROW_FLAG_*`, as the quoter reported them.
    pub flags: u8,
    pub padding: [u8; 5],
}

const_assert_eq!(std::mem::size_of::<QuotedRowV0>(), 64);
const_assert_eq!(
    std::mem::size_of::<QuotedRowV0>(),
    std::mem::size_of::<L3RowV0>()
);
unsafe impl bytemuck::Pod for QuotedRowV0 {}
unsafe impl bytemuck::Zeroable for QuotedRowV0 {}

/// Rows one call may report, across every source it carried.
///
/// A depth-100 book of one side fits, which is what a display asks for, and
/// the account is sized once for every market. A pass that fills the region
/// says so on the buffer rather than dropping rows silently.
pub const MAX_QUOTED_ROWS: usize = 128;

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct RouterQuoteBufferV0 {
    /// Only this signer may quote into the buffer, so two routers sharing a
    /// market don't overwrite each other's reads.
    pub authority: Pubkey,
    /// Taker size the books were quoted at. Meaningful output, not an echo:
    /// resting books (CLOB, DLOB) are size-independent and merely truncated
    /// by it, while the vAMM's and a PropAMM's levels genuinely depend on it.
    pub quoted_size: u64,
    /// Slot the quote ran at, so a cached book's staleness is checkable.
    pub slot: u64,
    pub market: u16,
    /// Live entries in `sources`.
    pub source_count: u8,
    /// Live entries in `rows`.
    pub row_count: u8,
    /// The rows region filled before every source had been described, so the
    /// last sources carry fewer rows than their books hold. The ladders are
    /// unaffected — a row is detail about a level, never the level itself.
    pub rows_truncated: bool,
    /// Taker direction quoted (`Direction` as u8: 0 = long, 1 = short).
    pub direction: u8,
    /// Pads the header to 128 bytes: 12 bytes of alignment slack (so the
    /// struct stays a multiple of 16 and `(SIZE - 8) % 16 == 0` holds — see
    /// docs/alignment-and-native-offsets.md) plus room for two more pubkeys,
    /// so naming another account in the header doesn't shift `sources` /
    /// `levels` and break every off-chain decoder of this buffer.
    pub padding: [u8; 74],
    pub sources: [QuotedSourceV0; MAX_QUOTED_SOURCES],
    /// One slot per source, parallel to `sources`.
    pub levels: [[QuotedLevelV0; MAX_LEVELS_PER_SOURCE]; MAX_QUOTED_SOURCES],
    /// The orders behind the ladders, in the order the sources were quoted.
    /// Each source names its own run through `row_start`/`row_len`.
    pub rows: [QuotedRowV0; MAX_QUOTED_ROWS],
}

// Zero-copy layout invariant (docs/alignment-and-native-offsets.md): no u128
// fields, and size including the 8-byte discriminator is ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<RouterQuoteBufferV0>(), 41728);
const_assert_eq!((RouterQuoteBufferV0::SIZE - 8) % 16, 0);

impl Size for RouterQuoteBufferV0 {
    const SIZE: usize = 41736;
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

    /// Append one source's book. Fails rather than truncating silently: a
    /// short book reads as thin liquidity, which would make the router route
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
    /// The cut is the verification a Custom quoter's book needs: its depth is
    /// never margin-reserved, so whatever it advertises past its `User`'s
    /// margin is depth no fill can take. Cutting while the levels are copied
    /// is also what keeps the book out of the heap — the caller reads it
    /// straight from the quoter's response account and never holds a second
    /// copy.
    ///
    /// Returns whether the cap bit, which a consumer reads to tell a thin
    /// quoter from a clamped one.
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
            ErrorCode::DefaultError,
            "router quote buffer holds at most {} sources",
            MAX_QUOTED_SOURCES
        )?;
        let mut remaining = cap;
        let mut written = 0usize;
        for level in levels {
            if remaining == 0 {
                break;
            }
            // Checked against what the cap admits rather than what the quoter
            // offered: a book deeper than the slot is only a problem if the
            // cap lets that much of it through.
            validate!(
                written < MAX_LEVELS_PER_SOURCE,
                ErrorCode::DefaultError,
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
    /// `false` when the region is full. Rows are detail about a ladder that
    /// stands on its own, so running out of room shortens the detail rather
    /// than failing the view — and the buffer says it happened.
    pub fn push_row(&mut self, row: QuotedRowV0) -> VelocityResult<bool> {
        let index = (self.source_count as usize).checked_sub(1).ok_or_else(|| {
            msg!("a row needs a source to belong to");
            ErrorCode::DefaultError
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

    /// The cut is what keeps depth a maker's margin cannot carry off the
    /// published book, so it lands on the rung that crosses the cap rather
    /// than dropping that rung whole.
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

    /// A book deeper than one slot fails the call rather than publishing a
    /// prefix as if it were the whole book — but only when the cap admits
    /// that much of it.
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

    /// The rivals the vAMM is shaded against are read out of the buffer, so
    /// the two level types have to be the same bytes.
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
