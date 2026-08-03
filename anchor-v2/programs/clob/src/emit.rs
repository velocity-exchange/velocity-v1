//! Event emission without a heap allocation.
//!
//! Anchor's `emit!` goes through `Event::data()`, which returns a `Vec<u8>` in
//! both of anchor v2's event flavours: the bytemuck one allocates a buffer of
//! exactly the right size and memcpys the struct into it, and the wincode one
//! allocates a 256-byte guess and reallocs its way up whenever the payload is
//! bigger than that (the execute record is, from ~15 fills on). What reaches
//! the runtime is just `[discriminator][body]` handed to `sol_log_data` as one
//! field, so everything here builds exactly that in a stack buffer and calls
//! the syscall directly.
//!
//! One field, not two: `sol_log_data` base64s each slice it is given
//! separately into a space-separated list, while decoders (velocity's
//! `EventSubscriber`, the TypeScript SDK) base64-decode the whole `Program
//! data:` line as a single blob. The discriminator and body therefore have to
//! be contiguous, which is why these helpers concatenate rather than pass two
//! slices.
//!
//! The bytes emitted are byte-identical to `Event::data()`'s — that is the
//! contract with every decoder, and `tests::emit` pins each event against the
//! trait impl.

use {
    crate::{
        error::ClobError,
        events::{ExecuteRecordV0, FillSlimV0, FILL_SLIM_BYTES},
        state::{COUNT_BYTES, EXECUTE_FILLS_CEILING, ORDER_ID_BYTES},
    },
    anchor_lang_v2::prelude::*,
};

/// Anchor derives every event discriminator as the first 8 bytes of
/// `sha256("event:<TypeName>")`. [`emit_pod`] checks the type it is handed
/// agrees, so this can be treated as the prefix width.
pub const DISCRIMINATOR_BYTES: usize = 8;

/// Widest [`ExecuteRecordV0`] log: the discriminator, the fixed prefix, and
/// both sequences at the widest a market config can drive them — at most
/// `EXECUTE_FILLS_CEILING` fills, and at most one culled order (a partial fill
/// only happens once the taker's size runs out, which ends the walk).
pub const EXECUTE_RECORD_LOG_BYTES: usize = DISCRIMINATOR_BYTES
    + core::mem::size_of::<i64>()
    + core::mem::size_of::<u64>()
    + core::mem::size_of::<u16>()
    + core::mem::size_of::<u8>()
    + COUNT_BYTES
    + EXECUTE_FILLS_CEILING as usize * FILL_SLIM_BYTES
    + COUNT_BYTES
    + ORDER_ID_BYTES;

/// `[discriminator][body]` for a fixed-size (`#[event(bytemuck)]`) record.
///
/// `N` is the record's full log width. Call through [`emit_pod`], which
/// computes `N` from the type and asserts at compile time that the slice math
/// below is exact — a wrong `N` would otherwise truncate or pad the event.
pub fn pod_log_bytes<E, const N: usize>(record: &E) -> [u8; N]
where
    E: Discriminator + bytemuck::Pod,
{
    let mut bytes = [0u8; N];
    bytes[..DISCRIMINATOR_BYTES].copy_from_slice(E::DISCRIMINATOR);
    bytes[DISCRIMINATOR_BYTES..].copy_from_slice(bytemuck::bytes_of(record));
    bytes
}

/// Emit a fixed-size (`#[event(bytemuck)]`) record: the same bytes `emit!`
/// would log, built on the stack. Takes the record's struct literal, so call
/// sites read the way `emit!` did.
macro_rules! emit_pod {
    ($ty:ident { $($field:tt)* }) => {{
        const LOG_BYTES: usize =
            $crate::emit::DISCRIMINATOR_BYTES + ::core::mem::size_of::<$ty>();
        const _: () = ::core::assert!(
            <$ty as anchor_lang_v2::Discriminator>::DISCRIMINATOR.len()
                == $crate::emit::DISCRIMINATOR_BYTES,
            "event discriminator is not 8 bytes wide",
        );
        anchor_lang_v2::sol_log_data(&[&$crate::emit::pod_log_bytes::<$ty, LOG_BYTES>(&$ty {
            $($field)*
        })]);
    }};
}
pub(crate) use emit_pod;

