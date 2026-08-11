use {
    super::constraints::is_vault_shares_base_for_tokenized_depositor,
    crate::{
        constraints::{
            is_authority_for_vault_depositor, is_mint_for_tokenized_depositor,
            is_tokenized_depositor_for_vault, is_user_for_vault,
        },
        error::ErrorCode,
        refresh_velocity_spot_market,
        state::{traits::VaultDepositorBase, FeeUpdateProvider, FeeUpdateStatus},
        token_cpi::MintTokensCPI,
        validate, AccountMapProvider, TokenizedVaultDepositor, Vault, VaultDepositor,
        VaultProtocolProvider, WithdrawUnit,
    },
    anchor_lang::prelude::*,
    anchor_spl::token::{mint_to, Mint, MintTo, Token, TokenAccount},
    velocity::{
        instructions::optional_accounts::AccountMaps,
        math::safe_math::SafeMath,
        program::Velocity,
        state::{spot_market::SpotMarket, user::User},
    },
};

pub fn tokenize_shares<'info>(
    ctx: Context<'info, TokenizeShares<'info>>,
    amount: u64,
    unit: WithdrawUnit,
) -> Result<()> {
    // Advance the denomination market's `cumulative_deposit_interest` BEFORE any
    // account is borrowed and before NAV is snapshotted (OtterSec #136/#137).
    // Must precede `load_mut`/`load_maps`: `invoke` rejects a CPI whose writable
    // accounts still have live borrows, and the maps must read post-refresh data.
    refresh_velocity_spot_market!(ctx);

    let clock = &Clock::get()?;

    let mut vault = ctx.accounts.vault.load_mut()?;

    validate!(!vault.in_liquidation(), ErrorCode::OngoingLiquidation)?;

    let mut vault_depositor = ctx.accounts.vault_depositor.load_mut()?;
    let mut tokenized_vault_depositor = ctx.accounts.tokenized_vault_depositor.load_mut()?;

    // backwards compatible: if last rem acct does not deserialize into [`VaultProtocol`] then it's a legacy vault.
    let mut vp = ctx.vault_protocol();
    vault.validate_vault_protocol(&vp)?;
    let mut vp = vp.as_mut().map(|vp| vp.load_mut()).transpose()?;

    validate!(
        vault.shares_base == tokenized_vault_depositor.vault_shares_base,
        ErrorCode::InvalidVaultRebase,
        "Vault has rebased, can no longer tokenize shares. Only redeem_tokens() is allowed. (shares base: {:?} vs. {:?})",
        vault.shares_base,
        tokenized_vault_depositor.vault_shares_base
    )?;

    let total_shares_before = vault_depositor
        .get_vault_shares()
        .safe_add(tokenized_vault_depositor.get_vault_shares())?;

    // #101: apply a matured fee update on this share-movement path (mirrors deposit/withdraw).
    let has_fee_update = FeeUpdateStatus::has_pending_fee_update(vault.fee_update_status);
    let mut fee_update = ctx.fee_update(vp.is_some(), has_fee_update);
    vault.validate_fee_update(&fee_update)?;

    let user = ctx.accounts.velocity_user.load()?;
    let spot_market_index = vault.spot_market_index;
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = ctx.load_maps(
        clock.slot,
        Some(spot_market_index),
        vp.is_some(),
        has_fee_update,
    )?;

    let vault_equity =
        vault.calculate_equity(&user, &perp_market_map, &spot_market_map, &mut oracle_map)?;

    validate!(
        !vault_depositor.last_withdraw_request.pending(),
        ErrorCode::InvalidVaultDeposit,
        "Cannot tokenize shares with a pending withdraw request"
    )?;

    let total_supply_before = ctx.accounts.mint.supply;

    let spot_market = spot_market_map.get_ref(&spot_market_index)?;
    let oracle = oracle_map.get_price_data(&spot_market.oracle_id())?;

    // transfer_shares is the first apply_fee on this path, so it applies the matured update.
    // Keep the VaultProtocol provider alive (capture the returned provider) so the subsequent
    // tokenize_shares accounting still sees protocol state.
    let (shares_transferred, mut vp) = vault_depositor.transfer_shares(
        &mut *tokenized_vault_depositor,
        &mut vault,
        &mut vp,
        &mut fee_update,
        amount,
        unit,
        vault_equity,
        clock.unix_timestamp,
        oracle.price,
    )?;

    let tokens_to_mint = tokenized_vault_depositor.tokenize_shares(
        &mut vault,
        &mut vp,
        &mut None,
        total_supply_before,
        vault_equity,
        shares_transferred,
        clock.unix_timestamp,
        oracle.price,
    )?;

    let total_shares_after = vault_depositor
        .get_vault_shares()
        .safe_add(tokenized_vault_depositor.get_vault_shares())?;

    validate!(
        total_shares_after.eq(&total_shares_before),
        ErrorCode::InvalidVaultSharesDetected,
        "Total vault depositor shares before != after"
    )?;

    let vault_name = vault.name;
    let vault_bump = vault.bump;

    drop(spot_market);
    drop(vault);
    drop(vault_depositor);
    drop(tokenized_vault_depositor);

    ctx.mint(vault_name, vault_bump, tokens_to_mint)?;

    msg!(
        "Minted {} tokens to {}",
        tokens_to_mint,
        ctx.accounts.user_token_account.key()
    );

    ctx.accounts.mint.reload()?;
    let total_supply_after = ctx.accounts.mint.supply;

    validate!(
        total_supply_after > total_supply_before,
        ErrorCode::InvalidTokenization,
        "Total supply after < total supply before"
    )?;

    let supply_delta = total_supply_after.safe_sub(total_supply_before)?;
    validate!(
        supply_delta.eq(&tokens_to_mint),
        ErrorCode::InvalidTokenization,
        "Tokens minted ({}) != supply delta ({})",
        tokens_to_mint,
        supply_delta
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct TokenizeShares<'info> {
    #[account(mut)]
    pub vault: AccountLoader<'info, Vault>,
    #[account(
        mut,
        seeds = [b"vault_depositor", vault.key().as_ref(), authority.key().as_ref()],
        bump,
        constraint = is_authority_for_vault_depositor(&vault_depositor, &authority)?,
    )]
    pub vault_depositor: AccountLoader<'info, VaultDepositor>,
    pub authority: Signer<'info>,
    #[account(
		mut,
		constraint = is_tokenized_depositor_for_vault(&tokenized_vault_depositor, &vault)?,
		constraint = is_vault_shares_base_for_tokenized_depositor(&vault.load()?.shares_base, &tokenized_vault_depositor)?,
	)]
    pub tokenized_vault_depositor: AccountLoader<'info, TokenizedVaultDepositor>,
    /// A vault can run several tokenized pools, each with its own mint, so this account cannot be
    /// seed-checked against one canonical mint address. `is_mint_for_tokenized_depositor` pins it
    /// instead, and pins it just as tightly:
    ///
    /// - `tokenized_vault_depositor` is an `AccountLoader`, so anchor already proved it is owned by
    ///   this program and carries the `TokenizedVaultDepositor` discriminator;
    /// - `is_tokenized_depositor_for_vault` above ties that account to this `vault`;
    /// - its `mint` field is written once, in `TokenizedVaultDepositor::new`, from the `init` mint
    ///   PDA of the initialize instruction, and no code path ever writes it again.
    ///
    /// So `mint.key() == tokenized_vault_depositor.mint` still resolves to a mint this program
    /// created for this vault under seeds it derived. `redeem_tokens` has always paired the mint
    /// this way, with no seed check of its own.
    #[account(
        mut,
        mint::authority = vault.key(),
		constraint = is_mint_for_tokenized_depositor(&mint.key(), &tokenized_vault_depositor)?,
    )]
    pub mint: Account<'info, Mint>,
    #[account(
        token::authority = authority,
        token::mint = tokenized_vault_depositor.load()?.mint
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    /// CHECK: checked in velocity cpi
    pub velocity_user: AccountLoader<'info, User>,
    pub token_program: Program<'info, Token>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    /// The vault's denomination spot market, refreshed by CPI before NAV is
    /// snapshotted (OtterSec #136/#137). Writable because velocity advances its
    /// `cumulative_deposit_interest`.
    #[account(
        mut,
        seeds = [b"spot_market".as_ref(), vault.load()?.spot_market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
    )]
    pub velocity_spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: must be `velocity_spot_market.oracle`; enforced by velocity's
    /// `valid_oracle_for_spot_market` access control on the refresh CPI.
    pub velocity_oracle: AccountInfo<'info>,
    /// CHECK: PDA-pinned to the denomination market's velocity vault;
    /// deserialized and validated inside the refresh CPI.
    #[account(
        seeds = [b"spot_market_vault".as_ref(), vault.load()?.spot_market_index.to_le_bytes().as_ref()],
        bump,
        seeds::program = velocity_program.key(),
    )]
    pub velocity_spot_market_vault: AccountInfo<'info>,
    pub velocity_program: Program<'info, Velocity>,
}

impl<'info> MintTokensCPI for Context<'info, TokenizeShares<'info>> {
    fn mint(&self, vault_name: [u8; 32], vault_bump: u8, amount: u64) -> Result<()> {
        let signature_seeds = Vault::get_vault_signer_seeds(&vault_name, &vault_bump);
        let signers = &[&signature_seeds[..]];

        let cpi_accounts = MintTo {
            mint: self.accounts.mint.to_account_info(),
            to: self.accounts.user_token_account.to_account_info(),
            authority: self.accounts.vault.to_account_info(),
        };

        let cpi_context =
            CpiContext::new_with_signer(self.accounts.token_program.key(), cpi_accounts, signers);

        mint_to(cpi_context, amount)?;

        Ok(())
    }
}
