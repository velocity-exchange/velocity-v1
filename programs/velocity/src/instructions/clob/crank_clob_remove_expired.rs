//! `crank_clob_remove_expired` and its resolver: reclaim an expired order
//! via the CLOB's `remove_expired_v0`. Expiry frees a placed trigger's
//! shadow slot (a dead order must not re-arm).

use {
    super::crank_common::{
        clob_reader, crank_clob_removal, derive_user_pdas, removal_call, ClobRemoval,
        CrankClobOrderRemoval, ResolveClobCrank,
    },
    crate::{
        instructions::relay_harness::StagedCall,
        state::prop_amm::{
            ClobNextRemovalArgsV0, ClobOrderRefV0, ClobRemovalKindV0, ClobRemoveExpiredArgsV0,
        },
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

/// The expiry slot's answer: the order the book says is due, if any.
pub(super) fn stage_expired_removal(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    // The book finds the expired order: expiry is its own bookkeeping,
    // and it holds the timestamps.
    let found = clob_reader(ctx).next_removal(ClobNextRemovalArgsV0 {
        kind: ClobRemovalKindV0::Expired,
    })?;
    if !found.found() {
        return Ok(None);
    }
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    Ok(Some(
        removal_call::<crate::instruction::CrankClobRemoveExpired>(
            &ctx,
            derive_user_pdas(&found.user).0,
        )?
        .arg(market_index)?
        .arg(found.order_ref)?,
    ))
}
