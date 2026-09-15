//! The direct liquidation entrypoints.
//!
//! Each one moves a failing account's risk onto a liquidator, or onto the
//! book. The flash-loan pair lives in [`super::liquidation_swap`], and the
//! insurance-fund resolvers in [`super::bankruptcy`].

use super::*;

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_perp<'c: 'info, 'info>(
    ctx: Context<'info, LiquidatePerp<'info>>,
    market_index: u16,
    liquidator_max_base_asset_amount: u64,
    limit_price: Option<u64>,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let state = ctx.accounts.state.load()?;

    // A position-acquiring liquidation is inventory the protocol must never
    // warehouse: the unsigned program-keeper mode exists for the with-fill
    // flavor only, where the liquidator is just the filler.
    validate!(
        ctx.accounts.liquidator.load()?.authority != state.signer,
        ErrorCode::DefaultError,
        "the protocol user only liquidates via liquidate_perp_with_fill"
    )?;

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;
    let liquidator_stats = &mut load_mut!(ctx.accounts.liquidator_stats)?;

    require_liquidator_not_frozen(liquidator_stats)?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::liquidation::liquidate_perp(
        market_index,
        liquidator_max_base_asset_amount,
        limit_price,
        user,
        &user_key,
        user_stats,
        liquidator,
        &liquidator_key,
        liquidator_stats,
        &mut maps,
        slot,
        now,
        &state,
    )?;

    Ok(())
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_perp_with_fill<'c: 'info, 'info>(
    ctx: Context<'info, LiquidatePerp<'info>>,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;
    // Whatever the map and user sections did not take is the quoter section:
    // the market's slab plus the consulted quoters' registered CPI accounts.
    let tail =
        &ctx.remaining_accounts[ctx.remaining_accounts.len() - remaining_accounts_iter.len()..];

    let books = LiquidationBooks {
        state: &state,
        market_index,
        user: &ctx.accounts.user,
        user_stats: &ctx.accounts.user_stats,
        liquidator: &ctx.accounts.liquidator,
        liquidator_stats: &ctx.accounts.liquidator_stats,
        makers_and_referrer: &makers_and_referrer,
        makers_and_referrer_stats: &makers_and_referrer_stats,
        tail,
        instructions_sysvar: &ctx.accounts.instructions_sysvar,
    };
    let parties = || controller::liquidation::LiquidationParties {
        user: &ctx.accounts.user,
        user_key: &user_key,
        liquidator: &ctx.accounts.liquidator,
        liquidator_key: &liquidator_key,
    };

    // Three steps, in the order they have to happen: the liquidation sizes
    // its order and places it, this handler routes the fill against the
    // accounts only it holds, and the liquidation books the result.
    let filled_quote = match controller::liquidation::place_liquidation_order(
        market_index,
        parties(),
        &mut maps,
        &clock,
        &state,
    )? {
        controller::liquidation::LiquidationStep::Settled => 0,
        controller::liquidation::LiquidationStep::Placed(placed) => {
            let fill = books.route_fill(placed.order_id, &mut maps, &clock)?;
            controller::liquidation::settle_liquidation_fill(
                placed,
                fill,
                parties(),
                &mut maps,
                &clock,
                &state,
            )?
        }
    };

    // Program-keeper mode: the caller's payout account earns reservoir
    // lamports for the crank — the same loop every other relay executor
    // closes.
    //
    // Only for a crank that actually liquidated something. Several paths
    // through the controller succeed without filling: a user who can exit
    // liquidation exits it, and a shortage that allows no transfer transfers
    // nothing. Those are correct outcomes rather than errors, but they are
    // not work, and paying for them would let anyone empty the reservoir by
    // cranking a healthy account in a loop — which stops the cranks that do
    // matter. The filled quote is the proof, and it is the same figure the
    // reimbursement is capped against.
    if filled_quote > 0 && ctx.accounts.liquidator.load()?.authority == state.signer {
        pay_liquidation_crank(&ctx, &state, &mut maps, market_index, filled_quote)?;
    }

    Ok(())
}

