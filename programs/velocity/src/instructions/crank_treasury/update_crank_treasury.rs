//! Price the treasury. The two levels say what a refill fills a reservoir up
//! to, and what balance wakes the refill.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        state::{
            crank_treasury::{CrankTreasuryV0, CRANK_TREASURY_PDA_SEED},
            state::State,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateCrankTreasury<'info> {
    #[account(mut, seeds = [CRANK_TREASURY_PDA_SEED], bump)]
    pub treasury: AccountLoader<'info, CrankTreasuryV0>,
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
}

/// Both levels are counted in cranks and not in lamports, so one setting serves
/// every market. A market whose cranks cost more then holds a proportionally
/// larger balance.
///
/// A refill reads the target, so a new target reaches every market at once. The
/// attach resolves the watermark to lamports and writes it onto the market,
/// because the market's wake condition carries that threshold. A new watermark
/// therefore reaches a market on that market's next attach.
///
/// This instruction does not set what a refill pays. That payment is priced from
/// the network rails like every other crank. It is stored on the market whose
/// reservoir the refill fills, because the condition that advertises it lives
/// there.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateCrankTreasuryArgs {
    /// Cranks' worth of lamports a refill fills a reservoir up to.
    pub refill_target_cranks: u16,
    /// Cranks' worth of lamports at or under which a refill wakes.
    pub refill_watermark_cranks: u16,
}

pub fn handle_update_crank_treasury(
    ctx: Context<UpdateCrankTreasury>,
    args: UpdateCrankTreasuryArgs,
) -> Result<()> {
    let UpdateCrankTreasuryArgs {
        refill_target_cranks,
        refill_watermark_cranks,
    } = args;

    validate!(
        refill_watermark_cranks > 0,
        ErrorCode::CrankTreasuryWatermarkInvalid,
        "a zero watermark never wakes a refill"
    )?;

    // The target must sit strictly above the level that wakes the refill. A
    // target at the watermark leaves the reservoir still due the moment it is
    // filled, and the condition would stay due against an executor that can
    // only revert.
    validate!(
        refill_target_cranks > refill_watermark_cranks,
        ErrorCode::CrankTreasuryWatermarkInvalid,
        "refill target of {} cranks must exceed the {} that wakes it",
        refill_target_cranks,
        refill_watermark_cranks
    )?;

    let mut treasury = ctx.accounts.treasury.load_mut()?;
    treasury.refill_target_cranks = refill_target_cranks;
    treasury.refill_watermark_cranks = refill_watermark_cranks;
    msg!(
        "crank treasury wakes a refill under {} cranks and fills to {}; markets take a new \
         watermark on their next attach",
        refill_watermark_cranks,
        refill_target_cranks
    );

    Ok(())
}
