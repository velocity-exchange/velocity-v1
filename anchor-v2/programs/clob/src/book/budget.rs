//! The caller's per-user limits on one walk: whose orders it can settle, how
//! much quote each user may lose, and how many distinct users one response
//! can carry.

use {
    crate::{
        error::ClobError,
        state::{
            OrderNodeV0, SideV0, UserCapsV0, UserRefV0, BASE_PRECISION, USER_CAPS_CAPACITY,
            USER_EXCLUSION_BITMAP_BYTES, USER_SET_CAPACITY,
        },
    },
    anchor_lang::prelude::*,
};

/// Whether the caller can settle for this order's owner, and if not, why that
/// matters.
pub(super) enum Settleable {
    /// The owner is in the caller's set, or the set is unrestricted.
    Yes,
    /// The owner is absent, and the walk passes over the order to the depth
    /// behind it. Either the order is younger than the grace window, or it is
    /// below `blocking_min_size` at any age.
    SteppedOver,
    /// The owner is absent, the order is old enough that the caller had every
    /// chance to carry it, and it is big enough to be worth the right. The walk
    /// ends here.
    Withheld,
}

/// A transaction locks at most 64 accounts and a maker costs two, so no caller
/// can carry every user a book might hold. The caller fills as deep as the users
/// it brought. Ending the walk rather than stepping over the order keeps that
/// honest. The walk is best-first, so a caller can trade less of the book, never
/// a worse part of it.
pub(super) fn settleable(
    users: &[UserRefV0],
    index: Option<usize>,
    node: &OrderNodeV0,
    grace_slots: u32,
    blocking_min_size: u64,
    slot: u64,
) -> Settleable {
    if users.is_empty() || index.is_some() {
        return Settleable::Yes;
    }

    // Ending a walk is a right, and a right that costs only `min_order_size` can
    // be bought in bulk. 49 orders on 49 fresh sub-accounts would put the depth
    // behind them out of everyone's reach for the price of rent. The floor is
    // checked before the age, because a small order never earns the right.
    if blocking_min_size != 0 && node.base_asset_amount < blocking_min_size {
        return Settleable::SteppedOver;
    }

    // The age runs from the slot the order became matchable, not from placement.
    // An order inside its activation delay is invisible to every reader, so a
    // caller cannot have carried its owner. Measuring from placement would let an
    // auction order arrive already past the window.
    if slot.saturating_sub(node.activation_slot) <= grace_slots as u64 {
        return Settleable::SteppedOver;
    }

    Settleable::Withheld
}

/// One user's remaining room in the current sweep.
#[derive(Clone, Copy)]
struct UserRoom {
    /// Index into the caller's user set.
    index: u8,
    /// Quote the user may still lose on the swept side. `u64::MAX` is unbounded.
    budget: u64,
    /// Base the book may still fill against this user's reduce-only orders on
    /// the swept side. `u64::MAX` means the user carries no reduce-only cap.
    cover: u64,
}

/// `execute` writes one balance-change record per user and stops when the next
/// does not fit, so `quote` counts the same way and stops at the same order. A
/// named owner is a bit at its set position, because a table of 34-byte refs does
/// not fit this frame. With no set, owners are refs in a heap table, as in `execute`.
pub(super) struct DistinctUsers {
    named_seen: [u8; USER_EXCLUSION_BITMAP_BYTES],
    unnamed_seen: Vec<UserRefV0>,
    count: usize,
}

impl DistinctUsers {
    /// `unnamed_capacity` is the most owners an empty set can count, and zero
    /// when the caller names a set.
    pub(super) fn new(unnamed_capacity: usize) -> Self {
        DistinctUsers {
            named_seen: [0u8; USER_EXCLUSION_BITMAP_BYTES],
            unnamed_seen: Vec::with_capacity(unnamed_capacity),
            count: 0,
        }
    }

    /// Records the owner and reports whether the walk may go on. An owner
    /// already counted is free. A new one past `max` refuses the walk. `index`
    /// is the owner's set position, and `None` only when the set is empty.
    pub(super) fn admit(&mut self, index: Option<usize>, node: &OrderNodeV0, max: usize) -> bool {
        let already_counted = match index {
            Some(index) if index < USER_SET_CAPACITY => {
                self.named_seen[index / 8] & (1u8 << (index % 8)) != 0
            }
            Some(_) => return true,
            // The newest owner first, because consecutive orders often share one.
            None => self
                .unnamed_seen
                .iter()
                .rev()
                .any(|seen| node.is_owned_by(seen)),
        };

        if already_counted {
            return true;
        }

        if self.count == max {
            return false;
        }

        match index {
            Some(index) => self.named_seen[index / 8] |= 1u8 << (index % 8),
            None => self.unnamed_seen.push(node.user_ref()),
        }

        self.count += 1;
        true
    }
}

