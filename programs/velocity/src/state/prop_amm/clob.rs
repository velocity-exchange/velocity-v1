//! The velocity-mediated CLOB CPI surface, and what velocity does with its
//! answers.
//!
//! [`ClobMarket`] changes the book and signs as its `place_authority`;
//! [`ClobReader`] only asks. Both speak the wire `clob-wire` declares —
//! nothing here holds a copy of the book account's layout. The registry
//! `execute_v0` leg is deliberately absent: that is the interface every
//! quoter type shares ([`super::QuoterConfigV0::execute`]).

use {
    super::{QuoterConfigV0, QuoterType, CLOB_USER_REF_BYTES},
    crate::{error::ErrorCode, msg, signer::get_clob_authority_seeds, validate},
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke, invoke_signed},
    },
};

/// Upper bound on a CLOB CPI's instruction data: discriminator plus the widest
/// args on that interface, which is `place` — a side, two `u64`s, an optional
/// delay, a timestamp, a user ref, the taker-origin flag and the reduce-only
/// flag.
///
/// Reserved in one shot for the same reason [`quoter_cpi_data_len`] is: a
/// `Vec` that starts at the discriminator and doubles into place leaks every
/// intermediate buffer, and velocity's bump allocator never reclaims. This path
/// runs on every order placed on the book and every removal crank, so it is the
/// busier of the two.
pub const CLOB_CPI_DATA_CAPACITY: usize = 8 + 1 + 8 + 8 + 5 + 8 + CLOB_USER_REF_BYTES + 1 + 1;

/// Order handle on the CLOB: an O(1) node hint verified against the order id
/// there, so a stale hint fails closed on the CLOB side.
pub use clob_wire::ClobOrderRefV0;
/// `place_order_v0` args on the CLOB wire.
///
/// `taker_origin` marks the order as an unfilled taker remainder velocity
/// migrated onto the book rather than a quote its owner chose to post. Only
/// velocity knows that, which is why the CLOB takes it as an argument. It
/// changes two things on the book — the order cannot be taken while a live
/// counterparty crosses it, and a cross involving it settles at the
/// counterparty's price.
pub use clob_wire::PlaceOrderArgsV0 as ClobPlaceOrderArgsV0;
/// Wire form of an order the CLOB removed — return data of its
/// cancel/evict/expire ixs, so velocity can decrement the maker's
/// open-order aggregates by the remaining size on the right side.
///
/// Its `taker_origin` flag is the only place the CLOB reports that a removed
/// order was a migrated taker remainder, which is what tells velocity which
/// side of a cross was demanding liquidity — hence which side's price the
/// match settles at.
pub use clob_wire::RemovedOrderV0 as ClobRemovedOrderV0;
/// `cancel_order_v0` args on the CLOB wire.
pub use clob_wire::{
    CancelOrderArgsV0 as ClobCancelOrderArgsV0, FillArgsV0 as ClobFillArgsV0,
    FillOutcomeV0 as ClobFillOutcomeV0, FillRequestV0 as ClobFillRequestV0,
    FilledOrderV0 as ClobFilledOrderV0,
};
/// Which sides a `cancel_all_v0` withdraws, on the CLOB wire. Declared by
/// `quoter-spec`; the alias keeps velocity's name for it.
pub use quoter_spec::CancelSidesV0 as ClobCancelSides;
/// What the wire's named sides mean to velocity: the maker positions they
/// unwind.
pub trait ClobCancelSidesExt {
    /// The maker position directions the named sides represent — a resting bid
    /// is a long, a resting ask a short. What the aggregate unwind iterates.
    fn directions(self) -> &'static [crate::controller::position::PositionDirection];
    fn includes(self, direction: crate::controller::position::PositionDirection) -> bool;
}

