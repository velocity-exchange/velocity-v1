//! The life of a user account: create it, delete it, and reclaim its rent.
//!
//! One `UserStats` holds what an authority accumulates over every subaccount.
//! Each `User` is one subaccount of that authority, and carries a conditions
//! block that the liquidation relay watches.

use super::*;

/// Create the conditions block that the liquidation relay watches.
///
/// The block is not optional and it is not created later. Relay can only watch
/// an account that exists, and coverage first matters when someone else gives
/// this user a position. A maker order that a keeper fills, and a
/// signed-message order that a filler submits, both do that. The user signs
/// neither, so there is no later point at which the rent can be charged to
/// them. The rent is part of what an account costs, and paying it here is what
/// makes every later sync permissionless.
///
/// Nothing is armed yet. The block carries no exposure and no margin map, and
/// the first sync writes both.
///
/// Call this before the `User` is loaded. `load_init` does not write the
/// discriminator until the instruction exits, so a `load_mut` of the user in
/// between reads a zeroed one and fails.
fn init_user_conditions(
    user_conditions: &AccountLoader<'_, UserConditionsV0>,
    user_key: Pubkey,
) -> Result<()> {
    let mut conditions = user_conditions
        .load_init()
        .or_else(|_| user_conditions.load_mut())?;
    conditions.user = user_key;
    conditions.init_block()?;
    Ok(())
}

/// Mark both sides of a referral and return the referrer authority. Returns
/// the default key when the caller passed no referrer accounts.
fn link_referrer<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    user_stats: &mut UserStats,
) -> Result<Pubkey> {
    let (referrer, referrer_stats) = get_referrer_and_referrer_stats(account_info_iter)?;
    let (Some(referrer), Some(referrer_stats)) = (referrer, referrer_stats) else {
        return Ok(Pubkey::default());
    };

    let referrer = load!(referrer)?;
    let mut referrer_stats = load_mut!(referrer_stats)?;

    validate!(referrer.sub_account_id == 0, ErrorCode::InvalidReferrer)?;

    validate!(
        referrer.authority == referrer_stats.authority,
        ErrorCode::ReferrerAndReferrerStatsAuthorityUnequal
    )?;

    referrer_stats.referrer_status |= ReferrerStatus::IsReferrer as u8;
    user_stats.referrer_status |= ReferrerStatus::IsReferred as u8;

    Ok(referrer.authority)
}

/// Charge the payer what a new account costs. The lamports stay on the `User`
/// account.
fn pay_init_user_fee<'info>(
    payer: &Signer<'info>,
    user: &AccountLoader<'info, User>,
    system_program: &Program<'info, System>,
    init_fee: u64,
) -> Result<()> {
    if init_fee == 0 {
        return Ok(());
    }

    let payer_lamports = payer.to_account_info().try_lamports()?;
    if payer_lamports < init_fee {
        msg!("payer lamports {} init fee {}", payer_lamports, init_fee);
        return Err(ErrorCode::CantPayUserInitFee.into());
    }

    invoke(
        &transfer(&payer.key(), &user.key(), init_fee),
        &[
            payer.to_account_info(),
            user.to_account_info(),
            system_program.to_account_info(),
        ],
    )?;

    Ok(())
}

/// A payer that is not the authority must be an allowlisted external
/// depositor. Only the mainnet build holds an allowlist.
#[cfg_attr(not(feature = "mainnet-beta"), allow(unused_variables))]
fn validate_external_payer(authority: &UncheckedAccount<'_>, payer: &Signer<'_>) -> Result<()> {
    #[cfg(feature = "mainnet-beta")]
    if !authority.is_signer && authority.key() != payer.key() {
        validate!(
            WHITELISTED_EXTERNAL_DEPOSITORS.contains(&payer.key()),
            ErrorCode::DefaultError,
            "Authority is not the payer"
        )?;
    }

    Ok(())
}

