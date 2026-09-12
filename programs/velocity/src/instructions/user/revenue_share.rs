//! Who is paid a share of an account's fees.
//!
//! A `RevenueShare` is the account a builder or referrer is paid into. A
//! `RevenueShareEscrow` is the account a trader pays out of: it holds the
//! builders the trader approved and one row per order that owes a fee. A
//! `ReferrerName` is the public name a referrer is found by.

use super::*;

pub fn handle_initialize_referrer_name(
    ctx: Context<InitializeReferrerName>,
    name: [u8; 32],
) -> Result<()> {
    let authority_key = ctx.accounts.authority.key();
    let user_stats_key = ctx.accounts.user_stats.key();
    let user_key = ctx.accounts.user.key();
    let mut referrer_name = ctx
        .accounts
        .referrer_name
        .load_init()
        .or(Err(ErrorCode::UnableToLoadAccountLoader))?;

    let user = load!(ctx.accounts.user)?;

    validate!(
        user.sub_account_id == 0,
        ErrorCode::InvalidReferrer,
        "must be subaccount 0"
    )?;

    validate!(
        user.pool_id == 0,
        ErrorCode::InvalidReferrer,
        "must be pool_id 0"
    )?;

    referrer_name.authority = authority_key;
    referrer_name.user = user_key;
    referrer_name.user_stats = user_stats_key;
    referrer_name.name = name;

    Ok(())
}

pub fn handle_initialize_revenue_share<'c: 'info, 'info>(
    ctx: Context<'info, InitializeRevenueShare<'info>>,
) -> Result<()> {
    let mut revenue_share = ctx
        .accounts
        .revenue_share
        .load_init()
        .or(Err(ErrorCode::UnableToLoadAccountLoader))?;
    revenue_share.authority = ctx.accounts.authority.key();
    revenue_share.total_referrer_rewards = 0;
    revenue_share.total_builder_rewards = 0;
    Ok(())
}

pub fn handle_initialize_revenue_share_escrow<'c: 'info, 'info>(
    ctx: Context<'info, InitializeRevenueShareEscrow<'info>>,
    num_orders: u16,
) -> Result<()> {
    let mut user_stats = ctx.accounts.user_stats.load_mut()?;

    // `escrow.referrer` is snapshotted here and never written again, while
    // `user_stats.referrer` is only ever set by the first `initialize_user` — so the
    // snapshot is correct if and only if that call has already happened. `authority`
    // is unchecked and only `payer` signs, so without this gate any third party could
    // create another authority's escrow in the window between `initialize_user_stats`
    // and its first `initialize_user`, freezing a defaulted referrer into the escrow
    // and permanently suppressing that user's referral rewards and referee discount
    // (nothing, not even the permissionless resize, can rewrite the field).
    // Requiring a created subaccount puts the escrow strictly after the point where
    // the referrer becomes immutable.
    //
    // Checked before the escrow is written or resized: it is a precondition on state
    // this handler does not own, so there is no reason to size the orders vec first.
    validate!(
        user_stats.number_of_sub_accounts_created > 0,
        ErrorCode::UserNotFound,
        "revenue share escrow requires the authority's first user to exist, otherwise it snapshots a defaulted referrer"
    )?;

    // An escrow with no order slots cannot hold a builder or referral row, so
    // `find_or_create_referral_index` and `add_builder_order` both fail to claim one and every
    // fee, discount and reward computation silently falls back to its no-revenue-share value.
    // `authority` is an `UncheckedAccount` here and only `payer` signs, so a third party can
    // create any user's escrow PDA; at zero capacity that suppresses their rewards until someone
    // notices and calls the (permissionless) resize (finding #114).
    //
    // Enforced at init rather than in `RevenueShareEscrow::validate`, which `resize` and
    // `change_approved_builder` also run: an escrow already sitting at zero capacity on chain
    // must stay able to resize its way out, and blocking its builder edits would be a new
    // liveness problem rather than a fix.
    validate!(
        num_orders > 0,
        ErrorCode::DefaultError,
        "revenue share escrow must be initialized with at least one order slot"
    )?;

    let escrow = &mut ctx.accounts.escrow;
    escrow.authority = ctx.accounts.authority.key();
    escrow
        .orders
        .resize_with(num_orders as usize, RevenueShareOrder::default);

    escrow.referrer = user_stats.referrer;
    user_stats.update_builder_referral_status();

    escrow.validate()?;
    Ok(())
}

