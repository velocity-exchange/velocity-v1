//! `crank_clob_evict` and its resolver: reclaim the worst order on a side
//! that grew past its soft cap, via the CLOB's `evict_worst_v0`. Eviction
//! re-arms a placed trigger's shadow slot eagerly (in this same tx),
//! edge-gated on a price recross.

use {
    super::crank_common::{
        crank_clob_removal, derive_user_pdas, removal_call, validate_linkage,
        CrankClobOrderRemoval, ResolveClobCrank,
    },
    crate::{
        error::ErrorCode,
        instructions::relay_harness::resolve_into,
        state::prop_amm::{
            read_clob_node, read_clob_u32, ClobEvictWorstArgsV0, ClobSide, CLOB_ASK_COUNT_OFFSET,
            CLOB_BID_COUNT_OFFSET, CLOB_EVICT_THRESHOLD_OFFSET, CLOB_EVICT_WORST_V0_DISCRIMINATOR,
            CLOB_NIL, CLOB_WORST_ASK_OFFSET, CLOB_WORST_BID_OFFSET,
        },
    },
    anchor_lang::prelude::*,
};

pub fn handle_crank_clob_evict(
    ctx: Context<CrankClobOrderRemoval>,
    market_index: u16,
    side: ClobSide,
) -> Result<()> {
    let mut data = CLOB_EVICT_WORST_V0_DISCRIMINATOR.to_vec();
    ClobEvictWorstArgsV0 { side }
        .serialize(&mut data)
        .map_err(|_| ErrorCode::DefaultError)?;
    crank_clob_removal(ctx, market_index, data, true)
}

pub fn handle_resolve_crank_clob_evict(ctx: Context<ResolveClobCrank>) -> Result<()> {
    validate_linkage(&ctx)?;
    let conditions = ctx.accounts.crank_conditions.clone();
    resolve_into(&conditions, || {
        let (side, maker) = {
            let data = ctx.accounts.clob_market.try_borrow_data()?;
            let read = |offset: usize| {
                read_clob_u32(&data, offset).ok_or_else(|| error!(ErrorCode::DefaultError))
            };
            let threshold = read(CLOB_EVICT_THRESHOLD_OFFSET)?;
            let bid_count = read(CLOB_BID_COUNT_OFFSET)?;
            let ask_count = read(CLOB_ASK_COUNT_OFFSET)?;
            // The fuller side at/above the soft cap; the CLOB itself re-checks
            // the threshold at execution.
            let side = match (bid_count >= threshold, ask_count >= threshold) {
                (true, true) if ask_count > bid_count => ClobSide::Ask,
                (true, _) => ClobSide::Bid,
                (_, true) => ClobSide::Ask,
                _ => return Ok(None),
            };
            let tail_offset = match side {
                ClobSide::Bid => CLOB_WORST_BID_OFFSET,
                ClobSide::Ask => CLOB_WORST_ASK_OFFSET,
            };
            let tail = read(tail_offset)?;
            if tail == CLOB_NIL {
                return Ok(None);
            }
            let node =
                read_clob_node(&data, tail).ok_or_else(|| error!(ErrorCode::DefaultError))?;
            (side, derive_user_pdas(&node.user_ref()).0)
        };

        let market_index = ctx.accounts.crank_conditions.load()?.market_index;
        Ok(Some(
            removal_call(&ctx, maker)?.arg(market_index)?.arg(side)?,
        ))
    })
}
