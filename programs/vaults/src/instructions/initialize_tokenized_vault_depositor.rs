use {
    crate::{constraints::is_manager_for_vault, Size, TokenizedVaultDepositor, Vault},
    anchor_lang::prelude::*,
    anchor_spl::{
        metadata::{
            create_metadata_accounts_v3, mpl_token_metadata::types::DataV2,
            CreateMetadataAccountsV3, Metadata,
        },
        token::{Mint, Token},
    },
};

pub fn initialize_tokenized_vault_depositor(
    ctx: Context<InitializeTokenizedVaultDepositor>,
    params: InitializeTokenizedVaultDepositorParams,
) -> Result<()> {
    // Cohort 0 is the legacy pool. Its PDA seeds carry no cohort id, so this instruction can only
    // ever create cohort 0. `initialize_tokenized_vault_depositor_v2` creates ids 1 and above.
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
        0,
        params,
    )
}

/// Body shared by `initialize_tokenized_vault_depositor` and its `_v2` cohort variant.
///
/// The two instructions differ only in the PDA seeds of `vault_depositor` and `mint_account`.
/// Anchor checks those seeds before the handler runs, so by this point both instructions hold the
/// same kind of accounts and do the same work.
#[allow(clippy::too_many_arguments)]
pub(crate) fn init_tokenized_pool<'info>(
    vault: &AccountLoader<'info, Vault>,
    vault_depositor: &AccountLoader<'info, TokenizedVaultDepositor>,
    vault_depositor_bump: u8,
    mint_account: &AccountInfo<'info>,
    metadata_account: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    token_metadata_program: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    rent: &AccountInfo<'info>,
    cohort_id: u32,
    params: InitializeTokenizedVaultDepositorParams,
) -> Result<()> {
    let vault_ref = vault.load()?;
    let mut tokenized_vault_depositor = vault_depositor.load_init()?;
    *tokenized_vault_depositor = TokenizedVaultDepositor::new(
        vault.key(),
        vault_depositor.key(),
        mint_account.key(),
        vault_ref.shares_base,
        cohort_id,
        vault_depositor_bump,
        Clock::get()?.unix_timestamp,
    );

    let signature_seeds = Vault::get_vault_signer_seeds(vault_ref.name.as_ref(), &vault_ref.bump);
    let signers = &[&signature_seeds[..]];

    create_metadata_accounts_v3(
        CpiContext::new_with_signer(
            token_metadata_program.key(),
            CreateMetadataAccountsV3 {
                metadata: metadata_account.clone(),
                mint: mint_account.clone(),
                mint_authority: vault.to_account_info(),
                update_authority: vault.to_account_info(),
                payer: payer.clone(),
                system_program: system_program.clone(),
                rent: rent.clone(),
            },
            signers,
        ),
        DataV2 {
            name: params.token_name,
            symbol: params.token_symbol,
            uri: params.token_uri,
            seller_fee_basis_points: 0,
            creators: None,
            collection: None,
            uses: None,
        },
        false, // Is mutable
        true,  // Update authority is signer
        None,  // Collection details
    )?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(params: InitializeTokenizedVaultDepositorParams)]
pub struct InitializeTokenizedVaultDepositor<'info> {
    pub vault: AccountLoader<'info, Vault>,
    #[account(
        init,
        seeds = [b"tokenized_vault_depositor", vault.key().as_ref(), vault.load()?.shares_base.to_string().as_bytes()],
        space = TokenizedVaultDepositor::SIZE,
        bump,
        payer = payer
    )]
    pub vault_depositor: AccountLoader<'info, TokenizedVaultDepositor>,
    #[account(
        init,
        seeds = [b"mint", vault.key().as_ref(), vault.load()?.shares_base.to_string().as_bytes()],
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

#[derive(Debug, Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq)]
pub struct InitializeTokenizedVaultDepositorParams {
    pub token_name: String,
    pub token_symbol: String,
    pub token_uri: String,
    pub decimals: u8,
}
