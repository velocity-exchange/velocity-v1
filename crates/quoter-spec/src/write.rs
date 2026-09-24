//! Writing a response into the region its caller reads it from.
//!
//! A quoter cannot hand a whole response type to `wincode::serialize`. It
//! produces the records while it walks, and does not know a section's count
//! until the walk ends. Counting first needs a `Vec` per section, which is the
//! cost this wire exists to avoid. So a writer writes the framing record by
//! record.
//!
//! It is written here, once, rather than once per quoter. A section a quoter
//! forgets to write is the same disagreement as a field read at the wrong
//! offset, and a hand-rolled writer per program is a second declaration of the
//! framing that nothing holds against the first. Here the sections are
//! [`ExecuteWriter::finish`]'s parameters, so a new section stops every quoter
//! compiling until it passes one.
//!
//! # Records go in whole
//!
//! Every record is [`bytemuck::Pod`], a response region starts on an 8-byte
//! step, and every section stride is a multiple of 8. A record therefore maps
//! onto its bytes and is written as one store. The reader needs the same
//! property, because [`crate::ExecuteResponseV0::parse`] borrows
//! `&[UserBalanceChangeV0]` out of these bytes. A writer that assembles a
//! record field by field keeps a second set of field offsets that can drift
//! from the declaration.
//!
//! # The region is a parameter
//!
//! A writer holds only its offsets, and every operation names the region it
//! writes into. That is the CLOB's constraint. Its walk re-lends the market
//! account to a callback on every order it visits, and the callback removes
//! orders and rewrites nodes through it. A writer that held a borrow of the
//! response region would be a second mutable borrow of the same account. A
//! writer therefore cannot own its region, and one that took it only at
//! construction would let a caller name two different regions.
//!
//! Every step of a walk goes through here, so the hot methods are marked
//! `#[inline]`. These are cross-crate generics over a `&mut [u8]`, and without
//! the hint a record reaches the region through a stack copy. The CLOB's own
//! compute benchmarks move by a few percent on the attribute alone.

use {
    crate::{
        completed_orders_fit, len_prefix, partial_orders_fit, CancelledRemainderV0,
        CompletedOrderV0, L3RowV0, PartiallyFilledOrderV0, PriceLevelV0, SpecError,
        UserBalanceChangeV0, LEN_BYTES,
    },
    bytemuck::Pod,
    core::mem::size_of,
};

/// Append-only cursor over a response region. Appends are bounds-checked against the
/// region and patches against the bytes written so far, so a miscomputed offset is a
/// [`SpecError`] rather than a write into whatever the account holds next.
struct Cursor {
    len: usize,
}

impl Cursor {
    /// A cursor positioned past the response's leading length prefix. The count is
    /// unknown until the walk ends, so [`Self::patch_len`] writes all eight bytes
    /// later rather than storing a zero that is always overwritten.
    const fn new() -> Self {
        Self { len: LEN_BYTES }
    }

    /// Append one record, and return the offset it landed at.
    #[inline]
    fn push<T: Pod>(&mut self, region: &mut [u8], value: T) -> Result<usize, SpecError> {
        let start = self.len;
        let end = start
            .checked_add(size_of::<T>())
            .ok_or(SpecError::RegionTooSmall)?;
        let slot = region
            .get_mut(start..end)
            .ok_or(SpecError::RegionTooSmall)?;
        *bytemuck::try_from_bytes_mut::<T>(slot).map_err(|_| SpecError::RegionMisaligned)? = value;
        self.len = end;
        Ok(start)
    }

    /// Append a whole section of records as one copy.
    #[inline]
    fn extend<T: Pod>(&mut self, region: &mut [u8], values: &[T]) -> Result<(), SpecError> {
        let start = self.len;
        let end = size_of::<T>()
            .checked_mul(values.len())
            .and_then(|bytes| start.checked_add(bytes))
            .ok_or(SpecError::RegionTooSmall)?;
        let slot = region
            .get_mut(start..end)
            .ok_or(SpecError::RegionTooSmall)?;
        // Cast rather than copy bytes. The reader casts this section in place,
        // so a start the records cannot be read at must fail here.
        bytemuck::try_cast_slice_mut::<u8, T>(slot)
            .map_err(|_| SpecError::RegionMisaligned)?
            .copy_from_slice(values);
        self.len = end;
        Ok(())
    }

