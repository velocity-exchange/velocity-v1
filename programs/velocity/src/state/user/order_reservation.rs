//! What open perp orders hold on their owner's account.
//!
//! An open order holds one open-order count on the account and one on its
//! position. It holds its unfilled base in the position's `open_bids` or
//! `open_asks`, except an armed trigger, which holds no base until it fires.
//! A reduce-only order on a CLOB book also holds one count in
//! `reduce_only_clob_orders`, and the router caps the owner's reduce-only
//! fills while that count is not zero.
//!
//! [`User::reserve_orders`] takes all of it and [`User::release_orders`] gives
//! all of it back, so no path moves one part without the others. A fill
//! shrinks an order in place and gives back only the base it filled. Every
//! other part is given back once, by the path that sees the order leave its
//! slot or its book.

use crate::{
    controller::position::{
        add_new_position, get_position_index, increase_open_bids_and_asks,
        release_reserved_open_base, release_reserved_open_base_for_exit,
        release_reserved_open_orders, PositionDirection,
    },
    error::VelocityResult,
    math::{casting::Cast, safe_math::SafeMath},
    state::{
        prop_amm::{ClobCancelAllOutcomeExt, ClobCancelAllOutcomeV0},
        user::{Order, User},
    },
};

#[cfg(test)]
mod tests;

/// What some open orders hold on one perp position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OrderReservation {
    pub market_index: u16,
    pub open_bids: u64,
    pub open_asks: u64,
    pub open_orders: u8,
    pub reduce_only_book_orders: u16,
}

/// What a release does with a figure above what the position reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseCheck {
    /// Fail. A figure above the reservation would free the margin behind
    /// orders that still rest.
    HeldToReservation,
    /// Release the whole reservation and log it. Only an exit that the owner
    /// chose or a keeper forced uses this, so a reservation that reads short
    /// cannot keep an order open.
    ClampedForExit,
}

impl OrderReservation {
    /// One order on a CLOB book with `unfilled` base left.
    pub fn book_order(
        market_index: u16,
        direction: PositionDirection,
        unfilled: u64,
        reduce_only: bool,
    ) -> Self {
        Self {
            market_index,
            open_orders: 1,
            reduce_only_book_orders: u16::from(reduce_only),
            ..Self::default()
        }
        .with_open_base(direction, unfilled)
    }

    /// One trigger order armed in a `User.orders` slot. It holds no base
    /// until it fires.
    pub fn armed_trigger(market_index: u16) -> Self {
        Self {
            market_index,
            open_orders: 1,
            ..Self::default()
        }
    }

    /// What a perp `order` holds while it is open outside a book, in a
    /// `User.orders` slot or modelled for a detached order's margin check.
    pub fn of_order(order: &Order) -> VelocityResult<Self> {
        let open_base = if order.update_open_bids_and_asks() {
            order.get_base_asset_amount_unfilled(None)?
        } else {
            0
        };

        Ok(Self {
            market_index: order.market_index,
            open_orders: 1,
            ..Self::default()
        }
        .with_open_base(order.direction, open_base))
    }

    /// Every order that one bulk cancel removed from a book.
    pub fn swept(market_index: u16, swept: &ClobCancelAllOutcomeV0) -> VelocityResult<Self> {
        Ok(Self {
            market_index,
            open_bids: swept.base_for(PositionDirection::Long),
            open_asks: swept.base_for(PositionDirection::Short),
            open_orders: swept.orders().cast()?,
            reduce_only_book_orders: swept.reduce_only_orders().cast()?,
        })
    }

    fn with_open_base(mut self, direction: PositionDirection, base_asset_amount: u64) -> Self {
        match direction {
            PositionDirection::Long => self.open_bids = base_asset_amount,
            PositionDirection::Short => self.open_asks = base_asset_amount,
        }

        self
    }
}

impl User {
    /// Take `reservation` onto the account, and report the index of the
    /// position that holds it. A market with no position yet gets one.
    pub fn reserve_orders(&mut self, reservation: &OrderReservation) -> VelocityResult<usize> {
        let position_index = get_position_index(&self.perp_positions, reservation.market_index)
            .or_else(|_| add_new_position(&mut self.perp_positions, reservation.market_index))?;

        let position = &mut self.perp_positions[position_index];
        increase_open_bids_and_asks(
            position,
            &PositionDirection::Long,
            reservation.open_bids,
            true,
        )?;
        increase_open_bids_and_asks(
            position,
            &PositionDirection::Short,
            reservation.open_asks,
            true,
        )?;
        position.open_orders = position.open_orders.safe_add(reservation.open_orders)?;
        position.reduce_only_clob_orders = position
            .reduce_only_clob_orders
            .safe_add(reservation.reduce_only_book_orders)?;

        (0..reservation.open_orders).for_each(|_| self.increment_open_orders());
        Ok(position_index)
    }

    /// Give `reservation` back, and report the index of the position that
    /// held it.
    ///
    /// The reduce-only count saturates under either check. A count that is too
    /// high costs the router a cap slot. A release that fails would trap the
    /// order on the book.
    pub fn release_orders(
        &mut self,
        reservation: &OrderReservation,
        check: ReleaseCheck,
    ) -> VelocityResult<usize> {
        let position_index = get_position_index(&self.perp_positions, reservation.market_index)?;

        let position = &mut self.perp_positions[position_index];
        for (direction, base_asset_amount) in [
            (PositionDirection::Long, reservation.open_bids),
            (PositionDirection::Short, reservation.open_asks),
        ] {
            match check {
                ReleaseCheck::HeldToReservation => {
                    release_reserved_open_base(position, &direction, base_asset_amount)?
                }
                ReleaseCheck::ClampedForExit => {
                    release_reserved_open_base_for_exit(position, &direction, base_asset_amount)?
                }
            }
        }

        match check {
            ReleaseCheck::HeldToReservation => {
                release_reserved_open_orders(position, reservation.open_orders)?
            }
            ReleaseCheck::ClampedForExit => {
                position.open_orders = position.open_orders.saturating_sub(reservation.open_orders)
            }
        }

        position.reduce_only_clob_orders = position
            .reduce_only_clob_orders
            .saturating_sub(reservation.reduce_only_book_orders);

        (0..reservation.open_orders).for_each(|_| self.decrement_open_orders());
        Ok(position_index)
    }

    /// Give back the base a fill took from a resting order. The order stays
    /// open, so its count stays held.
    pub fn release_filled_base(
        &mut self,
        position_index: usize,
        direction: PositionDirection,
        base_filled: u64,
    ) -> VelocityResult {
        release_reserved_open_base(
            &mut self.perp_positions[position_index],
            &direction,
            base_filled,
        )
    }

    /// Move an order from `released` to `reserved`. The new reservation is
    /// taken first, so the position never reads as available and keeps its
    /// index and settings.
    pub fn replace_reservation(
        &mut self,
        released: &OrderReservation,
        reserved: &OrderReservation,
    ) -> VelocityResult<usize> {
        self.reserve_orders(reserved)?;
        self.release_orders(released, ReleaseCheck::HeldToReservation)
    }
}
