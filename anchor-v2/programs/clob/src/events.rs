//! CLOB lifecycle events.
//!
//! Every event type carries an explicit version suffix, matching the `_v0`
//! instruction and `V0` wire generation. The event discriminator is derived
//! from the type name, so a layout change means a new `…RecordV1` type
//! emitted alongside (or in place of) the V0 one — never an edit in place,
//! which would silently repurpose a discriminator indexers already key on.
//!
//! Lifecycle events are `#[event(bytemuck)]` — fixed-size, zero-padding, the
//! cheapest emit path. `side`/`direction` are raw u8s (0 = bid/long).

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
