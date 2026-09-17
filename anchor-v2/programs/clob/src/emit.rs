//! Event emission without a heap allocation.
//!
//! Anchor's `emit!` goes through `Event::data()`, which returns a `Vec<u8>` in
//! both of anchor v2's event flavours. The bytemuck flavour allocates a buffer
//! of the right size and copies the struct into it. The wincode flavour
//! allocates a 256-byte guess and reallocates upward when the payload is
//! larger. The execute record is larger from about 15 fills on. What reaches
//! the runtime is `[discriminator][body]`, handed to `sol_log_data` as one
//! field. Everything here builds those bytes in a stack buffer and calls the
//! syscall directly.
//!
//! These helpers pass one field rather than two. `sol_log_data` base64-encodes
//! each slice it is given into a separate entry of a space-separated list.
//! Decoders such as velocity's `EventSubscriber` and the TypeScript SDK
//! base64-decode the whole `Program data:` line as one blob. The discriminator
//! and the body therefore have to be contiguous, so these helpers concatenate
//! them.
//!
//! The emitted bytes are identical to the bytes `Event::data()` returns. That
//! is the contract with every decoder, and `tests::emit` pins each event
//! against the trait implementation.

use {
    crate::{
        error::ClobError,
        events::{ExecuteRecordV0, FillSlimV0, OrdersCancelRecordV0, FILL_SLIM_BYTES},
        state::{
            CancelAllOutcome, CANCEL_ALL_ORDERS_CEILING, CLIENT_ORDER_ID_BYTES, COUNT_BYTES,
            EXECUTE_FILLS_CEILING, ORDER_ID_BYTES,
        },
    },
    anchor_lang::prelude::*,
};

/// Anchor derives every event discriminator as the first 8 bytes of
/// `sha256("event:<TypeName>")`. [`emit_pod`] checks that the type it is handed
/// agrees, so this constant is the prefix width.
pub const DISCRIMINATOR_BYTES: usize = 8;

/// Widest [`ExecuteRecordV0`] log. It holds the discriminator, the fixed
/// prefix, and both sequences at their widest. That is `EXECUTE_FILLS_CEILING`
/// fills and `FILL_BATCH_CEILING` culled orders. `execute_v0` culls at most one
/// order, because a partial fill happens only when the taker's size runs out,
/// which ends the walk. `fill_v0` reports a batch, and every order in it can
/// leave a leftover below the minimum.
pub const EXECUTE_RECORD_LOG_BYTES: usize = DISCRIMINATOR_BYTES
    + core::mem::size_of::<i64>()
    + core::mem::size_of::<u64>()
    + core::mem::size_of::<u16>()
    + core::mem::size_of::<u8>()
    + COUNT_BYTES
    + EXECUTE_FILLS_CEILING as usize * FILL_SLIM_BYTES
    + COUNT_BYTES
    + crate::state::FILL_BATCH_CEILING * CLIENT_ORDER_ID_BYTES;

/// Widest [`OrdersCancelRecordV0`] log. It holds the discriminator, the fixed
/// prefix, and the id list at [`CANCEL_ALL_ORDERS_CEILING`]. The prefix is the
/// authority, the timestamp, both base totals, the market index, the
/// sub-account id, the sides tag, and the exhaustive flag. The ceiling keeps
/// this bound reachable and never exceeded, whatever the book holds.
pub const CANCEL_ALL_RECORD_LOG_BYTES: usize = DISCRIMINATOR_BYTES
    + core::mem::size_of::<Address>()
    + core::mem::size_of::<i64>()
    + 2 * core::mem::size_of::<u64>()
    + 2 * core::mem::size_of::<u16>()
    + 2 * core::mem::size_of::<u8>()
    + COUNT_BYTES
    + CANCEL_ALL_ORDERS_CEILING as usize * CLIENT_ORDER_ID_BYTES;

/// `[discriminator][body]` for a fixed-size `#[event(bytemuck)]` record.
///
/// `N` is the record's full log width. Call through [`emit_pod`], which
/// computes `N` from the type and checks the discriminator width at compile
/// time. A wrong `N` would truncate or pad the event.
pub fn pod_log_bytes<E, const N: usize>(record: &E) -> [u8; N]
where
    E: Discriminator + bytemuck::Pod,
{
    let mut bytes = [0u8; N];
    bytes[..DISCRIMINATOR_BYTES].copy_from_slice(E::DISCRIMINATOR);
    bytes[DISCRIMINATOR_BYTES..].copy_from_slice(bytemuck::bytes_of(record));
    bytes
}

/// Emit a fixed-size `#[event(bytemuck)]` record. It logs the same bytes as
/// `emit!`, built on the stack. It takes the record's struct literal, so a call
/// site reads the way an `emit!` call site reads.
macro_rules! emit_pod {
    ($ty:ident { $($field:tt)* }) => {{
        const LOG_BYTES: usize =
            $crate::emit::DISCRIMINATOR_BYTES + ::core::mem::size_of::<$ty>();
        const _: () = ::core::assert!(
            <$ty as anchor_lang::Discriminator>::DISCRIMINATOR.len()
                == $crate::emit::DISCRIMINATOR_BYTES,
            "event discriminator is not 8 bytes wide",
        );
        anchor_lang::sol_log_data(&[&$crate::emit::pod_log_bytes::<$ty, LOG_BYTES>(&$ty {
            $($field)*
        })]);
    }};
}
pub(crate) use emit_pod;

