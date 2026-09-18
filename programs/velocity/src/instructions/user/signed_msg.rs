//! The accounts a signed-message order flows through.
//!
//! `SignedMsgUserOrders` holds the replay-protection ids of every order an
//! authority signs off chain. It is authority-scoped, so every subaccount
//! shares it. `SignedMsgWsDelegates` names the keys allowed to submit on the
//! authority's behalf.

use super::*;

pub fn handle_initialize_signed_msg_user_orders<'c: 'info, 'info>(
    ctx: Context<'info, InitializeSignedMsgUserOrders<'info>>,
    num_orders: u16,
) -> Result<()> {
    let signed_msg_user_orders = &mut ctx.accounts.signed_msg_user_orders;
    signed_msg_user_orders.authority_pubkey = ctx.accounts.authority.key();
    signed_msg_user_orders
        .signed_msg_order_data
        .resize_with(num_orders as usize, SignedMsgOrderId::default);
    signed_msg_user_orders.validate()?;
    Ok(())
}

pub fn handle_resize_signed_msg_user_orders<'c: 'info, 'info>(
    ctx: Context<'info, ResizeSignedMsgUserOrders<'info>>,
    num_orders: u16,
) -> Result<()> {
    let signed_msg_user_orders = &mut ctx.accounts.signed_msg_user_orders;
    // SignedMsgUserOrders is authority-scoped and shared across the authority's subaccounts, and
    // its replay-protection UUIDs cover every one. Shrinking it evicts active UUIDs of other
    // subaccounts and re-enables replay of their signed orders, so only the authority may shrink
    // it, not a per-subaccount delegate. Anyone else may only grow it and pays the rent.
    if ctx.accounts.payer.key != ctx.accounts.authority.key {
        validate!(
            num_orders as usize >= signed_msg_user_orders.signed_msg_order_data.len(),
            ErrorCode::InvalidSignedMsgUserOrdersResize,
            "Invalid shrinking resize for payer != user authority"
        )?;
    }

    signed_msg_user_orders
        .signed_msg_order_data
        .resize_with(num_orders as usize, SignedMsgOrderId::default);
    signed_msg_user_orders.validate()?;
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

pub fn handle_delete_signed_msg_user_orders(
    _ctx: Context<DeleteSignedMsgUserOrders>,
) -> Result<()> {
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
    #[account(
        mut,
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
        realloc = SignedMsgUserOrders::space(num_orders as usize),
        realloc::payer = payer,
        realloc::zero = false,
    )]
    pub signed_msg_user_orders: Box<Account<'info, SignedMsgUserOrders>>,
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
    #[account(
        mut,
        close = authority,
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
    )]
    pub signed_msg_user_orders: Box<Account<'info, SignedMsgUserOrders>>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
}
