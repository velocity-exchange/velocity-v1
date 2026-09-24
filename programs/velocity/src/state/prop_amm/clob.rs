//! The velocity-mediated CLOB CPI surface, and what velocity does with its
//! answers.
//!
//! [`ClobMarket`] changes the book and signs as its `place_authority`.
//! [`ClobReader`] only asks. Both speak the wire `clob-wire` declares, and
//! nothing here holds a copy of the book account's layout. The registry
//! `execute_v0` leg is absent on purpose. That leg is the interface every
//! quoter type shares. See [`super::QuoterConfigV0::execute`].

use {
    super::{
        get_quoter_slab_signer_seeds, ClobUserRefV0, QuoterConfigV0, QuoterSlabExt, QuoterSlabV0,
        QuoterType, CLOB_USER_REF_BYTES,
    },
    crate::{error::ErrorCode, msg, validate},
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke, invoke_signed},
    },
};

/// Capacity reserved for a CLOB CPI's instruction data. It counts the
/// discriminator, a side, two `u64`s, an optional delay, a timestamp, a user
/// ref, the taker-origin flag and the reduce-only flag. One reservation avoids
/// the intermediate buffers a growing `Vec` leaks into the bump allocator.
pub const CLOB_CPI_DATA_CAPACITY: usize = 8 + 1 + 8 + 8 + 5 + 8 + CLOB_USER_REF_BYTES + 1 + 1;

/// Order handle on the CLOB. It is an O(1) node hint, verified against the
/// order id there, so a stale hint fails closed on the CLOB side.
pub use clob_wire::ClobOrderRefV0;
/// `place_order_v0` args on the CLOB wire. `taker_origin` marks an unfilled
/// taker remainder velocity migrated onto the book, not a quote its owner
/// posted. The order cannot be taken while a live counterparty crosses it, and
/// a cross settles at the counterparty's price.
pub use clob_wire::PlaceOrderArgsV0 as ClobPlaceOrderArgsV0;
/// Wire form of an order the CLOB removed. Cancel, evict and expire return it,
/// so velocity can decrement the maker's aggregates by the remaining size on
/// the right side. Its `taker_origin` flag is the only report that the order
/// was a migrated taker remainder, which decides whose price a cross settles at.
pub use clob_wire::RemovedOrderV0 as ClobRemovedOrderV0;
/// `cancel_order_v0` args on the CLOB wire.
pub use clob_wire::{
    CancelOrderArgsV0 as ClobCancelOrderArgsV0, FillArgsV0 as ClobFillArgsV0,
    FillOutcomeV0 as ClobFillOutcomeV0, FillRequestV0 as ClobFillRequestV0,
    FilledOrderV0 as ClobFilledOrderV0,
};
/// Which sides a `cancel_all_v0` withdraws, on the CLOB wire. Declared by
/// `quoter-spec`. The alias keeps velocity's name for it.
pub use quoter_spec::CancelSidesV0 as ClobCancelSides;
/// What the wire's named sides mean to velocity: the maker positions they
/// unwind.
pub trait ClobCancelSidesExt {
    /// The maker position directions the named sides represent. A resting bid
    /// is a long and a resting ask is a short. The aggregate unwind iterates
    /// over these.
    fn directions(self) -> &'static [crate::controller::position::PositionDirection];
    fn includes(self, direction: crate::controller::position::PositionDirection) -> bool;
}

impl ClobCancelSidesExt for ClobCancelSides {
    fn directions(self) -> &'static [crate::controller::position::PositionDirection] {
        use crate::controller::position::PositionDirection;
        match (self.has_bids(), self.has_asks()) {
            (true, true) => &[PositionDirection::Long, PositionDirection::Short],
            (true, false) => &[PositionDirection::Long],
            (false, true) => &[PositionDirection::Short],
            (false, false) => &[],
        }
    }

    fn includes(self, direction: crate::controller::position::PositionDirection) -> bool {
        use crate::controller::position::PositionDirection;
        match direction {
            PositionDirection::Long => self.has_bids(),
            PositionDirection::Short => self.has_asks(),
        }
    }
}

/// `cancel_all_v0` args on the CLOB wire.
pub use clob_wire::CancelAllArgsV0 as ClobCancelAllArgsV0;
/// What the CLOB's `cancel_all_v0` withdrew: per-side totals rather than a list
/// of removals. That is the shape the open-order aggregates consume. It costs
/// one `decrease_open_bids_and_asks` per side and one count, however many
/// orders the sweep took.
pub use clob_wire::CancelAllOutcomeV0 as ClobCancelAllOutcomeV0;

/// What velocity reads into the sweep outcome beyond its shape: which side a
/// maker's position rests on. A type the wire crate owns takes no inherent
/// impl, and the direction is velocity's own type. [`ClobCancelSidesExt`] is a
/// trait for the same reason.
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
    /// on. A bid is a long and an ask is a short.
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