/// Append-only stack buffer holding one `sol_log_data` field.
///
/// Every push bounds-checks, so a payload wider than the buffer is an error
/// rather than a truncated event. `N` is sized from the config ceilings, which
/// makes that error unreachable for a market the init/update checks accepted.
pub struct LogBuf<const N: usize> {
    /// Uninitialized rather than zeroed: at the fill ceiling this is ~2KB, and
    /// zeroing it costs more compute than the `Vec` this path exists to avoid.
    /// Only `..len` is ever read, and [`Self::push`] is the only writer.
    bytes: [core::mem::MaybeUninit<u8>; N],
    len: usize,
}

impl<const N: usize> Default for LogBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LogBuf<N> {
    /// Always inlined: the buffer is wide enough that a `new()` frame handing
    /// it back would be a second live copy of it in one SBF stack frame.
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
        // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`; the write
        // lands inside the buffer (`end <= N`, just checked) and the source is
        // initialized. This is what extends the initialized prefix to `end`.
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
/// This is the one event whose payload is variable-length (per-order fill
/// detail), so it is written field by field instead of going through the
/// `emit_pod!` memcpy, and it takes the fields loose rather than an
/// `ExecuteRecordV0` — building the record would mean a `Vec` for the culled
/// id, which is exactly what this path exists to avoid. [`ExecuteRecordV0`]
/// stays the schema of record for the layout; `tests::emit` pins the two
/// against each other.
///
/// The buffer is borrowed, never returned by value. At the fill ceiling it is
/// ~2KB, and handing it back means two live copies of it in one SBF stack
/// frame (the local and the caller's return slot) — 4KB per frame is the hard
/// limit, and that combination overruns it.
#[inline(always)]
pub fn write_execute_record<const N: usize>(
    log: &mut LogBuf<N>,
    ts: i64,
    slot: u64,
    market_index: u16,
    direction: u8,
    fills: &[FillSlimV0],
    cancelled_order_id: Option<u64>,
) -> Result<()> {
    log.push(ExecuteRecordV0::DISCRIMINATOR)?;
    log.push(&ts.to_le_bytes())?;
    log.push(&slot.to_le_bytes())?;
    log.push(&market_index.to_le_bytes())?;
    log.push(&[direction])?;
    log.push(&(fills.len() as u32).to_le_bytes())?;
    // One push per fill rather than one per field: each push carries a bounds
    // check, and this is the loop that scales with the batch.
    fills.iter().try_for_each(|fill| {
        let mut entry = [0u8; FILL_SLIM_BYTES];
        entry[..ORDER_ID_BYTES].copy_from_slice(&fill.order_id.to_le_bytes());
        entry[ORDER_ID_BYTES..].copy_from_slice(&fill.base_size.to_le_bytes());
        log.push(&entry)
    })?;
    log.push(&(cancelled_order_id.iter().count() as u32).to_le_bytes())?;
    cancelled_order_id
        .iter()
        .try_for_each(|order_id| log.push(&order_id.to_le_bytes()))
}

/// Emit an [`ExecuteRecordV0`] from the stack.
///
/// `#[inline(never)]` so the buffer gets an SBF stack frame of its own instead
/// of adding its width to the handler's, which also holds the fill list and
/// the book's locals.
#[inline(never)]
pub fn emit_execute_record(
    ts: i64,
    slot: u64,
    market_index: u16,
    direction: u8,
    fills: &[FillSlimV0],
    cancelled_order_id: Option<u64>,
) -> Result<()> {
    let mut log = LogBuf::<EXECUTE_RECORD_LOG_BYTES>::new();
    write_execute_record(
        &mut log,
        ts,
        slot,
        market_index,
        direction,
        fills,
        cancelled_order_id,
    )?;
    log.emit();
    Ok(())
}
