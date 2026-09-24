//! `crank_clob_remove_expired` and its resolver. The crank reclaims an expired
//! order through the CLOB's `remove_expired_v0`. Expiry frees a placed
//! trigger's shadow slot, because a dead order must not re-arm.

use {
    super::helpers::crank_common::{
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

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CrankClobRemoveExpiredArgs {
    pub market_index: u16,
    /// The hinted expired order. The CLOB re-checks that it is due.
    pub order_ref: ClobOrderRefV0,
}

pub fn handle_crank_clob_remove_expired(
    ctx: Context<CrankClobOrderRemoval>,
    args: CrankClobRemoveExpiredArgs,
) -> Result<()> {
    let CrankClobRemoveExpiredArgs {
        market_index,
        order_ref,
    } = args;

    crank_clob_removal(
        ctx,
        market_index,
        ClobRemoval::Expire(ClobRemoveExpiredArgsV0 { order_ref }),
    )
}

/// The expiry slot's answer. It names the order the book reports as due, if
/// there is one.
pub(super) fn stage_expired_removal(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    // The book finds the expired order. Expiry is the book's own bookkeeping,
    // and the book holds the timestamps.
    let found = clob_reader(ctx).next_removal(ClobNextRemovalArgsV0 {
        kind: ClobRemovalKindV0::Expired,
    })?;

    if !found.found() {
        return Ok(None);
    }

    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    Ok(Some(
        removal_call::<crate::instruction::CrankClobRemoveExpired>(
            ctx,
            derive_user_pdas(&found.user).0,
        )?
        .arg(CrankClobRemoveExpiredArgs {
            market_index,
            order_ref: found.order_ref,
        })?,
    ))
}
