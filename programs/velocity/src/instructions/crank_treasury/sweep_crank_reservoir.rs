//! Move lamports from a market's crank reservoir back to the treasury.
//!
//! Lamports reach a reservoir through the refill and leave it as crank
//! payments. Without this instruction they travel one way only. A market that
//! was over-provisioned, or one whose book is retired, would hold them for
//! good.
//!
//! Only the admin may sweep, unlike the refill. Taking funds out of a reservoir
//! is not work anyone should be paid to do. A reservoir swept below its
//! watermark refills itself.

use {
    crate::{
        auth::check_warm,
        state::{
            clob_crank::ClobCrankConditionsV0,
            crank_treasury::{CrankTreasuryV0, CRANK_TREASURY_PDA_SEED},
            state::State,
        },
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct SweepCrankReservoirArgs {
    pub market_index: u16,
    pub lamports: u64,
}

#[derive(Accounts)]
#[instruction(args: SweepCrankReservoirArgs)]
pub struct SweepCrankReservoir<'info> {
    #[account(mut, seeds = [CRANK_TREASURY_PDA_SEED], bump)]
    pub treasury: AccountLoader<'info, CrankTreasuryV0>,
    #[account(
        mut,
        seeds = [
            crate::state::clob_crank::CLOB_CRANK_CONDITIONS_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
}

pub fn handle_sweep_crank_reservoir(
    ctx: Context<SweepCrankReservoir>,
    args: SweepCrankReservoirArgs,
) -> Result<()> {
    let SweepCrankReservoirArgs {
        market_index,
        lamports,
    } = args;
    let conditions = ctx.accounts.crank_conditions.to_account_info();
    let rent_minimum = Rent::get()?.minimum_balance(conditions.data_len());
    // The same helper the reservoir pays keepers with, so a sweep is held
    // above rent exactly as a payment is.
    let swept = ClobCrankConditionsV0::pay_keeper_lamports(
        &conditions,
        &ctx.accounts.treasury.to_account_info(),
        lamports,
        rent_minimum,
    )?;
    // `pay_keeper_lamports` already wrote the spendable mirror, so the refill
    // condition sees the swept balance. The refill tops the reservoir back up
    // when the sweep took it below the watermark.
    let mut treasury = ctx.accounts.treasury.load_mut()?;
    treasury.total_refilled = treasury.total_refilled.saturating_sub(swept);
    msg!(
        "swept {} lamports from market {} reservoir to the treasury",
        swept,
        market_index
    );
    Ok(())
}