impl ClobCancelSidesExt for ClobCancelSides {
    fn directions(self) -> &'static [crate::controller::position::PositionDirection] {
        use crate::controller::position::PositionDirection;
        match self {
            ClobCancelSides::Bids => &[PositionDirection::Long],
            ClobCancelSides::Asks => &[PositionDirection::Short],
            ClobCancelSides::Both => &[PositionDirection::Long, PositionDirection::Short],
        }
    }

    fn includes(self, direction: crate::controller::position::PositionDirection) -> bool {
        use crate::controller::position::PositionDirection;
        matches!(
            (self, direction),
            (ClobCancelSides::Both, _)
                | (ClobCancelSides::Bids, PositionDirection::Long)
                | (ClobCancelSides::Asks, PositionDirection::Short)
        )
    }
}

/// `cancel_all_v0` args on the CLOB wire.
pub use clob_wire::CancelAllArgsV0 as ClobCancelAllArgsV0;
/// What the CLOB's `cancel_all_v0` withdrew: per-side totals rather than a list
/// of removals, which is exactly the shape the open-order aggregates consume —
/// one `decrease_open_bids_and_asks` per side and one count, however many
/// orders the sweep took.
pub use clob_wire::CancelAllOutcomeV0 as ClobCancelAllOutcomeV0;

/// What velocity reads into the sweep outcome beyond its shape: which side a
/// maker's position rests on. An inherent impl is not available on a type the
/// wire crate owns, and the direction is velocity's own type — the same reason
/// [`ClobCancelSidesExt`] is a trait.
pub trait ClobCancelAllOutcomeExt {
    fn orders(&self) -> u32;
    fn reduce_only_orders(&self) -> u32;
    fn base_for(&self, direction: crate::controller::position::PositionDirection) -> u64;
    fn orders_for(&self, direction: crate::controller::position::PositionDirection) -> u32;
}

impl ClobCancelAllOutcomeExt for ClobCancelAllOutcomeV0 {
    fn orders(&self) -> u32 {
        self.bid_orders.saturating_add(self.ask_orders)
    }

    /// Reduce-only orders the sweep removed, across both sides. The caller
    /// disarms its per-user reduce-only tracking by this many.
    fn reduce_only_orders(&self) -> u32 {
        self.bid_reduce_only_orders
            .saturating_add(self.ask_reduce_only_orders)
    }

    /// Base amount withdrawn on the side a maker position of `direction` rests
    /// on — a bid is a long, an ask a short.
    fn base_for(&self, direction: crate::controller::position::PositionDirection) -> u64 {
        match direction {
            crate::controller::position::PositionDirection::Long => self.bid_base_asset_amount,
            crate::controller::position::PositionDirection::Short => self.ask_base_asset_amount,
        }
    }

    /// Orders withdrawn on that same side.
    fn orders_for(&self, direction: crate::controller::position::PositionDirection) -> u32 {
        match direction {
            crate::controller::position::PositionDirection::Long => self.bid_orders,
            crate::controller::position::PositionDirection::Short => self.ask_orders,
        }
    }
}

