use {
    crate::{
        emit::emit_pod, error::MidpointError, events::MidpointConfigRecordV0,
        state::MidpointQuoterV0,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct ProposeAuthorityV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    #[account(address = quoter.authority @ MidpointError::InvalidAuthority)]
    pub authority: Signer,
    /// The proposed next config key. It gains no power until it signs
    /// `accept_authority_v0`, so a mistyped target cannot lock the maker out
    /// of its own instance.
    pub new_authority: UncheckedAccount,
}

/// Start a config-key rotation. `accept_authority_v0` completes it, and only
/// the proposed key's own signature can do that.
pub fn handle_propose_authority_v0(ctx: &mut Context<ProposeAuthorityV0>) -> Result<()> {
    let target = *ctx.accounts.new_authority.address();
    let quoter = &mut ctx.accounts.quoter;
    quoter.pending_authority = target;
    quoter.validate()?;

    let ts = Clock::get()?.unix_timestamp;
    emit_pod!(MidpointConfigRecordV0 {
        ..quoter.config_record(ts)
    });
    Ok(())
}