    /// Write a section's length prefix, once the walk that produced the
    /// section has ended.
    #[inline]
    fn patch_len(&self, region: &mut [u8], offset: usize, count: usize) -> Result<(), SpecError> {
        let end = offset
            .checked_add(LEN_BYTES)
            .ok_or(SpecError::RegionTooSmall)?;
        // Confined to bytes this response wrote, so a patch can never reach
        // what the region held before it.
        if end > self.len {
            return Err(SpecError::RegionTooSmall);
        }

        region
            .get_mut(offset..end)
            .ok_or(SpecError::RegionTooSmall)?
            .copy_from_slice(&len_prefix(count));
        Ok(())
    }

    /// Append a complete section: its count, then its records.
    #[inline]
    fn push_section<T: Pod>(&mut self, region: &mut [u8], values: &[T]) -> Result<(), SpecError> {
        self.push(region, values.len() as u64)?;
        self.extend(region, values)
    }

    /// Records already written, mapped in place.
    #[inline]
    fn section<'a, T: Pod>(
        &self,
        region: &'a [u8],
        start: usize,
        count: usize,
    ) -> Result<&'a [T], SpecError> {
        let end = self.section_end::<T>(start, count)?;
        bytemuck::try_cast_slice(region.get(start..end).ok_or(SpecError::RegionTooSmall)?)
            .map_err(|_| SpecError::RegionMisaligned)
    }

    #[inline]
    fn section_mut<'a, T: Pod>(
        &self,
        region: &'a mut [u8],
        start: usize,
        count: usize,
    ) -> Result<&'a mut [T], SpecError> {
        let end = self.section_end::<T>(start, count)?;
        bytemuck::try_cast_slice_mut(
            region
                .get_mut(start..end)
                .ok_or(SpecError::RegionTooSmall)?,
        )
        .map_err(|_| SpecError::RegionMisaligned)
    }

    /// End of a section, held to the bytes this response wrote.
    #[inline]
    fn section_end<T>(&self, start: usize, count: usize) -> Result<usize, SpecError> {
        let end = size_of::<T>()
            .checked_mul(count)
            .and_then(|bytes| start.checked_add(bytes))
            .ok_or(SpecError::RegionTooSmall)?;
        if end > self.len {
            return Err(SpecError::RegionTooSmall);
        }

        Ok(end)
    }
}

/// Writes a [`crate::QuoteResponseV0`] as the ladder is produced. [`Self::new`] reserves
/// the length prefix and [`Self::finish`] backfills it, because a walk that stops once
/// the taker's size is covered does not know how many rungs it wrote.
pub struct QuoteWriter {
    cursor: Cursor,
    levels: usize,
}

impl Default for QuoteWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl QuoteWriter {
    pub const fn new() -> Self {
        Self {
            cursor: Cursor::new(),
            levels: 0,
        }
    }

    /// Rungs written so far.
    #[inline]
    pub fn levels(&self) -> usize {
        self.levels
    }

    #[inline]
    pub fn push_level(&mut self, region: &mut [u8], level: PriceLevelV0) -> Result<(), SpecError> {
        self.cursor.push(region, level)?;
        self.levels += 1;
        Ok(())
    }

    /// Backfill the ladder's count, write the withheld report behind it, and return the
    /// response's length in bytes. The report is a parameter because it is the whole
    /// tail, so a field added there changes this signature.
    pub fn finish(self, region: &mut [u8], withheld: PriceLevelV0) -> Result<usize, SpecError> {
        let Self { mut cursor, levels } = self;
        cursor.patch_len(region, 0, levels)?;
        cursor.push(region, withheld)?;
        Ok(cursor.len)
    }
}

