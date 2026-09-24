/// Declared by `clob-wire`. The owner is verified against each node.
pub use clob_wire::CancelAllArgsV0;
use {
    crate::{
        book::{BookHeader, ClobBook},
        emit::CancelAllRecord,
        instructions::GatedMarketV0,
        state::CancelAllOutcomeV0,
    },
    anchor_lang::prelude::*,
};

/// Withdraw every order one user holds on a side, or on both, in a single call.
///
/// The alternative is one `cancel_order_v0` per order, each costing a
/// velocity instruction and a CPI round trip. This sweep is one CPI and one
/// aggregate unwind. Return data carries per-side base totals and order
/// counts, the shape velocity's `open_bids` and `open_asks` accounting
/// consumes.
///
/// Removals are capped per call. See
/// [`crate::state::CANCEL_ALL_ORDERS_CEILING`]. `exhaustive` in the response
/// says whether the walk finished. A maker holding more than the cap repeats
/// the call. Velocity drives its unwind from the reported totals, so a repeat
/// is safe. Each call unwinds what that call removed.
///
/// The sweep passes over a taker-origin remainder whose claim the book still
/// honours, and then reports itself as not exhaustive. `force` removes
/// those too, which is what liquidation needs.
pub fn handle_cancel_all_v0(
    ctx: &mut Context<GatedMarketV0>,
    args: CancelAllArgsV0,
) -> Result<CancelAllOutcomeV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let market_index = market.market_index;

    // The record prefix is written before the walk, so removed ids stream into
    // its log buffer instead of a second buffer.
    let mut record = CancelAllRecord::new(
        &args.user.authority,
        clock.unix_timestamp,
        market_index,
        args.user.sub_account_id,
        args.sides.tag(),
    )?;
    let outcome = market.cancel_all(
        args.user,
        args.sides,
        clock.slot,
        args.force,
        &mut |order_id| record.push_id(order_id),
    )?;

    // The removal path takes no clock. An activation hint that the chain
    // already reached is dropped here.
    market.expire_activation_hint(clock.slot)?;
    record.finish(&outcome)?;

    Ok(CancelAllOutcomeV0 {
        user: args.user,
        bid_base_asset_amount: outcome.bid_base_asset_amount,
        ask_base_asset_amount: outcome.ask_base_asset_amount,
        bid_orders: outcome.bid_orders,
        ask_orders: outcome.ask_orders,
        bid_reduce_only_orders: outcome.bid_reduce_only_orders,
        ask_reduce_only_orders: outcome.ask_reduce_only_orders,
        exhaustive: outcome.exhaustive,
    })
}