/// The liquidity a forced liquidation order fills against: the market's book
/// and its quoters through the router, the vAMM, and any DLOB makers the
/// caller loaded.
///
/// A liquidation is the one taker order velocity writes for somebody else, so
/// it is also the one route nobody signs for. Everything it needs to answer
/// for that — who built the transaction, how many locks it holds — is here
/// rather than on the controller, which knows only the order.
struct LiquidationBooks<'a, 'info> {
    state: &'a State,
    market_index: u16,
    user: &'a AccountLoader<'info, User>,
    user_stats: &'a AccountLoader<'info, UserStats>,
    /// The liquidator, which a with-fill liquidation uses only as the filler:
    /// it routes the position to the book and acquires no balance.
    liquidator: &'a AccountLoader<'info, User>,
    liquidator_stats: &'a AccountLoader<'info, UserStats>,
    makers_and_referrer: &'a UserMap<'info>,
    makers_and_referrer_stats: &'a UserStatsMap<'info>,
    /// The quoter section of the account list: the market's `QuoterSlabV0`
    /// plus the union of the consulted quoters' registered CPI accounts.
    tail: &'info [AccountInfo<'info>],
    instructions_sysvar: &'a Option<UncheckedAccount<'info>>,
}

impl<'info> LiquidationBooks<'_, 'info> {
    /// Fill the order the liquidation placed, through the market's book, its
    /// quoters and the vAMM.
    fn route_fill(
        &self,
        order_id: u32,
        maps: &mut AccountMaps<'info>,
        clock: &Clock,
    ) -> Result<controller::liquidation::PerpFill> {
        let market_index = self.market_index;
        let order = {
            let user = load!(self.user)?;
            let order = user
                .get_order(order_id)
                .ok_or(ErrorCode::OrderDoesNotExist)?;
            RouteContext {
                market_index,
                maps,
                state: self.state,
                clock,
            }
            .routed_order(&user, order, FillMode::Liquidation)?
        };

        let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
        let users = crate::state::prop_amm::quoter_wire_users(
            self.makers_and_referrer.user_ref_index()?.into_keys().map(
                |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                    authority,
                    sub_account_id,
                },
            ),
        )?;
        let inputs = order.quote_inputs(
            market_index,
            &users,
            self.served_window(&order, clock.slot, &mut cpi_scratch)?,
        );
        let quoted = crate::instructions::quote_route(
            self.tail,
            inputs,
            // A liquidation order is written by the program, not signed by
            // its owner, so there is no route for a filler to substitute.
            None,
            &mut crate::instructions::CapInputs {
                taker_key: &self.user.key(),
                makers_and_referrer: self.makers_and_referrer,
                makers_and_referrer_stats: self.makers_and_referrer_stats,
                maps,
                slot: clock.slot,
                now: clock.unix_timestamp,
            },
            &mut cpi_scratch,
        )?;

        let obligation = crate::math::router::FillerObligation {
            // The liquidated account never signs its own liquidation, so the
            // caller answers for what its account list left out.
            taker_signed: false,
            tx_accounts: match self.instructions_sysvar {
                Some(sysvar) => Some(
                    crate::instructions::optional_accounts::tx_writable_lock_count(
                        &sysvar.to_account_info(),
                    )?,
                ),
                None => None,
            },
            unrouted_quoters: quoted.unrouted_quoters,
        };

        let (filled, _) = quoted.route_fill(
            crate::instructions::RouterTerms {
                protocol_authority: self.state.signer,
                taker_exposure_closed_by_caller: false,
                obligation,
            },
            clock,
            &mut cpi_scratch,
            |router| {
                controller::orders::fill_perp_order(
                    controller::orders::FillRequest {
                        target: controller::orders::FillTarget::Slot(order_id),
                        mode: FillMode::Liquidation,
                        referrer_is_accelerated: false,
                    },
                    self.state,
                    clock,
                    controller::orders::PerpFillAccounts {
                        user: self.user,
                        user_stats: self.user_stats,
                        filler: self.liquidator,
                        filler_stats: self.liquidator_stats,
                        rev_share_escrow: &mut None,
                    },
                    &mut controller::orders::FillParties {
                        maps,
                        makers_and_referrer: self.makers_and_referrer,
                        makers_and_referrer_stats: self.makers_and_referrer_stats,
                    },
                    router,
                )
                .map(
                    |(base_asset_amount, quote_asset_amount)| controller::liquidation::PerpFill {
                        base_asset_amount,
                        quote_asset_amount,
                    },
                )
                .map_err(Into::into)
            },
        )?;
        Ok(filled)
    }

    /// Whether the depth this liquidation can reach has measurably rested.
    ///
    /// A liquidation carries no attestation: its order is written by the
    /// program in the same transaction that fills it, so nothing about the
    /// taker vouches for protected flow. What can be vouched for is the other
    /// half of the same promise — that the book's makers have had time to
    /// reprice — and it is measured the way the cross cranks measure it,
    /// over the side this fill sweeps. Without it a book with a speed bump
    /// quotes a liquidation nothing at all, and the position it must close
    /// reaches the vAMM alone.
    fn served_window(
        &self,
        order: &RoutedOrder,
        slot: u64,
        cpi_scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> Result<bool> {
        let Some(slab) = crate::instructions::route_slab(self.tail, self.market_index)? else {
            return Ok(false);
        };
        crate::instructions::clob::helpers::crank_common::book_side_rested(
            &slab,
            self.tail,
            self.market_index,
            order.direction,
            order.unfilled,
            slot,
            cpi_scratch,
        )
    }
}

