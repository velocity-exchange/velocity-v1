//! CLOB lifecycle events.
//!
//! Every event type carries an explicit version suffix, which matches the
//! `_v0` instruction and the `V0` wire generation. Anchor derives the event
//! discriminator from the type name. A layout change therefore ships as a new
//! `…RecordV1` type, emitted beside the V0 one or in place of it. An edit in
//! place would repurpose a discriminator that indexers already key on, and no
//! reader would see the change.
//!
//! Lifecycle events are `#[event(bytemuck)]`, a fixed-size layout with zeroed
//! padding and the lowest cost. `side` and `direction` are raw `u8` values.
//! Zero is a bid or a long.
//!
//! No order record is emitted through anchor's `emit!`. Every
//! `Event::data()` flavour allocates, so [`crate::emit`] builds the same log
//! bytes in a stack buffer. The market initialize and update records carry a
//! nested settings struct and run on admin paths, so they use `emit!`. The types here stay the schema of record for what goes on the wire,
//! and `tests::emit` pins the two encodings against each other.

use anchor_lang::prelude::*;

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
    /// The placing caller's own id for this order. Every later record names
    /// the order by this, so a reader files it under the id its own records
    /// use and never holds a map between the two id spaces.
    pub client_order_id: u32,
    pub market_index: u16,
    pub sub_account_id: u16,
    pub side: u8,
    /// [`crate::state::OrderBitFlag::TakerOrigin`] and
    /// [`crate::state::OrderBitFlag::ReduceOnly`], at the same bit values.
    pub flags: u8,
    pub _pad: [u8; 2],
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
    pub client_order_id: u32,
}

/// One per `cancel_all_v0`. A maker withdraws a whole side, or both sides, in
/// one instruction, rather than one [`OrderCancelRecordV0`] per order.
///
/// The record names orders by id only, under the same contract
/// [`ExecuteRecordV0`] uses. An indexer resolves price and size against the
/// order table it built from place records. The sweep's log therefore stays
/// four bytes per order instead of restating what the reader already holds.
/// Ids come in book order, bids before asks.
///
/// `exhaustive` false means the call stopped at
/// [`crate::state::CANCEL_ALL_ORDERS_CEILING`] and this user still has resting
/// orders. A reader must not treat the side as empty.
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
    /// The swept orders, named by the placing caller's ids rather than the
    /// book's. A bulk list is read by whoever files orders under those ids,
    /// and carrying both would double the log for an id nothing downstream
    /// asks for.
    pub client_order_ids: Vec<u32>,
}

/// Crank eviction at the soft cap. It is a separate record from a cancel,
/// because the UI shows an order as evicted or re-armed, and velocity re-arms
/// triggers in the same transaction.
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
    pub client_order_id: u32,
}

/// Crank reclamation of an expired order. An execute only skips an expired
/// order.
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
    pub client_order_id: u32,
}

/// One per execute. The record names orders by id only, which keeps the hot
/// path cheap. A reader resolves user and price against the indexer's order
/// table, which it builds from place records. Quote amounts come from price
/// times base.
#[event]
pub struct ExecuteRecordV0 {
    pub ts: i64,
    pub slot: u64,
    pub market_index: u16,
    pub direction: u8,
    pub fills: Vec<FillSlimV0>,
    /// Orders culled because the post-fill remainder fell below
    /// `min_order_size`, by the placing caller's ids. See
    /// [`OrdersCancelRecordV0::client_order_ids`].
    pub cancelled_client_order_ids: Vec<u32>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct FillSlimV0 {
    pub order_id: u64,
    pub base_size: u64,
    /// The placing caller's own id for this order. Every reader downstream of
    /// velocity files orders under the caller's ids. The field lets such a
    /// reader join a fill to an order without a map between the two id spaces.
    pub client_order_id: u32,
}

/// Borsh width of a [`FillSlimV0`]. This is the per-fill stride of the execute
/// record's payload, and [`crate::emit`] sizes its stack buffer from it.
pub const FILL_SLIM_BYTES: usize = 2 * core::mem::size_of::<u64>() + core::mem::size_of::<u32>();

/// One per `fill_v0`. `execute_v0` fills a taker against makers already on
/// this book, so [`ExecuteRecordV0`] can name the maker by id and leave price
/// and owner to the reader's order table. `fill_v0` instead reports a resting
/// taker remainder that aggressed against a venue this program never saw, so
/// the record carries the remainder's own price and owner rather than sending
/// a reader to a fill it cannot resolve any other way.
#[event]
pub struct FillRecordV0 {
    pub ts: i64,
    pub slot: u64,
    pub market_index: u16,
    pub fills: Vec<FillEntryV0>,
    /// See [`ExecuteRecordV0::cancelled_client_order_ids`].
    pub cancelled_client_order_ids: Vec<u32>,
}

/// One taker remainder filled in a `fill_v0` batch. `price` is the remainder's
/// own resting price, the only price this program stores for it.
#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct FillEntryV0 {
    pub order_id: u64,
    pub owner: Address,
    pub price: u64,
    pub base_size: u64,
    pub client_order_id: u32,
}

