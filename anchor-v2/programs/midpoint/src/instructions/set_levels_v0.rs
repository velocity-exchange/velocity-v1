use {
    crate::{
        error::MidpointError,
        state::{Direction, MidpointQuoterV0, SplineLevelInputV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct SetLevelsV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    #[account(address = quoter.hot_authority @ MidpointError::InvalidAuthority)]
    pub hot_authority: Signer,
}

#[derive(Clone, Default, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct SetLevelsArgsV0 {
    /// When present, the write also stamps a new mid, so a shape move and a
    /// mid move stay atomic. The sequence rule from `set_mid_v0` applies.
    pub mid: Option<u64>,
    pub sequence: Option<u64>,
    /// Sides to replace. Writing a side resets its `filled` counters.
    pub bids: Option<Vec<SplineLevelInputV0>>,
    pub asks: Option<Vec<SplineLevelInputV0>>,
}

pub fn handle_set_levels_v0(ctx: &mut Context<SetLevelsV0>, args: SetLevelsArgsV0) -> Result<()> {
    let quoter = &mut ctx.accounts.quoter;
    if let Some(bids) = &args.bids {
        quoter.write_side(Direction::Short, bids)?;
    }
    if let Some(asks) = &args.asks {
        quoter.write_side(Direction::Long, asks)?;
    }
    if let Some(mid) = args.mid {
        let slot = Clock::get()?.slot;
        quoter.set_mid(mid, args.sequence.unwrap_or(0), slot)?;
    }
    // Shape writes are rare, so they pay for the full post-write invariant
    // scan. The scan checks counts against capacity, ascending live rungs with
    // `filled` inside `size`, and a zeroed tail behind a shrinking rewrite.
    quoter.validate()
}