/// `evict_worst_v0` args on the CLOB wire.
pub use clob_wire::EvictWorstArgsV0 as ClobEvictWorstArgsV0;
/// `order_rules_v0`'s answer: what the book requires of an order before it
/// will hold one. Asked rather than read, so a rule the book moves is a rule
/// velocity still gets right.
pub use clob_wire::OrderRulesV0 as ClobOrderRulesV0;
/// `remove_expired_v0` args on the CLOB wire.
pub use clob_wire::RemoveExpiredArgsV0 as ClobRemoveExpiredArgsV0;
/// `next_removal_v0` args and answer: which order the book would let a caller
/// reclaim, and why. The book decides both — velocity supplies the removal's
/// consequences, not the search.
pub use clob_wire::{ClobRemovalKindV0, NextRemovalArgsV0 as ClobNextRemovalArgsV0};
/// `set_crank_conditions_v0`: who resolves each of the book's own conditions.
/// The book owns the wakes; velocity registers the answers.
pub use clob_wire::{
    CrankAccountV0 as ClobCrankAccountV0, CrankBlockV0 as ClobCrankBlockV0,
    CrankConditionsArgsV0 as ClobCrankConditionsArgsV0, CrankResolverV0 as ClobCrankResolverV0,
};
/// The one shape every read-only CLOB answer describes an order in, and the
/// two answers built from it: the best matchable order on each side, and one
/// view per requested ref.
pub use clob_wire::{
    NextCrossV0 as ClobNextCrossV0, OrderViewV0 as ClobOrderViewV0,
    OrdersArgsV0 as ClobOrdersArgsV0, OrdersV0 as ClobOrdersV0,
    ORDER_VIEW_CEILING as CLOB_ORDER_VIEW_CEILING,
};
/// Which slot of the book's block each registered resolver lands in. Relay
/// names the fired condition by slot, so one resolver serving several of them
/// reads the mapping from here rather than from the book's own numbering.
pub use clob_wire::{
    CRANK_SLOT_ACTIVATION as CLOB_CRANK_SLOT_ACTIVATION,
    CRANK_SLOT_CAPACITY as CLOB_CRANK_SLOT_CAPACITY, CRANK_SLOT_CROSS as CLOB_CRANK_SLOT_CROSS,
    CRANK_SLOT_EXPIRY as CLOB_CRANK_SLOT_EXPIRY,
};

/// Anchor-default discriminators (`sha256("global:<name>")[..8]`) of the CLOB
/// ixs velocity CPIs directly (place/cancel are velocity-mediated and not part
/// of the registry's quote/execute surface, so they aren't stored per entry).
/// Nothing outside [`ClobMarket`] should reference these — it is the one
/// place that speaks this wire.
pub const CLOB_PLACE_ORDER_V0_DISCRIMINATOR: [u8; 8] = [100, 204, 57, 226, 245, 228, 61, 187];
pub const CLOB_CANCEL_ORDER_V0_DISCRIMINATOR: [u8; 8] = [70, 91, 225, 16, 228, 203, 124, 174];
pub const CLOB_FILL_V0_DISCRIMINATOR: [u8; 8] = [66, 113, 11, 94, 94, 23, 154, 137];
pub const CLOB_CANCEL_ALL_V0_DISCRIMINATOR: [u8; 8] = [212, 11, 203, 11, 184, 40, 88, 95];
pub const CLOB_EVICT_WORST_V0_DISCRIMINATOR: [u8; 8] = [106, 60, 27, 129, 80, 27, 37, 73];
pub const CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR: [u8; 8] = [241, 135, 215, 18, 254, 107, 179, 119];
pub const CLOB_NEXT_REMOVAL_V0_DISCRIMINATOR: [u8; 8] = [132, 65, 9, 126, 135, 115, 177, 92];
pub const CLOB_NEXT_CROSS_V0_DISCRIMINATOR: [u8; 8] = [234, 191, 102, 36, 183, 233, 127, 48];
pub const CLOB_SET_CRANK_CONDITIONS_V0_DISCRIMINATOR: [u8; 8] = [34, 160, 120, 93, 84, 133, 8, 95];
pub const CLOB_ORDERS_V0_DISCRIMINATOR: [u8; 8] = [124, 117, 208, 33, 202, 209, 58, 199];
pub const CLOB_ORDER_RULES_V0_DISCRIMINATOR: [u8; 8] = [201, 129, 212, 105, 18, 69, 149, 252];

