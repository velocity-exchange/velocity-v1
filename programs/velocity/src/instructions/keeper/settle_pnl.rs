//! Settling a user's perp pnl and funding.
//!
//! Both settle entrypoints run the same three steps per market. They settle the
//! pnl, discharge the revenue share the settle made payable, and return an
//! isolated-position deposit that the settle freed. [`PnlSettlement`] holds that
//! body, so the single-market and batch entrypoints cannot drift apart.

use super::*;

#[access_control(
    settle_pnl_not_paused(&ctx.accounts.state)
)]
pub fn handle_settle_pnl<'c: 'info, 'info>(
    ctx: Context<'info, SettlePNL>,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;

    validate!(
        user.pool_id == 0,
        ErrorCode::InvalidPoolId,
        "user have pool_id 0"
    )?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        &mut remaining_accounts,
        &get_writable_perp_market_set(market_index),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mut rev_share = RevenueShareSweep::load(&mut remaining_accounts, &user.authority, &state)?;

    PnlSettlement {
        state_loader: &ctx.accounts.state,
        state: &state,
        clock: &clock,
        user_key,
        authority: ctx.accounts.authority.key,
        // `settle_pnl` computes the requirement for itself on this path.
        meets_margin_requirement: None,
        mode: SettlePnlMode::MustSettle,
    }
    .settle_market(market_index, user, &mut maps, &mut rev_share)?;

    let spot_market = maps.spot_market_map.get_quote_spot_market()?;
    validate_spot_market_vault_amount(&spot_market, ctx.accounts.spot_market_vault.amount)?;

    Ok(())
}

#[access_control(
    settle_pnl_not_paused(&ctx.accounts.state)
)]
pub fn handle_settle_multiple_pnls<'c: 'info, 'info>(
    ctx: Context<'info, SettlePNL>,
    market_indexes: Vec<u16>,
    mode: SettlePnlMode,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        &mut remaining_accounts,
        &get_writable_perp_market_set_from_vec(&market_indexes),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mut rev_share = RevenueShareSweep::load(&mut remaining_accounts, &user.authority, &state)?;

    let settlement = PnlSettlement {
        state_loader: &ctx.accounts.state,
        state: &state,
        clock: &clock,
        user_key,
        authority: ctx.accounts.authority.key,
        // The margin walk runs once for the whole batch, not once per market.
        meets_margin_requirement: Some(meets_settle_pnl_maintenance_margin_requirement(
            user, &mut maps,
        )?),
        mode,
    };

    for market_index in market_indexes.iter() {
        settlement.settle_market(*market_index, user, &mut maps, &mut rev_share)?;
    }

    let spot_market = maps.spot_market_map.get_quote_spot_market()?;
    validate_spot_market_vault_amount(&spot_market, ctx.accounts.spot_market_vault.amount)?;

    Ok(())
}

/// The escrow and beneficiary accounts a settle pays revenue share out of.
///
/// Both are absent while the builder-codes feature is off, and the escrow is
/// absent for a user who never created one.
struct RevenueShareSweep<'info> {
    escrow: Option<RevenueShareEscrowZeroCopyMut<'info>>,
    map: Option<crate::state::revenue_share_map::RevenueShareMap<'info>>,
}

impl<'info> RevenueShareSweep<'info> {
    fn load(
        remaining_accounts: &mut std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
        authority: &Pubkey,
        state: &State,
    ) -> Result<Self> {
        if !state.builder_codes_enabled() {
            return Ok(Self {
                escrow: None,
                map: None,
            });
        }
        Ok(Self {
            escrow: get_revenue_share_escrow_account(remaining_accounts, authority)?,
            map: load_revenue_share_map(remaining_accounts).ok(),
        })
    }
}

/// One user's settle, across one or more markets.
struct PnlSettlement<'a, 'info> {
    /// Held next to the loaded `State` because the settlement branch runs the
    /// `amm_not_paused` access control, which takes the loader.
    state_loader: &'a AccountLoader<'info, State>,
    state: &'a State,
    clock: &'a Clock,
    user_key: Pubkey,
    /// The caller that asked for the settle. `settle_pnl` compares it against
    /// the user's authority and delegate. A third party may not settle some of
    /// the cases the user may settle.
    authority: &'a Pubkey,
    /// `None` lets `settle_pnl` compute the requirement for itself.
    meets_margin_requirement: Option<bool>,
    mode: SettlePnlMode,
}

