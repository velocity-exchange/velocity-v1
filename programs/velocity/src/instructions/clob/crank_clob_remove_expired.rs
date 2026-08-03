//! `crank_clob_remove_expired` and its resolver: reclaim an expired order
//! via the CLOB's `remove_expired_v0`. Expiry frees a placed trigger's
//! shadow slot (a dead order must not re-arm).

use {
    super::crank_common::{
        crank_clob_removal, derive_user_pdas, removal_call, validate_linkage, ClobRemoval,
        CrankClobOrderRemoval, ResolveClobCrank,
    },
    crate::{
        instructions::relay_harness::resolve_into,
        state::prop_amm::{clob_find_expired, ClobOrderRefV0, ClobRemoveExpiredArgsV0},
    },
    anchor_lang::prelude::*,
};

pub fn handle_crank_clob_remove_expired(
    ctx: Context<CrankClobOrderRemoval>,
    market_index: u16,
    order_ref: ClobOrderRefV0,
) -> Result<()> {
    crank_clob_removal(
        ctx,
        market_index,
        ClobRemoval::Expire(ClobRemoveExpiredArgsV0 { order_ref }),
    )
}

pub fn handle_resolve_crank_clob_remove_expired(ctx: Context<ResolveClobCrank>) -> Result<()> {
    validate_linkage(&ctx)?;
    resolve_into(&ctx.accounts.scratch, || {
        let now = Clock::get()?.unix_timestamp;
        let (node_index, node) = {
            let data = ctx.accounts.clob_market.try_borrow_data()?;
            match clob_find_expired(&data, now) {
                Some(found) => found,
                None => return Ok(None),
            }
        };
        let market_index = ctx.accounts.crank_conditions.load()?.market_index;
        Ok(Some(
            removal_call(&ctx, derive_user_pdas(&node.user_ref()).0)?
                .arg(market_index)?
                .arg(ClobOrderRefV0 {
                    node_index,
                    order_id: node.order_id,
                })?,
        ))
    })
}