/// The velocity-mediated CLOB CPI surface, bound to one book: the three
/// accounts every call takes plus the signer nonce that lets velocity sign as
/// the book's `place_authority`.
///
/// This and [`ClobReader`] are the only places in the program that speak the
/// CLOB's wire — the discriminators above, the borsh arg encoding, the
/// `invoke_signed` with the fixed `[market (w), clob_authority (s)]` account
/// pair, and the return-data decode (writer-checked, so a program the CLOB
/// CPI'd into can't spoof the response). Every caller — placement, cancel,
/// the evict/expire cranks, force-cancel — goes through a method here. The
/// split is signing: this half changes the book and signs as its
/// `place_authority`; the reader only asks, and signs nothing.
///
/// Velocity speaks *only* this wire. It holds no copy of the market account's
/// layout, so what the book has to answer is what these methods ask for, and
/// where a field sits inside its account is the book's own business.
///
/// `execute_v0` is deliberately absent: that leg is the *registry* wire every
/// quoter type shares ([`QuoterConfigV0::execute`] → `invoke_quoter`), whose
/// account list is per-entry registered rather than this fixed pair, and it
/// already has exactly one implementation. `quote_v0` and `quote_l3_v0` are
/// absent for the same reason — the book answers for its depth on the
/// interface every source shares.
pub struct ClobMarket<'a, 'info> {
    /// The book account, passed writable.
    pub market: &'a AccountInfo<'info>,
    /// The registered CLOB program.
    pub program: &'a AccountInfo<'info>,
    /// The CLOB place authority PDA — what a book's `place_authority` is set
    /// to. The CLOB gates place/cancel/evict/expire *and* `execute_v0` on that
    /// one field, so this leg and the registry's execute leg for a CLOB entry
    /// necessarily sign as the same key.
    ///
    /// It is its own key rather than the one a third-party quoter is handed.
    /// Signer privilege is inherited by a callee, and this key may place and
    /// cancel on any book for *any* user (`place_order_v0` takes the user as an
    /// argument), so a quoter that received it and also held a book in its
    /// account list could rest unreserved orders on that book or wipe it.
    pub clob_authority: &'a AccountInfo<'info>,
}

impl<'a, 'info> ClobMarket<'a, 'info> {
    /// Bind to the book a CLOB registry entry names. Checks what every
    /// caller needs: the entry is a CLOB, it serves `market_index`, and this
    /// book is one of the accounts the admin vetted onto the entry (so a
    /// caller can't point a valid entry at an arbitrary account it owns).
    ///
    /// Deliberately *not* gated on `is_active`/`is_approved`: those mean
    /// "may take new flow", and the removal paths must keep working on a
    /// killed or de-listed book. Placement applies that gate itself.
    pub fn from_quoter(
        quoter: &QuoterConfigV0,
        market_index: u16,
        market: &'a AccountInfo<'info>,
        program: &'a AccountInfo<'info>,
        clob_authority: &'a AccountInfo<'info>,
    ) -> Result<Self> {
        quoter.validate_clob_book(market_index, &market.key())?;
        Ok(Self {
            market,
            program,
            clob_authority,
        })
    }

    /// Rest a new order on the book; returns the CLOB's handle for it.
    pub fn place(&self, args: ClobPlaceOrderArgsV0) -> Result<ClobOrderRefV0> {
        self.invoke(&CLOB_PLACE_ORDER_V0_DISCRIMINATOR, &args, "place")
    }

    /// Report fills velocity made against taker remainders resting on this
    /// book, so they shrink in place.
    ///
    /// The mirror of [`Self::execute`]. That one is the book filling its own
    /// orders for a taker; this is velocity telling the book about a fill it
    /// could not have made itself, because a taker remainder aggresses against
    /// quoters and the vAMM as well as against the book.
    ///
    /// In place, so the order keeps its queue position and its id. Cancelling
    /// and re-placing would send a partly-filled remainder to the back of its
    /// own level for a fill that never changed its price.
    pub fn fill(&self, args: ClobFillArgsV0) -> Result<ClobFillOutcomeV0> {
        self.invoke(&CLOB_FILL_V0_DISCRIMINATOR, &args, "fill")
    }

    /// Pull one order off the book; returns what was removed so the caller
    /// can unwind the maker's aggregates by the remaining size.
    pub fn cancel(&self, args: ClobCancelOrderArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(&CLOB_CANCEL_ORDER_V0_DISCRIMINATOR, &args, "cancel")
    }