/// Anchor-default discriminators, `sha256("global:<name>")[..8]`, of the CLOB
/// instructions velocity calls directly. Place and cancel are velocity-mediated
/// and are not part of the registry's quote and execute surface, so no entry
/// stores them. Nothing outside [`ClobMarket`] and [`ClobReader`] should
/// reference these.
pub use clob_wire::discriminator::{
    CANCEL_ALL_V0 as CLOB_CANCEL_ALL_V0_DISCRIMINATOR,
    CANCEL_ORDER_V0 as CLOB_CANCEL_ORDER_V0_DISCRIMINATOR,
    EVICT_WORST_V0 as CLOB_EVICT_WORST_V0_DISCRIMINATOR, FILL_V0 as CLOB_FILL_V0_DISCRIMINATOR,
    NEXT_REMOVAL_V0 as CLOB_NEXT_REMOVAL_V0_DISCRIMINATOR,
    ORDERS_V0 as CLOB_ORDERS_V0_DISCRIMINATOR, ORDER_RULES_V0 as CLOB_ORDER_RULES_V0_DISCRIMINATOR,
    PLACE_ORDER_V0 as CLOB_PLACE_ORDER_V0_DISCRIMINATOR,
    REMOVE_EXPIRED_V0 as CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR,
    SET_CRANK_CONDITIONS_V0 as CLOB_SET_CRANK_CONDITIONS_V0_DISCRIMINATOR,
};
/// `evict_worst_v0` args on the CLOB wire.
pub use clob_wire::EvictWorstArgsV0 as ClobEvictWorstArgsV0;
/// `order_rules_v0`'s answer: what the book requires of an order before it
/// will hold one. Asked rather than read, so a rule the book moves is a rule
/// velocity still gets right.
pub use clob_wire::OrderRulesV0 as ClobOrderRulesV0;
/// `remove_expired_v0` args on the CLOB wire.
pub use clob_wire::RemoveExpiredArgsV0 as ClobRemoveExpiredArgsV0;
/// `next_removal_v0` args and answer: which order the book would let a caller
/// reclaim, and why. The book decides both. Velocity supplies the removal's
/// consequences rather than the search.
pub use clob_wire::{ClobRemovalKindV0, NextRemovalArgsV0 as ClobNextRemovalArgsV0};
/// `set_crank_conditions_v0` args: who resolves each of the book's own
/// conditions. The book owns the wakes and velocity registers the answers.
pub use clob_wire::{
    CrankAccountV0 as ClobCrankAccountV0, CrankBlockV0 as ClobCrankBlockV0,
    CrankConditionsArgsV0 as ClobCrankConditionsArgsV0, CrankResolverV0 as ClobCrankResolverV0,
};
/// The one shape every read-only CLOB answer describes an order in, and the
/// answer built from it: one view per requested ref. Velocity finds crosses
/// itself from `quote_l3_v0` rows, so it never calls `next_cross_v0`.
pub use clob_wire::{
    OrderViewV0 as ClobOrderViewV0, OrdersArgsV0 as ClobOrdersArgsV0, OrdersV0 as ClobOrdersV0,
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

/// The velocity-mediated CLOB CPI surface, bound to one book. It holds the
/// three accounts every call takes and the seeds that let velocity sign as the
/// book's `place_authority`, which is the market's quoter slab. The return-data
/// decode checks the writer, so a program the CLOB called cannot spoof it.
pub struct ClobMarket<'a, 'info> {
    /// The book account, passed writable.
    pub market: &'a AccountInfo<'info>,
    /// The registered CLOB program.
    pub program: &'a AccountInfo<'info>,
    /// The market's quoter slab, which a book's `place_authority` is set to.
    /// The CLOB gates place, cancel, evict, expire and `execute_v0` on that one
    /// field, so this leg and the registry's execute leg sign as the same key.
    /// See `crate::signer` for why the slab is safe as that shared identity.
    pub slab: &'a AccountInfo<'info>,
    /// The slab's PDA seeds, for signing: the market index and the stored
    /// bump.
    market_index_le: [u8; 2],
    bump: u8,
}

impl<'a, 'info> ClobMarket<'a, 'info> {
    /// Bind to the book the market's slab names. It checks what every caller
    /// needs: the slab serves `market_index`, it holds a book slot, and
    /// `market` is the account the admin vetted onto that slot. A caller
    /// therefore cannot point the slab at an arbitrary account it owns.
    ///
    /// Not gated on `is_active` or `suspended`. Those two mean the slot may
    /// take new flow, and the removal paths must keep working on a killed or
    /// de-listed book. Placement applies that gate itself.
    pub fn from_slab(
        slab: &'a AccountLoader<'info, QuoterSlabV0>,
        market_index: u16,
        market: &'a AccountInfo<'info>,
        program: &'a AccountInfo<'info>,
    ) -> Result<Self> {
        slab.clob_slot(market_index)?
            .config
            .validate_clob_book(market_index, &market.key())?;
        let bump = slab.load()?.bump;
        Ok(Self {
            market,
            program,
            slab: slab.as_ref(),
            market_index_le: market_index.to_le_bytes(),
            bump,
        })
    }

    /// Rest a new order on the book. It returns the CLOB's handle for it.
    pub fn place(&self, args: ClobPlaceOrderArgsV0) -> Result<ClobOrderRefV0> {
        self.invoke(&CLOB_PLACE_ORDER_V0_DISCRIMINATOR, &args, "place")
    }

    /// Report fills velocity made against taker remainders resting on this
    /// book. A taker remainder aggresses against quoters and the vAMM too, so
    /// the book could not have made the fill itself. The order shrinks in
    /// place, so it keeps its queue position and its id.
    pub fn fill(&self, args: ClobFillArgsV0) -> Result<ClobFillOutcomeV0> {
        self.invoke(&CLOB_FILL_V0_DISCRIMINATOR, &args, "fill")
    }

    /// Pull one order off the book. It returns what was removed, so the caller
    /// can unwind the maker's aggregates by the remaining size.
    pub fn cancel(&self, args: ClobCancelOrderArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(&CLOB_CANCEL_ORDER_V0_DISCRIMINATOR, &args, "cancel")
    }

    /// Pull every order this user holds on the named sides in one CPI. It returns
    /// the per-side totals to unwind their aggregates by. The CLOB caps how many
    /// orders it takes and says so in [`ClobCancelAllOutcomeV0::exhaustive`]. The
    /// totals describe exactly what that call removed, so repeating it is safe.
    pub fn cancel_all(&self, args: ClobCancelAllArgsV0) -> Result<ClobCancelAllOutcomeV0> {
        self.invoke(&CLOB_CANCEL_ALL_V0_DISCRIMINATOR, &args, "cancel all")
    }

    /// Reclaim the hinted expired order. The CLOB re-checks that it is due.
    pub fn remove_expired(&self, args: ClobRemoveExpiredArgsV0) -> Result<ClobRemovedOrderV0> {
        self.invoke(
            &CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR,
            &args,
            "remove expired",
        )
    }

    /// Register who resolves the book's own crank conditions. It returns the
    /// account offset the block sits at, which is what a relay watch
    /// registration points at. The offset is asked for rather than derived, so
    /// velocity never has to know the market account's layout.
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

    /// Reclaim the worst order on a side past its soft cap. The CLOB re-checks
    /// the threshold.
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

    /// One CPI. It sends the discriminator followed by the borsh args to the
    /// book, signing as the book's `place_authority`, then decodes the response
    /// the CLOB left as return data. `what` only names the call in error
    /// messages.
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
            ErrorCode::PropAmmArgsEncodeFailed
        })?;

        invoke_signed(
            &Instruction {
                program_id: self.program.key(),
                accounts: vec![
                    AccountMeta::new(self.market.key(), false),
                    AccountMeta::new_readonly(self.slab.key(), true),
                ],

                data,
            },
            &[self.market.clone(), self.slab.clone(), self.program.clone()],
            &[&get_quoter_slab_signer_seeds(
                &self.market_index_le,
                &self.bump,
            )],
        )?;

        clob_response(&self.program.key(), what)
    }
}

