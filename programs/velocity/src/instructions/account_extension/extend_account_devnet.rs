//! Grow a zero-copy account to an arbitrary larger size. Devnet and test
//! builds only. It exercises the extension flow, and a client's tolerance of
//! an extended account, before a real struct extension exists.

use {
    crate::{
        auth::check_hot,
        error::ErrorCode,
        instructions::account_extension::extension_target_len,
        state::state::{HotRole, State},
        validate,
    },
    anchor_lang::{
        prelude::*,
        system_program::{transfer, Transfer},
    },
};

#[derive(Accounts)]
pub struct ExtendAccountDevnet<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(constraint = check_hot(&authority.key(), &state, HotRole::AccountExtension)?)]
    pub authority: Signer<'info>,
    /// CHECK: must be velocity-owned. The handler requires a supported
    /// zero-copy discriminator.
    #[account(mut, owner = crate::ID)]
    pub account: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Grow `account` to `new_len` bytes. The handler only grows an account. The
/// payer tops up the rent and the runtime zero-fills the tail.
///
/// The caller chooses the target size, where `extend_account` derives it from
/// the compiled-in struct. A test or a devnet run can therefore reproduce the
/// state right after a struct-growing upgrade.
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
