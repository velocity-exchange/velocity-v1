use {
    crate::{
        emit::emit_pod,
        error::MidpointError,
        events::MidpointConfigRecordV0,
        state::{MidpointQuoterV0, ZERO_ADDRESS},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct AcceptAuthorityV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    /// No key maps to [`ZERO_ADDRESS`], so this constraint alone refuses an
    /// accept while no rotation is pending.
    #[account(address = quoter.pending_authority @ MidpointError::InvalidAuthority)]
    pub new_authority: Signer,
}

/// Complete a config-key rotation `propose_authority_v0` started. Only the
/// proposed key's own signature can move it into `authority`.
pub fn handle_accept_authority_v0(ctx: &mut Context<AcceptAuthorityV0>) -> Result<()> {
    let quoter = &mut ctx.accounts.quoter;
    quoter.authority = quoter.pending_authority;
    quoter.pending_authority = ZERO_ADDRESS;
    quoter.validate()?;

    let ts = Clock::get()?.unix_timestamp;
    emit_pod!(MidpointConfigRecordV0 {
        ..quoter.config_record(ts)
    });
    Ok(())
}