    /// Pull every order this user holds on the named sides in one CPI; returns
    /// the per-side totals to unwind their aggregates by. The CLOB caps how many
    /// it takes per call and says so in
    /// [`ClobCancelAllOutcomeV0::exhaustive`] — the totals always describe
    /// exactly what that call removed, so repeating it is safe.
    pub fn cancel_all(&self, args: ClobCancelAllArgsV0) -> Result<ClobCancelAllOutcomeV0> {
        self.invoke(&CLOB_CANCEL_ALL_V0_DISCRIMINATOR, &args, "cancel all")
    }

    /// Reclaim the hinted expired order (the CLOB re-checks that it is due).
    pub fn remove_expired(&self, args: ClobRemoveExpiredArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(
            &CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR,
            &args,
            "remove expired",
        )
    }

    /// Register who resolves the book's own crank conditions. Returns the
    /// account offset the block sits at, which is what a relay watch
    /// registration points at — asked for rather than derived, so velocity
    /// never has to know the market account's layout.
    pub fn set_crank_conditions(
        &self,
        args: ClobCrankConditionsArgsV0,
    ) -> Result<ClobCrankBlockV0> {
        self.invoke(
            &CLOB_SET_CRANK_CONDITIONS_V0_DISCRIMINATOR,
            &args,
            "set crank conditions",
        )
    }

    /// Reclaim the worst order on a side past its soft cap (the CLOB
    /// re-checks the threshold).
    pub fn evict(&self, args: ClobEvictWorstArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(&CLOB_EVICT_WORST_V0_DISCRIMINATOR, &args, "evict")
    }

    /// The read-only half of the same wire, for the questions this call site
    /// asks the book about its own memory rather than telling it to change.
    pub fn reader(&self) -> ClobReader<'a, 'info> {
        ClobReader {
            market: self.market,
            program: self.program,
        }
    }

    /// One CPI: `discriminator ++ borsh(args)` to the book as its
    /// `place_authority`, then decode the response the CLOB left as return
    /// data. `what` only names the call in error messages.
    fn invoke<A: AnchorSerialize, R: AnchorDeserialize>(
        &self,
        discriminator: &[u8; 8],
        args: &A,
        what: &str,
    ) -> Result<R> {
        let mut data = Vec::with_capacity(CLOB_CPI_DATA_CAPACITY);
        data.extend_from_slice(discriminator);
        args.serialize(&mut data).map_err(|_| {
            msg!("failed to serialize clob {} args", what);
            ErrorCode::DefaultError
        })?;
        invoke_signed(
            &Instruction {
                program_id: self.program.key(),
                accounts: vec![
                    AccountMeta::new(self.market.key(), false),
                    AccountMeta::new_readonly(self.clob_authority.key(), true),
                ],
                data,
            },
            &[
                self.market.clone(),
                self.clob_authority.clone(),
                self.program.clone(),
            ],
            &[&get_clob_authority_seeds(
                &crate::signer::CLOB_AUTHORITY_NONCE,
            )],
        )?;

        clob_response(&self.program.key(), what)
    }
}

/// Decode what a CLOB call left as return data.
///
/// Return data is last-writer-wins within the transaction, so the writer must
/// be the book's own program: otherwise a program the CLOB CPI'd into could
/// dictate the response velocity settles on. `what` only names the call in
/// error messages.
fn clob_response<R: AnchorDeserialize>(program: &Pubkey, what: &str) -> Result<R> {
    let (writer, response) = get_return_data().ok_or_else(|| -> Error {
        msg!("clob {} returned no response", what);
        ErrorCode::DefaultError.into()
    })?;
    validate!(
        writer == *program,
        ErrorCode::DefaultError,
        "clob {} return data written by {} instead of the book's program",
        what,
        writer
    )?;
    R::deserialize(&mut response.as_slice()).map_err(|_| {
        msg!("clob {} returned an undecodable response", what);
        ErrorCode::DefaultError.into()
    })
}

