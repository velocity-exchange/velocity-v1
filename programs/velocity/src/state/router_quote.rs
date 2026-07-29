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
        state::{prop_amm::PriceLevel, traits::Size},
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
    pub padding: [u8; 3],
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
    /// Taker direction quoted (`Direction` as u8: 0 = long, 1 = short).
    pub direction: u8,
    /// Pads the header to 64 bytes so the struct stays a multiple of 16 and
    /// `(SIZE - 8) % 16 == 0` holds (docs/alignment-and-native-offsets.md).
    pub padding: [u8; 12],
    pub sources: [QuotedSourceV0; MAX_QUOTED_SOURCES],
    /// One slot per source, parallel to `sources`.
    pub levels: [[QuotedLevelV0; MAX_LEVELS_PER_SOURCE]; MAX_QUOTED_SOURCES],
}

// Zero-copy layout invariant (docs/alignment-and-native-offsets.md): no u128
// fields, and size including the 8-byte discriminator is ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<RouterQuoteBufferV0>(), 33472);
const_assert_eq!((RouterQuoteBufferV0::SIZE - 8) % 16, 0);

impl Size for RouterQuoteBufferV0 {
    const SIZE: usize = 33480;
}

impl RouterQuoteBufferV0 {
    /// Reset the buffer for a fresh quote round.
    pub fn begin(&mut self, direction: u8, quoted_size: u64, slot: u64) {
        self.source_count = 0;
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
        clamped: bool,
        levels: &[PriceLevel],
    ) -> VelocityResult {
        let index = self.source_count as usize;
        validate!(
            index < MAX_QUOTED_SOURCES,
            ErrorCode::DefaultError,
            "router quote buffer holds at most {} sources",
            MAX_QUOTED_SOURCES
        )?;
        validate!(
            levels.len() <= MAX_LEVELS_PER_SOURCE,
            ErrorCode::DefaultError,
            "a source's book holds at most {} levels, got {}",
            MAX_LEVELS_PER_SOURCE,
            levels.len()
        )?;
        levels.iter().enumerate().for_each(|(i, level)| {
            self.levels[index][i] = QuotedLevelV0 {
                price: level.price,
                size: level.size,
            };
        });
        self.sources[index] = QuotedSourceV0 {
            key,
            level_count: levels.len() as u16,
            priority,
            kind,
            clamped,
            padding: [0; 3],
        };
        self.source_count += 1;
        Ok(())
    }

    /// One source's levels, by its index in `sources`.
    pub fn levels_for(&self, index: usize) -> &[QuotedLevelV0] {
        &self.levels[index][..self.sources[index].level_count as usize]
    }
}