impl PnlSettlement<'_, '_> {
    /// Settle one market and discharge what the settle made payable.
    fn settle_market<'info>(
        &self,
        market_index: u16,
        user: &mut User,
        maps: &mut AccountMaps<'info>,
        rev_share: &mut RevenueShareSweep<'info>,
    ) -> Result<()> {
        let settled = self.settle_one_market(market_index, user, maps)?;
        self.discharge_revenue_share(market_index, user, maps, rev_share, settled)?;
        self.return_isolated_deposit(market_index, user, maps)
    }

    /// Settle the market and report whether the settle happened.
    ///
    /// Two paths settle nothing. A `settle_pnl` under `TrySettle` turns a pause
    /// or a degraded oracle into a no-op. A `settle_expired_position` for a user
    /// with no position returns before the market's SettlePnl pause checks. The
    /// caller ties the revenue-share sweep to this answer. The sweep moves
    /// builder and referrer fees out of the market's pnl pool, and a market that
    /// never settled must not be drained.
    fn settle_one_market(
        &self,
        market_index: u16,
        user: &mut User,
        maps: &mut AccountMaps,
    ) -> Result<bool> {
        let market_in_settlement =
            maps.perp_market_map.get_ref(&market_index)?.status == MarketStatus::Settlement;

        if !market_in_settlement {
            // No `update_amm` here: settle_pnl reads the live oracle and falls
            // back to the AMM's slot-fresh check only when the live oracle is
            // degraded. Either path is satisfied without an in-ix AMM refresh;
            // the keeper's `update_amms` crank or any prior fill in the same
            // slot provides the freshness when needed.
            let settled = controller::pnl::settle_pnl(
                market_index,
                user,
                self.authority,
                &self.user_key,
                maps,
                self.clock,
                self.state,
                self.meets_margin_requirement,
                self.mode,
            )?;
            return Ok(settled);
        }

        amm_not_paused(self.state_loader)?;

        let settled = controller::pnl::settle_expired_position(
            market_index,
            user,
            &self.user_key,
            maps,
            self.clock,
            self.state,
        )?;

        user.update_last_active_slot(self.clock.slot);
        Ok(settled)
    }

    /// Pay out the builder and referrer rows this settle completed.
    fn discharge_revenue_share<'info>(
        &self,
        market_index: u16,
        user: &User,
        maps: &mut AccountMaps<'info>,
        rev_share: &mut RevenueShareSweep<'info>,
        settled: bool,
    ) -> Result<()> {
        if !self.state.builder_codes_enabled() {
            return Ok(());
        }
        let Some(escrow) = rev_share.escrow.as_mut() else {
            return Ok(());
        };
        escrow.revoke_completed_orders(user)?;

        // Only sweep the market's pnl pool when the settle happened. A
        // soft-skipped settle must not move builder or referrer fees out of a
        // market that never settled.
        if !settled {
            return Ok(());
        }
        let Some(builder_map) = rev_share.map.as_ref() else {
            msg!("Builder Users not provided, but RevenueEscrow was provided");
            return Ok(());
        };

        // The settle above gated this oracle price for validity in the same
        // slot. The sweep reserves max(net_user_pnl, 0) with it, so it cannot
        // pay revenue share out of tokens that back a user's positive pnl.
        let oracle_price = {
            let perp_market = maps.perp_market_map.get_ref(&market_index)?;
            maps.oracle_map
                .get_price_data(&perp_market.oracle_id())?
                .price
        };
        let _ = controller::revenue_share::sweep_completed_revenue_share_for_market(
            market_index,
            escrow,
            &maps.perp_market_map,
            &maps.spot_market_map,
            builder_map,
            self.clock.unix_timestamp,
            oracle_price,
            self.state.builder_codes_enabled(),
            self.state.funding_paused()?,
        )?;
        Ok(())
    }

    /// Return an isolated-position deposit the settle freed.
    fn return_isolated_deposit(
        &self,
        market_index: u16,
        user: &mut User,
        maps: &mut AccountMaps,
    ) -> Result<()> {
        let Ok(position_index) = get_position_index(&user.perp_positions, market_index) else {
            return Ok(());
        };
        if !user.perp_positions[position_index].can_transfer_isolated_position_deposit() {
            return Ok(());
        }
        transfer_isolated_perp_position_deposit(
            user,
            None,
            maps,
            self.clock.slot,
            self.clock.unix_timestamp,
            QUOTE_SPOT_MARKET_INDEX,
            market_index,
            i64::MIN,
            self.state.funding_paused()?,
        )?;
        Ok(())
    }
}

#[access_control(
    funding_not_paused(&ctx.accounts.state)
)]
pub fn handle_settle_funding_payment<'c: 'info, 'info>(
    ctx: Context<'info, SettleFunding>,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;

    let maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &get_market_set_for_user_positions(&user.perp_positions),
        &MarketSet::new(),
        clock.slot,
        ctx.accounts.state.load()?.slot_clock(),
        None,
    )?;

    controller::funding::settle_funding_payments(user, &user_key, &maps.perp_market_map, now)?;
    user.update_last_active_slot(clock.slot);
    Ok(())
}

#[derive(Accounts)]
pub struct SettlePNL<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), 0_u16.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct SettleFunding<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}
