//! Lifecycle events are `#[event(bytemuck)]` — fixed-size, zero-padding, the
//! cheapest emit path. `side`/`direction` are raw u8s (0 = bid/long).

use anchor_lang_v2::prelude::*;

#[event(bytemuck)]
#[repr(C)]
pub struct OrderPlaceRecord {
    pub user: Address,
    pub ts: i64,
    pub slot: u64,
    pub order_id: u64,
    pub activation_slot: u64,
    pub max_ts: i64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub node_index: u32,
    pub market_index: u16,
    pub side: u8,
    pub _pad: [u8; 1],
}

#[event(bytemuck)]
#[repr(C)]
pub struct OrderCancelRecord {
    pub user: Address,
    pub ts: i64,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub market_index: u16,
    pub _pad: [u8; 6],
}

/// Crank eviction at the soft cap. Distinct from cancel: the UI shows
/// "re-armed"/"evicted", and velocity re-arms triggers in the same tx.
#[event(bytemuck)]
#[repr(C)]
pub struct OrderEvictRecord {
    pub user: Address,
    pub ts: i64,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub market_index: u16,
    pub _pad: [u8; 6],
}

/// Crank reclamation of an expired order (execute only skips expired).
#[event(bytemuck)]
#[repr(C)]
pub struct OrderExpireRecord {
    pub user: Address,
    pub ts: i64,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub market_index: u16,
    pub _pad: [u8; 6],
}

/// One per execute — the hot path stays cheap by referencing orders by id
/// only. `user`/`price` resolve against the indexer's order table (built
/// from place records); quote amounts derive from price × base.
#[event]
pub struct ExecuteRecord {
    pub ts: i64,
    pub slot: u64,
    pub market_index: u16,
    pub direction: u8,
    pub fills: Vec<FillSlim>,
    /// Orders culled because the post-fill remainder fell below
    /// `min_order_size`.
    pub cancelled_order_ids: Vec<u64>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct FillSlim {
    pub order_id: u64,
    pub base_size: u64,
}
