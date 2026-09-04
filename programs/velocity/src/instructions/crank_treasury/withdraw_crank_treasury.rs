//! Take lamports back out of the treasury.
//!
//! Funding is a plain transfer and needs no instruction, but recovering an
//! overfunded treasury does: the account is program-owned, so only the program
//! can move its lamports back out.

use {
    crate::{
        auth::check_warm,
        state::{
            crank_treasury::{CrankTreasuryV0, CRANK_TREASURY_PDA_SEED},
            state::State,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct WithdrawCrankTreasury<'info> {
    #[account(mut, seeds = [CRANK_TREASURY_PDA_SEED], bump)]
    pub treasury: AccountLoader<'info, CrankTreasuryV0>,
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct WithdrawCrankTreasuryArgs {
    pub lamports: u64,
}

pub fn handle_withdraw_crank_treasury(
    ctx: Context<WithdrawCrankTreasury>,
    args: WithdrawCrankTreasuryArgs,
) -> Result<()> {
    let WithdrawCrankTreasuryArgs { lamports } = args;
    let treasury = ctx.accounts.treasury.to_account_info();
    let rent_minimum = Rent::get()?.minimum_balance(treasury.data_len());
    // `pay_out` holds the account above its own rent exemption, so a withdraw
    // cannot close the treasury out from under the markets that draw on it.
    let paid = CrankTreasuryV0::pay_out(
        &treasury,
        &ctx.accounts.admin.to_account_info(),
        lamports,
        rent_minimum,
    )?;
    msg!("withdrew {} lamports from the crank treasury", paid);
    Ok(())
}