/// The read-only leg of the CLOB wire: ask the book what removal work it has.
///
/// Separate from [`ClobMarket`] because it signs nothing. A crank resolver
/// runs under simulation and holds no quoter signer, and the question it asks
/// — which order is expired, which order is past the eviction threshold — is
/// one the book answers about its own memory. Velocity supplies a removal's
/// consequences, not the search for it.
pub struct ClobReader<'a, 'info> {
    pub market: &'a AccountInfo<'info>,
    pub program: &'a AccountInfo<'info>,
}

impl ClobReader<'_, '_> {
    /// The next order of `args.kind` the book would let a caller reclaim, or
    /// [`ClobOrderViewV0::NONE`] when it has none.
    pub fn next_removal(&self, args: ClobNextRemovalArgsV0) -> Result<ClobOrderViewV0> {
        let mut data = Vec::with_capacity(CLOB_CPI_DATA_CAPACITY);
        data.extend_from_slice(&CLOB_NEXT_REMOVAL_V0_DISCRIMINATOR);
        args.serialize(&mut data).map_err(|_| {
            msg!("failed to serialize clob next removal args");
            ErrorCode::DefaultError
        })?;
        self.ask(data, "next removal")
    }

    /// What the book requires of an order before it will hold one. Asked
    /// rather than read off the market account, so a rule the book moves is a
    /// rule velocity still gets right.
    pub fn order_rules(&self) -> Result<ClobOrderRulesV0> {
        self.ask(CLOB_ORDER_RULES_V0_DISCRIMINATOR.to_vec(), "order rules")
    }

    /// What the book holds for each of `refs`, in the order given. A ref that
    /// no longer names a live order comes back as [`ClobOrderViewV0::NONE`] —
    /// a fill or a crank got there first, which is the expected outcome of
    /// the race rather than an error.
    pub fn orders(&self, refs: Vec<ClobOrderRefV0>) -> Result<Vec<ClobOrderViewV0>> {
        let mut data = Vec::with_capacity(CLOB_CPI_DATA_CAPACITY);
        data.extend_from_slice(&CLOB_ORDERS_V0_DISCRIMINATOR);
        ClobOrdersArgsV0 { refs }
            .serialize(&mut data)
            .map_err(|_| {
                msg!("failed to serialize clob orders args");
                ErrorCode::DefaultError
            })?;
        self.ask::<ClobOrdersV0>(data, "orders")
            .map(|answer| answer.orders)
    }

    /// One read-only CPI, unsigned: the book answers about its own memory, so
    /// nothing here needs the quoter signer.
    fn ask<R: AnchorDeserialize>(&self, data: Vec<u8>, what: &str) -> Result<R> {
        invoke(
            &Instruction {
                program_id: self.program.key(),
                accounts: vec![AccountMeta::new_readonly(self.market.key(), false)],
                data,
            },
            &[self.market.clone(), self.program.clone()],
        )?;
        clob_response(&self.program.key(), what)
    }
}

/// Unwind one user's reserve for everything a bulk sweep took, and report how
/// many orders that was.
///
/// One `decrease_open_bids_and_asks` per side by the summed base the sweep
/// reported: identical arithmetic to N per-order unwinds — each placement
/// reserved its own amount, so the sum can never exceed what is reserved — at
/// a cost that does not grow with the count. Both directions regardless of
/// which sides were asked for, so the reserve moves by exactly what left the
/// book rather than by what the caller intended to take.
pub fn unwind_swept_orders(
    user: &mut crate::state::user::User,
    clob: &ClobReader,
    market_index: u16,
    sides: ClobCancelSides,
    swept: &ClobCancelAllOutcomeV0,
) -> Result<u32> {
    use crate::controller::position::{
        decrease_open_bids_and_asks, get_position_index, PositionDirection,
    };
    let position_index = get_position_index(&user.perp_positions, market_index)?;
    for direction in [PositionDirection::Long, PositionDirection::Short] {
        decrease_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &direction,
            swept.base_for(direction),
            true,
        )?;
    }
    let orders = swept.orders();
    user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
        .open_orders
        .saturating_sub(orders.min(u8::MAX as u32) as u8);
    // The sweep took this many reduce-only orders off the book, so disarm the
    // counter by the same amount.
    user.perp_positions[position_index]
        .disarm_reduce_only_clob_by(swept.reduce_only_orders().min(u16::MAX as u32) as u16);
    (0..orders).for_each(|_| user.decrement_open_orders(false));
    release_swept_trigger_shadows(user, clob, market_index, sides)?;
    Ok(orders)
}

