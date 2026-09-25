//! The life of a user account: create it, delete it, and reclaim its rent.
//!
//! One `UserStats` holds what an authority accumulates over every subaccount.
//! Each `User` is one subaccount of that authority, and carries a conditions
//! block that the liquidation relay watches.

use super::*;

/// Create the conditions block that the liquidation relay watches.
///
/// Call this before the `User` loads: `load_init` defers the discriminator
/// write, so a `load_mut` in between reads a zeroed account. Paying its rent
/// here, before the user signs anything, keeps every later relay sync permissionless.
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

/// Close the conditions block of a `User` that is being deleted, and send
/// every lamport it holds to `recipient`. A user created before
/// `initialize_user` made the block may have none, and then this does nothing.
///
/// The caller pins the address with `seeds`. So a non-empty account there is
/// one that velocity created, and a relay `WatchV0` on it becomes inert.
pub fn close_user_conditions<'info>(
    user_conditions: &AccountInfo<'info>,
    recipient: &AccountInfo<'info>,
) -> Result<()> {
    if user_conditions.data_is_empty() {
        return Ok(());
    }

    validate!(
        user_conditions.owner == &crate::ID,
        ErrorCode::DefaultError,
        "user conditions account is not owned by velocity"
    )?;

    let lamports = user_conditions.lamports();
    **recipient.try_borrow_mut_lamports()? = recipient.lamports().safe_add(lamports)?;
    **user_conditions.try_borrow_mut_lamports()? = 0;

    user_conditions.assign(&anchor_lang::system_program::ID);
    user_conditions.resize(0)?;
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
    validate!(
        payer_lamports >= init_fee,
        ErrorCode::CantPayUserInitFee,
        "payer lamports {} init fee {}",
        payer_lamports,
        init_fee
    )?;

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

/// Only an admin payer creates the protocol's own `User` and `UserStats`.
/// Their authority is `State::signer`, a program address that cannot sign. So
/// without this gate any payer could create them first, with its own name and
/// referrer.
///
/// For any other authority, a payer that is not the authority must be an
/// allowlisted external depositor. Only the mainnet build holds an allowlist.
fn validate_payer(
    authority: &UncheckedAccount<'_>,
    payer: &Signer<'_>,
    state: &State,
) -> Result<()> {
    if authority.key() == state.signer {
        validate!(
            state.is_warm(&payer.key()),
            ErrorCode::Unauthorized,
            "only an admin creates the protocol user"
        )?;

        return Ok(());
    }

    #[cfg(feature = "mainnet-beta")]
    if !authority.is_signer && authority.key() != payer.key() {
        validate!(
            WHITELISTED_EXTERNAL_DEPOSITORS.contains(&payer.key()),
            ErrorCode::Unauthorized,
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

    validate_payer(&ctx.accounts.authority, &ctx.accounts.payer, &state)
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

    validate_payer(&ctx.accounts.authority, &ctx.accounts.payer, &state)
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

    // OtterSec #128: revoke this subaccount's revenue-share rows before its id
    // retires. The id is never reissued, so a row left open after that point
    // is unreachable forever, stranding the builder's fee and holding the
    // market's `pending_revenue_share` high for good. The escrow PDA is
    // pinned by `seeds`, so an empty account proves this authority has none.
    if !ctx.accounts.revenue_share_escrow.data_is_empty() {
        let mut escrow = ctx.accounts.revenue_share_escrow.load_zc_mut()?;
        escrow.revoke_completed_orders(user)?;

        // Nothing for this subaccount may be outstanding after the revoke. If one
        // is, fail rather than retire the id over it.
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

    close_user_conditions(
        &ctx.accounts.user_conditions,
        &ctx.accounts.authority.to_account_info(),
    )
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
    /// Relay liquidation coverage, created alongside the account it watches.
    /// It is required because relay can only watch an account that exists.
    /// Coverage first matters when somebody else's transaction gives the user a
    /// position. The user signs nothing there, so no rent can be charged to
    /// them. `deploy-scripts/migrate.ts` backfills the accounts that predate
    /// the field. An empty block is fine, because the first sync writes the
    /// thresholds.
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
    /// CHECK: the authority's `RevenueShareEscrow`, which may not exist. Most users
    /// never create one. It is an `UncheckedAccount` pinned by `seeds`, not a typed
    /// `AccountLoader`. The address is derived and not caller-chosen, so
    /// `data_is_empty()` proves absence. The handler can then tell "this authority has
    /// no escrow" from "the caller omitted it to skip the check". A typed loader would
    /// make deletion impossible for the many users who have no escrow account to pass.
    ///
    /// It is required rather than `Option`, so a caller holding fee-bearing builder
    /// rows cannot leave it out (OtterSec #128).
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
    /// CHECK: the user's conditions block, closed with the user. It is an
    /// `UncheckedAccount` because a user created before the block existed may
    /// have none. The `seeds` stop a caller from naming another account.
    #[account(
        mut,
        seeds = [USER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        bump,
    )]
    pub user_conditions: UncheckedAccount<'info>,
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

#[cfg(test)]
mod payer_tests {
    use {
        super::validate_payer,
        crate::state::state::State,
        anchor_lang::prelude::{AccountInfo, Pubkey, Signer, UncheckedAccount},
    };

    struct Accounts {
        authority_key: Pubkey,
        payer_key: Pubkey,
        owner: Pubkey,
        authority_lamports: u64,
        payer_lamports: u64,
        authority_data: Vec<u8>,
        payer_data: Vec<u8>,
    }

    impl Accounts {
        fn new(authority_key: Pubkey, payer_key: Pubkey) -> Self {
            Self {
                authority_key,
                payer_key,
                owner: Pubkey::default(),
                authority_lamports: 0,
                payer_lamports: 0,
                authority_data: vec![],
                payer_data: vec![],
            }
        }

        /// The authority never signs here. That is the case the allowlist
        /// governs, and it is the case the protocol `User` is always in.
        fn check(&mut self, state: &State) -> anchor_lang::Result<()> {
            let authority_info = AccountInfo::new(
                &self.authority_key,
                false,
                false,
                &mut self.authority_lamports,
                &mut self.authority_data,
                &self.owner,
                false,
            );
            let payer_info = AccountInfo::new(
                &self.payer_key,
                true,
                true,
                &mut self.payer_lamports,
                &mut self.payer_data,
                &self.owner,
                false,
            );

            validate_payer(
                &UncheckedAccount::try_from(&authority_info),
                &Signer::try_from(&payer_info)?,
                state,
            )
        }
    }

    fn state_with_admin(admin: Pubkey) -> State {
        State {
            signer: Pubkey::new_unique(),
            cold_admin: admin,
            ..State::default()
        }
    }

    #[test]
    fn an_admin_payer_creates_the_protocol_user() {
        let admin = Pubkey::new_unique();
        let state = state_with_admin(admin);
        let mut accounts = Accounts::new(state.signer, admin);
        assert!(accounts.check(&state).is_ok());
    }

    #[test]
    fn a_payer_that_is_not_an_admin_cannot_create_the_protocol_user() {
        let state = state_with_admin(Pubkey::new_unique());
        let mut accounts = Accounts::new(state.signer, Pubkey::new_unique());
        assert!(accounts.check(&state).is_err());
    }

    /// Any other unsigned authority still needs an allowlisted payer on a
    /// mainnet build.
    #[cfg(feature = "mainnet-beta")]
    #[test]
    fn a_third_party_authority_still_needs_an_allowlisted_payer() {
        let state = state_with_admin(Pubkey::new_unique());
        let mut accounts = Accounts::new(Pubkey::new_unique(), Pubkey::new_unique());
        assert!(accounts.check(&state).is_err());
    }

    #[test]
    fn a_payer_that_is_the_authority_passes() {
        let state = state_with_admin(Pubkey::new_unique());
        let key = Pubkey::new_unique();
        let mut accounts = Accounts::new(key, key);
        assert!(accounts.check(&state).is_ok());
    }
}

#[cfg(test)]
mod close_user_conditions_tests {
    use {
        super::close_user_conditions,
        anchor_lang::prelude::{AccountInfo, Pubkey},
    };

    /// `AccountInfo::resize` reads the original length 4 bytes before the key
    /// and writes the new length 8 bytes before the data, as the runtime lays
    /// out an input buffer. These types give it that memory on the host.
    #[repr(C)]
    struct KeyWithOriginalLen {
        original_data_len: u32,
        key: Pubkey,
    }

    struct ClosingAccounts {
        conditions_key: KeyWithOriginalLen,
        recipient_key: Pubkey,
        owner: Pubkey,
        recipient_owner: Pubkey,
        conditions_lamports: u64,
        recipient_lamports: u64,
        conditions_words: Vec<u64>,
        recipient_data: Vec<u8>,
    }

    impl ClosingAccounts {
        fn new(owner: Pubkey, data_len: usize) -> Self {
            Self {
                conditions_key: KeyWithOriginalLen {
                    original_data_len: data_len as u32,
                    key: Pubkey::new_unique(),
                },
                recipient_key: Pubkey::new_unique(),
                owner,
                recipient_owner: Pubkey::default(),
                conditions_lamports: 1_000,
                recipient_lamports: 5,
                conditions_words: vec![0; 1 + data_len / 8],
                recipient_data: vec![],
            }
        }

        fn close(&mut self) -> anchor_lang::Result<ClosedAccounts> {
            let conditions_bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut self.conditions_words);
            let conditions = AccountInfo::new(
                &self.conditions_key.key,
                false,
                true,
                &mut self.conditions_lamports,
                &mut conditions_bytes[8..],
                &self.owner,
                false,
            );
            let recipient = AccountInfo::new(
                &self.recipient_key,
                false,
                true,
                &mut self.recipient_lamports,
                &mut self.recipient_data,
                &self.recipient_owner,
                false,
            );

            close_user_conditions(&conditions, &recipient)?;

            Ok(ClosedAccounts {
                conditions_lamports: conditions.lamports(),
                conditions_owner: *conditions.owner,
                conditions_data_len: conditions.data_len(),
                recipient_lamports: recipient.lamports(),
            })
        }
    }

    struct ClosedAccounts {
        conditions_lamports: u64,
        conditions_owner: Pubkey,
        conditions_data_len: usize,
        recipient_lamports: u64,
    }

    #[test]
    fn a_velocity_owned_block_is_closed_to_the_recipient() {
        let closed = ClosingAccounts::new(crate::ID, 64).close().unwrap();

        assert_eq!(closed.conditions_lamports, 0);
        assert_eq!(closed.conditions_owner, anchor_lang::system_program::ID);
        assert_eq!(closed.conditions_data_len, 0);
        assert_eq!(closed.recipient_lamports, 1_005);
    }

    #[test]
    fn an_absent_block_is_left_alone() {
        let closed = ClosingAccounts::new(Pubkey::default(), 0).close().unwrap();

        assert_eq!(closed.conditions_lamports, 1_000);
        assert_eq!(closed.recipient_lamports, 5);
    }

    #[test]
    fn a_block_owned_by_another_program_is_refused() {
        assert!(ClosingAccounts::new(Pubkey::new_unique(), 64)
            .close()
            .is_err());
    }
}