/// Pay the caller out of the market's reservoir for a liquidation crank.
fn pay_liquidation_crank<'info>(
    ctx: &Context<'info, LiquidatePerp<'info>>,
    state: &State,
    maps: &mut AccountMaps,
    market_index: u16,
    filled_quote: u64,
) -> Result<()> {
    let reservoir =
        ctx.accounts
            .crank_conditions
            .as_ref()
            .ok_or_else(|| -> anchor_lang::error::Error {
                msg!("program-keeper liquidation requires the market's conditions account");
                ErrorCode::DefaultError.into()
            })?;

    // The priority fee and the fixed part of the flat payment are both
    // whole-transaction costs, so both are shared between the liquidations
    // batched into one transaction. Count the peers once here.
    let claimants = match &ctx.accounts.instructions_sysvar {
        Some(sysvar) => crate::instructions::optional_accounts::tx_reimbursement_claimants(
            sysvar,
            crate::instruction::LiquidatePerpWithFill::DISCRIMINATOR,
        )?,
        None => 1,
    };

    let payment = {
        let conditions = reservoir.load()?;
        validate!(
            conditions.market_index == market_index,
            ErrorCode::DefaultError,
            "conditions are for market {}, the liquidation is market {}",
            conditions.market_index,
            market_index
        )?;
        // A fill below the dust floor liquidates the position but earns no
        // flat payment. Paying it per tiny step would let a keeper farm
        // the flat reward by slicing one liquidation into many.
        let flat = if filled_quote >= LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE {
            u64::from(conditions.crank_payments.liquidation)
        } else {
            0
        };
        // The flat payment prices one transaction's fixed cost once. A
        // batch shares that cost, so give back the part a lone crank would
        // over-claim across the peers that share the transaction.
        let fixed = state.transaction_fee_rails.fixed_cost();
        let over_claimed = fixed.saturating_sub(fixed / u64::from(claimants));
        let flat = flat.saturating_sub(over_claimed);
        flat.saturating_add(liquidation_reimbursement(
            &ctx.accounts.instructions_sysvar,
            state,
            &maps.spot_market_map,
            &mut maps.oracle_map,
            filled_quote,
        )?)
    };

    ClobCrankConditionsV0::pay_keeper(
        reservoir,
        &ctx.accounts.authority.to_account_info(),
        payment,
    )?;
    Ok(())
}

