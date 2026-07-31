use {
    crate::{
        error::MidpointError,
        state::{MidpointQuoterV0, QuoterConfigV0, ZERO_ADDRESS},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
#[instruction(config: QuoterConfigV0)]
pub struct InitializeQuoterV0 {
    #[account(mut)]
    pub payer: Signer,
    /// The quoted wallet. Its signature is the consent to quote for its
    /// sub-account — the same rule the velocity registry enforces on
    /// Custom-entry creation — and it rides the seeds, so one instance
    /// exists per (market, wallet, sub-account) and nobody can squat
    /// another maker's address.
    pub authority: Signer,
    /// The only signer `execute_v0` accepts (the velocity signer PDA).
    /// Immutable after init. Accounts, not args: duplicated accounts cost
    /// one index byte in the tx.
    pub execute_authority: UncheckedAccount,
    /// Hot key for mid/level writes; rotatable by `authority`.
    pub hot_authority: UncheckedAccount,
    /// Attested-flow co-signer; required when the config demands
    /// attestation.
    pub flow_authority: Option<UncheckedAccount>,
    #[account(
        init,
        payer = payer,
        seeds = [
            b"midpoint",
            &config.market_index.to_le_bytes(),
            authority.address().as_ref(),
            &config.user_sub_account_id.to_le_bytes(),
        ],
        bump
    )]
    pub quoter: Account<MidpointQuoterV0>,
    pub system_program: Program<System>,
}

pub fn handle_initialize_quoter_v0(
    ctx: &mut Context<InitializeQuoterV0>,
    config: QuoterConfigV0,
) -> Result<()> {
    require!(config.base_precision != 0, MidpointError::InvalidConfig);
    require!(
        !config.require_attested_flow || ctx.accounts.flow_authority.is_some(),
        MidpointError::InvalidConfig
    );
    let flow_authority = ctx
        .accounts
        .flow_authority
        .as_ref()
        .map(|account| *account.address())
        .unwrap_or(ZERO_ADDRESS);

    let quoter = &mut ctx.accounts.quoter;
    quoter.authority = *ctx.accounts.authority.address();
    quoter.hot_authority = *ctx.accounts.hot_authority.address();
    quoter.execute_authority = *ctx.accounts.execute_authority.address();
    quoter.flow_authority = flow_authority;
    quoter.max_mid_staleness_slots = config.max_mid_staleness_slots;
    quoter.price_tick_size = config.price_tick_size;
    quoter.size_step = config.size_step;
    quoter.min_quote_size = config.min_quote_size;
    quoter.base_precision = config.base_precision;
    quoter.user_sub_account_id = config.user_sub_account_id;
    quoter.market_index = config.market_index;
    quoter.require_attested_flow = config.require_attested_flow as u8;
    // mid/levels start unset: the quoter is live but quotes nothing until
    // the hot key writes a mid and at least one side.
    Ok(())
}
