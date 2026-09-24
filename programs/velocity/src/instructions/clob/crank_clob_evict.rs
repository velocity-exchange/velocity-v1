//! `crank_clob_evict` and its resolver. The crank reclaims the worst order on
//! a side that grew past its soft cap, through the CLOB's `evict_worst_v0`.
//! Eviction re-arms a placed trigger's shadow slot in the same transaction,
//! behind an edge gate on a price recross.

use {
    super::helpers::crank_common::{
        clob_reader, crank_clob_removal, removal_call, ClobRemoval, CrankClobOrderRemoval,
        ResolveClobCrank,
    },
    crate::{
        instructions::relay_harness::StagedCall,
        state::prop_amm::{ClobRemovalKindV0, EvictWorstArgsV0, NextRemovalArgsV0, SideV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CrankClobEvictArgs {
    pub market_index: u16,
    /// The side past its soft cap. The CLOB re-checks the threshold.
    pub side: SideV0,
}

pub fn handle_crank_clob_evict(
    ctx: Context<CrankClobOrderRemoval>,
    args: CrankClobEvictArgs,
) -> Result<()> {
    let CrankClobEvictArgs { market_index, side } = args;
    crank_clob_removal(
        ctx,
        market_index,
        ClobRemoval::Evict(EvictWorstArgsV0 { side }),
    )
}

/// The capacity slot's answer. It names the order the book would evict, if
/// there is one.
pub(super) fn stage_eviction(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    // The book picks the side and the order. The eviction threshold and the
    // choice of which side to relieve first are the book's policy.
    let found = clob_reader(ctx).next_removal(NextRemovalArgsV0 {
        kind: ClobRemovalKindV0::Evictable,
    })?;

    if !found.found() {
        return Ok(None);
    }

    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    Ok(Some(
        removal_call::<crate::instruction::CrankClobEvict>(ctx, &found)?.arg(
            CrankClobEvictArgs {
                market_index,
                side: found.side,
            },
        )?,
    ))
}
