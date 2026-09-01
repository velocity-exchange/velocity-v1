//! What the book holds for a set of refs.
//!
//! A caller holding order refs — from its own records, or from a client that
//! read the book off chain — cannot tell which of them still name a live
//! order, or what those orders hold, without the book's memory. It used to
//! find out by decoding the arena, which meant knowing where a node keeps its
//! size, its side and its owner, and which bit says the node is live.
//!
//! Asking is what replaces that. A ref that no longer names a live order
//! comes back as [`OrderViewV0::NONE`] instead of being dropped, so the
//! answers read straight against the caller's own list, and a stale ref is
//! the expected outcome of a race with a fill or a crank rather than an
//! error.
//!
//! Read-only, and cheap enough to sit inside a landed transaction: a caller
//! that has to decide something about an order *before* removing it — whether
//! cancelling it would make an account worse — asks here first.

use {
    crate::{
        book::NodeArena,
        error::ClobError,
        state::{order_view, ClobMarketV0, OrderBitFlag, OrderViewV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct OrdersV0Accounts {
    pub market: ClobMarketV0,
}

/// Declared by `clob-wire`.
pub use clob_wire::{OrdersArgsV0, OrdersV0, ORDER_VIEW_CEILING};

pub fn handle_orders_v0(
    ctx: &mut Context<OrdersV0Accounts>,
    args: OrdersArgsV0,
) -> Result<OrdersV0> {
    require!(
        args.refs.len() <= ORDER_VIEW_CEILING,
        ClobError::InvalidConfig
    );
    let market = &ctx.accounts.market;
    let orders = args
        .refs
        .iter()
        .map(|order_ref| {
            // A ref whose node was freed or reused names no order any more.
            // The id is what proves it: ids are never reused, so a node
            // holding a different one holds somebody else's order.
            market
                .read_node(order_ref.node_index)
                .ok()
                .filter(|node| {
                    node.is_bit_flag_set(OrderBitFlag::Open) && node.order_id == order_ref.order_id
                })
                .map_or(OrderViewV0::NONE, |node| {
                    order_view(&node, order_ref.node_index)
                })
        })
        .collect();
    Ok(OrdersV0 { orders })
}
