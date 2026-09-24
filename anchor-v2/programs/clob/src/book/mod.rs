//! The order book. It is a node arena with a free list, plus two best-first
//! sorted intrusive doubly-linked lists, one per side, over the market slab
//! defined in [`crate::state`]. Two more intrusive lists, one per side, thread
//! that side's taker-origin orders in rest order. [`CrossReservation`] is the
//! one reader of those two lists.
//!
//! ## Arena access is centralized
//!
//! Every read or write of an arena slot goes through [`NodeArena`]. The
//! methods are [`NodeArena::read_node`], [`NodeArena::write_node`],
//! [`NodeArena::update_node`], and the two sentinel-tolerant link setters.
//! Each validates the index against the live arena before it touches memory,
//! so a corrupt or hostile link produces [`crate::error::ClobError::NodeIndexOutOfRange`]
//! instead of a read or a write outside the arena. No code in the crate
//! indexes the slab directly. Link changes are confined to [`arena::insert_order`]
//! and [`arena::unlink_order`], the claimant lists included, which is how every
//! removal path maintains those lists without knowing they exist. The one
//! other way a slot is written is `Slab::try_push`. It appends within the tail
//! it owns, and it lays out a fresh or a freshly grown arena.
//!
//! ## Traversal
//!
//! Every walk over a side goes through [`walk_side`], or through
//! [`walk_side_ref`] where the caller holds only `&`. Both validate each hop
//! and refuse to walk further than the arena can hold, so a corrupt list
//! cannot spin forever.
//!
//! `quote` and `execute` ask one [`walk::SweepGate`] what a caller may take of each
//! order, so the ladder a quote publishes ends where the fill would end.
//!
//! ## Invariants
//!
//! Every mutating operation ends by re-checking what it wrote.
//! [`ClobBook::validate_book`] covers the O(1) header and endpoint invariants.
//! Counts sum to capacity, endpoints are live nodes of the right side with
//! null outer links, the free head agrees with the free count, and each
//! claimant list's endpoints are live taker-origin orders of that side. Each
//! operation adds its own postcondition. The exhaustive O(n) version runs in
//! the unit tests after every operation rather than on-chain. It walks every
//! list, checks price ordering, checks that claimant ids ascend, and accounts
//! for every slot.
//!
//! Quote and execute also check what they are about to report. Every level or
//! fill carries a nonzero price and size, and the sequence runs
//! best-price-first for the side. A response that broke either rule would be
//! worth more to the router than the book can honour, because a zero-priced
//! level wins any routing waterfall outright. Such a response fails the
//! instruction instead of shipping. See [`walk::QuoteLadder::write`] and
//! [`walk::check_fill_price`].
//!
//! Neither instruction trades some of the depth. That depth is the units a
//! crossing taker remainder has claimed, and the whole of a remainder that a
//! counterparty crosses. See [`CrossReservation`], which every read of a side
//! asks, so the depth quote publishes is always depth execute can deliver.
//! Both pass over claimed units the way they pass over an expired or a
//! not-yet-activated order, so the rest of the side stays tradeable. Every
//! order still fills at its own stored price. The book neither reprices a
//! cross nor resolves one. It only declines to sell the taker's improvement to
//! whoever gets there first, and it reports the flag on
//! [`crate::state::RemovedOrderV0`] so velocity can settle the cross at the
//! counterparty's price.
//!
//! Velocity also holds execute to its own quote on the way out. The response's
//! total quote must be the notional of these same orders at the prices `quote`
//! published, to within the one unavoidable division. Fills are therefore
//! priced by differencing a running total rather than rounded one at a time.
//! See [`walk::FillPricing`].
//!
//! ## Layout
//!
//! [`arena`] holds arena access, the free list, and the link and unlink steps
//! every mutation goes through. [`walk`] holds the side walks, `quote`,
//! `quote_l3` and `execute`. [`placement`] holds the operations that add, fill
//! and remove single orders. [`budget`] holds the caller's per-user limits,
//! [`reservation`] the claims of taker remainders, and [`hints`] the wake hints
//! the book's crank conditions publish.

