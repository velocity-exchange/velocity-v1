//! Keeper repairs to a user account.
//!
//! Force-cancelling a failing account's orders, flagging it idle, resyncing
//! counters that drifted, and tripping the authority-wide equity breaker.
//! Anyone may call every handler here. Each one proves its own case by
//! calculation, so the caller's identity does not matter.

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

    // The trip threshold is real net equity, which is unweighted assets and pnl
    // minus unweighted spot liabilities. The margin numerator does not serve
    // here. Weighted collateral overstates equity when borrows exist, and asset
    // weights, strict pricing and the positive-pnl clamp make it understate
    // equity. The walk returns the trip's own upper bound. Invalid-oracle
    // liabilities and shorts count at zero, and invalid-oracle assets and longs
    // block the proof.
    let trip_equity = calculate_user_equity_for_trip(&user, &mut maps)?;

    // An authority-wide freeze must not arm over exposure the program cannot
    // value. Any invalid-oracle asset or long blocks the proof. The floor gates
    // on withdrawals and fills hold whether or not the breaker is armed. The two
    // validates decompose `TripNetEquity::proves_breach`, so each failure keeps
    // its own error code.
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

    let counts = count_user_open_orders(&user);

    user.open_orders = counts.open_orders;
    user.has_open_order = counts.open_orders > 0;
    user.open_auctions = counts.open_auctions;
    user.has_open_auction = counts.open_auctions > 0;

    Ok(())
}

/// The counters that `update_user_open_orders_count` writes back.
pub struct UserOpenOrderCounts {
    pub open_orders: u8,
    pub open_auctions: u8,
}

/// Recount an account's open orders and open auctions from its own state.
///
/// An order rests in one of two places, and the count must cover both. A DLOB
/// order holds an `Order` row. A plain CLOB order holds no row, and only the
/// position's `open_orders` reservation records it. A count of rows alone
/// therefore drops every order that rests on a book. It also desyncs the count
/// from the per-position reservations that this instruction does not touch.
///
/// A fired trigger order is the one order that appears in both places. It keeps
/// its `Order` row `Open` with `PlacedOnClob` set, and its book reservation
/// reuses the `open_orders` slot that row already holds. The row pass skips it
/// because `clob_resident_open_orders` counts it, so it counts exactly once.
pub fn count_user_open_orders(user: &User) -> UserOpenOrderCounts {
    let mut open_orders = 0_u8;
    let mut open_auctions = 0_u8;

    for order in user.orders.iter() {
        let book_resident = order.market_type == MarketType::Perp && order.is_placed_on_clob();
        if order.status == OrderStatus::Open && !book_resident {
            open_orders = open_orders.saturating_add(1);
        }

        if order.has_auction() {
            open_auctions = open_auctions.saturating_add(1);
        }
    }

    let open_orders = user
        .perp_positions
        .iter()
        .map(|position| position.market_index)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .fold(open_orders, |total, market_index| {
            total.saturating_add(user.clob_resident_open_orders(market_index))
        });

    UserOpenOrderCounts {
        open_orders,
        open_auctions,
    }
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
    /// Any signer may trip the breaker. The margin calculation is the proof.
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

#[cfg(test)]
mod open_order_count_tests {
    use {
        super::count_user_open_orders,
        crate::state::user::{MarketType, Order, OrderBitFlag, OrderStatus, PerpPosition, User},
    };

    const MARKET: u16 = 4;

    fn dlob_row() -> Order {
        Order {
            status: OrderStatus::Open,
            market_type: MarketType::Perp,
            market_index: MARKET,
            ..Order::default()
        }
    }

    fn placed_trigger_shadow() -> Order {
        let mut order = dlob_row();
        order.add_bit_flag(OrderBitFlag::PlacedOnClob);
        order
    }

    fn user_with(reserved_open_orders: u8, rows: Vec<Order>) -> User {
        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: MARKET,
            open_orders: reserved_open_orders,
            open_bids: 1,
            ..PerpPosition::default()
        };
        for (index, row) in rows.into_iter().enumerate() {
            user.orders[index] = row;
        }
        user
    }

    /// A fired trigger order holds one reservation and one row. The recount
    /// must return one, not two.
    #[test]
    fn placed_trigger_shadow_counts_once() {
        let user = user_with(1, vec![placed_trigger_shadow()]);
        assert_eq!(count_user_open_orders(&user).open_orders, 1);
    }

    #[test]
    fn dlob_row_counts_once() {
        let user = user_with(1, vec![dlob_row()]);
        assert_eq!(count_user_open_orders(&user).open_orders, 1);
    }

    /// A plain CLOB order has no row, so only the reservation reports it.
    #[test]
    fn plain_clob_order_counts_once() {
        let user = user_with(1, vec![]);
        assert_eq!(count_user_open_orders(&user).open_orders, 1);
    }

    /// One order of each kind reserves three slots and writes two rows.
    #[test]
    fn mixed_orders_count_once_each() {
        let user = user_with(3, vec![dlob_row(), placed_trigger_shadow()]);
        assert_eq!(count_user_open_orders(&user).open_orders, 3);
    }

    /// A spot row has no perp reservation, so it counts by its row alone.
    #[test]
    fn spot_row_counts_once() {
        let mut spot_row = dlob_row();
        spot_row.market_type = MarketType::Spot;
        spot_row.market_index = 0;
        let user = user_with(1, vec![dlob_row(), spot_row]);
        assert_eq!(count_user_open_orders(&user).open_orders, 2);
    }
}