/// Writes an [`crate::L3ResponseV0`] as the walk produces it. Same shape as
/// [`QuoteWriter`] and for the same reason. The row count is not known until the walk
/// ends, so the prefix is backfilled and the tail marker is a [`Self::finish`]
/// parameter.
pub struct L3Writer {
    cursor: Cursor,
    rows: usize,
}

impl Default for L3Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl L3Writer {
    pub const fn new() -> Self {
        Self {
            cursor: Cursor::new(),
            rows: 0,
        }
    }

    /// Rows written so far.
    #[inline]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[inline]
    pub fn push_row(&mut self, region: &mut [u8], row: L3RowV0) -> Result<(), SpecError> {
        self.cursor.push(region, row)?;
        self.rows += 1;
        Ok(())
    }

    /// Backfill the row count, mark whether depth remains behind the last
    /// row, and return the response's length in bytes.
    pub fn finish(self, region: &mut [u8], more: bool) -> Result<usize, SpecError> {
        let Self { mut cursor, rows } = self;
        cursor.patch_len(region, 0, rows)?;
        cursor.push(region, u8::from(more))?;
        Ok(cursor.len)
    }
}

/// Offset of the balance-change section: the response's first length prefix,
/// then the records.
const CHANGES_START: usize = LEN_BYTES;

/// Writes a [`crate::ExecuteResponseV0`] as the fill is produced. The balance changes
/// are streamed because a fill revisits them through [`Self::change_mut`] when a maker
/// fills twice. The other sections are written once, so they pass to [`Self::finish`]
/// whole.
pub struct ExecuteWriter {
    cursor: Cursor,
    changes: usize,
}

impl Default for ExecuteWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecuteWriter {
    pub const fn new() -> Self {
        Self {
            cursor: Cursor::new(),
            changes: 0,
        }
    }

    /// Balance changes written so far.
    #[inline]
    pub fn changes_len(&self) -> usize {
        self.changes
    }

    /// Append a change, and return the index a [`CompletedOrderV0`] names it
    /// by.
    #[inline]
    pub fn push_change(
        &mut self,
        region: &mut [u8],
        change: UserBalanceChangeV0,
    ) -> Result<u32, SpecError> {
        let index = u32::try_from(self.changes).map_err(|_| SpecError::RegionTooSmall)?;
        self.cursor.push(region, change)?;
        self.changes += 1;
        Ok(index)
    }

    /// The changes written so far, mapped in place. This is how a fill finds
    /// the record its maker already has.
    #[inline]
    pub fn changes<'a>(&self, region: &'a [u8]) -> Result<&'a [UserBalanceChangeV0], SpecError> {
        self.cursor.section(region, CHANGES_START, self.changes)
    }

    /// One change, mapped in place, so a repeat fill adds into the totals its
    /// maker already has.
    #[inline]
    pub fn change_mut<'a>(
        &self,
        region: &'a mut [u8],
        index: u32,
    ) -> Result<&'a mut UserBalanceChangeV0, SpecError> {
        self.cursor
            .section_mut(region, CHANGES_START, self.changes)?
            .get_mut(index as usize)
            .ok_or(SpecError::DanglingCompletedOrder)
    }

    /// Close the response: backfill the change count, then write the remaining
    /// sections in wire order. Returns the response's length in bytes.
    pub fn finish(
        self,
        region: &mut [u8],
        cancelled: &[CancelledRemainderV0],
        completed: &[CompletedOrderV0],
        partial: &[PartiallyFilledOrderV0],
    ) -> Result<usize, SpecError> {
        let Self {
            mut cursor,
            changes,
        } = self;

        // The reader indexes with this number, and a dangling one unwinds some other
        // user's live margin. Refuse to write one, so a quoter fails on its own bug
        // rather than on the router's rejection of it.
        if !completed_orders_fit(completed, changes) || !partial_orders_fit(partial, changes) {
            return Err(SpecError::DanglingCompletedOrder);
        }

        cursor.patch_len(region, 0, changes)?;
        cursor.push_section(region, cancelled)?;
        cursor.push_section(region, completed)?;
        cursor.push_section(region, partial)?;
        Ok(cursor.len)
    }
}