pub fn handle_initialize_user<'c: 'info, 'info>(
    ctx: Context<'info, InitializeUser<'info>>,
    sub_account_id: u16,
    name: [u8; 32],
) -> Result<()> {
    let user_key = ctx.accounts.user.key();
    init_user_conditions(&ctx.accounts.user_conditions, user_key)?;

    let mut user = ctx
        .accounts
        .user
        .load_init()
        .or(Err(ErrorCode::UnableToLoadAccountLoader))?;
    user.authority = ctx.accounts.authority.key();
    user.sub_account_id = sub_account_id;
    user.name = name;
    user.next_order_id = 1;
    user.next_liquidation_id = 1;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();

    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;
    user_stats.number_of_sub_accounts = user_stats.number_of_sub_accounts.safe_add(1)?;

    // Only the first subaccount of an authority may record a referrer.
    if user_stats.number_of_sub_accounts_created == 0 {
        let referrer = link_referrer(remaining_accounts_iter, &mut user_stats)?;
        user_stats.referrer = referrer;
    }

    let whitelist_mint = ctx.accounts.state.load()?.whitelist_mint;
    if !whitelist_mint.eq(&Pubkey::default()) {
        validate_whitelist_token(
            get_whitelist_token(remaining_accounts_iter)?,
            &whitelist_mint,
            &ctx.accounts.authority.key(),
        )?;
    }

    validate!(
        sub_account_id == user_stats.number_of_sub_accounts_created,
        ErrorCode::InvalidUserSubAccountId,
        "Invalid sub account id {}, must be {}",
        sub_account_id,
        user_stats.number_of_sub_accounts_created
    )?;

    user_stats.number_of_sub_accounts_created =
        user_stats.number_of_sub_accounts_created.safe_add(1)?;

    let mut state = ctx.accounts.state.load_mut()?;
    let now_ts = Clock::get()?.unix_timestamp;
    user_stats.try_auto_enroll_accelerated_referral_and_emit(now_ts);
    safe_increment!(state.number_of_sub_accounts, 1);

    let max_number_of_sub_accounts = state.max_number_of_sub_accounts();

    validate!(
        max_number_of_sub_accounts == 0
            || state.number_of_sub_accounts <= max_number_of_sub_accounts,
        ErrorCode::MaxNumberOfUsers
    )?;

    emit!(NewUserRecord {
        ts: now_ts,
        user_authority: ctx.accounts.authority.key(),
        user: user_key,
        sub_account_id,
        name,
        referrer: user_stats.referrer
    });

    drop(user);

    pay_init_user_fee(
        &ctx.accounts.payer,
        &ctx.accounts.user,
        &ctx.accounts.system_program,
        state.get_init_user_fee()?,
    )?;

    validate_external_payer(&ctx.accounts.authority, &ctx.accounts.payer)
}

pub fn handle_initialize_user_stats<'c: 'info, 'info>(
    ctx: Context<'info, InitializeUserStats>,
) -> Result<()> {
    let clock = Clock::get()?;

    let mut user_stats = ctx
        .accounts
        .user_stats
        .load_init()
        .or(Err(ErrorCode::UnableToLoadAccountLoader))?;

    *user_stats = UserStats {
        authority: ctx.accounts.authority.key(),
        number_of_sub_accounts: 0,
        last_taker_volume_30d_ts: clock.unix_timestamp,
        last_maker_volume_30d_ts: clock.unix_timestamp,
        last_filler_volume_30d_ts: clock.unix_timestamp,
        ..UserStats::default()
    };

    let mut state = ctx.accounts.state.load_mut()?;
    user_stats.try_auto_enroll_accelerated_referral_and_emit(clock.unix_timestamp);
    safe_increment!(state.number_of_authorities, 1);

    let max_number_of_sub_accounts = state.max_number_of_sub_accounts();

    validate!(
        max_number_of_sub_accounts == 0
            || state.number_of_authorities <= max_number_of_sub_accounts,
        ErrorCode::MaxNumberOfUsers
    )?;

    validate_external_payer(&ctx.accounts.authority, &ctx.accounts.payer)
}

pub fn handle_delete_user(ctx: Context<DeleteUser>) -> Result<()> {
    let user = &load!(ctx.accounts.user)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;

    validate_user_deletion(
        user,
        user_stats,
        &*ctx.accounts.state.load()?,
        Clock::get()?.unix_timestamp,
    )?;

    // OtterSec #128: settle this subaccount's revenue-share rows before the id goes
    // away for good.
    //
    // `revoke_completed_orders` only transitions rows whose `sub_account_id` matches
    // the `User` it is handed, and `delete_user` retires that id permanently — the
    // allocation counter (`number_of_sub_accounts_created`) has no decrement site, so
    // the id is never reissued and no future `User` can ever match those rows again.
    // A row left `open && !completed` therefore became unreachable: the builder's
    // accrued fee was stranded and the market's `pending_revenue_share` stayed
    // inflated for the life of the market.
    //
    // Note the window is *not* the open-order case — `validate_user_deletion` already
    // requires every order closed. It is the filled-but-not-yet-revoked row, which is
    // exactly the state `revoke_completed_orders` exists to resolve.
    //
    // Resolve rather than block: because every order is already closed, each row for
    // this subaccount transitions to `Completed` (or is cleared when it carries no
    // fees), which is the state the permissionless sweep pays out of — and the sweep
    // needs no `User`, so it still pays after the account is gone. Blocking deletion
    // instead would punish the wrong party, holding a user's rent hostage until a
    // keeper happened to crank.
    //
    // The escrow is pinned to the authority's PDA by `seeds`, so an empty account
    // proves this authority has no escrow (nothing to orphan) rather than signalling
    // an omitted account.
    if !ctx.accounts.revenue_share_escrow.data_is_empty() {
        let mut escrow = ctx.accounts.revenue_share_escrow.load_zc_mut()?;
        escrow.revoke_completed_orders(user)?;

        // Belt and braces: after the above, nothing for this subaccount may still be
        // outstanding. If it somehow is, fail rather than retire the id over it.
        validate!(
            !escrow.has_outstanding_orders_for_sub_account(user.sub_account_id)?,
            ErrorCode::UserCantBeDeleted,
            "sub account {} still has outstanding revenue-share orders",
            user.sub_account_id
        )?;
    }

    safe_decrement!(user_stats.number_of_sub_accounts, 1);

    let mut state = ctx.accounts.state.load_mut()?;
    safe_decrement!(state.number_of_sub_accounts, 1);

    Ok(())
}

