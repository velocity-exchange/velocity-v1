//! `crank_clob_evict` and its resolver: reclaim the worst order on a side
//! that grew past its soft cap, via the CLOB's `evict_worst_v0`. Eviction
//! re-arms a placed trigger's shadow slot eagerly (in this same tx),
//! edge-gated on a price recross.

use {
    super::helpers::crank_common::{
        clob_reader, crank_clob_removal, derive_user_pdas, removal_call, ClobRemoval,
        CrankClobOrderRemoval, ResolveClobCrank,
    },
    crate::{
        instructions::relay_harness::StagedCall,
        state::prop_amm::{
            ClobEvictWorstArgsV0, ClobNextRemovalArgsV0, ClobRemovalKindV0, ClobSide,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CrankClobEvictArgs {
    pub market_index: u16,
    /// The side past its soft cap. The CLOB re-checks the threshold.
    pub side: ClobSide,
}

pub fn handle_crank_clob_evict(
    ctx: Context<CrankClobOrderRemoval>,
    args: CrankClobEvictArgs,
) -> Result<()> {
    let CrankClobEvictArgs { market_index, side } = args;
    crank_clob_removal(
        ctx,
        market_index,
        ClobRemoval::Evict(ClobEvictWorstArgsV0 { side }),
    )
}

/// The capacity slot's answer: the order the book would evict, if any.
pub(super) fn stage_eviction(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    // The book picks the side and the order: the eviction threshold and
    // which side to relieve first are its policy, not velocity's.
    let found = clob_reader(ctx).next_removal(ClobNextRemovalArgsV0 {
        kind: ClobRemovalKindV0::Evictable,
    })?;
    if !found.found() {
        return Ok(None);
    }
    let maker = derive_user_pdas(&found.user).0;

    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    Ok(Some(
        removal_call::<crate::instruction::CrankClobEvict>(ctx, maker)?.arg(
            CrankClobEvictArgs {
                market_index,
                side: found.side,
            },
        )?,
    ))
}
