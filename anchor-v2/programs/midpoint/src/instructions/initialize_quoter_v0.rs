use {
    crate::{
        error::MidpointError,
        state::{MidpointQuoterV0, QuoterConfigV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(config: QuoterConfigV0)]
pub struct InitializeQuoterV0 {
    #[account(mut)]
    pub payer: Signer,
    /// The maker's config key. It pauses the instance, reconfigures it, and
    /// rotates the hot key. It is independent of the quoted user, so a desk can
    /// hold it cold, or share one operator key across the wallets it quotes
    /// for.
    pub authority: Signer,
    /// The quoted wallet. Its signature is the consent to quote its sub-account,
    /// which velocity's registry enforces on Custom-entry creation. The address
    /// seeds the PDA, so only one instance exists per market, wallet and
    /// sub-account, and the binding is immutable once set.
    pub user_authority: Signer,
    /// The only signer `execute_v0` accepts. It is velocity's quoter CPI signer
    /// PDA, and it is immutable after creation. It arrives as an account and
    /// not as an argument, because a duplicated account costs one index byte in
    /// the transaction.
    pub execute_authority: UncheckedAccount,
    /// The hot key for mid and level writes. `authority` can rotate it.
    pub hot_authority: UncheckedAccount,
    #[account(
        init,
        payer = payer,
        seeds = [
            b"midpoint",
            &config.market_index.to_le_bytes(),
            user_authority.address().as_ref(),
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
    require!(
        config.base_precision == crate::state::BASE_PRECISION,
        MidpointError::InvalidConfig
    );

    let quoter = &mut ctx.accounts.quoter;
    quoter.authority = *ctx.accounts.authority.address();
    quoter.hot_authority = *ctx.accounts.hot_authority.address();
    quoter.execute_authority = *ctx.accounts.execute_authority.address();
    quoter.user_authority = *ctx.accounts.user_authority.address();
    quoter.max_mid_staleness_slots = config.max_mid_staleness_slots;
    quoter.price_tick_size = config.price_tick_size;
    quoter.size_step = config.size_step;
    quoter.min_quote_size = config.min_quote_size;
    quoter.base_precision = config.base_precision;
    quoter.user_sub_account_id = config.user_sub_account_id;
    quoter.market_index = config.market_index;
    quoter.require_attested_flow = config.require_attested_flow as u8;
    quoter.max_mid_deviation_ppm = config.max_mid_deviation_ppm;
    // The mid and the levels start unset. The quoter exists but quotes nothing
    // until the hot key writes a mid and at least one side.
    quoter.validate()
}