mod arena;
mod budget;
mod hints;
mod placement;
mod reservation;
mod walk;

// Only the unit tests walk a side from outside this module.
#[cfg(test)]
pub(crate) use walk::walk_side;
use {
    crate::state::{
        CancelAllOutcomeV0, CancelSidesV0, ClobMarketV0, ClobOrderRefV0, DirectionV0,
        ExecuteOutcome, FilledOrderV0, MarketConfigV0, OrderNodeV0, PlaceOrderParams,
        RemovedOrderV0, ResponsePointerV0, SideV0, UserRefV0,
    },
    anchor_lang::prelude::*,
    quoter_spec::{ExecuteArgsV0, QuoteArgsV0},
};
pub(crate) use {
    arena::{validate_evict_threshold, BookHeader},
    reservation::{evictable_order, CrossReservation},
    walk::{is_live, walk_side_ref, Walk},
};

/// Two conditions that must agree, such as a `NIL` best link and a zero count.
/// Written inline the test reads `(a == b) == (c == d)`, which looks like a typo
/// for `&&`.
#[inline(always)]
const fn both_or_neither(a: bool, b: bool) -> bool {
    a == b
}

/// Book operations over the market slab. This is a trait because Rust does not
/// allow an inherent impl on the foreign `Slab` type.
pub trait ClobBook {
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()>;
    fn place(&mut self, params: PlaceOrderParams) -> Result<ClobOrderRefV0>;
    fn cancel(
        &mut self,
        user: UserRefV0,
        order_ref: ClobOrderRefV0,
        slot: u64,
        force: bool,
    ) -> Result<RemovedOrderV0>;
    fn cancel_all(
        &mut self,
        user: UserRefV0,
        sides: CancelSidesV0,
        slot: u64,
        force: bool,
        removed_ids: &mut dyn FnMut(u32) -> Result<()>,
    ) -> Result<CancelAllOutcomeV0>;
    fn evict_worst(&mut self, side: SideV0, slot: u64) -> Result<RemovedOrderV0>;
    fn remove_expired(&mut self, order_ref: ClobOrderRefV0, now: i64) -> Result<RemovedOrderV0>;
    fn fill(
        &mut self,
        order_ref: ClobOrderRefV0,
        base_asset_amount: u64,
        slot: u64,
        now: i64,
    ) -> Result<FilledOrderV0>;
    fn quote(&mut self, args: &QuoteArgsV0, slot: u64, now: i64) -> Result<ResponsePointerV0>;
    fn quote_l3(
        &mut self,
        direction: DirectionV0,
        size: u64,
        max_rows: u16,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ResponsePointerV0>;
    fn execute(&mut self, args: &ExecuteArgsV0, slot: u64, now: i64) -> Result<ExecuteOutcome>;
    fn grow_free_list(&mut self) -> Result<()>;

    /// Live order count on `side`.
    fn node_count(&self, side: SideV0) -> u32;
    /// Head of `side`: the best-priced, oldest order. [`crate::state::NIL`] when empty.
    fn best(&self, side: SideV0) -> u32;
    /// Tail of `side`: the worst-priced, youngest order there. [`crate::state::NIL`] when
    /// empty. Eviction starts its search here.
    fn worst(&self, side: SideV0) -> u32;
    /// O(1) invariants, re-checked after every mutating operation.
    fn validate_book(&self) -> Result<()>;
}

/// The whole of the program's arena access. Each method validates the index
/// against the live arena length, so no caller can address a slot that is not
/// there. These five methods are the only way to reach the arena.
pub(crate) trait NodeArena {
    /// Copy a node out. The copy, rather than a borrow, lets a traversal
    /// visitor keep mutating the book while it holds the node.
    fn read_node(&self, index: u32) -> Result<OrderNodeV0>;
    fn write_node(&mut self, index: u32, node: OrderNodeV0) -> Result<()>;
    fn update_node(&mut self, index: u32, edit: impl FnOnce(&mut OrderNodeV0)) -> Result<()>;
    /// Point `index`'s successor link at `next`. [`crate::state::NIL`] for `index` means no
    /// such neighbour and does nothing, so a link change does not repeat the
    /// branch at every call site.
    fn set_next(&mut self, index: u32, next: u32) -> Result<()>;
    fn set_prev(&mut self, index: u32, prev: u32) -> Result<()>;
}

/// Each operation lives in the file of its subject. This impl only routes to it.
impl ClobBook for ClobMarketV0 {
    fn initialize(
        &mut self,
        new_authority: Address,
        new_place_authority: Address,
        config: MarketConfigV0,
    ) -> Result<()> {
        arena::initialize(self, new_authority, new_place_authority, config)
    }

