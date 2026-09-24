//! The wake hints: the earliest expiry and the next pending activation. The
//! book keeps them current as it places and removes orders, and copies them
//! into the crank conditions a turner watches.

use {
    super::{
        walk::{walk_side_ref, Walk},
        BookHeader,
    },
    crate::{
        error::ClobError,
        state::{ClobMarketV0, OrderNodeV0, SideV0},
    },
    anchor_lang::prelude::*,
    relay_spec::ConditionBlock,
};

/// Moves a hint earlier only. An early hint costs one wasted simulation. A
/// late hint is work nobody is woken for.
///
/// An activation at or behind its placement slot is not pending, so a
/// zero-delay order does not peg the hint to the past.
pub(super) fn fold_wake_hints(
    book: &mut ClobMarketV0,
    max_ts: i64,
    activation_slot: u64,
    placed_slot: u64,
) -> Result<()> {
    if max_ts != 0 && max_ts < book.next_expiry_ts {
        book.next_expiry_ts = max_ts;
    }

    if activation_slot > placed_slot && activation_slot < book.next_activation_slot {
        book.next_activation_slot = activation_slot;
    }

    book.publish_wakes()
}

/// Copy the two hints into the conditions that watch for them.
///
/// The block holds the turner-facing copy of the same two facts, so every
/// write to either hint ends here. A market that nobody has registered
/// cranks for has an inactive block. No turner reads a wake written into an
/// inactive slot, so this needs no guard.
pub(super) fn publish_wakes(book: &mut ClobMarketV0) -> Result<()> {
    let (unix_ts, slot) = (book.next_expiry_ts, book.next_activation_slot);
    let mut write = |index: usize, wake: relay_spec::WakeView| {
        book.crank
            .update_condition(index, |condition| condition.set_wake(wake))
            .map_err(|_| ClobError::WakeWriteFailed)
    };

    write(
        crate::state::CRANK_EXPIRY,
        relay_spec::WakeView::AtTimestamp { unix_ts },
    )?;
    write(
        crate::state::CRANK_ACTIVATION,
        relay_spec::WakeView::AtSlot { slot },
    )?;

    Ok(())
}

/// Repairs the expiry hint only. A removal takes no clock, and the
/// activation hint needs the current slot to repair. An early activation
/// hint is safe, and [`expire_activation_hint`] moves it on at the
/// next write that knows the slot.
pub(super) fn repair_expiry_hint_for(book: &mut ClobMarketV0, removed: &OrderNodeV0) -> Result<()> {
    if removed.max_ts != 0 && removed.max_ts <= book.next_expiry_ts {
        book.recompute_wake_hints(true, None)?;
    }

    Ok(())
}

/// Moves the activation hint past a slot the chain has reached. The expiry
/// hint needs no equivalent, because a timestamp stays true as time moves. A
/// pending activation arrives with nothing writing to the book, so a stored
/// slot behind the current one would stay due for good.
pub(super) fn expire_activation_hint(book: &mut ClobMarketV0, slot: u64) -> Result<()> {
    if book.next_activation_slot != u64::MAX && book.next_activation_slot <= slot {
        book.recompute_wake_hints(false, Some(slot))?;
    }

    Ok(())
}

/// One walk of the arena for whichever hints were asked for. `activation`
/// carries the slot a pending activation has to be ahead of.
///
/// The walk lives here rather than in a caller because the arena is this
/// program's own state. A reader outside the program would have to know
/// where a node keeps its expiry, and that is the coupling these hints
/// exist to remove.
pub(super) fn recompute_wake_hints(
    book: &mut ClobMarketV0,
    expiry: bool,
    activation: Option<u64>,
) -> Result<()> {
    let (mut min_ts, mut min_slot) = (i64::MAX, u64::MAX);
    // Walk the side lists, not the arena. The cost is `bid_count +
    // ask_count` hops instead of the arena capacity. Every removal in
    // `cancel_all` and in a deep `execute` can land here, and a full-arena
    // walk per removal put both over the compute budget on a large market.
    for side in [SideV0::Bid, SideV0::Ask] {
        walk_side_ref(book, side, |_, node| {
            if node.max_ts != 0 && node.max_ts < min_ts {
                min_ts = node.max_ts;
            }

            if activation.is_some_and(|slot| node.activation_slot > slot)
                && node.activation_slot < min_slot
            {
                min_slot = node.activation_slot;
            }

            Ok(Walk::Continue)
        })?;
    }

    if expiry {
        book.next_expiry_ts = min_ts;
    }

    if activation.is_some() {
        book.next_activation_slot = min_slot;
    }

    book.publish_wakes()
}

/// Whether `node` is holding the expiry hint, so a caller batching removals
/// knows whether it owes a repair once it is done.
pub(super) fn holds_expiry_hint(book: &ClobMarketV0, node: &OrderNodeV0) -> bool {
    node.max_ts != 0 && node.max_ts <= book.next_expiry_ts
}