pub fn handle_resize_revenue_share_escrow_orders<'c: 'info, 'info>(
    ctx: Context<'info, ResizeRevenueShareEscrowOrders<'info>>,
    num_orders: u16,
) -> Result<()> {
    let escrow = &mut ctx.accounts.escrow;
    validate!(
        num_orders as usize >= escrow.orders.len(),
        ErrorCode::InvalidRevenueShareResize,
        "Invalid shrinking resize for revenue share escrow"
    )?;

    escrow
        .orders
        .resize_with(num_orders as usize, RevenueShareOrder::default);
    escrow.validate()?;
    Ok(())
}

pub fn handle_change_approved_builder<'c: 'info, 'info>(
    ctx: Context<'info, ChangeApprovedBuilder<'info>>,
    builder: Pubkey,
    max_fee_tenth_bps: u16,
    add: bool,
) -> Result<()> {
    validate!(
        ctx.accounts.escrow.authority != builder,
        ErrorCode::DefaultError,
        "Builder cannot be the same as the escrow authority"
    )?;

    let existing_builder_index = ctx
        .accounts
        .escrow
        .approved_builders
        .iter()
        .position(|b| b.authority == builder);
    if let Some(index) = existing_builder_index {
        if add {
            msg!(
                "Updated builder: {} with max fee tenth bps: {} -> {}",
                builder,
                ctx.accounts.escrow.approved_builders[index].max_fee_tenth_bps,
                max_fee_tenth_bps
            );
            ctx.accounts.escrow.approved_builders[index].max_fee_tenth_bps = max_fee_tenth_bps;
        } else {
            if ctx
                .accounts
                .escrow
                .orders
                .iter()
                .any(|o| (o.builder_idx == index as u8) && (!o.is_available()))
            {
                msg!("Builder has open orders, must cancel orders and settle_pnl before revoking");
                return Err(ErrorCode::CannotRevokeBuilderWithOpenOrders.into());
            }
            msg!(
                "Revoking builder: {}, max fee tenth bps: {} -> 0",
                builder,
                ctx.accounts.escrow.approved_builders[index].max_fee_tenth_bps,
            );
            ctx.accounts.escrow.approved_builders[index].max_fee_tenth_bps = 0;
        }
    } else if add {
        ctx.accounts.escrow.approved_builders.push(BuilderInfo {
            authority: builder,
            max_fee_tenth_bps,
            ..BuilderInfo::default()
        });
        msg!(
            "Added builder: {} with max fee tenth bps: {}",
            builder,
            max_fee_tenth_bps
        );
    } else {
        msg!("Tried to revoke builder: {}, but it was not found", builder);
    }

    Ok(())
}

#[derive(Accounts)]
#[instruction(
    name: [u8; 32],
)]
pub struct InitializeReferrerName<'info> {
    #[account(
        init,
        seeds = [b"referrer_name", name.as_ref()],
        space = ReferrerName::SIZE,
        bump,
        payer = payer
    )]
    pub referrer_name: AccountLoader<'info, ReferrerName>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction()]
pub struct InitializeRevenueShare<'info> {
    #[account(
        init,
        seeds = [REVENUE_SHARE_PDA_SEED.as_bytes(), authority.key().as_ref()],
        space = RevenueShare::space(),
        bump,
        payer = payer
    )]
    pub revenue_share: AccountLoader<'info, RevenueShare>,
    /// CHECK: The builder and/or referrer authority, beneficiary of builder/ref fees
    pub authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(num_orders: u16)]
pub struct InitializeRevenueShareEscrow<'info> {
    #[account(
        init,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), authority.key().as_ref()],
        space = RevenueShareEscrow::space(num_orders as usize, 1),
        bump,
        payer = payer
    )]
    pub escrow: Box<Account<'info, RevenueShareEscrow>>,
    /// CHECK: The auth owning this account, payer of builder/ref fees
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(num_orders: u16)]
pub struct ResizeRevenueShareEscrowOrders<'info> {
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
        realloc = RevenueShareEscrow::space(num_orders as usize, escrow.approved_builders.len()),
        realloc::payer = payer,
        realloc::zero = false,
        has_one = authority
    )]
    pub escrow: Box<Account<'info, RevenueShareEscrow>>,
    /// CHECK: The owner of RevenueShareEscrow
    pub authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(builder: Pubkey, max_fee_tenth_bps: u16, add: bool)]
pub struct ChangeApprovedBuilder<'info> {
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
        // revoking a builder does not remove the slot to avoid unintended reuse
        realloc = RevenueShareEscrow::space(escrow.orders.len(), if add { escrow.approved_builders.len() + 1 } else { escrow.approved_builders.len() }),
        realloc::payer = payer,
        realloc::zero = false,
        has_one = authority
    )]
    pub escrow: Box<Account<'info, RevenueShareEscrow>>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}