/// What the protocol adds to a liquidation crank's flat payment: the priority
/// fee the transaction paid, bounded by a share of what the liquidation
/// recovered.
///
/// Reads the fee out of the transaction's own compute-budget instructions,
/// and prices it against the *stored* cost units rather than the limit the
/// caller requested — a keeper made whole for a fair crank has no reason to
/// ask for room it does not use, and cannot inflate the bill by asking
/// anyway.
///
/// Every reason to decline pays nothing extra rather than erroring: a crank
/// that lands is worth more than one that reverts over its own tip, and the
/// flat payment still stands.
fn liquidation_reimbursement<'info>(
    instructions_sysvar: &Option<UncheckedAccount<'info>>,
    state: &State,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
    filled_quote: u64,
) -> Result<u64> {
    let Some(sysvar) = instructions_sysvar else {
        return Ok(0);
    };
    if state.liquidation_crank_reimbursement_bps == 0 || state.sol_spot_market_index == 0 {
        return Ok(0);
    }
    let (price_per_unit, requested_units) =
        crate::instructions::optional_accounts::tx_compute_budget(sysvar)?;
    if price_per_unit == 0 || requested_units == 0 {
        return Ok(0);
    }
    let Some(sol_price) = crank_sol_price(state, spot_market_map, oracle_map)? else {
        return Ok(0);
    };

    // The priority fee is a whole-transaction cost, so it is shared between the
    // liquidations batched into that transaction. Reimbursing each one the full
    // figure would pay the same fee over again per victim.
    let claimants = crate::instructions::optional_accounts::tx_reimbursement_claimants(
        sysvar,
        crate::instruction::LiquidatePerpWithFill::DISCRIMINATOR,
    )?;
    let priority_lamports = CrankPaymentsV0::crank_priority_lamports(
        price_per_unit,
        requested_units,
        u64::from(
            state
                .transaction_fee_rails
                .max_priority_micro_lamports_per_cu,
        ),
    )?
    .safe_div(u64::from(claimants))?;
    CrankPaymentsV0::liquidation_reimbursement(
        filled_quote,
        sol_price,
        priority_lamports,
        state.liquidation_crank_reimbursement_bps,
    )
    .map_err(Into::into)
}