/// Free the placed-trigger shadows on `sides` whose live orders a bulk sweep
/// took, and report how many were freed.
///
/// Driven off what the *book* still holds rather than off a list of removed
/// ids: a shadow is released exactly when its ref no longer names its order,
/// which is true whether the sweep was capped or not, and is strictly more
/// robust than matching ids — a shadow whose order left the book by any route
/// reads as released here. The scan is over `User.orders`, so it is bounded by
/// that array, not by the book.
///
/// Asked in chunks because the book answers about
/// [`CLOB_ORDER_VIEW_CEILING`] refs per call, and a user may hold more
/// shadows than that.
pub fn release_swept_trigger_shadows(
    user: &mut crate::state::user::User,
    clob: &ClobReader,
    market_index: u16,
    sides: ClobCancelSides,
) -> Result<usize> {
    use crate::state::user::{MarketType, OrderStatus};
    let candidates: Vec<(usize, ClobOrderRefV0)> = user
        .orders
        .iter()
        .enumerate()
        .filter(|(_, order)| {
            order.status == OrderStatus::Open
                && order.is_placed_on_clob()
                && order.market_type == MarketType::Perp
                && order.market_index == market_index
                && sides.includes(order.direction)
        })
        .map(|(index, order)| {
            let (node_index, order_id) = order.clob_order_ref();
            (
                index,
                ClobOrderRefV0 {
                    node_index,
                    order_id,
                },
            )
        })
        .collect();

    let mut freed = 0usize;
    for chunk in candidates.chunks(CLOB_ORDER_VIEW_CEILING) {
        let views = clob.orders(chunk.iter().map(|(_, r)| *r).collect())?;
        chunk
            .iter()
            .zip(views.iter())
            .filter(|(_, view)| !view.found())
            .for_each(|((index, _), _)| {
                user.orders[*index].status = OrderStatus::Canceled;
                freed += 1;
            });
    }
    Ok(freed)
}

impl QuoterConfigV0 {
    /// This entry really is the CLOB serving `market_index`, and `book` is one
    /// of the accounts the admin vetted onto it — so a caller can't point an
    /// otherwise-valid entry at an arbitrary account it happens to own.
    ///
    /// Deliberately says nothing about `is_active`/`is_approved`: those mean
    /// "may take new flow", and the removal paths must keep working on a
    /// killed or de-listed book. Callers that add flow gate on them too.
    pub fn validate_clob_book(&self, market_index: u16, book: &Pubkey) -> Result<()> {
        validate!(
            self.quoter_type == QuoterType::Clob,
            ErrorCode::DefaultError,
            "quoter entry is not a CLOB"
        )?;
        // A Clob entry's program is pinned at registration to the CLOB velocity
        // wrote. Re-check on the hot path so the book trust here never rests on
        // a stale entry the pin did not cover.
        validate!(
            self.program_id == crate::ids::clob_program::id(),
            ErrorCode::DefaultError,
            "clob quoter runs program {}, not velocity's CLOB",
            self.program_id
        )?;
        validate!(
            self.market == market_index,
            ErrorCode::DefaultError,
            "quoter entry is for market {}, call is for market {}",
            self.market,
            market_index
        )?;
        // A Clob entry's response account is the book itself (the CLOB's
        // response region lives in its market account), so the vetted book
        // binding is that one field.
        validate!(
            self.response_account == *book,
            ErrorCode::DefaultError,
            "clob market is not the quoter entry's registered book"
        )?;
        Ok(())
    }
}