/// Append-only stack buffer holding one `sol_log_data` field.
///
/// Every push checks its bounds, so a payload wider than the buffer is an
/// error rather than a truncated event. `N` comes from the config ceilings,
/// which makes that error unreachable for a market the init and update checks
/// accepted.
pub struct LogBuf<const N: usize> {
    /// The buffer is uninitialized rather than zeroed. At the fill ceiling it
    /// is about 2KB, and zeroing it costs more compute than the `Vec` this path
    /// avoids. Only `..len` is read, and [`Self::push`] is the only writer.
    bytes: [core::mem::MaybeUninit<u8>; N],
    len: usize,
}

impl<const N: usize> Default for LogBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LogBuf<N> {
    /// Always inlined. The buffer is wide, so a `new()` frame that returns it
    /// would put a second live copy in one SBF stack frame.
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            bytes: [core::mem::MaybeUninit::uninit(); N],
            len: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self
            .len
            .checked_add(bytes.len())
            .ok_or(ClobError::EventTooLarge)?;
        require!(end <= N, ClobError::EventTooLarge);
        // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`. The check
        // above gives `end <= N`, so the write lands inside the buffer, and the
        // source is initialized. The write extends the initialized prefix to
        // `end`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.bytes.as_mut_ptr().add(self.len).cast::<u8>(),
                bytes.len(),
            );
        }
        self.len = end;
        Ok(())
    }

    /// Push `len` zero bytes and return their offset. This holds a field whose
    /// value is not known until later fields are written. The cancel-all
    /// record's totals and id count settle only when its walk ends. The bytes
    /// are zeros rather than a gap, so every counted byte stays initialized,
    /// which is what [`Self::as_slice`] relies on.
    pub fn reserve(&mut self, len: usize) -> Result<usize> {
        let at = self.len;
        (0..len).try_for_each(|_| self.push(&[0]))?;
        Ok(at)
    }

    /// Overwrite bytes that were already pushed. The write stays inside the
    /// initialized prefix, so a reserved field can be filled in and nothing
    /// lands past the end.
    pub fn patch(&mut self, at: usize, bytes: &[u8]) -> Result<()> {
        let end = at
            .checked_add(bytes.len())
            .ok_or(ClobError::EventTooLarge)?;
        require!(end <= self.len, ClobError::EventTooLarge);
        // SAFETY: `end <= self.len` and `push` initialized every byte it
        // counted, so this overwrites initialized memory inside the buffer.
        // Layout as in `push`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.bytes.as_mut_ptr().add(at).cast::<u8>(),
                bytes.len(),
            );
        }
        Ok(())
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `push` is the only way `len` grows, and it initializes every
        // byte it counts, so `..len` is initialized. Layout as above.
        unsafe { core::slice::from_raw_parts(self.bytes.as_ptr().cast::<u8>(), self.len) }
    }

    pub fn emit(&self) {
        sol_log_data(&[self.as_slice()]);
    }
}

/// Borsh-encode an [`ExecuteRecordV0`] payload into `log`.
///
/// The payload is variable-length, because it carries per-order fill detail.
/// It is written field by field rather than through the `emit_pod!` copy. It
/// takes the fields loose rather than an `ExecuteRecordV0`, because building
/// the record would need a `Vec` for the culled ids, which is what this path
/// avoids. [`ExecuteRecordV0`] stays the schema of record for the layout, and
/// `tests::emit` pins the two against each other.
///
/// The buffer is borrowed and never returned by value. At the fill ceiling it
/// is about 2KB. Returning it would put two live copies in one SBF stack
/// frame, the local and the caller's return slot. The hard limit is 4KB per
/// frame, and that pair overruns it.
#[inline(always)]
pub fn write_execute_record<const N: usize>(
    log: &mut LogBuf<N>,
    ts: i64,
    slot: u64,
    market_index: u16,
    direction: u8,
    fills: &[FillSlimV0],
    cancelled_client_order_ids: &[u32],
) -> Result<()> {
    log.push(ExecuteRecordV0::DISCRIMINATOR)?;
    log.push(&ts.to_le_bytes())?;
    log.push(&slot.to_le_bytes())?;
    log.push(&market_index.to_le_bytes())?;
    log.push(&[direction])?;
    log.push(&(fills.len() as u32).to_le_bytes())?;
    // One push per fill rather than one per field. Each push carries a bounds
    // check, and this loop scales with the batch.
    fills.iter().try_for_each(|fill| {
        let mut entry = [0u8; FILL_SLIM_BYTES];
        entry[..ORDER_ID_BYTES].copy_from_slice(&fill.order_id.to_le_bytes());
        entry[ORDER_ID_BYTES..2 * ORDER_ID_BYTES].copy_from_slice(&fill.base_size.to_le_bytes());
        entry[2 * ORDER_ID_BYTES..].copy_from_slice(&fill.client_order_id.to_le_bytes());
        log.push(&entry)
    })?;
    log.push(&(cancelled_client_order_ids.len() as u32).to_le_bytes())?;
    cancelled_client_order_ids
        .iter()
        .try_for_each(|order_id| log.push(&order_id.to_le_bytes()))
}