/// The SOL price that converts a quote reimbursement into lamports.
///
/// `None` declines the reimbursement. This is a value transfer driven by an
/// oracle, so it takes the same validity gate as any other, and an unusable
/// price pays the flat figure alone rather than reverting the crank.
fn crank_sol_price(
    state: &State,
    spot_market_map: &SpotMarketMap,
    oracle_map: &mut OracleMap,
) -> Result<Option<i64>> {
    let Ok(sol_market) = spot_market_map.get_ref(&state.sol_spot_market_index) else {
        return Ok(None);
    };
    let (oracle_data, validity) = oracle_map.get_price_data_and_validity(
        MarketType::Spot,
        sol_market.market_index,
        &sol_market.oracle_id(),
        sol_market.historical_oracle_data.last_oracle_price_twap,
        sol_market.get_max_confidence_interval_multiplier()?,
        -1,
        0,
        None,
    )?;
    if !matches!(validity, crate::math::oracle::OracleValidity::Valid) {
        msg!("sol oracle is not valid for pricing the crank; paying the flat figure");
        return Ok(None);
    }
    Ok(Some(oracle_data.price))
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_spot<'c: 'info, 'info>(
    ctx: Context<'info, LiquidateSpot<'info>>,
    asset_market_index: u16,
    liability_market_index: u16,
    liquidator_max_liability_transfer: u128,
    limit_price: Option<u64>,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let state = ctx.accounts.state.load()?;

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;
    let liquidator_stats = load!(ctx.accounts.liquidator_stats)?;
    require_liquidator_not_frozen(&liquidator_stats)?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![asset_market_index, liability_market_index]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::liquidation::liquidate_spot(
        asset_market_index,
        liability_market_index,
        liquidator_max_liability_transfer,
        limit_price,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        clock.slot,
        &state,
    )?;

    Ok(())
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_borrow_for_perp_pnl<'c: 'info, 'info>(
    ctx: Context<'info, LiquidateBorrowForPerpPnl<'info>>,
    perp_market_index: u16,
    spot_market_index: u16,
    liquidator_max_liability_transfer: u128,
    limit_price: Option<u64>, // currently unimplemented
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let state = ctx.accounts.state.load()?;

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;
    // Taking over the user's borrow in exchange for positive pnl acquires
    // balance-sheet risk and earns a liquidation fee.
    let liquidator_stats = load!(ctx.accounts.liquidator_stats)?;
    require_liquidator_not_frozen(&liquidator_stats)?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set(spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::liquidation::liquidate_borrow_for_perp_pnl(
        perp_market_index,
        spot_market_index,
        liquidator_max_liability_transfer,
        limit_price,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        clock.slot,
        state.liquidation_margin_buffer_ratio,
        state.initial_pct_to_liquidate as u128,
        state.liquidation_duration_ms(),
        state.funding_paused()?,
    )?;

    Ok(())
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_perp_pnl_for_deposit<'c: 'info, 'info>(
    ctx: Context<'info, LiquidatePerpPnlForDeposit<'info>>,
    perp_market_index: u16,
    spot_market_index: u16,
    liquidator_max_pnl_transfer: u128,
    limit_price: Option<u64>, // currently unimplemented
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;
    // Taking over the user's deposit in exchange for negative pnl acquires
    // balance-sheet risk and earns a liquidation fee.
    let liquidator_stats = load!(ctx.accounts.liquidator_stats)?;
    require_liquidator_not_frozen(&liquidator_stats)?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set(spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::liquidation::liquidate_perp_pnl_for_deposit(
        perp_market_index,
        spot_market_index,
        liquidator_max_pnl_transfer,
        limit_price,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        clock.slot,
        state.liquidation_margin_buffer_ratio,
        state.initial_pct_to_liquidate as u128,
        state.liquidation_duration_ms(),
        state.funding_paused()?,
    )?;

    Ok(())
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_set_user_status_to_being_liquidated<'c: 'info, 'info>(
    ctx: Context<'info, SetUserStatusToBeingLiquidated<'info>>,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let user = &mut load_mut!(ctx.accounts.user)?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::liquidation::set_user_status_to_being_liquidated(
        user, &mut maps, clock.slot, &state,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct LiquidatePerp<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `liquidator`; in
    /// program-keeper mode (protocol `User` as liquidator, relay turners —
    /// `liquidate_perp_with_fill` ONLY, the plain path rejects it) it is
    /// only the lamport payout target and no signature is required.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = can_crank_for_filler(&liquidator, &authority, &state)?
    )]
    pub liquidator: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&liquidator, &liquidator_stats)?
    )]
    pub liquidator_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// The fired market's crank conditions — the reservoir that pays the
    /// keeper in program-keeper mode (validated against `market_index` in
    /// the handler). Required in program-keeper mode.
    #[account(mut)]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    /// CHECK: the instructions sysvar, locked by address. Present only for a
    /// crank that wants its priority fee reimbursed — the fee is stated in
    /// the transaction's own compute-budget instructions and read back from
    /// here. Absent, the crank takes the flat payment.
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct LiquidateSpot<'info> {
    pub state: AccountLoader<'info, State>,
    /// A spot liquidation settles by handing the liquidator the borrow and
    /// the collateral behind it, so whoever liquidates takes on that inventory
    /// and its price risk. That rules out a protocol keeper, which has no way
    /// to unwind it, and therefore rules out relay: an executor may name no
    /// signer, so a path that requires one is a signed-keeper path only.
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&liquidator, &authority)?
    )]
    pub liquidator: AccountLoader<'info, User>,
    #[account(
        constraint = is_stats_for_user(&liquidator, &liquidator_stats)?
    )]
    pub liquidator_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct LiquidateBorrowForPerpPnl<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&liquidator, &authority)?
    )]
    pub liquidator: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&liquidator, &liquidator_stats)?
    )]
    pub liquidator_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
pub struct LiquidatePerpPnlForDeposit<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&liquidator, &authority)?
    )]
    pub liquidator: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&liquidator, &liquidator_stats)?
    )]
    pub liquidator_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
pub struct SetUserStatusToBeingLiquidated<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
}
