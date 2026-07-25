//! Devnet-only grow of a zero-copy account to an arbitrary larger size, so the
//! extension flow (and client tolerance of extended accounts) can be exercised
//! end to end before a real struct extension exists.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer, Transfer};

use crate::error::ErrorCode;
use crate::instructions::account_extension::extension_target_len;
use crate::validate;

#[derive(Accounts)]
pub struct ExtendAccountDevnet<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: must be velocity-owned; the handler requires a supported
    /// zero-copy discriminator
    #[account(mut, owner = crate::ID)]
    pub account: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Grow `account` to `new_len` bytes (grow-only, rent topped up by the payer,
/// tail zero-filled). Unlike `extend_account`, the target size is caller-chosen
/// rather than derived from the compiled-in struct, which lets tests and devnet
/// simulate the state right after a struct-growing upgrade.
pub fn handle_extend_account_devnet(ctx: Context<ExtendAccountDevnet>, new_len: u64) -> Result<()> {
    let account = &ctx.accounts.account;
    let new_len = new_len as usize;

    {
        let data = account.try_borrow_data()?;
        validate!(
            data.len() >= 8 && extension_target_len(&data[..8]).is_some(),
            ErrorCode::InvalidAccountExtension,
            "account is not a supported zero-copy account"
        )?;
    }

    let current_len = account.data_len();
    validate!(
        new_len >= current_len,
        ErrorCode::InvalidAccountExtension,
        "extension can only grow an account (current={}, requested={})",
        current_len,
        new_len
    )?;
    if new_len == current_len {
        return Ok(());
    }

    let required_lamports = Rent::get()?.minimum_balance(new_len);
    let shortfall = required_lamports.saturating_sub(account.lamports());
    if shortfall > 0 {
        transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: account.to_account_info(),
                },
            ),
            shortfall,
        )?;
    }

    account.resize(new_len).map_err(Into::<Error>::into)?;

    Ok(())
}
