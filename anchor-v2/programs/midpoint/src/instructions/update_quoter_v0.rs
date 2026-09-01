use {
    crate::{error::MidpointError, state::MidpointQuoterV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    /// The maker's config key — the only key allowed to reconfigure.
    /// Deliberately not reassignable: the maker's kill switch must stay
    /// theirs. Note this is *not* the quoted wallet: the quoted user is fixed
    /// at creation (it seeds the PDA), so no config update can redirect fills
    /// to a different `User`.
    #[account(address = quoter.authority @ MidpointError::InvalidAuthority)]
    pub authority: Signer,
    /// Present = becomes the new hot key.
    pub new_hot_authority: Option<UncheckedAccount>,
}

#[derive(Clone, Default, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct UpdateQuoterArgsV0 {
    pub max_mid_staleness_slots: Option<u64>,
    pub price_tick_size: Option<u64>,
    pub size_step: Option<u64>,
    pub min_quote_size: Option<u64>,
    /// Attested flow is gated on velocity's live `State.hot_flow_authority`,
    /// so turning it on takes no local key — and an unassigned role on
    /// velocity's side silences the book rather than opening it.
    pub require_attested_flow: Option<bool>,
    pub is_paused: Option<bool>,
    /// Max mid deviation from velocity's oracle, parts per million. 0 disables
    /// the bound.
    pub max_mid_deviation_ppm: Option<u64>,
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
    let quoter = &mut ctx.accounts.quoter;
    if let Some(hot) = new_hot {
        quoter.hot_authority = hot;
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
        quoter.require_attested_flow = v as u8;
    }
    if let Some(v) = args.is_paused {
        quoter.is_paused = v as u8;
    }
    if let Some(v) = args.max_mid_deviation_ppm {
        quoter.max_mid_deviation_ppm = v;
    }
    quoter.validate()
}