/// Borsh width of a [`FillEntryV0`]. Sizes [`crate::emit`]'s stack buffer for
/// [`FillRecordV0`].
pub const FILL_ENTRY_BYTES: usize =
    3 * core::mem::size_of::<u64>() + core::mem::size_of::<Address>() + core::mem::size_of::<u32>();

// Market administration records. Each names the market account, because one
// authority may administer several books.

/// `propose_market_authority_v0` named a successor. A zero `proposed_authority`
/// withdraws the open proposal.
#[event(bytemuck)]
#[repr(C)]
pub struct MarketAuthorityProposedRecordV0 {
    pub market: Address,
    pub authority: Address,
    pub proposed_authority: Address,
    pub ts: i64,
}

/// `accept_market_authority_v0` installed the proposed authority.
#[event(bytemuck)]
#[repr(C)]
pub struct MarketAuthorityAcceptedRecordV0 {
    pub market: Address,
    pub previous_authority: Address,
    pub authority: Address,
    pub ts: i64,
}

/// Every mutable setting of a market, as `update_market_v0` can change it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct MarketSettingsV0 {
    pub order_tick_size: u64,
    pub order_step_size: u64,
    pub min_order_size: u64,
    pub blocking_min_size: u64,
    pub default_activation_delay_slots: u32,
    pub max_activation_delay_slots: u32,
    pub unknown_user_grace_slots: u32,
    pub evict_threshold_per_side: u32,
    pub max_quote_levels: u16,
    pub max_execute_fills: u16,
    pub max_execute_users: u16,
    pub reservation_grace_slots: u16,
}

impl MarketSettingsV0 {
    pub fn of(header: &crate::state::ClobHeaderV0) -> Self {
        Self {
            order_tick_size: header.order_tick_size,
            order_step_size: header.order_step_size,
            min_order_size: header.min_order_size,
            blocking_min_size: header.blocking_min_size,
            default_activation_delay_slots: header.default_activation_delay_slots,
            max_activation_delay_slots: header.max_activation_delay_slots,
            unknown_user_grace_slots: header.unknown_user_grace_slots,
            evict_threshold_per_side: header.evict_threshold_per_side,
            max_quote_levels: header.max_quote_levels,
            max_execute_fills: header.max_execute_fills,
            max_execute_users: header.max_execute_users,
            reservation_grace_slots: header.reservation_grace_slots,
        }
    }
}

/// `initialize_market_v0` created a market.
#[event]
pub struct MarketInitializeRecordV0 {
    pub market: Address,
    pub authority: Address,
    pub place_authority: Address,
    pub ts: i64,
    pub base_precision: u64,
    pub capacity: u32,
    pub market_index: u16,
    pub settings: MarketSettingsV0,
}

/// `update_market_v0` changed a market's settings.
#[event]
pub struct MarketUpdateRecordV0 {
    pub market: Address,
    pub authority: Address,
    pub ts: i64,
    pub before: MarketSettingsV0,
    pub after: MarketSettingsV0,
}

/// `resize_market_v0` grew a market's arena.
#[event(bytemuck)]
#[repr(C)]
pub struct MarketResizeRecordV0 {
    pub market: Address,
    pub authority: Address,
    pub ts: i64,
    pub previous_capacity: u32,
    pub capacity: u32,
}

/// `close_market_v0` closed an empty market and paid its rent to
/// `rent_recipient`.
#[event(bytemuck)]
#[repr(C)]
pub struct MarketCloseRecordV0 {
    pub market: Address,
    pub authority: Address,
    pub rent_recipient: Address,
    pub ts: i64,
}

/// `set_crank_conditions_v0` registered the resolver program for each of the
/// book's four conditions. A zero program deactivated that condition.
#[event(bytemuck)]
#[repr(C)]
pub struct CrankConditionsRecordV0 {
    pub market: Address,
    pub place_authority: Address,
    pub expiry_program: Address,
    pub activation_program: Address,
    pub capacity_program: Address,
    pub cross_program: Address,
    pub ts: i64,
    /// Accounts the registered resolvers take.
    pub account_count: u32,
    pub _pad: [u8; 4],
}
