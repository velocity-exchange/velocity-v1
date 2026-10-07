//! The hot path. A maker calls this thousands of times a day to track fair
//! value, so the instruction holds the least the account model allows. It takes
//! one zero-copy account with an owner check and a discriminator check, one
//! address-matched signer, one clock read, and three u64 stores. It re-derives
//! no PDA, because the hot-authority match already binds the write to this
//! quoter. It emits no event and allocates nothing. A litesvm test pins the
//! compute budget.

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
    /// An optional monotonic guard for racing writers. A nonzero sequence must
    /// strictly increase. A sequence of 0 skips the check only while no writer
    /// has stamped a sequence yet. A mid of 0 is a withdrawal, so it skips the
    /// guard whatever the sequence holds.
    pub sequence: u64,
}

pub fn handle_set_mid_v0(ctx: &mut Context<SetMidV0>, args: SetMidArgsV0) -> Result<()> {
    let slot = Clock::get()?.slot;
    ctx.accounts.quoter.set_mid(args.mid, args.sequence, slot)
}
