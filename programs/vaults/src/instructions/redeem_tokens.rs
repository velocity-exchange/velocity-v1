use {
    crate::{
        constraints::{
            is_ata, is_authority_for_vault_depositor, is_mint_for_tokenized_depositor,
            is_tokenized_depositor_for_vault, is_user_for_vault,
        },
        error::ErrorCode,
        refresh_velocity_spot_market,
        state::{traits::VaultDepositorBase, FeeUpdateProvider, FeeUpdateStatus},
        token_cpi::{BurnTokensCPI, TokenTransferCPI},
        validate, AccountMapProvider, TokenizedVaultDepositor, Vault, VaultDepositor,
        VaultProtocolProvider, WithdrawUnit,
    },
    anchor_lang::prelude::*,
    anchor_spl::token::{burn, transfer, Burn, Mint, Token, TokenAccount, Transfer},
    velocity::{math::safe_math::SafeMath, program::Velocity, state::user::User},
};

pub fn redeem_tokens<'info>(
    ctx: Context<'info, RedeemTokens<'info>>,
    tokens_to_burn: u64,
) -> Result<()> {
    // Book the lending interest of every market that prices NAV before any
    // account is borrowed and before NAV is snapshotted (OtterSec #136/#137). The refresh must run
    // before `load_mut` and `load_maps`. `invoke` rejects a CPI whose writable
    // accounts still have live borrows, and the maps must read refreshed data.
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

    // A matured fee update applies on every path that moves shares, so a redeem
    // cannot run under stale fee terms (OtterSec #101).
    let has_fee_update = FeeUpdateStatus::has_pending_fee_update(vault.fee_update_status);
    let mut fee_update = ctx.fee_update(vp.is_some(), has_fee_update);
    vault.validate_fee_update(&fee_update)?;

    let user = ctx.accounts.velocity_user.load()?;
    let spot_market_index = vault.spot_market_index;
    let mut maps = ctx.load_maps(
        clock.slot,
        Some(spot_market_index),
        vp.is_some(),
        has_fee_update,
        &ctx.accounts.velocity_state,
    )?;

    let vault_equity = vault.calculate_equity(&user, &mut maps)?;

    validate!(
        !vault_depositor.last_withdraw_request.pending(),
        ErrorCode::InvalidVaultDeposit,
        "Cannot redeem tokens with a pending withdraw request"
    )?;

    let total_supply_before = ctx.accounts.mint.supply;
    let spot_market = maps.spot_market_map.get_ref(&spot_market_index)?;
    let oracle = maps.oracle_map.get_price_data(&spot_market.oracle_id())?;
    // redeem_tokens crystallizes the management fee and the tokenized depositor's profit share.
    let (shares_to_transfer, mut vp) = tokenized_vault_depositor.redeem_tokens(
        &mut vault,
        &mut vp,
        &mut fee_update,
        total_supply_before,
        vault_equity,
        tokens_to_burn,
        clock.unix_timestamp,
        oracle.price,
    )?;

    // The snapshot runs after the fees above are crystallized and before the
    // transfer (OtterSec #100). It counts protocol shares because the receiving depositor's
    // profit share can mint to the manager and to the protocol. Counting both
    // lets that mint net to zero across the two snapshots. The VaultProtocol
    // provider stays alive across the transfer, so `get_manager_shares` excludes
    // protocol shares the same way before and after.
    let manager_shares_before = vault.get_manager_shares(&mut vp)?;
    let protocol_shares_before = vault.get_protocol_shares(&mut vp);
    let total_shares_before = vault_depositor
        .get_vault_shares()
        .safe_add(tokenized_vault_depositor.get_vault_shares())?
        .safe_add(manager_shares_before)?
        .safe_add(protocol_shares_before)?;

    let (shares_transferred, mut vp) = tokenized_vault_depositor.transfer_shares(
        &mut *vault_depositor,
        &mut vault,
        &mut vp,
        &mut None,
        shares_to_transfer,
        WithdrawUnit::Shares,
        vault_equity,
        clock.unix_timestamp,
        oracle.price,
    )?;

    // The transfer moved shares out of the tokenized depositor. Re-checkpoint
    // `last_vault_shares` to the new balance so a later tokenize_shares works
    // (OtterSec #105).
    tokenized_vault_depositor.checkpoint_vault_shares();

    // An empty pool leaves a cost basis with no shares behind it. A basis above
    // the current value shelters the next tokenizer from a loss. A basis below
    // it charges the next tokenizer a fee they did not earn. Clear it
    // (OtterSec #140).
    //
    // The test reads `total_supply_before` instead of the post-burn supply,
    // because the depositor is dropped before `ctx.burn` runs. This instruction
    // asserts `supply_delta == tokens_to_burn`, so the two tests are equivalent.
    if tokenized_vault_depositor.get_vault_shares() == 0 && tokens_to_burn == total_supply_before {
        msg!("tokenized depositor emptied; clearing orphaned cost basis");
        tokenized_vault_depositor.reset_orphaned_cost_basis();
    }

    let manager_shares_after = vault.get_manager_shares(&mut vp)?;
    let protocol_shares_after = vault.get_protocol_shares(&mut vp);
    let total_shares_after = vault_depositor
        .get_vault_shares()
        .safe_add(tokenized_vault_depositor.get_vault_shares())?
        .safe_add(manager_shares_after)?
        .safe_add(protocol_shares_after)?;

    validate!(
        total_shares_after.eq(&total_shares_before),
        ErrorCode::InvalidVaultSharesDetected,
        "Total vault depositor shares before != after"
    )?;

    validate!(
        shares_transferred == shares_to_transfer.into(),
        ErrorCode::InvalidVaultSharesDetected
    )?;

    let vault_name = vault.name;
    let vault_bump = vault.bump;

    drop(spot_market);
    drop(vault);
    drop(vault_depositor);
    drop(tokenized_vault_depositor);

    ctx.token_transfer(tokens_to_burn)?;
    ctx.burn(vault_name, vault_bump, tokens_to_burn)?;

    msg!(
        "Burned {} tokens from {}",
        tokens_to_burn,
        ctx.accounts.user_token_account.key()
    );

    ctx.accounts.mint.reload()?;
    let total_supply_after = ctx.accounts.mint.supply;

    validate!(
        total_supply_after < total_supply_before,
        ErrorCode::InvalidTokenization,
        "Total supply after > total supply before"
    )?;

    let supply_delta = total_supply_before.safe_sub(total_supply_after)?;
    validate!(
        supply_delta.eq(&tokens_to_burn),
        ErrorCode::InvalidTokenization,
        "Tokens burned ({}) != supply delta ({})",
        tokens_to_burn,
        supply_delta
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct RedeemTokens<'info> {
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
	)]
    pub tokenized_vault_depositor: AccountLoader<'info, TokenizedVaultDepositor>,
    #[account(
        mut,
        mint::authority = vault.key(),
		constraint = is_mint_for_tokenized_depositor(&mint.key(), &tokenized_vault_depositor)?,
    )]
    pub mint: Account<'info, Mint>,
    #[account(
        mut,
        token::authority = authority,
        token::mint = tokenized_vault_depositor.load()?.mint
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        token::authority = vault.key(),
        token::mint = tokenized_vault_depositor.load()?.mint,
        constraint = is_ata(&vault_token_account.key(), &vault.key(), &mint.key())?
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = is_user_for_vault(&vault, &velocity_user.key())?
    )]
    pub velocity_user: AccountLoader<'info, User>,
    pub token_program: Program<'info, Token>,
    /// CHECK: checked in velocity cpi
    pub velocity_state: AccountInfo<'info>,
    pub velocity_program: Program<'info, Velocity>,
}

