use {
    crate::{
        error::MidpointError,
        state::{MidpointQuoterV0, ZERO_ADDRESS},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    /// The quoted wallet — the only key allowed to reconfigure. Deliberately
    /// not reassignable: the maker's kill switch must stay theirs.
    #[account(address = quoter.authority @ MidpointError::InvalidAuthority)]
    pub authority: Signer,
    /// Present = becomes the new hot key.
    pub new_hot_authority: Option<UncheckedAccount>,
    /// Present = becomes the new flow authority.
    pub new_flow_authority: Option<UncheckedAccount>,
}

#[derive(Clone, Default, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UpdateQuoterArgsV0 {
    pub max_mid_staleness_slots: Option<u64>,
    pub price_tick_size: Option<u64>,
    pub size_step: Option<u64>,
    pub min_quote_size: Option<u64>,
    pub require_attested_flow: Option<bool>,
    pub is_paused: Option<bool>,
}

pub fn handle_update_quoter_v0(
    ctx: &mut Context<UpdateQuoterV0>,
    args: UpdateQuoterArgsV0,
) -> Result<()> {
    let new_hot = ctx
        .accounts
        .new_hot_authority
        .as_ref()
        .map(|account| *account.address());
    let new_flow = ctx
        .accounts
        .new_flow_authority
        .as_ref()
        .map(|account| *account.address());
    let quoter = &mut ctx.accounts.quoter;
    if let Some(hot) = new_hot {
        quoter.hot_authority = hot;
    }
    if let Some(flow) = new_flow {
        quoter.flow_authority = flow;
    }
    if let Some(v) = args.max_mid_staleness_slots {
        quoter.max_mid_staleness_slots = v;
    }
    if let Some(v) = args.price_tick_size {
        quoter.price_tick_size = v;
    }
    if let Some(v) = args.size_step {
        quoter.size_step = v;
    }
    if let Some(v) = args.min_quote_size {
        quoter.min_quote_size = v;
    }
    if let Some(v) = args.require_attested_flow {
        require!(
            !v || quoter.flow_authority != ZERO_ADDRESS,
            MidpointError::InvalidConfig
        );
        quoter.require_attested_flow = v as u8;
    }
    if let Some(v) = args.is_paused {
        quoter.is_paused = v as u8;
    }
    Ok(())
}