pub fn handle_reclaim_rent(ctx: Context<ReclaimRent>) -> Result<()> {
    let user_size = ctx.accounts.user.to_account_info().data_len();
    let minimum_lamports = ctx.accounts.rent.minimum_balance(user_size);
    let current_lamports = ctx.accounts.user.to_account_info().try_lamports()?;
    let reclaim_amount = current_lamports.saturating_sub(minimum_lamports);

    validate!(
        reclaim_amount > 0,
        ErrorCode::CantReclaimRent,
        "user account has no excess lamports to reclaim"
    )?;

    **ctx
        .accounts
        .user
        .to_account_info()
        .try_borrow_mut_lamports()? = minimum_lamports;

    **ctx
        .accounts
        .authority
        .to_account_info()
        .try_borrow_mut_lamports()? += reclaim_amount;

    let user_stats = &mut load!(ctx.accounts.user_stats)?;

    // Skip age check if is no max sub accounts
    let max_sub_accounts = ctx.accounts.state.load()?.max_number_of_sub_accounts();
    let estimated_user_stats_age = user_stats.get_age_ts(Clock::get()?.unix_timestamp);
    validate!(
        max_sub_accounts == 0 || estimated_user_stats_age >= THIRTEEN_DAY,
        ErrorCode::CantReclaimRent,
        "user stats too young to reclaim rent. age ={} minimum = {}",
        estimated_user_stats_age,
        THIRTEEN_DAY
    )?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(
    sub_account_id: u16,
)]
pub struct InitializeUser<'info> {
    #[account(
        init,
        seeds = [b"user", authority.key.as_ref(), sub_account_id.to_le_bytes().as_ref()],
        space = User::SIZE,
        bump,
        payer = payer
    )]
    pub user: AccountLoader<'info, User>,
    /// Relay liquidation coverage, created alongside the account it
    /// watches. Required: relay can only watch an account that exists, and
    /// the moment coverage matters is the moment somebody else's transaction
    /// gave the user a position, where the user signs nothing and no rent can
    /// be charged to them. `deploy-scripts/migrate.ts` backfills the accounts
    /// that predate the field. Coming up empty is fine — the first sync
    /// writes the thresholds.
    #[account(
        init_if_needed,
        seeds = [USER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        space = UserConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub user_conditions: AccountLoader<'info, UserConditionsV0>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    /// CHECK: Just a normal authority account
    pub authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct InitializeUserStats<'info> {
    #[account(
        init,
        seeds = [b"user_stats", authority.key.as_ref()],
        space = UserStats::SIZE,
        bump,
        payer = payer
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    /// CHECK: Just a normal authority account
    pub authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DeleteUser<'info> {
    #[account(
        mut,
        has_one = authority,
        close = authority
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: the authority's `RevenueShareEscrow`, which may legitimately not exist —
    /// most users never create one. Deliberately an `UncheckedAccount` **pinned by
    /// `seeds`** rather than a typed `AccountLoader`: because the address is derived
    /// and not caller-chosen, absence is *provable* (`data_is_empty()`), so the handler
    /// can distinguish "this authority has no escrow" from "the caller omitted it to
    /// skip the check". A typed loader would instead make deletion impossible for the
    /// majority of users, who have no escrow account to pass.
    ///
    /// Required rather than `Option` so a caller holding fee-bearing builder rows
    /// cannot simply leave it out (OtterSec #128).
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ReclaimRent<'info> {
    #[account(
        mut,
        has_one = authority,
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
}
