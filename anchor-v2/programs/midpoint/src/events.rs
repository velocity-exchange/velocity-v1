//! Execute is the only evented path — mid/level writes are the hot loop and
//! deliberately emit nothing (log serialization is compute the maker pays
//! for thousands of times a day; fills are rare and worth a record).
//!
//! Records are versioned twice over, and both mechanisms are load-bearing for
//! an indexer:
//!
//! - the type name carries the wire version (`…RecordV0`), which is what the
//!   8-byte event discriminator hashes, so a *layout* change ships as a new
//!   `V1` type and old consumers keep decoding old logs;
//! - the leading `version` byte carries the *semantic* revision within a
//!   layout, so a field whose meaning changes (a new direction encoding, a
//!   unit change) can be distinguished without a discriminator break.
//!
//! Emitters must always stamp [`MIDPOINT_EVENT_VERSION`], never a literal, and
//! go through [`crate::emit`] rather than anchor's `emit!`: `Event::data()`
//! allocates a `Vec` even for a fixed-size record, and the log bytes are
//! identical either way.

use anchor_lang::prelude::*;

/// Current semantic revision of every `…RecordV0` in this module.
pub const MIDPOINT_EVENT_VERSION: u8 = 0;

#[event(bytemuck)]
#[repr(C)]
pub struct MidpointExecuteRecordV0 {
    /// The quoted wallet (`MidpointQuoterV0::user_authority`) — the maker's
    /// config authority is deliberately not in the record; it is not part of
    /// the fill.
    pub user_authority: Address,
    pub ts: i64,
    pub slot: u64,
    pub mid_price: u64,
    pub base_size: u64,
    pub quote_size: u64,
    pub market_index: u16,
    pub sub_account_id: u16,
    /// 0 = Long (taker bought the ask side), 1 = Short.
    pub direction: u8,
    /// [`MIDPOINT_EVENT_VERSION`].
    pub version: u8,
    pub _pad: [u8; 2],
}
