//! The devnet-only account wipe.
//!
//! A layout-breaking upgrade leaves every account unreadable. This instruction
//! closes them so the deploy scripts can create them again. It is compiled out
//! of a mainnet build.

use super::*;

// ----- Force wipe (non-mainnet only) -----
//
// One-shot escape hatch for devnet: closes velocity-owned PDAs whose on-chain
// layout no longer matches the program (e.g. after a layout-breaking upgrade).
// Bypasses `AccountLoader::try_from` size checks by reading State's admin
// pubkey directly from raw bytes — the first pubkey field lives at offset
// 8..40 in both the legacy `#[account]` State (admin) and the new zero-copy
// State (cold_admin), so this admin gate works across layouts.
//
// Compiled out of mainnet builds via `cfg(not(feature = "mainnet-beta"))`.

#[cfg(not(feature = "mainnet-beta"))]
pub fn handle_force_wipe_accounts_devnet<'info>(
    ctx: Context<'info, ForceWipeAccountsDevnet<'info>>,
    velocity_signer_nonce: u8,
) -> Result<()> {
    require_stored_admin(
        &ctx.accounts.state.to_account_info(),
        ctx.accounts.admin.key(),
    )?;

    let admin_ai = ctx.accounts.admin.to_account_info();
    let velocity_signer = ctx.accounts.velocity_signer.to_account_info();
    let token_program_id = ctx.accounts.token_program.key();

    let velocity_section_start = close_wiped_token_vaults(
        ctx.remaining_accounts,
        &admin_ai,
        &velocity_signer,
        token_program_id,
        velocity_signer_nonce,
    )?;

    drain_wiped_velocity_accounts(&ctx.remaining_accounts[velocity_section_start..], &admin_ai)
}

/// Reads the admin out of the raw State account and holds the caller to it.
///
/// The wipe runs after a layout-breaking upgrade, when Anchor can no longer
/// deserialize State. Both the old and the new layout keep the admin pubkey at
/// offset 8 to 40, so the check reads it by offset.
fn require_stored_admin(state_ai: &AccountInfo, admin: Pubkey) -> Result<()> {
    require_keys_eq!(*state_ai.owner, crate::ID, ErrorCode::DefaultError);

    let data = state_ai.try_borrow_data()?;
    require!(data.len() >= 40, ErrorCode::DefaultError);
    let mut admin_bytes = [0u8; 32];
    admin_bytes.copy_from_slice(&data[8..40]);
    require_keys_eq!(Pubkey::from(admin_bytes), admin, ErrorCode::Unauthorized);

    Ok(())
}

/// Closes the token vaults at the head of the target list, and returns where
/// the velocity-owned accounts begin.
///
/// The targets arrive in `(vault, mint)` pairs, because the token program
/// refuses to close a vault that still holds a balance and a burn needs the
/// mint. The velocity-owned accounts follow every pair, so the first account
/// the token program does not own ends this pass.
fn close_wiped_token_vaults<'info>(
    targets: &[AccountInfo<'info>],
    admin_ai: &AccountInfo<'info>,
    velocity_signer: &AccountInfo<'info>,
    token_program_id: Pubkey,
    velocity_signer_nonce: u8,
) -> Result<usize> {
    use anchor_spl::token_interface;

    let signer_seeds = crate::signer::get_signer_seeds(&velocity_signer_nonce);
    let cpi_signers = &[&signer_seeds[..]];

    let mut i = 0;
    while i < targets.len() {
        let target = &targets[i];
        if *target.owner != token_program_id {
            break; // start of velocity-owned section
        }
        if target.lamports() == 0 {
            i += 1;
            continue;
        }
        // pair: next account is the mint
        let mint_ai = targets.get(i + 1).ok_or(ErrorCode::DefaultError)?;
        require_keys_eq!(*mint_ai.owner, token_program_id, ErrorCode::DefaultError);

        // read current token amount (offset 64..72 in SPL token account layout)
        let amount = {
            let data = target.try_borrow_data()?;
            require!(data.len() >= 72, ErrorCode::DefaultError);
            u64::from_le_bytes(data[64..72].try_into().unwrap())
        };

        if amount > 0 {
            let burn_accounts = token_interface::Burn {
                mint: mint_ai.clone(),
                from: target.clone(),
                authority: velocity_signer.clone(),
            };
            let burn_ctx =
                CpiContext::new_with_signer(token_program_id, burn_accounts, cpi_signers);
            token_interface::burn(burn_ctx, amount)?;
            msg!("burned {} from {}", amount, target.key());
        }

        let close_accounts = token_interface::CloseAccount {
            account: target.clone(),
            destination: admin_ai.clone(),
            authority: velocity_signer.clone(),
        };
        let close_ctx = CpiContext::new_with_signer(token_program_id, close_accounts, cpi_signers);
        token_interface::close_account(close_ctx)?;
        msg!("closed token vault {}", target.key());

        i += 2; // skip past the mint
    }

    Ok(i)
}

/// Moves every lamport of a velocity-owned target to the admin. The runtime
/// collects an account with no lamports at the end of the transaction, so
/// zeroing the balance is what removes it.
fn drain_wiped_velocity_accounts(targets: &[AccountInfo], admin_ai: &AccountInfo) -> Result<()> {
    use anchor_lang::solana_program::system_program;

    for target in targets {
        if *target.owner == system_program::ID || target.lamports() == 0 {
            msg!("skip {} (already empty)", target.key());
            continue;
        }
        if *target.owner != crate::ID {
            msg!(
                "skip {} (owner {} not velocity)",
                target.key(),
                target.owner,
            );
            continue;
        }
        let take = target.lamports();
        **admin_ai.try_borrow_mut_lamports()? = admin_ai
            .lamports()
            .checked_add(take)
            .ok_or_else(math_error!())?;
        **target.try_borrow_mut_lamports()? = 0;
        msg!("wiped {} (reclaimed {} lamports)", target.key(), take);
    }

    Ok(())
}

#[cfg(not(feature = "mainnet-beta"))]
#[derive(Accounts)]
pub struct ForceWipeAccountsDevnet<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    /// CHECK: read raw bytes manually; both old and new State layouts have the
    /// (cold-)admin pubkey at offset 8..40.
    pub state: UncheckedAccount<'info>,
    /// CHECK: PDA seeded by [b"velocity_signer", nonce]. Verified by Token Program
    /// at CPI time when closing token vaults; ignored otherwise.
    pub velocity_signer: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    // Targets are passed via `remaining_accounts` so a single call can wipe
    // many accounts in one tx. Velocity-owned PDAs are drained; token-owned vaults
    // are closed via CPI (rent → admin).
}
