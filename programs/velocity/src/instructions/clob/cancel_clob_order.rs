//! Cancel a resting CLOB order. The CLOB verifies the order belongs to the
//! passed `User` (velocity has already verified the signer controls that
//! `User`) and returns the removed order, whose remaining size velocity
//! unwinds from the open-order aggregates. Deliberately NOT gated on the
//! quoter entry's active/approved flags — a maker must always be able to
//! pull their orders off a killed or de-listed book.

use {
    crate::{
        controller::position::{decrease_open_bids_and_asks, get_position_index},
        error::ErrorCode,
        instructions::constraints::*,
        load_mut, msg,
        signer::get_signer_seeds,
        state::{
            prop_amm::{
                ClobCancelOrderArgsV0, ClobOrderRefV0, ClobRemovedOrderV0, QuoterType, QuoterV0,
                CLOB_CANCEL_ORDER_V0_DISCRIMINATOR,
            },
            state::State,
            user::User,
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
pub struct CancelClobOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
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
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CancelClobOrderParams {
    pub market_index: u16,
    /// The hint returned at placement; the CLOB fails closed on a stale one.
    pub order_ref: ClobOrderRefV0,
}

pub fn handle_cancel_clob_order(
    ctx: Context<CancelClobOrder>,
    params: CancelClobOrderParams,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    {
        let quoter = ctx.accounts.quoter.load()?;
        validate!(
            quoter.quoter_type == QuoterType::Clob,
            ErrorCode::DefaultError,
            "quoter entry is not a CLOB"
        )?;
        validate!(
            quoter.market == params.market_index,
            ErrorCode::DefaultError,
            "quoter entry is for market {}, order is for market {}",
            quoter.market,
            params.market_index
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

    // CPI cancel while no user borrows are held.
    let mut data = CLOB_CANCEL_ORDER_V0_DISCRIMINATOR.to_vec();
    ClobCancelOrderArgsV0 {
        order_ref: params.order_ref,
    }
    .serialize(&mut data)
    .map_err(|_| ErrorCode::DefaultError)?;
    invoke_signed(
        &Instruction {
            program_id: ctx.accounts.clob_program.key(),
            accounts: vec![
                AccountMeta::new(ctx.accounts.clob_market.key(), false),
                AccountMeta::new_readonly(ctx.accounts.velocity_signer.key(), true),
                AccountMeta::new_readonly(ctx.accounts.user.key(), false),
            ],
            data,
        },
        &[
            ctx.accounts.clob_market.to_account_info(),
            ctx.accounts.velocity_signer.to_account_info(),
            ctx.accounts.user.to_account_info(),
            ctx.accounts.clob_program.to_account_info(),
        ],
        &[&get_signer_seeds(&state.signer_nonce)],
    )?;
    let (writer, removed_data) =
        get_return_data().ok_or_else(|| -> anchor_lang::error::Error {
            msg!("clob cancel returned no removed order");
            ErrorCode::DefaultError.into()
        })?;
    validate!(
        writer == ctx.accounts.clob_program.key(),
        ErrorCode::DefaultError,
        "clob cancel return data written by {}",
        writer
    )?;
    let removed = ClobRemovedOrderV0::deserialize(&mut removed_data.as_slice()).map_err(|_| {
        msg!("clob cancel returned undecodable removed order");
        ErrorCode::DefaultError
    })?;
    validate!(
        removed.user == ctx.accounts.user.key(),
        ErrorCode::DefaultError,
        "clob cancelled an order for {} instead of the passed user",
        removed.user
    )?;

    // Unwind the removed order's remaining size from the aggregates the
    // placement reserved.
    let mut user = load_mut!(ctx.accounts.user)?;
    let position_index = get_position_index(&user.perp_positions, params.market_index)?;
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
    // If this was a placed trigger's live order, its shadow slot frees too —
    // cancelling here is how a user cancels a placed trigger.
    user.release_placed_trigger_slot(
        params.market_index,
        removed.order_id,
        crate::state::user::OrderStatus::Canceled,
    );
    user.update_last_active_slot(clock.slot);

    msg!(
        "cancelled clob order {} for user {}",
        removed.order_id,
        ctx.accounts.user.key()
    );
    Ok(())
}
