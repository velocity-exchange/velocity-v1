//! Permissionless cranks over the CLOB's removal ixs (`evict_worst_v0`,
//! `remove_expired_v0`). Removal is velocity-mediated so the maker's
//! open-order aggregates stay exact: the caller passes the maker's `User`
//! (read off the book off-chain, or staged by the resolver), the CLOB returns
//! the removed order, and velocity unwinds the remaining size — failing
//! closed if the book's tail changed and the removal hit someone else's
//! order.
//!
//! The cranks are dual-mode, selected by which `filler` rides the call:
//!
//! - **Signed keeper** (today's path): the caller signs for its own filler
//!   `User` and earns the flat removal reward from the maker, mirroring DLOB
//!   order-expiry cranks. No lamports move.
//! - **Program keeper** (relay): the filler is the protocol-owned `User`
//!   (authority = the velocity signer PDA, which nobody can sign for), so no
//!   signature is required — relay turners submit executors unsigned. The
//!   maker's reward accrues to the protocol `User`, and the caller's
//!   `authority` account is instead paid `keeper_payment_lamports` from the
//!   market's conditions-account reservoir, giving relay's `assert_paid_v0`
//!   a real fee to measure.
//!
//! Whenever the conditions account is passed, the executor also repairs the
//! expire condition's `wake_ts` hint to the true minimum over the
//! post-removal book, so a due hint goes quiet instead of waking turners
//! forever.

