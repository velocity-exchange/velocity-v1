use {
    crate::{
        controller,
        error::ErrorCode,
        instructions::constraints::*,
        load_mut,
        math::{self, safe_math::SafeMath},
        optional_accounts::get_token_mint,
        state::{
            insurance_fund_stake::InsuranceFundStake,
            market_status::MarketStatus,
            paused_operations::{InsuranceFundOperation, SpotOperation},
            spot_market::SpotMarket,
            state::State,
            traits::Size,
            user::UserStats,
        },
        validate,
    },
    anchor_lang::prelude::*,
    anchor_spl::token_interface::{TokenAccount, TokenInterface},
};

pub fn handle_initialize_insurance_fund_stake(
    ctx: Context<InitializeInsuranceFundStake>,
    market_index: u16,
) -> Result<()> {
    let mut if_stake = ctx
        .accounts
        .insurance_fund_stake
        .load_init()
        .or(Err(ErrorCode::UnableToLoadAccountLoader))?;

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    *if_stake = InsuranceFundStake::new(*ctx.accounts.authority.key, market_index, now);

    let spot_market = ctx.accounts.spot_market.load()?;

    validate!(
        !spot_market.is_insurance_fund_operation_paused(InsuranceFundOperation::Init),
        ErrorCode::InsuranceFundOperationPaused,
        "if staking init disabled",
    )?;

    Ok(())
}