impl<'info> TokenTransferCPI for Context<'info, RedeemTokens<'info>> {
    fn token_transfer(&self, amount: u64) -> Result<()> {
        let cpi_accounts = Transfer {
            from: self.accounts.user_token_account.to_account_info(),
            to: self.accounts.vault_token_account.to_account_info(),
            authority: self.accounts.authority.to_account_info(),
        };
        let token_program = self.accounts.token_program.key();
        let cpi_context = CpiContext::new(token_program, cpi_accounts);

        transfer(cpi_context, amount)?;

        Ok(())
    }
}

impl<'info> BurnTokensCPI for Context<'info, RedeemTokens<'info>> {
    fn burn(&self, vault_name: [u8; 32], vault_bump: u8, amount: u64) -> Result<()> {
        let signature_seeds = Vault::get_vault_signer_seeds(&vault_name, &vault_bump);
        let signers = &[&signature_seeds[..]];

        let cpi_accounts = Burn {
            mint: self.accounts.mint.to_account_info(),
            from: self.accounts.vault_token_account.to_account_info(),
            authority: self.accounts.vault.to_account_info(),
        };

        let cpi_context =
            CpiContext::new_with_signer(self.accounts.token_program.key(), cpi_accounts, signers);

        burn(cpi_context, amount)?;

        Ok(())
    }
}
