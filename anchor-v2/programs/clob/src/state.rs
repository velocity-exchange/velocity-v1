//! CLOB market state: node arena + free list + two best-first sorted
//! intrusive doubly-linked lists. Design doc: "PropAMM + Order Flow Design".
//!
//! The market is a [`Slab`]: `[disc][ClobHeaderV0][len][OrderNodeV0 tail]`.
//! Capacity is derived from the account's data length at load, so each
//! market picks its arena size at creation (and can grow via realloc).

use anchor_lang_v2::accounts::Slab;
use anchor_lang_v2::{address_eq, prelude::*};
use static_assertions::const_assert_eq;

use crate::error::ClobError;

/// Null link sentinel. The account zero-inits and 0 is a valid node index,
/// so `initialize` must thread the free list before the book is usable.
pub const NIL: u32 = u32::MAX;

pub const ZERO_ADDRESS: Address = Address::new_from_array([0u8; 32]);

/// Response region size. Responses live in the header (quoter interface:
/// return data carries only a [`ResponsePointerV0`]), so payload size is not
/// bound by the 1024-byte return-data cap.
pub const RESPONSE_BUFFER_BYTES: usize = 8192;

// Hard ceilings on the per-market response/batch config — bound by the
// response region and the 32KB program heap, which don't vary per market.
// The per-market operating points live on the header. Partial execution is
// the interface contract; the router sees smaller balance changes.
pub const QUOTE_LEVELS_CEILING: u16 = ((RESPONSE_BUFFER_BYTES - 4) / 16) as u16;
pub const EXECUTE_FILLS_CEILING: u16 = 128;
pub const EXECUTE_USERS_CEILING: u16 = ((RESPONSE_BUFFER_BYTES - 4) / 48) as u16;