/// Decode what a CLOB call left as return data.
///
/// Return data is last-writer-wins within the transaction, so the writer must
/// be the book's own program. Otherwise a program the CLOB called could dictate
/// the response velocity settles on. `what` only names the call in error
/// messages.
fn clob_response<R: AnchorDeserialize>(program: &Pubkey, what: &str) -> Result<R> {
    let (writer, response) = get_return_data().ok_or_else(|| -> Error {
        msg!("clob {} returned no response", what);
        ErrorCode::InvalidQuoterResponse.into()
    })?;

    validate!(
        writer == *program,
        ErrorCode::InvalidQuoterResponse,
        "clob {} return data written by {} instead of the book's program",
        what,
        writer
    )?;

    R::deserialize(&mut response.as_slice()).map_err(|_| {
        msg!("clob {} returned an undecodable response", what);
        ErrorCode::InvalidQuoterResponse.into()
    })
}

/// The read-only leg of the CLOB wire: ask the book what removal work it has.
/// It is separate from [`ClobMarket`] because it signs nothing. A crank
/// resolver runs under simulation and holds no quoter signer. The book answers
/// which order is expired and which order is past the eviction threshold.
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
            ErrorCode::PropAmmArgsEncodeFailed
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
    /// no longer names a live order comes back as [`ClobOrderViewV0::NONE`].
    /// A fill or a crank got there first, which is the expected outcome of the
    /// race rather than an error.
    pub fn orders(&self, refs: Vec<ClobOrderRefV0>) -> Result<Vec<ClobOrderViewV0>> {
        let mut data = Vec::with_capacity(CLOB_CPI_DATA_CAPACITY);
        data.extend_from_slice(&CLOB_ORDERS_V0_DISCRIMINATOR);
        ClobOrdersArgsV0 { refs }
            .serialize(&mut data)
            .map_err(|_| {
                msg!("failed to serialize clob orders args");
                ErrorCode::PropAmmArgsEncodeFailed
            })?;
        self.ask::<ClobOrdersV0>(data, "orders")
            .map(|answer| answer.orders)
    }

    /// One read-only CPI, unsigned. The book answers about its own memory, so
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