    fn place(&mut self, params: PlaceOrderParams) -> Result<ClobOrderRefV0> {
        placement::place(self, params)
    }

    fn cancel(
        &mut self,
        user: UserRefV0,
        order_ref: ClobOrderRefV0,
        slot: u64,
        force: bool,
    ) -> Result<RemovedOrderV0> {
        placement::cancel(self, user, order_ref, slot, force)
    }

    fn cancel_all(
        &mut self,
        user: UserRefV0,
        sides: CancelSidesV0,
        slot: u64,
        force: bool,
        removed_ids: &mut dyn FnMut(u32) -> Result<()>,
    ) -> Result<CancelAllOutcomeV0> {
        placement::cancel_all(self, user, sides, slot, force, removed_ids)
    }

    fn evict_worst(&mut self, side: SideV0, slot: u64) -> Result<RemovedOrderV0> {
        placement::evict_worst(self, side, slot)
    }

    fn remove_expired(&mut self, order_ref: ClobOrderRefV0, now: i64) -> Result<RemovedOrderV0> {
        placement::remove_expired(self, order_ref, now)
    }

    fn fill(
        &mut self,
        order_ref: ClobOrderRefV0,
        base_asset_amount: u64,
        slot: u64,
        now: i64,
    ) -> Result<FilledOrderV0> {
        placement::fill(self, order_ref, base_asset_amount, slot, now)
    }

    fn quote(&mut self, args: &QuoteArgsV0, slot: u64, now: i64) -> Result<ResponsePointerV0> {
        walk::quote(self, args, slot, now)
    }

    fn quote_l3(
        &mut self,
        direction: DirectionV0,
        size: u64,
        max_rows: u16,
        include_taker_origin_reservations: bool,
        slot: u64,
        now: i64,
    ) -> Result<ResponsePointerV0> {
        walk::quote_l3(
            self,
            direction,
            size,
            max_rows,
            include_taker_origin_reservations,
            slot,
            now,
        )
    }

    fn execute(&mut self, args: &ExecuteArgsV0, slot: u64, now: i64) -> Result<ExecuteOutcome> {
        walk::execute(self, args, slot, now)
    }

    fn grow_free_list(&mut self) -> Result<()> {
        arena::grow_free_list(self)
    }

    fn node_count(&self, side: SideV0) -> u32 {
        match side {
            SideV0::Bid => self.bid_count,
            SideV0::Ask => self.ask_count,
        }
    }

    fn best(&self, side: SideV0) -> u32 {
        match side {
            SideV0::Bid => self.best_bid,
            SideV0::Ask => self.best_ask,
        }
    }

    fn worst(&self, side: SideV0) -> u32 {
        match side {
            SideV0::Bid => self.worst_bid,
            SideV0::Ask => self.worst_ask,
        }
    }

    fn validate_book(&self) -> Result<()> {
        arena::validate_book(self)
    }
}