pub fn handle_add_insurance_fund_stake<'c: 'info, 'info>(
    ctx: Context<'info, AddInsuranceFundStake<'info>>,
    market_index: u16,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Err(ErrorCode::InsufficientDeposit.into());
    }

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let insurance_fund_stake = &mut load_mut!(ctx.accounts.insurance_fund_stake)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    validate!(
        !spot_market.is_insurance_fund_operation_paused(InsuranceFundOperation::Add),
        ErrorCode::InsuranceFundOperationPaused,
        "if staking add disabled",
    )?;

    validate!(
        insurance_fund_stake.market_index == market_index,
        ErrorCode::IncorrectSpotMarketAccountPassed,
        "insurance_fund_stake does not match market_index"
    )?;

    validate!(
        spot_market.status != MarketStatus::Initialized,
        ErrorCode::InvalidSpotMarketState,
        "spot market = {} not active for insurance_fund_stake",
        spot_market.market_index
    )?;

    validate!(
        insurance_fund_stake.last_withdraw_request_shares == 0
            && insurance_fund_stake.last_withdraw_request_value == 0,
        ErrorCode::IFWithdrawRequestInProgress,
        "withdraw request in progress"
    )?;

    {
        if spot_market.has_transfer_hook() {
            controller::insurance::attempt_settle_revenue_to_insurance_fund(
                &ctx.accounts.spot_market_vault,
                &ctx.accounts.insurance_fund_vault,
                spot_market,
                now,
                &ctx.accounts.token_program,
                &ctx.accounts.velocity_signer,
                &state,
                &mint,
                Some(&mut remaining_accounts_iter.clone()),
            )?;
        } else {
            controller::insurance::attempt_settle_revenue_to_insurance_fund(
                &ctx.accounts.spot_market_vault,
                &ctx.accounts.insurance_fund_vault,
                spot_market,
                now,
                &ctx.accounts.token_program,
                &ctx.accounts.velocity_signer,
                &state,
                &mint,
                None,
            )?;
        };

        // reload the vault balances so they're up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        ctx.accounts.insurance_fund_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    // Only the portion of `amount` that prices to whole insurance-fund shares is staked;
    // the remainder is never transferred, so it stays with the depositor instead of
    // accruing to existing shareholders as rounding.
    let amount_deposited = controller::insurance::add_insurance_fund_stake(
        amount,
        ctx.accounts.insurance_fund_vault.amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
        clock.unix_timestamp,
        false,
    )?;

    if amount_deposited < amount {
        msg!(
            "staking {} of requested {}; remaining {} is below the price of one IF share",
            amount_deposited,
            amount,
            amount.safe_sub(amount_deposited)?
        );
    }

    controller::token::receive(
        &ctx.accounts.token_program,
        &ctx.accounts.user_token_account,
        &ctx.accounts.insurance_fund_vault,
        &ctx.accounts.authority,
        amount_deposited,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    Ok(())
}

pub fn handle_request_remove_insurance_fund_stake<'c: 'info, 'info>(
    ctx: Context<'info, RequestRemoveInsuranceFundStake<'info>>,
    market_index: u16,
    amount: u64,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let insurance_fund_stake = &mut load_mut!(ctx.accounts.insurance_fund_stake)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    validate!(
        !spot_market.is_insurance_fund_operation_paused(InsuranceFundOperation::RequestRemove),
        ErrorCode::InsuranceFundOperationPaused,
        "if staking request remove disabled",
    )?;

    // The pre-freeze revenue settle below goes through
    // `attempt_settle_revenue_to_insurance_fund`, which silently SKIPS while the
    // global withdraw status or this market's `SpotOperation::Withdraw` bit is
    // paused (so a pause never bricks the liquidation/IF-add paths that share
    // it). A request accepted during such a pause would therefore freeze a
    // pre-settle exit value and reintroduce the already-due-revenue leak this
    // instruction exists to close. Reject the request instead: the frozen value
    // of any accepted request is always post-settle, and gating the request
    // mirrors how every other vault-egress path treats the withdraw pauses.
    validate!(
        !state.withdraw_paused()?,
        ErrorCode::ExchangePaused,
        "withdraws paused exchange-wide; cannot freeze unstake exit value"
    )?;

    validate!(
        !spot_market.is_operation_paused(SpotOperation::Withdraw),
        ErrorCode::MarketWithdrawPaused,
        "spot market {} withdraws paused; cannot freeze unstake exit value",
        spot_market.market_index
    )?;

    validate!(
        insurance_fund_stake.market_index == market_index,
        ErrorCode::IncorrectSpotMarketAccountPassed,
        "insurance_fund_stake does not match market_index"
    )?;

    validate!(
        insurance_fund_stake.last_withdraw_request_shares == 0,
        ErrorCode::IFWithdrawRequestInProgress,
        "Withdraw request is already in progress"
    )?;

    // Settle any already-due revenue into the IF vault before freezing the exit
    // value, mirroring the add path. Otherwise the frozen `last_withdraw_request_value`
    // would exclude revenue the staker was already entitled to at request time, and a
    // later public settle between request and remove would shift that share to the
    // remaining stakers. Revenue accruing *after* this point is still (intentionally)
    // excluded by the freeze — that is the escrow tradeoff, not this bug.
    {
        if spot_market.has_transfer_hook() {
            controller::insurance::attempt_settle_revenue_to_insurance_fund(
                &ctx.accounts.spot_market_vault,
                &ctx.accounts.insurance_fund_vault,
                spot_market,
                now,
                &ctx.accounts.token_program,
                &ctx.accounts.velocity_signer,
                &state,
                &mint,
                Some(&mut remaining_accounts_iter.clone()),
            )?;
        } else {
            controller::insurance::attempt_settle_revenue_to_insurance_fund(
                &ctx.accounts.spot_market_vault,
                &ctx.accounts.insurance_fund_vault,
                spot_market,
                now,
                &ctx.accounts.token_program,
                &ctx.accounts.velocity_signer,
                &state,
                &mint,
                None,
            )?;
        };

        // reload the vault balances so they're up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        ctx.accounts.insurance_fund_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    let n_shares = math::insurance::vault_amount_to_if_shares(
        amount,
        spot_market.insurance_fund.total_shares,
        ctx.accounts.insurance_fund_vault.amount,
    )?;

    validate!(
        n_shares > 0,
        ErrorCode::IFWithdrawRequestTooSmall,
        "Requested if_shares = 0"
    )?;

    let user_if_shares = insurance_fund_stake.checked_if_shares(spot_market)?;
    validate!(user_if_shares >= n_shares, ErrorCode::InsufficientIFShares)?;

    controller::insurance::request_remove_insurance_fund_stake(
        n_shares,
        ctx.accounts.insurance_fund_vault.amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
        clock.unix_timestamp,
    )?;

    Ok(())
}

