//! Open an additional tokenized pool ("cohort") on a vault.
//!
//! A [`TokenizedVaultDepositor`] holds ONE pooled cost basis for every holder of its mint, and the
//! profit-share fee comes out of the pool's own shares. So the fee dilutes every token equally,
//! whoever accrued the loss. A pool is therefore only fair to a newcomer while its value is at or
//! above its pooled cost basis, and `tokenize_shares` refuses to mint into it below that mark.
//!
//! With one pool per vault, that refusal closes tokenizing until the vault recovers past a
//! high-water mark that can sit far above any newcomer's entry. This instruction restores
//! availability: the manager opens a fresh pool, at par by construction, for the newcomer to mint
//! into. Each cohort keeps its own mint, its own shares and its own basis, so nothing leaks between
//! them. Tokens of different cohorts are separate SPL mints and are not fungible with each other.

use {
    super::initialize_tokenized_vault_depositor::{
        init_tokenized_pool, InitializeTokenizedVaultDepositorParams,
    },
    crate::{
        constraints::is_manager_for_vault, error::ErrorCode, validate, Size,
        TokenizedVaultDepositor, Vault,
    },
    anchor_lang::prelude::*,
    anchor_spl::{
        metadata::Metadata,
        token::{Mint, Token},
    },
};

pub fn initialize_tokenized_vault_depositor_v2(
    ctx: Context<InitializeTokenizedVaultDepositorV2>,
    params: InitializeTokenizedVaultDepositorParams,
    cohort_id: u32,
) -> Result<()> {
    // Cohort 0 belongs to `initialize_tokenized_vault_depositor`, whose seeds omit the id entirely
    // so that every pool deployed before cohorts keeps its address. Anchor cannot shape a `seeds =`
    // list on a condition, so cohort 0 needs its own instruction and this one must refuse it.
    // Without the refusal, cohort 0 would name two different addresses.
    //
    // The upper bound keeps the seed concatenation unambiguous against the legacy list. See
    // [`TokenizedVaultDepositor::MAX_COHORT_ID`].
    validate!(
        cohort_id > 0 && cohort_id <= TokenizedVaultDepositor::MAX_COHORT_ID,
        ErrorCode::InvalidVaultDepositorInitialization,
        "cohort_id must be in 1..={}, got {}",
        TokenizedVaultDepositor::MAX_COHORT_ID,
        cohort_id
    )?;

    init_tokenized_pool(
        &ctx.accounts.vault,
        &ctx.accounts.vault_depositor,
        ctx.bumps.vault_depositor,
        &ctx.accounts.mint_account.to_account_info(),
        &ctx.accounts.metadata_account.to_account_info(),
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.token_metadata_program.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.rent.to_account_info(),
        cohort_id,
        params,
    )
}

#[derive(Accounts)]
#[instruction(params: InitializeTokenizedVaultDepositorParams, cohort_id: u32)]
pub struct InitializeTokenizedVaultDepositorV2<'info> {
    pub vault: AccountLoader<'info, Vault>,
    /// Same seeds as the cohort-0 account plus the cohort id. `init` makes each id single-use.
    #[account(
        init,
        seeds = [b"tokenized_vault_depositor", vault.key().as_ref(), vault.load()?.shares_base.to_string().as_bytes(), cohort_id.to_le_bytes().as_ref()],
        space = TokenizedVaultDepositor::SIZE,
        bump,
        payer = payer
    )]
    pub vault_depositor: AccountLoader<'info, TokenizedVaultDepositor>,
    #[account(
        init,
        seeds = [b"mint", vault.key().as_ref(), vault.load()?.shares_base.to_string().as_bytes(), cohort_id.to_le_bytes().as_ref()],
        bump,
        payer = payer,
        mint::decimals = params.decimals,
        mint::authority = vault.key(),
        mint::freeze_authority = vault.key(),
    )]
    pub mint_account: Account<'info, Mint>,
    /// CHECK: Validate address by deriving pda
    #[account(
		mut,
		seeds = [b"metadata", token_metadata_program.key().as_ref(), mint_account.key().as_ref()],
		bump,
		seeds::program = token_metadata_program.key(),
	)]
    pub metadata_account: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = is_manager_for_vault(&vault, &payer)?,
    )]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub token_metadata_program: Program<'info, Metadata>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}