/// Streaming writer for an [`OrdersCancelRecordV0`].
///
/// The sweep cannot know its own totals until it ends. The base amounts, the
/// `exhaustive` flag and the id count all settle at the last removal. The ids
/// have to be written as the walk frees them, or they would need a second
/// buffer. The prefix is written with those four fields reserved, the ids are
/// appended during the walk, and [`Self::finish`] patches and emits.
///
/// [`OrdersCancelRecordV0`] stays the schema of record for the layout, and
/// `tests::emit` pins the two encodings against each other.
pub struct CancelAllRecord {
    log: LogBuf<CANCEL_ALL_RECORD_LOG_BYTES>,
    bid_base_at: usize,
    ask_base_at: usize,
    exhaustive_at: usize,
    count_at: usize,
    ids: u32,
}

impl CancelAllRecord {
    /// Always inlined. The buffer is about 1KB, and a `new()` frame that
    /// returns it would put two live copies in one SBF stack frame.
    #[inline(always)]
    pub fn new(
        authority: &Address,
        ts: i64,
        market_index: u16,
        sub_account_id: u16,
        sides: u8,
    ) -> Result<Self> {
        let mut log = LogBuf::<CANCEL_ALL_RECORD_LOG_BYTES>::new();
        log.push(OrdersCancelRecordV0::DISCRIMINATOR)?;
        log.push(&authority.to_bytes())?;
        log.push(&ts.to_le_bytes())?;
        let bid_base_at = log.reserve(core::mem::size_of::<u64>())?;
        let ask_base_at = log.reserve(core::mem::size_of::<u64>())?;
        log.push(&market_index.to_le_bytes())?;
        log.push(&sub_account_id.to_le_bytes())?;
        log.push(&[sides])?;
        let exhaustive_at = log.reserve(core::mem::size_of::<u8>())?;
        let count_at = log.reserve(COUNT_BYTES)?;
        Ok(Self {
            log,
            bid_base_at,
            ask_base_at,
            exhaustive_at,
            count_at,
            ids: 0,
        })
    }

    /// Append one removed order id. The record lists the placing caller's ids.
    /// The bounds check in [`LogBuf::push`] enforces the ceiling here too. A
    /// walk that ran past it fails the instruction instead of logging a
    /// truncated record.
    pub fn push_id(&mut self, client_order_id: u32) -> Result<()> {
        self.log.push(&client_order_id.to_le_bytes())?;
        self.ids = self.ids.checked_add(1).ok_or(ClobError::EventTooLarge)?;
        Ok(())
    }

    /// Patch in what the sweep settled and log the record.
    pub fn finish(&mut self, outcome: &CancelAllOutcome) -> Result<()> {
        self.patch_totals(outcome)?;
        self.log.emit();
        Ok(())
    }

    /// The patched record's bytes, without logging them. Split out so
    /// `tests::emit` can hold this encoding against `Event::data()`.
    pub fn log_bytes(&mut self, outcome: &CancelAllOutcome) -> Result<&[u8]> {
        self.patch_totals(outcome)?;
        Ok(self.log.as_slice())
    }

    /// Fill the four fields reserved before the walk. The id count comes from
    /// what the walk pushed. A count that disagrees with the outcome is an
    /// error, rather than a record an indexer would reconcile wrongly.
    fn patch_totals(&mut self, outcome: &CancelAllOutcome) -> Result<()> {
        require!(self.ids == outcome.orders(), ClobError::EventTooLarge);
        let (bid_base_at, ask_base_at) = (self.bid_base_at, self.ask_base_at);
        let (exhaustive_at, count_at) = (self.exhaustive_at, self.count_at);
        self.log
            .patch(bid_base_at, &outcome.bid_base_asset_amount.to_le_bytes())?;
        self.log
            .patch(ask_base_at, &outcome.ask_base_asset_amount.to_le_bytes())?;
        self.log
            .patch(exhaustive_at, &[u8::from(outcome.exhaustive)])?;
        self.log.patch(count_at, &self.ids.to_le_bytes())
    }
}

/// Emit an [`ExecuteRecordV0`] from the stack.
///
/// `#[inline(never)]` gives the buffer its own SBF stack frame. Otherwise its
/// width is added to the handler's frame, which also holds the fill list and
/// the book's locals.
#[inline(never)]
pub fn emit_execute_record(
    ts: i64,
    slot: u64,
    market_index: u16,
    direction: u8,
    fills: &[FillSlimV0],
    cancelled_client_order_ids: &[u32],
) -> Result<()> {
    let mut log = LogBuf::<EXECUTE_RECORD_LOG_BYTES>::new();
    write_execute_record(
        &mut log,
        ts,
        slot,
        market_index,
        direction,
        fills,
        cancelled_client_order_ids,
    )?;
    log.emit();
    Ok(())
}