/// Taker direction, as passed through the quoter interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    /// The book side this taker direction consumes.
    pub fn book_side(self) -> Side {
        match self {
            Direction::Long => Side::Ask,
            Direction::Short => Side::Bid,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Direction::Long => 0,
            Direction::Short => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn to_u8(self) -> u8 {
        match self {
            Side::Bid => 0,
            Side::Ask => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrderBitFlag {
    /// Node holds a live order (clear = node is on the free list).
    Open = 1,
    /// Order is an ask (clear = bid).
    Ask = 2,
}

#[account]
pub struct ClobHeaderV0 {
    /// Admin able to configure the market.
    pub authority: Address,
    /// Only signer allowed to place/cancel/execute (the velocity signer PDA;
    /// velocity verifies `User` authority and flow-attestation policy —
    /// including zero-delay activation — before CPI'ing here).
    pub place_authority: Address,
    /// Prices must be a multiple of this (PRICE_PRECISION). Enforced at
    /// placement, not baked into the stored representation, so it can be
    /// changed without repricing the resting book.
    pub order_tick_size: u64,
    /// Sizes must be a multiple of this (base precision).
    pub order_step_size: u64,
    /// Floor on order size so every resting order has real capital at risk.
    pub min_order_size: u64,
    /// Base units per whole unit (velocity perps: 1e9; spot varies).
    /// Immutable after init — resting order sizes are denominated in it.
    pub base_precision: u64,
    /// Starts at 1 so a zeroed (freed) node can never match a live order id.
    pub next_order_id: u64,
    pub best_bid: u32,
    pub best_ask: u32,
    /// List tails, so eviction of the worst order is O(1).
    pub worst_bid: u32,
    pub worst_ask: u32,
    pub free_head: u32,
    pub free_count: u32,
    pub bid_count: u32,
    pub ask_count: u32,
    /// Default taker speed bump: slots added to the placement slot to get
    /// `activation_slot` when the caller doesn't choose a delay.
    pub default_activation_delay_slots: u32,
    /// Upper bound on a caller-chosen activation delay (auction flow).
    pub max_activation_delay_slots: u32,
    /// Fills race the tx's fixed account set: quote/execute take the set of
    /// users the caller can settle, and an order whose user is absent is
    /// skipped while younger than this many slots (the keeper couldn't have
    /// known it) but fails the call once older (the keeper is stale).
    pub unknown_user_grace_slots: u32,
    /// Soft cap: `evict_worst` is allowed once a side holds at least this
    /// many orders. Eviction is crank-mediated through velocity (so the
    /// evicted maker's margin aggregates stay exact); the buffer up to the
    /// per-side hard cap is what the crank has to work with.
    pub evict_threshold_per_side: u32,
    /// Velocity perp market index this book serves.
    pub market_index: u16,
    /// Per-market response/batch tuning, each bounded by its `*_CEILING`.
    pub max_quote_levels: u16,
    pub max_execute_fills: u16,
    pub max_execute_users: u16,
    /// Scratch region `quote_v0`/`execute_v0` write their borsh response
    /// into; return data carries a [`ResponsePointerV0`] locating it.
    pub response: [u8; RESPONSE_BUFFER_BYTES],
}

const_assert_eq!(core::mem::size_of::<ClobHeaderV0>(), 8352);

/// The market account: header + order-node tail, capacity from data length.
pub type ClobMarketV0 = Slab<ClobHeaderV0, OrderNodeV0>;

/// Account-data offset of the header's `response` region.
pub const RESPONSE_OFFSET: usize = 8 + core::mem::size_of::<ClobHeaderV0>() - RESPONSE_BUFFER_BYTES;

/// Account-data offset of the order-node tail: `[disc][H][len: u32]` padded
/// to the node's 8-byte alignment.
pub const ORDERS_OFFSET: usize = (8 + core::mem::size_of::<ClobHeaderV0>() + 4).next_multiple_of(8);

/// One arena slot: a live order threaded into a side's price-time list, or a
/// free node threaded into the free list via `next`. The velocity `User` is
/// stored inline (no seat table): user capacity is order capacity, governed
/// by the one eviction rule.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct OrderNodeV0 {
    /// Velocity `User` account fills settle against. Authority over that
    /// account is verified by velocity before it CPIs place/cancel.
    pub user: Address,
    /// PRICE_PRECISION.
    pub price: u64,
    /// Remaining unfilled size, base precision.
    pub base_asset_amount: u64,
    /// First slot at which this order may match, in either direction.
    pub activation_slot: u64,
    /// Timestamp after which the order is expired (0 = good-till-cancelled).
    pub max_ts: i64,
    pub order_id: u64,
    /// Slot the order was placed — age input for the unknown-user grace
    /// check (see `ClobHeaderV0::unknown_user_grace_slots`).
    pub placed_slot: u64,
    /// Toward the best of book; [`NIL`] if head.
    pub prev: u32,
    /// Away from the best of book (or next free node); [`NIL`] if tail.
    pub next: u32,
    pub bit_flags: u8,
    pub padding: [u8; 7],
}

const_assert_eq!(core::mem::size_of::<OrderNodeV0>(), 96);

impl OrderNodeV0 {
    pub fn is_bit_flag_set(&self, flag: OrderBitFlag) -> bool {
        self.bit_flags & flag as u8 != 0
    }

    pub fn side(&self) -> Side {
        if self.is_bit_flag_set(OrderBitFlag::Ask) {
            Side::Ask
        } else {
            Side::Bid
        }
    }

    pub fn is_expired(&self, now: i64) -> bool {
        self.max_ts != 0 && self.max_ts < now
    }

    pub fn is_active(&self, slot: u64) -> bool {
        self.activation_slot <= slot
    }
}

/// Order handle: an O(1) node hint verified against the order id, so a stale
/// hint (node freed/reused) fails closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct OrderRefV0 {
    pub node_index: u32,
    pub order_id: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct PriceLevel {
    pub price: u64,
    pub size: u64,
}

/// One user's share of an executed fill. Mirrors velocity's quoter-interface
/// `UserBalanceChange`.
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UserBalanceChange {
    pub user: Address,
    pub base_size: u64,
    pub quote_size: u64,
    /// Orders of this user fully consumed (and removed) by the fill, by id.
    /// The caller decrements the user's open-order count by the length, and
    /// the ids let it release per-order state it keeps against the book (a
    /// placed trigger slot). Sub-min culls ride the separate `cancelled` vec
    /// because their remainders also need unwinding.
    pub completed_order_ids: Vec<u64>,
}

/// Where in the market account the borsh response was written. Returned via
/// return data by `quote_v0`/`execute_v0`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ResponsePointerV0 {
    pub offset: u32,
    pub len: u32,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct QuoteResponseV0 {
    /// Levels the quoter will fill at, best price first.
    pub levels: Vec<PriceLevel>,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ExecuteResponseV0 {
    pub balance_changes: Vec<UserBalanceChange>,
    /// Sub-min remainders removed by this execute (see
    /// [`CancelledRemainderV0`]).
    pub cancelled: Vec<CancelledRemainderV0>,
}

/// Per-order fill detail. Events carry this; the wire response merges it by
/// user into [`UserBalanceChange`]s.
#[derive(Clone, Copy, Debug)]
pub struct FillDetail {
    pub user: Address,
    pub order_id: u64,
    pub price: u64,
    pub base_size: u64,
    pub quote_size: u64,
}

/// A removed order, for events (cancel/evict/expire).
#[derive(Clone, Copy, Debug)]
pub struct RemovedOrder {
    pub user: Address,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: Side,
}

pub struct ExecuteOutcome {
    /// Per-order detail, for events.
    pub fills: Vec<FillDetail>,
    /// Fills merged by user — the wire response.
    pub balance_changes: Vec<UserBalanceChange>,
    /// Post-fill remainders below `min_order_size`, culled.
    pub cancelled: Vec<RemovedOrder>,
}

/// Wire form of a removed order — return data of cancel/evict/expire, so
/// velocity can decrement the maker's open-order aggregates. `side` tells
/// velocity whether the remaining size unwinds `open_bids` or `open_asks`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct RemovedOrderV0 {
    pub user: Address,
    pub order_id: u64,
    pub price: u64,
    pub base_asset_amount: u64,
    pub side: Side,
}

/// A sub-`min_order_size` remainder culled during execute, on the wire so
/// velocity decrements the maker's aggregates (the maker was just filled,
/// so their `User` is always in the loaded set).
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelledRemainderV0 {
    pub user: Address,
    pub order_id: u64,
    pub base_asset_amount: u64,
}

/// `activation_slot` is computed by the instruction handler: placement slot
/// plus the default delay, or a chosen delay clamped to
/// `max_activation_delay_slots`. Zero-delay (attested-flow) placement is
/// velocity policy — the CLOB trusts its `place_authority`.
#[derive(Clone, Copy, Debug)]
pub struct PlaceOrderParams {
    pub side: Side,
    pub price: u64,
    pub base_asset_amount: u64,
    pub user: Address,
    pub activation_slot: u64,
    pub placed_slot: u64,
    pub max_ts: i64,
}

/// Per-market configuration, set at init (also the init wire args).
/// `base_precision` and `market_index` are immutable afterwards; the rest
/// are updatable via `update_market_v0`.
#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct MarketConfigV0 {
    pub market_index: u16,
    pub base_precision: u64,
    pub order_tick_size: u64,
    pub order_step_size: u64,
    pub min_order_size: u64,
    pub default_activation_delay_slots: u32,
    pub max_activation_delay_slots: u32,
    pub unknown_user_grace_slots: u32,
    pub evict_threshold_per_side: u32,
    pub max_quote_levels: u16,
    pub max_execute_fills: u16,
    pub max_execute_users: u16,
}

/// Book operations over the market slab. A trait because inherent impls
/// aren't allowed on the foreign `Slab` type.
pub trait ClobBook {
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()>;
    fn place(&mut self, params: PlaceOrderParams) -> Result<OrderRefV0>;
    fn cancel(&mut self, user: Address, order_ref: OrderRefV0) -> Result<RemovedOrder>;
    fn evict_worst(&mut self, side: Side) -> Result<RemovedOrder>;
    fn remove_expired(&mut self, order_ref: OrderRefV0, now: i64) -> Result<RemovedOrder>;
    fn quote(
        &self,
        direction: Direction,
        size: u64,
        users: Option<&[Address]>,
        taker: Option<&Address>,
        slot: u64,
        now: i64,
    ) -> Result<Vec<PriceLevel>>;
    fn execute(
        &mut self,
        direction: Direction,
        size: u64,
        users: Option<&[Address]>,
        taker: Option<&Address>,
        slot: u64,
        now: i64,
    ) -> Result<ExecuteOutcome>;
    fn write_response(&mut self, data: &[u8]) -> Result<ResponsePointerV0>;
    fn grow_free_list(&mut self) -> Result<()>;
}

impl ClobBook for ClobMarketV0 {
    /// Exhaustive header destructure (the zero-copy `set_inner`): adding a
    /// header field without initializing it here is a compile error. Then
    /// fill the tail to capacity, threading the free list.
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()> {
        let cap = self.capacity() as u32;
        require!(cap >= 2, ClobError::InvalidCapacity);
        require!(config.base_precision != 0, ClobError::InvalidConfig);
        require!(
            config.default_activation_delay_slots <= config.max_activation_delay_slots,
            ClobError::InvalidConfig
        );
        require!(
            config.max_quote_levels != 0 && config.max_quote_levels <= QUOTE_LEVELS_CEILING,
            ClobError::InvalidConfig
        );
        require!(
            config.max_execute_fills != 0 && config.max_execute_fills <= EXECUTE_FILLS_CEILING,
            ClobError::InvalidConfig
        );
        require!(
            config.max_execute_users != 0 && config.max_execute_users <= EXECUTE_USERS_CEILING,
            ClobError::InvalidConfig
        );
        let ClobHeaderV0 {
            authority,
            place_authority,
            order_tick_size,
            order_step_size,
            min_order_size,
            base_precision,
            next_order_id,
            best_bid,
            best_ask,
            worst_bid,
            worst_ask,
            free_head,
            free_count,
            bid_count,
            ask_count,
            default_activation_delay_slots,
            max_activation_delay_slots,
            unknown_user_grace_slots,
            evict_threshold_per_side,
            market_index,
            max_quote_levels,
            max_execute_fills,
            max_execute_users,
            response,
        } = &mut **self;
        *authority = new_authority;
        *place_authority = new_place_authority;
        *order_tick_size = config.order_tick_size;
        *order_step_size = config.order_step_size;
        *min_order_size = config.min_order_size;
        *base_precision = config.base_precision;
        *next_order_id = 1;
        *best_bid = NIL;
        *best_ask = NIL;
        *worst_bid = NIL;
        *worst_ask = NIL;
        *bid_count = 0;
        *ask_count = 0;
        *default_activation_delay_slots = config.default_activation_delay_slots;
        *max_activation_delay_slots = config.max_activation_delay_slots;
        *unknown_user_grace_slots = config.unknown_user_grace_slots;
        *evict_threshold_per_side = config.evict_threshold_per_side;
        *market_index = config.market_index;
        *max_quote_levels = config.max_quote_levels;
        *max_execute_fills = config.max_execute_fills;
        *max_execute_users = config.max_execute_users;
        *response = [0; RESPONSE_BUFFER_BYTES];

        *free_head = 0;
        *free_count = cap;
        for i in 0..cap {
            let mut node: OrderNodeV0 = bytemuck::Zeroable::zeroed();
            node.next = if i + 1 == cap { NIL } else { i + 1 };
            self.try_push(node)
                .map_err(|_| ClobError::InvalidOrderParams)?;
        }
        Ok(())
    }

    /// Insert with price-time priority: walk from the best of book past every
    /// order at an equal-or-better price, so the new order queues behind its
    /// own level.
    ///
    /// A full side (half the arena each) rejects every placement, even
    /// better-priced ones: eviction is crank-mediated through velocity (see
    /// [`Self::evict_worst`]) so the evicted maker's margin aggregates stay
    /// exact, and the soft-cap buffer exists so the hard cap is an ops
    /// failure, not a normal state.
    fn place(&mut self, params: PlaceOrderParams) -> Result<OrderRefV0> {
        let PlaceOrderParams {
            side,
            price,
            base_asset_amount,
            user,
            activation_slot,
            placed_slot,
            max_ts,
        } = params;
        require!(
            price != 0 && base_asset_amount != 0 && !address_eq(&user, &ZERO_ADDRESS),
            ClobError::InvalidOrderParams
        );
        require!(
            base_asset_amount >= self.min_order_size,
            ClobError::OrderTooSmall
        );
        require!(
            price % self.order_tick_size.max(1) == 0,
            ClobError::PriceNotTickAligned
        );
        require!(
            base_asset_amount % self.order_step_size.max(1) == 0,
            ClobError::SizeNotStepAligned
        );

        let per_side = (self.capacity() / 2) as u32;
        let side_count = match side {
            Side::Bid => self.bid_count,
            Side::Ask => self.ask_count,
        };
        require!(side_count < per_side, ClobError::SideAtCapacity);

        let mut prev = NIL;
        let mut cursor = match side {
            Side::Bid => self.best_bid,
            Side::Ask => self.best_ask,
        };
        while cursor != NIL {
            let node = &self[cursor as usize];
            let worse = match side {
                Side::Bid => node.price < price,
                Side::Ask => node.price > price,
            };
            if worse {
                break;
            }
            prev = cursor;
            cursor = node.next;
        }

        // Free node guaranteed: both sides below capacity leaves free arena.
        let index = alloc_node(self);
        let order_id = self.next_order_id;
        self.next_order_id += 1;
        let mut bit_flags = OrderBitFlag::Open as u8;
        if side == Side::Ask {
            bit_flags |= OrderBitFlag::Ask as u8;
        }
        self[index as usize] = OrderNodeV0 {
            user,
            price,
            base_asset_amount,
            activation_slot,
            placed_slot,
            max_ts,
            order_id,
            prev,
            next: cursor,
            bit_flags,
            padding: [0; 7],
        };

        if prev != NIL {
            self[prev as usize].next = index;
        } else {
            match side {
                Side::Bid => self.best_bid = index,
                Side::Ask => self.best_ask = index,
            }
        }
        if cursor != NIL {
            self[cursor as usize].prev = index;
        } else {
            match side {
                Side::Bid => self.worst_bid = index,
                Side::Ask => self.worst_ask = index,
            }
        }
        match side {
            Side::Bid => self.bid_count += 1,
            Side::Ask => self.ask_count += 1,
        }

        Ok(OrderRefV0 {
            node_index: index,
            order_id,
        })
    }

    /// Fails closed on a stale hint: node out of range, free, or holding a
    /// different order. `user` must own the order.
    fn cancel(&mut self, user: Address, order_ref: OrderRefV0) -> Result<RemovedOrder> {
        let node = self
            .get(order_ref.node_index as usize)
            .ok_or(ClobError::StaleOrderRef)?;
        require!(
            node.is_bit_flag_set(OrderBitFlag::Open) && node.order_id == order_ref.order_id,
            ClobError::StaleOrderRef
        );
        require!(address_eq(&node.user, &user), ClobError::OrderUserMismatch);
        let removed = RemovedOrder {
            user: node.user,
            order_id: node.order_id,
            price: node.price,
            base_asset_amount: node.base_asset_amount,
            side: node.side(),
        };
        remove_order(self, order_ref.node_index);
        Ok(removed)
    }

    /// Crank-mediated eviction: only the side's tail (worst price, youngest
    /// there), and only while the side holds at least
    /// `evict_threshold_per_side` orders — the crank works the soft-cap
    /// buffer down so placements never hit the hard cap. Velocity is the
    /// caller and loads the evicted maker's `User`, so aggregates stay exact.
    fn evict_worst(&mut self, side: Side) -> Result<RemovedOrder> {
        let count = match side {
            Side::Bid => self.bid_count,
            Side::Ask => self.ask_count,
        };
        require!(
            count > 0 && count >= self.evict_threshold_per_side,
            ClobError::BelowEvictThreshold
        );
        let tail = match side {
            Side::Bid => self.worst_bid,
            Side::Ask => self.worst_ask,
        };
        let node = &self[tail as usize];
        let removed = RemovedOrder {
            user: node.user,
            order_id: node.order_id,
            price: node.price,
            base_asset_amount: node.base_asset_amount,
            side: node.side(),
        };
        remove_order(self, tail);
        Ok(removed)
    }

    /// Crank-mediated expiry reclamation (execute only skips expired orders;
    /// removal without the maker's `User` loaded is exactly the aggregate
    /// leak this design eliminates). Fails closed on a stale hint.
    fn remove_expired(&mut self, order_ref: OrderRefV0, now: i64) -> Result<RemovedOrder> {
        let node = self
            .get(order_ref.node_index as usize)
            .ok_or(ClobError::StaleOrderRef)?;
        require!(
            node.is_bit_flag_set(OrderBitFlag::Open) && node.order_id == order_ref.order_id,
            ClobError::StaleOrderRef
        );
        require!(node.is_expired(now), ClobError::OrderNotExpired);
        let removed = RemovedOrder {
            user: node.user,
            order_id: node.order_id,
            price: node.price,
            base_asset_amount: node.base_asset_amount,
            side: node.side(),
        };
        remove_order(self, order_ref.node_index);
        Ok(removed)
    }

    /// Aggregate the levels a taker of `direction`/`size` would clear,
    /// best-first, capped at [`MAX_QUOTE_LEVELS`]. Skips expired orders,
    /// orders still inside their activation delay, and the taker's own
    /// orders (self-trade prevention — same rule as [`Self::execute`]).
    /// Applies the same unknown-user grace rule as execute so the router's
    /// split math matches what execute will deliver.
    fn quote(
        &self,
        direction: Direction,
        size: u64,
        users: Option<&[Address]>,
        taker: Option<&Address>,
        slot: u64,
        now: i64,
    ) -> Result<Vec<PriceLevel>> {
        let max_levels = self.max_quote_levels.min(QUOTE_LEVELS_CEILING) as usize;
        let mut levels: Vec<PriceLevel> = Vec::with_capacity(max_levels);
        let mut remaining = size;
        let mut cursor = match direction.book_side() {
            Side::Bid => self.best_bid,
            Side::Ask => self.best_ask,
        };
        while cursor != NIL && remaining > 0 {
            let node = &self[cursor as usize];
            cursor = node.next;
            if node.is_expired(now) || !node.is_active(slot) {
                continue;
            }
            if taker.is_some_and(|t| address_eq(t, &node.user)) {
                continue;
            }
            if skip_unknown_user(users, node, self.unknown_user_grace_slots, slot)? {
                continue;
            }
            let take = remaining.min(node.base_asset_amount);
            match levels.last_mut() {
                Some(level) if level.price == node.price => {
                    level.size += take;
                }
                _ => {
                    if levels.len() == max_levels {
                        break;
                    }
                    levels.push(PriceLevel {
                        price: node.price,
                        size: take,
                    });
                }
            }
            remaining -= take;
        }
        Ok(levels)
    }

    /// Consume matchable orders best-first, removing filled orders and
    /// reporting each maker's share for velocity to apply. Expired orders
    /// are skipped, never removed here — reclamation goes through
    /// [`Self::remove_expired`] so the maker's aggregates update. A partial
    /// fill that leaves a remainder below `min_order_size` culls the order
    /// (dust can't hold an arena slot); the cull rides the wire response
    /// since that maker was just filled and is therefore loaded. Orders
    /// whose user is outside the caller's set are skipped inside the grace
    /// window, and fail the call past it (see [`skip_unknown_user`]); the
    /// taker's own orders are skipped unconditionally (self-trade
    /// prevention). No price bound: the router already chose this quoter's
    /// allocation from its quote.
    fn execute(
        &mut self,
        direction: Direction,
        size: u64,
        users: Option<&[Address]>,
        taker: Option<&Address>,
        slot: u64,
        now: i64,
    ) -> Result<ExecuteOutcome> {
        let max_fills = self.max_execute_fills.min(EXECUTE_FILLS_CEILING) as usize;
        let max_users = self.max_execute_users.min(EXECUTE_USERS_CEILING) as usize;
        let mut fills: Vec<FillDetail> = Vec::with_capacity(max_fills);
        let mut balance_changes: Vec<UserBalanceChange> = Vec::with_capacity(max_users);
        // A partial fill only happens when `remaining` runs out, so at most
        // one cull per execute.
        let mut cancelled: Vec<RemovedOrder> = Vec::new();
        let mut remaining = size;
        let mut cursor = match direction.book_side() {
            Side::Bid => self.best_bid,
            Side::Ask => self.best_ask,
        };
        while cursor != NIL && remaining > 0 && fills.len() < max_fills {
            let index = cursor;
            let node = self[index as usize];
            cursor = node.next;
            if node.is_expired(now) {
                continue;
            }
            if !node.is_active(slot) {
                continue;
            }
            if taker.is_some_and(|t| address_eq(t, &node.user)) {
                continue;
            }
            if skip_unknown_user(users, &node, self.unknown_user_grace_slots, slot)? {
                continue;
            }
            let existing = balance_changes
                .iter()
                .position(|c| address_eq(&c.user, &node.user));
            if existing.is_none() && balance_changes.len() == max_users {
                break;
            }
            let take = remaining.min(node.base_asset_amount);
            let quote_size: u64 = (node.price as u128)
                .checked_mul(take as u128)
                .ok_or(ClobError::MathError)?
                .checked_div(self.base_precision.max(1) as u128)
                .ok_or(ClobError::MathError)?
                .try_into()
                .map_err(|_| ClobError::MathError)?;
            let change_index = match existing {
                Some(i) => {
                    let change = &mut balance_changes[i];
                    change.base_size = change
                        .base_size
                        .checked_add(take)
                        .ok_or(ClobError::MathError)?;
                    change.quote_size = change
                        .quote_size
                        .checked_add(quote_size)
                        .ok_or(ClobError::MathError)?;
                    i
                }
                None => {
                    balance_changes.push(UserBalanceChange {
                        user: node.user,
                        base_size: take,
                        quote_size,
                        completed_order_ids: Vec::new(),
                    });
                    balance_changes.len() - 1
                }
            };
            fills.push(FillDetail {
                user: node.user,
                order_id: node.order_id,
                price: node.price,
                base_size: take,
                quote_size,
            });
            if take == node.base_asset_amount {
                balance_changes[change_index]
                    .completed_order_ids
                    .push(node.order_id);
                remove_order(self, index);
            } else {
                let remainder = node.base_asset_amount - take;
                if remainder < self.min_order_size {
                    cancelled.push(RemovedOrder {
                        user: node.user,
                        order_id: node.order_id,
                        price: node.price,
                        base_asset_amount: remainder,
                        side: node.side(),
                    });
                    remove_order(self, index);
                } else {
                    self[index as usize].base_asset_amount = remainder;
                }
            }
            remaining -= take;
        }
        Ok(ExecuteOutcome {
            fills,
            balance_changes,
            cancelled,
        })
    }

    /// Copy serialized response bytes into the header's response region; the
    /// returned pointer (set as instruction return data) locates them.
    fn write_response(&mut self, data: &[u8]) -> Result<ResponsePointerV0> {
        require!(
            data.len() <= RESPONSE_BUFFER_BYTES,
            ClobError::ResponseTooLarge
        );
        self.response[..data.len()].copy_from_slice(data);
        Ok(ResponsePointerV0 {
            offset: RESPONSE_OFFSET as u32,
            len: data.len() as u32,
        })
    }

    /// After a capacity grow: push zeroed nodes for the new slots and thread
    /// them into the free list.
    fn grow_free_list(&mut self) -> Result<()> {
        while !self.is_full() {
            let index = self.len() as u32;
            let mut node: OrderNodeV0 = bytemuck::Zeroable::zeroed();
            node.next = self.free_head;
            self.try_push(node)
                .map_err(|_| ClobError::InvalidCapacity)?;
            self.free_head = index;
            self.free_count += 1;
        }
        Ok(())
    }
}

/// Grace rule for a matchable order whose user is missing from the caller's
/// set: `Ok(true)` (skip) while the order is at most `grace_slots` old — the
/// keeper couldn't have known it when the tx's account set was formed — and
/// [`ClobError::StaleUserSet`] once older, because a keeper that misses an
/// aged order is stale (or pruning makers) and the whole fill must not land.
fn skip_unknown_user(
    users: Option<&[Address]>,
    node: &OrderNodeV0,
    grace_slots: u32,
    slot: u64,
) -> Result<bool> {
    let Some(users) = users else {
        return Ok(false);
    };
    if users.iter().any(|u| address_eq(u, &node.user)) {
        return Ok(false);
    }
    require!(
        slot.saturating_sub(node.placed_slot) <= grace_slots as u64,
        ClobError::StaleUserSet
    );
    Ok(true)
}

fn alloc_node(book: &mut ClobMarketV0) -> u32 {
    debug_assert!(book.free_head != NIL);
    let index = book.free_head;
    book.free_head = book[index as usize].next;
    book.free_count -= 1;
    index
}

/// Unlink a live order and push the node onto the free list, zeroed so its
/// old order id can never verify again.
fn remove_order(book: &mut ClobMarketV0, index: u32) {
    let node = book[index as usize];
    if node.prev != NIL {
        book[node.prev as usize].next = node.next;
    } else {
        match node.side() {
            Side::Bid => book.best_bid = node.next,
            Side::Ask => book.best_ask = node.next,
        }
    }
    if node.next != NIL {
        book[node.next as usize].prev = node.prev;
    } else {
        match node.side() {
            Side::Bid => book.worst_bid = node.prev,
            Side::Ask => book.worst_ask = node.prev,
        }
    }
    match node.side() {
        Side::Bid => book.bid_count -= 1,
        Side::Ask => book.ask_count -= 1,
    }

    let mut freed: OrderNodeV0 = bytemuck::Zeroable::zeroed();
    freed.next = book.free_head;
    book[index as usize] = freed;
    book.free_head = index;
    book.free_count += 1;
}
