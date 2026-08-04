use {
    crate::{
        book::ClobBook,
        emit::CancelAllRecord,
        error::ClobError,
        state::{CancelAllOutcomeV0, CancelSidesV0, ClobMarketV0, UserRefV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct CancelAllV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelAllArgsV0 {
    /// Whose orders to withdraw (velocity verified control before the CPI).
    pub user: UserRefV0,
    pub sides: CancelSidesV0,
}

/// Withdraw every order one user holds on a side (or both) in a single call.
///
/// This exists because the alternative — `cancel_order_v0` per order — costs a
/// velocity instruction and a CPI round trip each, so a maker repricing a
/// twenty-quote ladder pays twenty of them. Here the whole sweep is one CPI and
/// one aggregate unwind: return data carries per-side base totals and order
/// counts rather than a list, because that is exactly the shape velocity's
/// `open_bids`/`open_asks` accounting consumes.
///
/// Removals are capped per call ([`crate::state::CANCEL_ALL_ORDERS_CEILING`]).
/// `exhaustive` in the response says whether the walk finished; a maker holding
/// more than the cap repeats the call. Velocity's unwind is driven by the
/// reported totals, so repeating is safe — each call unwinds exactly what it
/// removed.
pub fn handle_cancel_all_v0(
    ctx: &mut Context<CancelAllV0>,
    args: CancelAllArgsV0,
) -> Result<CancelAllOutcomeV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let market_index = market.market_index;

    // The record's prefix goes down before the walk so removed ids can stream
    // straight into its log buffer instead of a second one.
    let mut record = CancelAllRecord::new(
        &args.user.authority,
        clock.unix_timestamp,
        market_index,
        args.user.sub_account_id,
        sides_tag(args.sides),
    )?;
    let outcome = market.cancel_all(args.user, args.sides, &mut |order_id| {
        record.push_id(order_id)
    })?;
    record.finish(&outcome)?;

    Ok(CancelAllOutcomeV0 {
        user: args.user,
        bid_base_asset_amount: outcome.bid_base_asset_amount,
        ask_base_asset_amount: outcome.ask_base_asset_amount,
        bid_orders: outcome.bid_orders,
        ask_orders: outcome.ask_orders,
        exhaustive: outcome.exhaustive,
    })
}

/// The borsh tag the wire enum encodes to, for the record's `sides` byte.
fn sides_tag(sides: CancelSidesV0) -> u8 {
    match sides {
        CancelSidesV0::Bids => 0,
        CancelSidesV0::Asks => 1,
        CancelSidesV0::Both => 2,
    }
}
