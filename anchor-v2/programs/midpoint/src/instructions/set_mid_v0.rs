//! THE hot path. A maker calls this thousands of times a day to track fair
//! value, so it is kept to the bare minimum the account model allows: one
//! owner/discriminator-checked zero-copy account, one address-matched
//! signer, a clock read, three u64 stores. No PDA re-derivation (the
//! hot-authority match already binds the write to this quoter), no event,
//! no allocation. The CU budget is pinned by a litesvm test.

use {
    crate::{error::MidpointError, state::MidpointQuoterV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct SetMidV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    #[account(address = quoter.hot_authority @ MidpointError::InvalidAuthority)]
    pub hot_authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct SetMidArgsV0 {
    /// PRICE_PRECISION. 0 stops quoting.
    pub mid: u64,
    /// Opt-in monotonic guard for racing writers: nonzero must strictly
    /// increase; zero skips the check.
    pub sequence: u64,
}

pub fn handle_set_mid_v0(ctx: &mut Context<SetMidV0>, args: SetMidArgsV0) -> Result<()> {
    let slot = Clock::get()?.slot;
    ctx.accounts.quoter.set_mid(args.mid, args.sequence, slot)
}
