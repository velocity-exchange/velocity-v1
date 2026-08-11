//! Streaming borsh encoder for the quoter-interface responses.
//!
//! `quote_v0`/`execute_v0` return a [`ResponsePointerV0`] locating their
//! payload inside the market header's `response` region. The payload is
//! written straight into that region as it is produced — no `Vec` of levels
//! or balance changes, no intermediate serialization buffer, nothing staged
//! on the 32KB program heap. Sequence counts are unknown until the book walk
//! ends, so they are reserved as a 4-byte slot and backpatched
//! ([`ResponseWriter::patch_count`]).
//!
//! The encoding is plain borsh: little-endian scalars, `Vec<T>` as a u32
//! count followed by the elements. [`crate::state::QuoteResponseV0`] and
//! [`crate::state::ExecuteResponseV0`] remain the schema of record for what
//! is written here; `tests::streamed_*` pin the streamed bytes against
//! wincode's encoding of those types so the two cannot drift.

use {
    crate::{
        error::ClobError,
        state::{
            ClobMarketV0, ResponsePointerV0, RESPONSE_BUFFER_BYTES, RESPONSE_LEN_BYTES, RESPONSE_OFFSET,
        },
    },
    anchor_lang_v2::prelude::*,
};

/// Append-only cursor over the market's response region.
///
/// Every write goes through [`Self::append`] or one of the patch helpers,
/// and each bounds-checks against the region size (appends) or the bytes
/// written so far (patches), so a miscomputed offset is a program error
/// rather than a write past the response into the order arena.
pub struct ResponseWriter {
    len: usize,
}

impl Default for ResponseWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseWriter {
    pub fn new() -> Self {
        Self { len: 0 }
    }

    /// Bytes written so far — also the offset the next append lands at.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append raw bytes, returning the offset they were written at.
    pub fn append(&mut self, book: &mut ClobMarketV0, bytes: &[u8]) -> Result<usize> {
        let start = self.len;
        let end = start
            .checked_add(bytes.len())
            .ok_or(ClobError::ResponseTooLarge)?;
        require!(end <= RESPONSE_BUFFER_BYTES, ClobError::ResponseTooLarge);
        book.response[start..end].copy_from_slice(bytes);
        self.len = end;
        Ok(start)
    }

    pub fn append_u64(&mut self, book: &mut ClobMarketV0, value: u64) -> Result<usize> {
        self.append(book, &value.to_le_bytes())
    }

    /// Reserve a sequence length, returning the offset to backpatch once the
    /// element count is known. The width is wincode's, not a choice made here.
    pub fn reserve_count(&mut self, book: &mut ClobMarketV0) -> Result<usize> {
        self.append(book, &quoter_spec::len_prefix(0))
    }

    /// Backpatch a count reserved by [`Self::reserve_count`].
    pub fn patch_count(
        &mut self,
        book: &mut ClobMarketV0,
        offset: usize,
        count: usize,
    ) -> Result<()> {
        self.written_mut(book, offset, RESPONSE_LEN_BYTES)?
            .copy_from_slice(&quoter_spec::len_prefix(count));
        Ok(())
    }

    pub fn read_count(&self, book: &ClobMarketV0, offset: usize) -> Result<u64> {
        let bytes = self.written(book, offset, RESPONSE_LEN_BYTES)?;
        Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| ClobError::MathError)?,
        ))
    }

    pub fn read_u64(&self, book: &ClobMarketV0, offset: usize) -> Result<u64> {
        let bytes = self.written(book, offset, 8)?;
        Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| ClobError::MathError)?,
        ))
    }

    /// Add to an already-written u64 — how a fill merges into the running
    /// totals of a balance-change record that is no longer at the cursor.
    /// Read/modify/write through one bounds-checked slice: this runs a few
    /// times per fill.
    pub fn add_u64(&mut self, book: &mut ClobMarketV0, offset: usize, delta: u64) -> Result<()> {
        let slot = self.written_mut(book, offset, 8)?;
        let value = u64::from_le_bytes((&*slot).try_into().map_err(|_| ClobError::MathError)?);
        let sum = value.checked_add(delta).ok_or(ClobError::MathError)?;
        slot.copy_from_slice(&sum.to_le_bytes());
        Ok(())
    }

    /// Whether the bytes already written at `offset` equal `expected`.
    pub fn matches(&self, book: &ClobMarketV0, offset: usize, expected: &[u8]) -> Result<bool> {
        Ok(self.written(book, offset, expected.len())? == expected)
    }

    /// Splice a u64 into the middle of the written region, shifting
    /// everything after `offset` right by 8 bytes. Used to grow a balance
    /// change's `completed_order_ids` in place; when the record is the last
    /// one written (the common case — a maker's first fill often completes
    /// their order) nothing needs moving and this is a plain append.
    pub fn insert_u64(&mut self, book: &mut ClobMarketV0, offset: usize, value: u64) -> Result<()> {
        require!(offset <= self.len, ClobError::ResponseTooLarge);
        let end = self.len.checked_add(8).ok_or(ClobError::ResponseTooLarge)?;
        require!(end <= RESPONSE_BUFFER_BYTES, ClobError::ResponseTooLarge);
        book.response[..end].copy_within(offset..self.len, offset + 8);
        book.response[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        self.len = end;
        Ok(())
    }

    /// The pointer the instruction returns: the response region's account
    /// offset plus the number of bytes written.
    pub fn finish(self) -> ResponsePointerV0 {
        ResponsePointerV0 {
            offset: RESPONSE_OFFSET as u32,
            len: self.len as u32,
        }
    }

    /// A slice of the already-written region. Patches and comparisons are
    /// confined to it so they can never reach bytes this response has not
    /// produced.
    fn written<'a>(&self, book: &'a ClobMarketV0, offset: usize, len: usize) -> Result<&'a [u8]> {
        let end = offset.checked_add(len).ok_or(ClobError::MathError)?;
        require!(end <= self.len, ClobError::ResponseTooLarge);
        Ok(&book.response[offset..end])
    }

    fn written_mut<'a>(
        &self,
        book: &'a mut ClobMarketV0,
        offset: usize,
        len: usize,
    ) -> Result<&'a mut [u8]> {
        let end = offset.checked_add(len).ok_or(ClobError::MathError)?;
        require!(end <= self.len, ClobError::ResponseTooLarge);
        Ok(&mut book.response[offset..end])
    }
}