pub fn handle_cancel_request_remove_insurance_fund_stake<'c: 'info, 'info>(
    ctx: Context<'info, CancelRequestRemoveInsuranceFundStake<'info>>,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let insurance_fund_stake = &mut load_mut!(ctx.accounts.insurance_fund_stake)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    validate!(
        insurance_fund_stake.market_index == market_index,
        ErrorCode::IncorrectSpotMarketAccountPassed,
        "insurance_fund_stake does not match market_index"
    )?;

    validate!(
        insurance_fund_stake.last_withdraw_request_shares != 0,
        ErrorCode::NoIFWithdrawRequestInProgress,
        "No withdraw request in progress"
    )?;

    // Settle any already-due revenue into the IF vault BEFORE the cancel prices the
    // forfeiture, mirroring the add and request-remove paths (OtterSec #141).
    //
    // `cancel_request_remove_insurance_fund_stake` implements the anti-free-option rule:
    // it withdraws at the frozen `last_withdraw_request_value` and restakes at the live
    // vault price, so any appreciation during the escrow window is forfeited to the
    // stakers who stayed. Pricing that restake against a *pre-settle* vault understates
    // the live value, so the cancel burns no shares (or too few) and the canceller keeps
    // revenue the rule assigns to the remaining stakers. A staker could simply order
    // their signed cancel ahead of an already-due signerless settle to take it.
    //
    // Settling here rather than gating the cancel is deliberate: #34 exists precisely so
    // a pending request can always be cancelled, and refusing the cancel until someone
    // else cranks the settle would reintroduce a cancel-blocking condition. Revenue
    // accruing *after* this point is still forfeited by the freeze — that is the intended
    // escrow tradeoff, not this bug.
    {
        if spot_market.has_transfer_hook() {
            controller::insurance::attempt_settle_revenue_to_insurance_fund(
                &ctx.accounts.spot_market_vault,
                &ctx.accounts.insurance_fund_vault,
                spot_market,
                now,
                &ctx.accounts.token_program,
                &ctx.accounts.velocity_signer,
                &state,
                &mint,
                Some(&mut remaining_accounts_iter.clone()),
            )?;
        } else {
            controller::insurance::attempt_settle_revenue_to_insurance_fund(
                &ctx.accounts.spot_market_vault,
                &ctx.accounts.insurance_fund_vault,
                spot_market,
                now,
                &ctx.accounts.token_program,
                &ctx.accounts.velocity_signer,
                &state,
                &mint,
                None,
            )?;
        };

        // reload the vault balances so they're up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        ctx.accounts.insurance_fund_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    controller::insurance::cancel_request_remove_insurance_fund_stake(
        ctx.accounts.insurance_fund_vault.amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
        now,
    )?;

    Ok(())
}

#[access_control(
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_remove_insurance_fund_stake<'c: 'info, 'info>(
    ctx: Context<'info, RemoveInsuranceFundStake<'info>>,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let insurance_fund_stake = &mut load_mut!(ctx.accounts.insurance_fund_stake)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    validate!(
        !spot_market.is_insurance_fund_operation_paused(InsuranceFundOperation::Remove),
        ErrorCode::InsuranceFundOperationPaused,
        "if staking remove disabled",
    )?;

    validate!(
        insurance_fund_stake.market_index == market_index,
        ErrorCode::IncorrectSpotMarketAccountPassed,
        "insurance_fund_stake does not match market_index"
    )?;

    // check if spot market is healthy
    validate!(
        spot_market.is_healthy_utilization()?,
        ErrorCode::SpotMarketInsufficientDeposits,
        "spot market utilization above health threshold"
    )?;

    let amount = controller::insurance::remove_insurance_fund_stake(
        ctx.accounts.insurance_fund_vault.amount,
        insurance_fund_stake,
        user_stats,
        spot_market,
        now,
    )?;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.insurance_fund_vault,
        &ctx.accounts.user_token_account,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    ctx.accounts.insurance_fund_vault.reload()?;
    validate!(
        ctx.accounts.insurance_fund_vault.amount > 0,
        ErrorCode::InvalidIFDetected,
        "insurance_fund_vault.amount must remain > 0"
    )?;

    // validate relevant spot market balances before unstake
    math::spot_withdraw::validate_spot_balances(spot_market)?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(
    market_index: u16,
)]
pub struct InitializeInsuranceFundStake<'info> {
    #[account(
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        init,
        seeds = [b"insurance_fund_stake", authority.key.as_ref(), market_index.to_le_bytes().as_ref()],
        space = InsuranceFundStake::SIZE,
        bump,
        payer = payer
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct AddInsuranceFundStake<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    #[account(
        mut,
        token::mint = insurance_fund_vault.mint,
        token::authority = authority
    )]
    pub user_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct RequestRemoveInsuranceFundStake<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct CancelRequestRemoveInsuranceFundStake<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    // OtterSec #141: cancel must price against a settled IF vault, so it needs the
    // same settle plumbing `request_remove` gained in #31.
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct RemoveInsuranceFundStake<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    #[account(
        mut,
        token::mint = insurance_fund_vault.mint,
        token::authority = authority
    )]
    pub user_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}
