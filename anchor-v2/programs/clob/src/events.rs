//! CLOB lifecycle events.
//!
//! Every event type carries an explicit version suffix, matching the `_v0`
//! instruction and `V0` wire generation. The event discriminator is derived
//! from the type name, so a layout change means a new `…RecordV1` type
//! emitted alongside (or in place of) the V0 one — never an edit in place,
//! which would silently repurpose a discriminator indexers already key on.
//!
//! Lifecycle events are `#[event(bytemuck)]` — fixed-size, zero-padding, the
//! cheapest layout. `side`/`direction` are raw u8s (0 = bid/long).
//!
//! None of them is emitted through anchor's `emit!`: every `Event::data()`
//! flavour allocates, so [`crate::emit`] builds the same log bytes in a stack
//! buffer instead. The types here stay the schema of record for what goes on
//! the wire, and `tests::emit` pins the two encodings against each other.

use anchor_lang_v2::prelude::*;

#[event(bytemuck)]
#[repr(C)]
pub struct OrderPlaceRecordV0 {
    pub authority: Address,
    pub ts: i64,
    pub slot: u64,
    pub order_id: u64,
    pub activation_slot: u64,
    pub max_ts: i64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub node_index: u32,
    pub market_index: u16,
    pub sub_account_id: u16,
    pub side: u8,
    pub _pad: [u8; 7],
}

#[event(bytemuck)]
#[repr(C)]
pub struct OrderCancelRecordV0 {
    pub authority: Address,
    pub ts: i64,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub market_index: u16,
    pub sub_account_id: u16,
    pub _pad: [u8; 4],
}

/// One per `cancel_all_v0` — a maker withdrawing a whole side (or both) in one
/// instruction, rather than one [`OrderCancelRecordV0`] per order.
///
/// Orders are referenced by id only, the same contract [`ExecuteRecordV0`]
/// uses: an indexer resolves price and size against the order table it built
/// from place records, so the sweep's log stays ~8 bytes per order instead of
/// re-stating what the reader already has. Ids come in book order, bids before
/// asks.
///
/// `exhaustive` false means the call stopped at
/// [`crate::state::CANCEL_ALL_ORDERS_CEILING`] and this user still has resting
/// orders — a reader must not treat the side as empty.
#[event]
pub struct OrdersCancelRecordV0 {
    pub authority: Address,
    pub ts: i64,
    pub bid_base_asset_amount: u64,
    pub ask_base_asset_amount: u64,
    pub market_index: u16,
    pub sub_account_id: u16,
    /// Which sides the sweep covered, as the wire enum's tag
    /// (0 = bids, 1 = asks, 2 = both).
    pub sides: u8,
    pub exhaustive: bool,
    pub order_ids: Vec<u64>,
}

/// Crank eviction at the soft cap. Distinct from cancel: the UI shows
/// "re-armed"/"evicted", and velocity re-arms triggers in the same tx.
#[event(bytemuck)]
#[repr(C)]
pub struct OrderEvictRecordV0 {
    pub authority: Address,
    pub ts: i64,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub market_index: u16,
    pub sub_account_id: u16,
    pub _pad: [u8; 4],
}

/// Crank reclamation of an expired order (execute only skips expired).
#[event(bytemuck)]
#[repr(C)]
pub struct OrderExpireRecordV0 {
    pub authority: Address,
    pub ts: i64,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub market_index: u16,
    pub sub_account_id: u16,
    pub _pad: [u8; 4],
}

/// One per execute — the hot path stays cheap by referencing orders by id
/// only. `user`/`price` resolve against the indexer's order table (built
/// from place records); quote amounts derive from price × base.
#[event]
pub struct ExecuteRecordV0 {
    pub ts: i64,
    pub slot: u64,
    pub market_index: u16,
    pub direction: u8,
    pub fills: Vec<FillSlimV0>,
    /// Orders culled because the post-fill remainder fell below
    /// `min_order_size`.
    pub cancelled_order_ids: Vec<u64>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct FillSlimV0 {
    pub order_id: u64,
    pub base_size: u64,
}

/// Borsh width of a [`FillSlimV0`] — the per-fill stride of the execute
/// record's payload, which [`crate::emit`] sizes its stack buffer from.
pub const FILL_SLIM_BYTES: usize = 2 * core::mem::size_of::<u64>();
