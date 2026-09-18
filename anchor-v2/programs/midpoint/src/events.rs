//! Execute is the only evented path. Mid and level writes are the hot loop and
//! emit nothing. Log serialization costs compute that the maker pays thousands
//! of times a day. Fills are rare and worth a record.
//!
//! A record carries two version markers, and an indexer needs both.
//!
//! The type name carries the wire version, such as `…RecordV0`. The 8-byte
//! event discriminator hashes that name. A layout change therefore ships as a
//! new `V1` type, and old consumers keep decoding old logs.
//!
//! The leading `version` byte carries the semantic revision within one layout.
//! A field whose meaning changes, such as a new direction encoding or a new
//! unit, stays readable without a discriminator break.
//!
//! An emitter must stamp [`MIDPOINT_EVENT_VERSION`], never a literal. An
//! emitter must also go through [`crate::emit`] rather than anchor's `emit!`.
//! `Event::data()` allocates a `Vec` even for a fixed-size record, and the log
//! bytes are identical either way.

use anchor_lang::prelude::*;

/// Current semantic revision of every `…RecordV0` in this module.
pub const MIDPOINT_EVENT_VERSION: u8 = 0;

#[event(bytemuck)]
#[repr(C)]
pub struct MidpointExecuteRecordV0 {
    /// The quoted wallet, `MidpointQuoterV0::user_authority`. The maker's
    /// config authority is not in the record. It is not part of the fill.
    pub user_authority: Address,
    pub ts: i64,
    pub slot: u64,
    pub mid_price: u64,
    pub base_size: u64,
    pub quote_size: u64,
    /// The market index this instance was created for. It rides the PDA seeds,
    /// so it is fixed, not proof of where the fill settled. Velocity's registry
    /// decides the settled market and matches the balance-change user against it
    /// before settling, so no value here turns on this field.
    pub configured_market_index: u16,
    pub sub_account_id: u16,
    /// 0 is Long, which means the taker bought the ask side. 1 is Short.
    pub direction: u8,
    /// [`MIDPOINT_EVENT_VERSION`].
    pub version: u8,
    pub _pad: [u8; 2],
}
