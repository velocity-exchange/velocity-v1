//! Top a market's crank reservoir back up out of the protocol treasury.
//!
//! Permissionless and relay-cranked, like the work it keeps funded. A
//! reservoir mirrors its spendable balance into its own account data, the
//! refill condition wakes when that falls to the treasury's watermark, and
//! this instruction moves the difference. Nobody watches per-market balances.
//!
//! The caller is paid out of the treasury rather than out of the reservoir it
//! just filled, because the reservoir is by definition low at that moment.

use {
    super::helpers::crank_common::ResolveClobCrank,
    crate::{
        error::ErrorCode,
        instructions::relay_harness::StagedCall,
        math::safe_math::SafeMath,
        state::{
            clob_crank::ClobCrankConditionsV0,
            crank_treasury::{CrankTreasuryV0, CRANK_TREASURY_PDA_SEED},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// Stage a refill when this market's reservoir has fallen to the watermark.
///
/// The condition already fired on the mirrored balance, so this reads the real
/// one and confirms. A mirror can lag its account — a payment writes both, but
/// a plain lamport transfer into the reservoir writes only the balance — so the
/// wake is a hint and this is the check.
pub(super) fn stage_refill(ctx: &Context<ResolveClobCrank>) -> Result<Option<StagedCall>> {
    let (market_index, watermark, target) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            conditions.market_index,
            conditions.refill_watermark_lamports,
            ctx.accounts
                .treasury
                .load()?
                .refill_target(conditions.crank_payments.max_payment())?,
        )
    };
    let conditions_info = ctx.accounts.crank_conditions.to_account_info();
    let spendable = conditions_info
        .lamports()
        .saturating_sub(Rent::get()?.minimum_balance(conditions_info.data_len()));
    if spendable > watermark {
        return Ok(None);
    }
    Ok(Some(
        crate::staged_call!(RefillCrankReservoir {
            treasury: crate::state::pdas::crank_treasury(),
            crank_conditions: ctx.accounts.crank_conditions.key(),
            authority: crate::state::pdas::keeper_placeholder(),
        })
        .arg(market_index)?,
    ))
}

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct RefillCrankReservoir<'info> {
    /// The protocol's lamport pool.
    #[account(mut, seeds = [CRANK_TREASURY_PDA_SEED], bump)]
    pub treasury: AccountLoader<'info, CrankTreasuryV0>,
    /// The market reservoir being filled.
    #[account(
        mut,
        seeds = [
            crate::state::clob_crank::CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    /// CHECK: the lamport payout target — relay's keeper-placeholder slot. It
    /// never signs, so a turner can name a payout account that is not the key
    /// paying for the transaction.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
}

pub fn handle_refill_crank_reservoir(
    ctx: Context<RefillCrankReservoir>,
    market_index: u16,
) -> Result<()> {
    let (refill_payment, watermark, target) = {
        let treasury = ctx.accounts.treasury.load()?;
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            u64::from(conditions.crank_payments.refill),
            conditions.refill_watermark_lamports,
            treasury.refill_target(conditions.crank_payments.max_payment())?,
        )
    };

    let conditions_info = ctx.accounts.crank_conditions.to_account_info();
    let conditions_rent = Rent::get()?.minimum_balance(conditions_info.data_len());
    let spendable = conditions_info.lamports().saturating_sub(conditions_rent);

    // The no-work guard. Without it a caller could refill a full reservoir on
    // repeat and draw the keeper payment each time, which is the treasury
    // paying to move its own lamports.
    validate!(
        spendable <= watermark,
        ErrorCode::CrankReservoirNotLow,
        "reservoir holds {} spendable lamports, above the {} watermark",
        spendable,
        watermark
    )?;

    let treasury_info = ctx.accounts.treasury.to_account_info();
    let treasury_rent = Rent::get()?.minimum_balance(treasury_info.data_len());
    // The keeper is paid from the same pool, so its fee is reserved before the
    // refill is sized. A refill that consumed the last lamports would leave
    // nothing to pay the caller and the whole transaction would fail.
    let reserved = treasury_rent.safe_add(refill_payment)?;
    let available = treasury_info.lamports().saturating_sub(reserved);
    // A target at or below what the reservoir already holds adds nothing, and
    // an unpriced treasury has no target. Either way there is no work, and
    // paying for none is how a treasury is drained by repetition.
    validate!(
        target > spendable,
        ErrorCode::CrankReservoirNotLow,
        "reservoir holds {} spendable lamports against a refill target of {}",
        spendable,
        target
    )?;
    let amount = target.safe_sub(spendable)?.min(available);
    // A refill has to leave the reservoir above the level that woke it. A
    // treasury too poor to manage that would otherwise dribble: each partial
    // refill pays a keeper, leaves the condition due, and is cranked again,
    // spending more on the payments than it moves. Reverting here holds the
    // remaining lamports for the markets that can still be served, and says
    // plainly that the treasury needs funding.
    validate!(
        spendable.safe_add(amount)? > watermark,
        ErrorCode::InsufficientCrankTreasury,
        "treasury can spare {} lamports for market {}, short of the {} watermark",
        available,
        market_index,
        watermark
    )?;

    CrankTreasuryV0::pay_out(&treasury_info, &conditions_info, amount, treasury_rent)?;
    // The mirror is what the refill condition reads. Restating it here is what
    // takes the condition back below its wake, so the crank does not re-fire.
    ClobCrankConditionsV0::write_spendable_mirror(&conditions_info, conditions_rent)?;

    let paid = CrankTreasuryV0::pay_out(
        &treasury_info,
        &ctx.accounts.authority.to_account_info(),
        refill_payment,
        treasury_rent,
    )?;

    let mut treasury = ctx.accounts.treasury.load_mut()?;
    treasury.total_refilled = treasury.total_refilled.saturating_add(amount);
    treasury.total_paid = treasury.total_paid.saturating_add(paid);

    msg!(
        "refilled market {} reservoir with {} lamports, paid {} to {}",
        market_index,
        amount,
        paid,
        ctx.accounts.authority.key()
    );
    Ok(())
}
