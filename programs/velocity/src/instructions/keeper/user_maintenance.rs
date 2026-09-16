//! Keeper repairs to a user account.
//!
//! Force-cancelling a failing account's orders, flagging it idle, resyncing
//! counters that drifted, and tripping the authority-wide equity breaker.
//! Every handler here is permissionless: the proof is the calculation, not the
//! caller.

use super::*;

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_force_cancel_orders<'c: 'info, 'info>(
    ctx: Context<'info, ForceCancelOrder>,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;

    // Load the map under the live State guard rails. The equity-floor arm of
    // force-cancel requires an oracle-validity verdict, so this handler must
    // apply the same validity policy as `withdraw` and the permissionless trip.
    // Without the guard rails the same account gets a different floor verdict
    // here than everywhere else.
    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        Clock::get()?.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::orders::force_cancel_orders(
        &state,
        &ctx.accounts.user,
        &mut maps,
        &ctx.accounts.filler,
        &Clock::get()?,
    )?;

    Ok(())
}

/// Permissionless breaker trip: proves a single subaccount is below its
/// equity floor and sets the authority-wide `equity_breaker_tripped` flag on
/// `UserStats`, freezing every subaccount of the authority (no risk-increasing
/// fills, withdrawals or transfers out). Cleared only by the warm admin via
/// `reset_equity_floor_breaker`.
pub fn handle_trip_equity_floor_breaker<'c: 'info, 'info>(
    ctx: Context<'info, TripEquityFloorBreaker<'info>>,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let user = load!(ctx.accounts.user)?;
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;

    validate!(
        user.equity_floor > 0,
        ErrorCode::SufficientCollateral,
        "user has no equity floor set"
    )?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // The trip threshold is real net equity (unweighted assets and pnl minus
    // unweighted spot liabilities), not the margin numerator: weighted
    // collateral overstates equity when borrows exist and understates it via
    // asset weights, strict pricing and the positive-pnl clamp. The walk is
    // the trip's own upper bound: invalid-oracle liabilities and shorts count
    // at zero, while invalid-oracle assets and longs block the proof.
    let trip_equity = calculate_user_equity_for_trip(&user, &mut maps)?;

    // An authority-wide freeze must not arm over exposure the program cannot
    // value: any invalid-oracle asset or long blocks the proof. The floor
    // gates on withdrawals/fills still hold independently of the breaker. The two
    // validates decompose `TripNetEquity::proves_breach` so each failure
    // keeps its error code.
    validate!(
        trip_equity.provable,
        ErrorCode::InvalidOracle,
        "cannot trip equity floor breaker: invalid oracle leaves equity without a finite upper bound"
    )?;

    validate!(
        user.is_below_equity_floor(trip_equity.equity_upper_bound),
        ErrorCode::SufficientCollateral,
        "user net equity upper bound {} not below equity floor {}",
        trip_equity.equity_upper_bound,
        user.equity_floor
    )?;

    msg!(
        "equity floor breaker tripped for authority {:?}: subaccount {} net equity upper bound {} below floor {}",
        user.authority,
        user.sub_account_id,
        trip_equity.equity_upper_bound,
        user.equity_floor
    );

    user_stats.set_equity_breaker_tripped(true);

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_update_user_idle<'c: 'info, 'info>(
    ctx: Context<'info, UpdateUserIdle<'info>>,
) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;
    let clock = Clock::get()?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        ctx.accounts.state.load()?.slot_clock(),
        None,
    )?;

    let (equity, _) = calculate_user_equity(&user, &mut maps)?;

    // user flipped to idle faster if equity is less than 1000
    let accelerated = equity < QUOTE_PRECISION_I128 * 1000;

    validate_user_is_idle(
        &user,
        clock.slot,
        accelerated,
        ctx.accounts.state.load()?.slot_clock(),
    )?;

    user.idle = true;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_log_user_balances<'c: 'info, 'info>(
    ctx: Context<'info, LogUserBalances<'info>>,
) -> Result<()> {
    let user_key = ctx.accounts.user.key();
    let user = load!(ctx.accounts.user)?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        ctx.accounts.state.load()?.slot_clock(),
        None,
    )?;

    let (equity, _) = calculate_user_equity(&user, &mut maps)?;

    msg!(
        "Authority key {} subaccount id {} user key {}",
        user.authority,
        user.sub_account_id,
        user_key
    );

    msg!("Equity {}", equity);

    log_spot_positions(&user, &maps)?;
    log_perp_positions(&user, &mut maps)
}

/// Log every spot position that holds a balance.
fn log_spot_positions(user: &User, maps: &AccountMaps) -> Result<()> {
    for spot_position in user.spot_positions.iter() {
        if spot_position.scaled_balance == 0 {
            continue;
        }

        let spot_market = maps.spot_market_map.get_ref(&spot_position.market_index)?;
        let token_amount = spot_position.get_signed_token_amount(&spot_market)?;
        msg!(
            "Spot position {} balance {}",
            spot_position.market_index,
            token_amount
        );
    }
    Ok(())
}

/// Log every perp position that carries unrealized pnl.
fn log_perp_positions(user: &User, maps: &mut AccountMaps) -> Result<()> {
    for perp_position in user.perp_positions.iter() {
        if perp_position.is_available() {
            continue;
        }

        let perp_market = maps.perp_market_map.get_ref(&perp_position.market_index)?;
        let oracle_price = maps
            .oracle_map
            .get_price_data(&perp_market.oracle_id())?
            .price;
        let (_, unrealized_pnl) =
            calculate_base_asset_value_and_pnl_with_oracle_price(perp_position, oracle_price)?;

        if unrealized_pnl == 0 {
            continue;
        }

        msg!(
            "Perp position {} unrealized pnl {}",
            perp_position.market_index,
            unrealized_pnl
        );
    }
    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_update_user_stats_referrer_info<'c: 'info, 'info>(
    ctx: Context<'info, UpdateUserStatsReferrerInfo<'info>>,
) -> Result<()> {
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;

    user_stats.update_referrer_status();

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_update_user_open_orders_count<'info>(ctx: Context<UpdateUserIdle>) -> Result<()> {
    let mut user = load_mut!(ctx.accounts.user)?;

    let mut open_orders = 0_u8;
    let mut open_auctions = 0_u8;

    for order in user.orders.iter() {
        if order.status == OrderStatus::Open {
            open_orders += 1;
        }

        if order.has_auction() {
            open_auctions += 1;
        }
    }

    // A CLOB-resident order occupies no `orders` slot — only the position's
    // `open_orders` reservation records it — so counting rows alone would
    // wipe the count for every order resting on a book, desyncing it from
    // the per-position reservations this instruction does not touch. Add
    // them back per market.
    open_orders = user
        .perp_positions
        .iter()
        .map(|position| position.market_index)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .fold(open_orders, |total, market_index| {
            total.saturating_add(user.clob_resident_open_orders(market_index))
        });

    user.open_orders = open_orders;
    user.has_open_order = open_orders > 0;
    user.open_auctions = open_auctions;
    user.has_open_auction = open_auctions > 0;

    Ok(())
}

#[derive(Accounts)]
pub struct ForceCancelOrder<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct UpdateUserIdle<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct TripEquityFloorBreaker<'info> {
    pub state: AccountLoader<'info, State>,
    /// Any signer may trip the breaker; the proof is the margin calculation.
    pub keeper: Signer<'info>,
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
pub struct LogUserBalances<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct UpdateUserStatsReferrerInfo<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}
