//! Firing a trigger order, and the relay resolver that stages the crank.

use super::*;

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_legacy_trigger_order<'c: 'info, 'info>(
    ctx: Context<'info, TriggerOrder<'info>>,
    order_id: u32,
) -> Result<()> {
    let (market_type, market_index_of_order) = match load!(ctx.accounts.user)?.get_order(order_id) {
        Some(order) => (order.market_type, order.market_index),
        None => {
            msg!("order_id not found {}", order_id);
            return Ok(());
        }
    };

    validate_spot_dlob_trading_enabled_for_market_type(market_type)?;

    let (writeable_perp_markets, writeable_spot_markets) = (MarketSet::new(), MarketSet::new());

    let state = ctx.accounts.state.load()?;

    // Load the map under the live State guard rails. Every oracle-validity
    // decision on this path then uses the same policy as the permissionless
    // trip. That includes the lazy breaker trip on the cancel branch.
    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &writeable_perp_markets,
        &writeable_spot_markets,
        Clock::get()?.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let triggered = controller::orders::trigger_order(
        order_id,
        &state,
        &controller::orders::TriggerAccounts {
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            filler: &ctx.accounts.filler,
        },
        &mut maps,
        &Clock::get()?,
    )?;

    // Only a trigger that placed the order did payable work. A cancel, an
    // already-triggered order and a no-op must not draw the reservoir. A cancel
    // happens when a failing account meets its trigger condition, and that
    // branch pays the user no flat reward. Paying the caller from the reservoir
    // for it would be free lamports. `trigger_limit_order_v1` does the same,
    // and its cancel branch returns before this call.
    if triggered {
        crate::instructions::finish_trigger_crank(
            &ctx.accounts.state,
            &ctx.accounts.filler,
            &ctx.accounts.authority,
            &ctx.accounts.user,
            &ctx.accounts.trigger_conditions,
            &ctx.accounts.crank_conditions,
            market_index_of_order,
            order_id,
        )?;
    }

    Ok(())
}

#[derive(Accounts)]
pub struct TriggerOrder<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this account must sign for `filler`. In
    /// program-keeper mode the filler is the protocol `User` and the caller is
    /// a relay turner. This account is then only the lamport payout target, and
    /// it needs no signature.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = can_crank_for_filler(&filler, &authority, &state)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// The user's relay trigger conditions. The crank releases the fired slot,
    /// so its level-triggered wake goes quiet. The account is optional. A
    /// keeper cranks without it on a market or a user that has no relay
    /// plumbing.
    #[account(
        mut,
        seeds = [USER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        bump
    )]
    pub trigger_conditions: Option<AccountLoader<'info, UserConditionsV0>>,
    /// The fired market's crank conditions. Its reservoir pays the keeper in
    /// program-keeper mode, and the crank checks it against the order's market.
    /// Program-keeper mode requires the account.
    #[account(mut)]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
}

/// The relay resolver for `trigger_order` (`Resolve<EndpointName>`). It runs in
/// simulation only. It stages the crank from the user's synced trigger
/// conditions.
#[derive(Accounts)]
pub struct ResolveTriggerOrder<'info> {
    /// The shared staging account. Convention puts it at index 0, and a
    /// resolver's response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only. A resolver stages into the shared scratch account and never
    /// into the block it reads.
    #[account(constraint = trigger_conditions.load()?.user == user.key())]
    pub trigger_conditions: AccountLoader<'info, UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
    /// CHECK: the perp market's `has_one` binds it.
    pub oracle: UncheckedAccount<'info>,
    #[account(has_one = oracle)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

pub fn handle_resolve_trigger_order(ctx: Context<ResolveTriggerOrder>) -> Result<()> {
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        let fired = {
            let conditions = ctx.accounts.trigger_conditions.load()?;
            let user = load!(ctx.accounts.user)?;
            let market = ctx.accounts.perp_market.load()?;
            crate::instructions::find_fired_trigger(
                &conditions,
                &user,
                &market,
                &ctx.accounts.oracle,
                clock.slot,
                crate::instructions::TriggerResolverKind::Flip,
            )?
        };
        let Some(meta) = fired else {
            return Ok(None);
        };

        let (protocol_user, _) = crate::state::pdas::protocol_user_pair();
        let user_stats = crate::state::pdas::user_stats(&load!(ctx.accounts.user)?.authority);
        Ok(Some(
            crate::staged_call!(TriggerOrder {
                state: crate::state::pdas::state(),
                authority: crate::state::pdas::keeper_placeholder(),
                filler: protocol_user,
                user: ctx.accounts.user.key(),
                user_stats,
                trigger_conditions: Some(ctx.accounts.trigger_conditions.key()),
                crank_conditions: Some(crate::state::pdas::clob_crank_conditions(
                    meta.market_index,
                )),
            })
            .refs(ctx.accounts.trigger_conditions.load()?.read_sync_accounts())
            .arg(meta.order_id)?,
        ))
    })
}