/// The caller's per-user budgets for one walk, spent as the walk fills. A budget
/// is quote the user may lose, not base it may take, because only this walk knows
/// the price each order fills at. `quote` and `execute` spend it in the same
/// place, so a ladder never promises depth the fill would decline.
pub(super) struct UserBudget<'a> {
    /// Borrowed, and its exclusions name users by set index. Copies of the
    /// 34-byte refs overflowed the 4 KB SBF stack this walk's frame sits in.
    caps: &'a UserCapsV0,
    any_excluded: bool,
    /// Per-user room for the users that have some room.
    entries: [UserRoom; USER_CAPS_CAPACITY],
    len: usize,
    /// The side these orders rest on, which decides which way a price has to
    /// move for the fill to cost their owner anything.
    side: SideV0,
    reference_price: u64,
}

impl<'a> UserBudget<'a> {
    /// A quote budget is spent against the reference price, so a walk that
    /// carries one needs the price. Without it the book refuses the call.
    pub(super) fn new(
        caps: &'a UserCapsV0,
        side: SideV0,
        reference_price: Option<u64>,
    ) -> Result<Self> {
        let spends_quote = caps.as_slice().iter().any(|cap| cap.quote_cap != u64::MAX);
        require!(
            reference_price.is_some() || !spends_quote,
            ClobError::MissingReferencePrice
        );

        let mut budget = UserBudget {
            caps,
            any_excluded: caps.any_excluded(),
            entries: [UserRoom {
                index: 0,
                budget: 0,
                cover: u64::MAX,
            }; USER_CAPS_CAPACITY],

            len: 0,
            side,
            reference_price: reference_price.unwrap_or(0),
        };

        for cap in caps.as_slice() {
            budget.entries[budget.len] = UserRoom {
                index: cap.index,
                budget: cap.quote_cap,
                cover: cap.base_cap,
            };

            budget.len += 1;
        }

        Ok(budget)
    }

    /// What one base of an order at `price` costs its owner. That is the
    /// distance the fill puts between what they pay and what the mark says they
    /// hold. A price in the owner's favour costs nothing.
    fn cost_per_base(&self, price: u64) -> u64 {
        match self.side {
            SideV0::Bid => price.saturating_sub(self.reference_price),
            SideV0::Ask => self.reference_price.saturating_sub(price),
        }
    }

    /// How much of `want` the user at `index` may still take from an order at
    /// `price`, spending their budget for it.
    ///
    /// `index` is the position the membership scan already resolved, so the
    /// bitmap costs a bit test rather than a second walk of the set.
    ///
    /// `reduce_only` is the resting order's own flag. A reduce-only order fills
    /// only against an authoritative `base_cover` on a named cap entry. The book
    /// cannot see a position, so no caps at all, an unnamed owner, and an
    /// unconstrained owner all leave the order uncovered, and it does not fill.
    /// A non-reduce-only order ignores the cover.
    pub(super) fn allow(
        &mut self,
        index: Option<usize>,
        want: u64,
        price: u64,
        reduce_only: bool,
    ) -> u64 {
        // The uncovered fast paths are no caps at all, and an owner the set does
        // not name. Both are free for an ordinary order and refused for a
        // reduce-only one.
        if !self.any_excluded && self.len == 0 {
            return if reduce_only { 0 } else { want };
        }

        let Some(index) = index else {
            return if reduce_only { 0 } else { want };
        };

        if self.caps.is_excluded(index) {
            return 0;
        }

        for slot in 0..self.len {
            let UserRoom {
                index: named,
                budget: room,
                cover,
            } = self.entries[slot];

            if named as usize != index {
                continue;
            }

            let cost_per_base = self.cost_per_base(price);
            // The quote budget does not bind when it is unbounded, or when the
            // cost is zero. Otherwise the base rounds down and the spend rounds
            // up, so a long run cannot creep past the budget one remainder at a
            // time.
            let budget_allowed = if room == u64::MAX || cost_per_base == 0 {
                want
            } else {
                let affordable = (room as u128 * BASE_PRECISION as u128) / cost_per_base as u128;
                want.min(affordable.min(u64::MAX as u128) as u64)
            };

            // The authoritative base cover binds only a reduce-only order.
            let allowed = if reduce_only {
                budget_allowed.min(cover)
            } else {
                budget_allowed
            };

            // Spend the quote budget for what was actually taken, and draw the
            // cover down by the same base for a reduce-only fill.
            if room != u64::MAX && cost_per_base != 0 {
                let spent =
                    (allowed as u128 * cost_per_base as u128).div_ceil(BASE_PRECISION as u128);
                self.entries[slot].budget = room.saturating_sub(spent.min(u64::MAX as u128) as u64);
            }

            if reduce_only {
                self.entries[slot].cover = cover.saturating_sub(allowed);
            }

            return allowed;
        }

        // An owner named in the set but carrying no cap entry is
        // unconstrained. It is uncovered, so a reduce-only order does not fill.
        if reduce_only {
            0
        } else {
            want
        }
    }
}
