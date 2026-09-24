//! The liquidated account's orders on CLOB books.
//!
//! A book order holds `open_bids` or `open_asks` and a count, as a slot order
//! does. A slot cancel cannot remove it, so a liquidation takes every book
//! order in its scope off the book here, where it cancels its slot orders.
//! Cross covers every market that is not isolated, and isolated covers its
//! own market.
//!
//! One sweep runs per market per instruction, which bounds the compute. The
//! book caps how many orders one sweep takes, and a caller can leave a book
//! out. In both cases orders stay in scope, and the liquidation stops after
//! the cancel. The account stays latched, so it cannot rest new orders, and
//! the next call continues the sweep.

use crate::{
    error::{ErrorCode, VelocityResult},
    msg,
    state::{
        prop_amm::{CancelAllOutcomeV0, UserRefV0},
        user::{MarketType, OrderReservation, OrderStatus, PerpPosition, ReleaseCheck, User},
    },
    validate,
};

#[cfg(test)]
mod tests;

/// The CLOB books one instruction can reach.
pub trait BookOrderSweep {
    /// Take every order `user` rests on the book of `market_index`, in one
    /// sweep that also takes orders the owner cannot cancel yet. `None` when
    /// the instruction carries no book for that market.
    fn cancel_all(
        &mut self,
        market_index: u16,
        user: UserRefV0,
    ) -> VelocityResult<Option<CancelAllOutcomeV0>>;
}

/// An instruction that carries no book. Every market answers `None`.
pub struct NoBooks;

impl BookOrderSweep for NoBooks {
    fn cancel_all(
        &mut self,
        _market_index: u16,
        _user: UserRefV0,
    ) -> VelocityResult<Option<CancelAllOutcomeV0>> {
        Ok(None)
    }
}

/// The markets whose book orders one cancel takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookCancelScope {
    /// Every perp market that is not isolated.
    Cross,
    /// One isolated perp market.
    Isolated(u16),
    /// Every perp market.
    All,
}

impl BookCancelScope {
    /// The scope that matches a liquidation mode's slot cancel. That cancel
    /// names one market for an isolated mode, and no market for the cross mode.
    pub fn of_liquidation(slot_scope: (Option<MarketType>, Option<u16>)) -> Self {
        match slot_scope.1 {
            Some(market_index) => Self::Isolated(market_index),
            None => Self::Cross,
        }
    }

    fn covers(&self, position: &PerpPosition) -> bool {
        match self {
            Self::Cross => !position.is_isolated(),
            Self::Isolated(market_index) => position.market_index == *market_index,
            Self::All => true,
        }
    }
}

/// What one cancel took off the books.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BookCancel {
    pub orders: u32,
    /// An order in scope still rests, because the book stopped at its cap or
    /// the instruction carried no book for its market.
    pub orders_remain: bool,
}

/// Take `user`'s book orders in `scope` off their books, and release what
/// they reserved.
pub fn cancel_book_orders(
    user: &mut User,
    scope: BookCancelScope,
    books: &mut dyn BookOrderSweep,
) -> VelocityResult<BookCancel> {
    let user_ref = user.clob_user_ref();
    let markets: Vec<u16> = user
        .perp_positions
        .iter()
        .filter(|position| !position.is_available() && scope.covers(position))
        .map(|position| position.market_index)
        .filter(|market_index| user.clob_resident_open_orders(*market_index) > 0)
        .collect();

    markets
        .into_iter()
        .try_fold(BookCancel::default(), |mut cancel, market_index| {
            let Some(swept) = books.cancel_all(market_index, user_ref)? else {
                msg!("no book for market {}; its orders stay", market_index);
                cancel.orders_remain = true;
                return Ok(cancel);
            };

            cancel.orders =
                cancel
                    .orders
                    .saturating_add(release_book_sweep(user, market_index, &swept)?);
            cancel.orders_remain |= !swept.exhaustive;
            Ok(cancel)
        })
}

/// Release what one sweep of `market_index` took, and report how many orders
/// that was.
///
/// An exhaustive sweep leaves none of the user's orders on the book, so every
/// placed-trigger shadow in the market lost its order and is freed. A capped
/// sweep frees none, because some shadowed orders may still rest.
pub fn release_book_sweep(
    user: &mut User,
    market_index: u16,
    swept: &CancelAllOutcomeV0,
) -> VelocityResult<u32> {
    validate!(
        swept.user == user.clob_user_ref(),
        ErrorCode::InvalidUserAccount,
        "clob swept orders for {}/{} instead of the liquidated user",
        swept.user.authority,
        swept.user.sub_account_id
    )?;

    user.release_orders(
        &OrderReservation::swept(market_index, swept)?,
        ReleaseCheck::ClampedForExit,
    )?;

    if swept.exhaustive {
        user.orders
            .iter_mut()
            .filter(|order| {
                order.status == OrderStatus::Open
                    && order.is_placed_on_clob()
                    && order.market_type == MarketType::Perp
                    && order.market_index == market_index
            })
            .for_each(|order| order.status = OrderStatus::Canceled);
    }

    Ok(swept.orders())
}