impl crate::state::user::User {
    /// This user on the quoter wire, in its derivable form. Every CPI leg and
    /// book report names a user this way, so the construction lives here
    /// instead of at each call site.
    pub fn clob_user_ref(&self) -> ClobUserRefV0 {
        ClobUserRefV0 {
            authority: self.authority,
            sub_account_id: self.sub_account_id.into(),
        }
    }

    /// Release this user's reservation for everything a bulk sweep took, and
    /// report how many orders that was. Both callers are exits, one by the
    /// owner and one forced by a keeper, so the release clamps at the
    /// reservation instead of failing.
    pub fn release_swept_orders(
        &mut self,
        clob: &ClobReader,
        market_index: u16,
        sides: ClobCancelSides,
        swept: &ClobCancelAllOutcomeV0,
    ) -> Result<u32> {
        use crate::state::user::{OrderReservation, ReleaseCheck};

        self.release_orders(
            &OrderReservation::swept(market_index, swept)?,
            ReleaseCheck::ClampedForExit,
        )?;

        let shadows = self.release_swept_trigger_shadows(clob, market_index, sides)?;
        if shadows > 0 {
            crate::msg!("released {} placed-trigger shadows", shadows);
        }

        Ok(swept.orders())
    }

    /// Free this user's placed-trigger shadows on `sides` whose live orders a
    /// bulk sweep took, and report how many were freed.
    ///
    /// Driven off what the book still holds rather than off a list of removed
    /// ids. A shadow is released exactly when its ref no longer names its
    /// order, which holds whether the sweep was capped or not. Matching ids
    /// would miss a shadow whose order left the book by another route. The scan
    /// is over `User.orders`, so the book's size does not bound it.
    ///
    /// Asked in chunks, because the book answers about
    /// [`CLOB_ORDER_VIEW_CEILING`] refs per call and a user may hold more
    /// shadows than that.
    pub fn release_swept_trigger_shadows(
        &mut self,
        clob: &ClobReader,
        market_index: u16,
        sides: ClobCancelSides,
    ) -> Result<usize> {
        use crate::state::user::{MarketType, OrderStatus};
        let candidates: Vec<(usize, ClobOrderRefV0)> = self
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
                    self.orders[*index].status = OrderStatus::Canceled;
                    freed += 1;
                });
        }

        Ok(freed)
    }
}

impl QuoterConfigV0 {
    /// Check that this entry is the CLOB serving `market_index`, and that
    /// `book` is one of the accounts the admin vetted onto it. A caller
    /// therefore cannot point an otherwise-valid entry at an arbitrary account
    /// it happens to own.
    ///
    /// Says nothing about `is_active` or `suspended`. Those two mean the slot
    /// may take new flow, and the removal paths must keep working on a killed
    /// or de-listed book. Callers that add flow gate on them too.
    pub fn validate_clob_book(&self, market_index: u16, book: &Pubkey) -> Result<()> {
        validate!(
            self.quoter_type == QuoterType::Clob,
            ErrorCode::InvalidQuoterConfig,
            "quoter entry is not a CLOB"
        )?;

        // A Clob entry's program is pinned at registration to the CLOB velocity
        // wrote. Re-check on the hot path so the book trust here never rests on
        // a stale entry the pin did not cover.
        validate!(
            self.program_id == crate::ids::clob_program::id(),
            ErrorCode::InvalidQuoterConfig,
            "clob quoter runs program {}, not velocity's CLOB",
            self.program_id
        )?;
        validate!(
            self.market == market_index,
            ErrorCode::InvalidQuoterConfig,
            "quoter entry is for market {}, call is for market {}",
            self.market,
            market_index
        )?;

        // A Clob entry's response account is the book itself, because the
        // CLOB's response region lives in its market account. That one field
        // is therefore the vetted book binding.
        validate!(
            self.response_account == *book,
            ErrorCode::InvalidQuoterConfig,
            "clob market is not the quoter entry's registered book"
        )?;

        Ok(())
    }
}
