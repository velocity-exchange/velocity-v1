//! The accounts a signed-message order flows through.
//!
//! `SignedMsgUserOrders` holds the replay-protection ids of every order an
//! authority signs off chain. It is authority-scoped, so every subaccount
//! shares it. `SignedMsgWsDelegates` names the keys allowed to submit on the
//! authority's behalf.
//!
//! Resize and delete take the record as an unchecked account, because a
//! record in the legacy layout does not decode as `Account<SignedMsgUserOrders>`.
//! See `state::signed_msg_user` for the layouts.

use {
    super::*,
    crate::state::signed_msg_user::{
        validate_signed_msg_user_orders_account, SignedMsgUserOrdersSnapshot,
        SIGNED_MSG_USER_ORDERS_VERSION,
    },
};

pub fn handle_initialize_signed_msg_user_orders<'c: 'info, 'info>(
    ctx: Context<'info, InitializeSignedMsgUserOrders<'info>>,
    num_orders: u16,
) -> Result<()> {
    let signed_msg_user_orders = &mut ctx.accounts.signed_msg_user_orders;
    signed_msg_user_orders.authority_pubkey = ctx.accounts.authority.key();
    signed_msg_user_orders.version = SIGNED_MSG_USER_ORDERS_VERSION;
    signed_msg_user_orders
        .signed_msg_order_data
        .resize_with(num_orders as usize, SignedMsgOrderId::default);
    signed_msg_user_orders.validate()?;
    Ok(())
}

/// Resize the record to `num_orders` entries. The record is read in either
/// layout, and the write migrates a legacy record to the current one.
pub fn handle_resize_signed_msg_user_orders<'c: 'info, 'info>(
    ctx: Context<'info, ResizeSignedMsgUserOrders<'info>>,
    num_orders: u16,
) -> Result<()> {
    let account = ctx.accounts.signed_msg_user_orders.to_account_info();
    let mut snapshot = SignedMsgUserOrdersSnapshot::read(&account)?;
    // SignedMsgUserOrders is authority-scoped and shared across the authority's subaccounts, and
    // its replay-protection UUIDs cover every one. Shrinking it evicts active UUIDs of other
    // subaccounts and re-enables replay of their signed orders, so only the authority may shrink
    // it, not a per-subaccount delegate. Anyone else may only grow it and pays the rent.
    if ctx.accounts.payer.key != ctx.accounts.authority.key {
        validate!(
            num_orders as usize >= snapshot.header_len as usize,
            ErrorCode::InvalidSignedMsgUserOrdersResize,
            "Invalid shrinking resize for payer != user authority"
        )?;
    }

    snapshot.resize(num_orders as usize)?;
    resize_paid_by(
        &account,
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.system_program,
        SignedMsgUserOrders::space(num_orders as usize),
    )?;

    snapshot.write(&account)?;

    Ok(())
}

/// Resize `account` the way Anchor's `realloc` constraint does. The payer
/// covers the rent shortfall of a growth, and a shrink refunds the excess
/// rent to the payer.
fn resize_paid_by<'info>(
    account: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    system_program: &Program<'info, System>,
    new_space: usize,
) -> Result<()> {
    let rent_minimum = Rent::get()?.minimum_balance(new_space);
    if new_space > account.data_len() {
        let shortfall = rent_minimum.saturating_sub(account.lamports());
        if shortfall > 0 {
            anchor_lang::system_program::transfer(
                CpiContext::new(
                    system_program.key(),
                    anchor_lang::system_program::Transfer {
                        from: payer.clone(),
                        to: account.clone(),
                    },
                ),
                shortfall,
            )?;
        }
    } else if new_space < account.data_len() {
        let refund = account.lamports().saturating_sub(rent_minimum);
        **account.try_borrow_mut_lamports()? -= refund;
        **payer.try_borrow_mut_lamports()? += refund;
    }

    account.resize(new_space).map_err(Into::<Error>::into)?;

    Ok(())
}

pub fn handle_initialize_signed_msg_ws_delegates<'c: 'info, 'info>(
    ctx: Context<'info, InitializeSignedMsgWsDelegates<'info>>,
    delegates: Vec<Pubkey>,
) -> Result<()> {
    ctx.accounts
        .signed_msg_ws_delegates
        .delegates
        .extend(delegates);
    Ok(())
}

pub fn handle_change_signed_msg_ws_delegate_status<'c: 'info, 'info>(
    ctx: Context<'info, ChangeSignedMsgWsDelegateStatus<'info>>,
    delegate: Pubkey,
    add: bool,
) -> Result<()> {
    if add {
        ctx.accounts
            .signed_msg_ws_delegates
            .delegates
            .push(delegate);
    } else {
        ctx.accounts
            .signed_msg_ws_delegates
            .delegates
            .retain(|&pubkey| pubkey != delegate);
    }

    Ok(())
}

/// Close the record to the authority, the way Anchor's `close` constraint
/// does. The record is read unchecked, so a legacy record closes too.
pub fn handle_delete_signed_msg_user_orders(ctx: Context<DeleteSignedMsgUserOrders>) -> Result<()> {
    let account = ctx.accounts.signed_msg_user_orders.to_account_info();
    validate_signed_msg_user_orders_account(&account)?;

    let authority = ctx.accounts.authority.to_account_info();
    let closed_lamports = account.lamports();
    **authority.try_borrow_mut_lamports()? = authority
        .lamports()
        .checked_add(closed_lamports)
        .ok_or(ErrorCode::MathError)?;
    **account.try_borrow_mut_lamports()? = 0;

    account.assign(&anchor_lang::system_program::ID);
    account.resize(0).map_err(Into::<Error>::into)?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(num_orders: u16)]
pub struct InitializeSignedMsgUserOrders<'info> {
    #[account(
        init,
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), authority.key().as_ref()],
        space = SignedMsgUserOrders::space(num_orders as usize),
        bump,
        payer = payer
    )]
    pub signed_msg_user_orders: Box<Account<'info, SignedMsgUserOrders>>,
    /// CHECK: Just a normal authority account
    pub authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(num_orders: u16)]
pub struct ResizeSignedMsgUserOrders<'info> {
    /// CHECK: the handler checks the owner and the discriminator in either layout.
    #[account(
        mut,
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
    )]
    pub signed_msg_user_orders: UncheckedAccount<'info>,
    /// CHECK: authority
    pub authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(delegates: Vec<Pubkey>)]
pub struct InitializeSignedMsgWsDelegates<'info> {
    #[account(
        seeds = [SIGNED_MSG_WS_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
        init,
        space = 8 + 4 + delegates.len() * 32,
        payer=authority
    )]
    pub signed_msg_ws_delegates: Account<'info, SignedMsgWsDelegates>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(_delegate: Pubkey, add: bool)]
pub struct ChangeSignedMsgWsDelegateStatus<'info> {
    #[account(
        mut,
        seeds = [SIGNED_MSG_WS_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
        realloc = SignedMsgWsDelegates::space(&signed_msg_ws_delegates, add),
        realloc::payer = authority,
        realloc::zero = false,
    )]
    pub signed_msg_ws_delegates: Account<'info, SignedMsgWsDelegates>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DeleteSignedMsgUserOrders<'info> {
    /// CHECK: the handler checks the owner and the discriminator in either layout.
    #[account(
        mut,
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
    )]
    pub signed_msg_user_orders: UncheckedAccount<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
}