use {
    crate::{
        controller::{
            orders::pay_keeper_flat_reward_for_perps,
            position::{decrease_open_bids_and_asks, get_position_index},
        },
        error::ErrorCode,
        instructions::constraints::*,
        load_mut, msg,
        signer::get_signer_seeds,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            perp_market::PerpMarket,
            prop_amm::{
                clob_min_expiry, ClobEvictWorstArgsV0, ClobOrderRefV0, ClobRemoveExpiredArgsV0,
                ClobRemovedOrderV0, ClobSide, QuoterType, QuoterV0,
                CLOB_EVICT_WORST_V0_DISCRIMINATOR, CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR,
            },
            state::State,
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke_signed},
    },
};

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct CrankClobOrderRemoval<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler` (enforced by
    /// the constraint below); in program-keeper mode it is only the lamport
    /// payout target — relay's `KEEPER_PLACEHOLDER` slot — and no signature
    /// is required.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = can_crank_for_filler(&filler, &authority, &state)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The owner of the order being removed (the book's tail for evict, the
    /// hinted order for expire). Verified against the CLOB's return data —
    /// a race that removed someone else's order fails the whole crank.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Deliberately not gated on active/approved: dead books still need
    /// their resting orders reclaimed.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the protocol signer PDA — the CLOB's `place_authority`.
    #[account(address = state.load()?.signer)]
    pub velocity_signer: UncheckedAccount<'info>,
    /// The market's relay conditions account: the expiry-hint host and the
    /// lamport reservoir. Optional so signed keepers can crank markets whose
    /// conditions were never initialized; required in program-keeper mode.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
}

pub fn handle_crank_clob_evict(
    ctx: Context<CrankClobOrderRemoval>,
    market_index: u16,
    side: ClobSide,
) -> Result<()> {
    let mut data = CLOB_EVICT_WORST_V0_DISCRIMINATOR.to_vec();
    ClobEvictWorstArgsV0 { side }
        .serialize(&mut data)
        .map_err(|_| ErrorCode::DefaultError)?;
    crank_clob_removal(ctx, market_index, data, true)
}

pub fn handle_crank_clob_remove_expired(
    ctx: Context<CrankClobOrderRemoval>,
    market_index: u16,
    order_ref: ClobOrderRefV0,
) -> Result<()> {
    let mut data = CLOB_REMOVE_EXPIRED_V0_DISCRIMINATOR.to_vec();
    ClobRemoveExpiredArgsV0 { order_ref }
        .serialize(&mut data)
        .map_err(|_| ErrorCode::DefaultError)?;
    crank_clob_removal(ctx, market_index, data, false)
}

/// Shared crank body: CPI the removal, verify it hit the passed maker,
/// unwind the aggregates, pay the keeper — quote from the maker in both
/// modes, plus reservoir lamports in program-keeper mode. `is_evict` decides
/// what happens to a placed trigger's shadow slot: eviction re-arms it
/// (eager, in this same tx), expiry frees it.
fn crank_clob_removal(
    ctx: Context<CrankClobOrderRemoval>,
    market_index: u16,
    cpi_data: Vec<u8>,
    is_evict: bool,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let program_keeper_mode = ctx.accounts.filler.load()?.authority == state.signer;
    validate!(
        !program_keeper_mode || ctx.accounts.crank_conditions.is_some(),
        ErrorCode::DefaultError,
        "program-keeper crank requires the market's conditions account"
    )?;

    {
        let quoter = ctx.accounts.quoter.load()?;
        validate!(
            quoter.quoter_type == QuoterType::Clob,
            ErrorCode::DefaultError,
            "quoter entry is not a CLOB"
        )?;
        validate!(
            quoter.market == market_index,
            ErrorCode::DefaultError,
            "quoter entry is for market {}, crank is for market {}",
            quoter.market,
            market_index
        )?;
        let registered = &quoter.execute_accounts[..quoter.execute_accounts_count as usize];
        validate!(
            registered
                .iter()
                .any(|meta| meta.pubkey == ctx.accounts.clob_market.key()),
            ErrorCode::DefaultError,
            "clob market is not registered on the quoter entry"
        )?;
    }

    // CPI while no user borrows are held.
    invoke_signed(
        &Instruction {
            program_id: ctx.accounts.clob_program.key(),
            accounts: vec![
                AccountMeta::new(ctx.accounts.clob_market.key(), false),
                AccountMeta::new_readonly(ctx.accounts.velocity_signer.key(), true),
            ],
            data: cpi_data,
        },
        &[
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.velocity_signer.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ],
        &[&get_signer_seeds(&state.signer_nonce)],
    )?;
    let (writer, removed_data) =
        get_return_data().ok_or_else(|| -> anchor_lang::error::Error {
            msg!("clob removal returned no removed order");
            ErrorCode::DefaultError.into()
        })?;
    validate!(
        writer == ctx.accounts.clob_program.key(),
        ErrorCode::DefaultError,
        "clob removal return data written by {}",
        writer
    )?;
    let removed = ClobRemovedOrderV0::deserialize(&mut removed_data.as_slice()).map_err(|_| {
        msg!("clob removal returned undecodable removed order");
        ErrorCode::DefaultError
    })?;
    {
        let user = crate::load!(ctx.accounts.user)?;
        validate!(
            removed.user.authority == user.authority
                && removed.user.sub_account_id == user.sub_account_id,
            ErrorCode::DefaultError,
            "clob removed an order for {}/{} but the crank loaded {}",
            removed.user.authority,
            removed.user.sub_account_id,
            ctx.accounts.user.key()
        )?;
    }

    // Pay the keeper from the maker first (the same flat reward DLOB order
    // expiry pays; in program-keeper mode the filler is the protocol User),
    // THEN unwind — unwinding an otherwise-empty position frees its slot,
    // and the reward needs to resolve it.
    {
        let mut user = load_mut!(ctx.accounts.user)?;
        let mut filler = load_mut!(ctx.accounts.filler)?;
        let mut market = load_mut!(ctx.accounts.perp_market)?;
        validate!(
            market.market_index == market_index,
            ErrorCode::DefaultError,
            "perp market {} passed for market {}",
            market.market_index,
            market_index
        )?;
        pay_keeper_flat_reward_for_perps(
            &mut user,
            Some(&mut filler),
            &mut market,
            state.perp_fee_structure.flat_filler_fee,
            clock.slot,
        )?;

        let position_index = get_position_index(&user.perp_positions, market_index)?;
        decrease_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &removed.side.to_position_direction(),
            removed.base_asset_amount,
            true,
        )?;
        user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
            .open_orders
            .saturating_sub(1);
        user.decrement_open_orders(false);

        // A placed trigger's shadow slot follows its CLOB order: eviction
        // re-arms it (with the unfilled remainder, edge-gated on a price
        // recross), expiry frees it.
        if is_evict {
            user.re_arm_placed_trigger_slot(
                market_index,
                removed.order_id,
                removed.base_asset_amount,
                clock.slot,
            )?;
        } else {
            user.release_placed_trigger_slot(
                market_index,
                removed.order_id,
                crate::state::user::OrderStatus::Canceled,
            );
        }
    }

    if let Some(conditions_loader) = &ctx.accounts.crank_conditions {
        // Repair the expire hint against the post-removal book, so a due
        // hint goes quiet once the last expiring order is gone.
        let true_min = clob_min_expiry(&ctx.accounts.clob_market.try_borrow_data()?);
        let payment = {
            let mut conditions = load_mut!(conditions_loader)?;
            conditions.repair_expiry(true_min)?;
            conditions.keeper_payment_lamports
        };
        if program_keeper_mode {
            let conditions_info = conditions_loader.to_account_info();
            let rent_minimum = Rent::get()?.minimum_balance(conditions_info.data_len());
            ClobCrankConditionsV0::pay_keeper_lamports(
                &conditions_info,
                &ctx.accounts.authority.to_account_info(),
                payment,
                rent_minimum,
            )?;
        }
    }

    msg!(
        "cranked clob removal of order {} for user {}",
        removed.order_id,
        ctx.accounts.user.key()
    );
    Ok(())
}
