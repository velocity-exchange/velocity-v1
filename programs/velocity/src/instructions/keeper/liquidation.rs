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

    // The protocol must never warehouse the inventory a position-acquiring
    // liquidation takes on. The unsigned program-keeper mode therefore exists
    // for the with-fill flavor only, where the liquidator is only the filler.
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
    // Whatever the map and user sections did not take is the quoter section. It
    // holds the market's slab and the consulted quoters' registered CPI
    // accounts.
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

    // Three steps run in the order they must happen. The liquidation sizes its
    // order and places it. This handler routes the fill against the accounts
    // only it holds. The liquidation then books the result.
    let filled_quote = match controller::liquidation::place_liquidation_order(
        market_index,
        parties(),
        &mut maps,
        &clock,
        &state,
    )? {
        controller::liquidation::LiquidationStep::Settled => 0,
        controller::liquidation::LiquidationStep::Placed(mut placed) => {
            let fill = books.route_fill(&mut placed.order, &mut maps, &clock)?;
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

    // In program-keeper mode the caller's payout account earns reservoir
    // lamports for the crank; every other relay executor closes the same loop.
    // Only a crank that filled something is paid, since paying a no-op success
    // (exit or shortage) would let anyone drain the reservoir by looping on it.
    if filled_quote > 0 && ctx.accounts.liquidator.load()?.authority == state.signer {
        pay_liquidation_crank(&ctx, &state, &mut maps, market_index, filled_quote)?;
    }

    Ok(())
}

/// The liquidity a forced liquidation order fills against: the market's book
/// and its quoters through the router, the vAMM, and any makers the caller
/// loaded. A liquidation is the one taker order velocity writes for somebody
/// else, so it is also the one route nobody signs for. Who built the
/// transaction and how many locks it holds live here, not on the controller,
/// which knows only the order.
struct LiquidationBooks<'a, 'info> {
    state: &'a State,
    market_index: u16,
    user: &'a AccountLoader<'info, User>,
    user_stats: &'a AccountLoader<'info, UserStats>,
    /// The liquidator. A with-fill liquidation uses it only as the filler. It
    /// routes the position to the book and acquires no balance.
    liquidator: &'a AccountLoader<'info, User>,
    liquidator_stats: &'a AccountLoader<'info, UserStats>,
    makers_and_referrer: &'a UserMap<'info>,
    makers_and_referrer_stats: &'a UserStatsMap<'info>,
    /// The quoter section of the account list. It holds the market's
    /// `QuoterSlabV0` and the union of the consulted quoters' registered CPI
    /// accounts.
    tail: &'info [AccountInfo<'info>],
    instructions_sysvar: &'a Option<UncheckedAccount<'info>>,
}

impl<'info> LiquidationBooks<'_, 'info> {
    /// Fill the order the liquidation built, through the market's book, its
    /// quoters and the vAMM.
    ///
    /// The order holds no slot, so it arrives by reference and the fill writes
    /// its progress back through the same reference.
    fn route_fill(
        &self,
        order: &mut Order,
        maps: &mut AccountMaps<'info>,
        clock: &Clock,
    ) -> Result<controller::orders::FillAmounts> {
        let market_index = self.market_index;
        let routed = {
            let user = load!(self.user)?;
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
        let inputs = routed.quote_inputs(
            market_index,
            &users,
            self.served_window(&routed, clock.slot, &mut cpi_scratch)?,
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

        let mut books = quoted.books(clock, &mut cpi_scratch)?;
        let mut router = books.for_fill(crate::instructions::FillerStanding {
            protocol_authority: self.state.signer,
            taker_exposure_closed_by_caller: false,
            obligation,
        });

        Ok(controller::orders::fill_perp_order(
            controller::orders::FillRequest {
                // The forced order never reserved `open_bids`/`open_asks`, so
                // the fill must not unwind a reservation for it.
                order,
                reserved: false,
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
            &mut router,
        )?)
    }

    /// Whether the depth this liquidation can reach has measurably rested.
    ///
    /// A liquidation carries no attestation. The program writes its order in the
    /// same transaction that fills it, so nothing about the taker vouches for
    /// protected flow. This function measures the other half of the same
    /// promise, which is that the book's makers have had time to reprice. It
    /// measures it the way the cross cranks do, over the side this fill sweeps.
    /// Without the measure a book with a speed bump quotes a liquidation
    /// nothing, and the position it must close reaches the vAMM alone.
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

    let payment = {
        let conditions = reservoir.load()?;
        validate!(
            conditions.market_index == market_index,
            ErrorCode::DefaultError,
            "conditions are for market {}, the liquidation is market {}",
            conditions.market_index,
            market_index
        )?;

        // A fill below the dust floor liquidates the position but earns no flat
        // payment. Paying for each small step would let a keeper farm the flat
        // reward by slicing one liquidation into many.
        let flat = if filled_quote >= LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE {
            u64::from(conditions.crank_payments.liquidation)
        } else {
            0
        };

        // The liquidations batched into one transaction do not share the flat
        // payment. A relay crank carries its own payment guard, which measures
        // what this one instruction paid. A figure that fell with the size of
        // the batch would fail that guard and revert a liquidation that already
        // ran. The condition that arms the crank must also name the payment
        // before the batch exists. The priority fee is a whole-transaction cost,
        // so the reimbursement below does share it.
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

/// What the protocol adds to a liquidation crank's flat payment. It is the
/// priority fee the transaction paid, bounded by a share of what the
/// liquidation recovered.
///
/// This function reads the fee out of the transaction's own compute-budget
/// instructions. `CrankPaymentsV0::crank_priority_lamports` then prices it on
/// the lesser of the units the caller requested and the stored cost units. A
/// caller therefore cannot inflate the bill by asking for room it does not use.
///
/// Every reason to decline pays nothing extra instead of failing. A crank that
/// lands is worth more than one that reverts over its own tip, and the flat
/// payment still stands.
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

    // The priority fee is a whole-transaction cost, so the liquidations batched
    // into that transaction share it. Reimbursing each one the full figure would
    // pay the same fee once per liquidated account.
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
/// `None` declines the reimbursement. An oracle drives this value transfer, so
/// it takes the same validity gate as any other. An unusable price pays the flat
/// figure alone and does not revert the crank.
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
    /// CHECK: in signed-keeper mode this account must sign for `liquidator`. In
    /// program-keeper mode the liquidator is the protocol `User` and the caller
    /// is a relay turner. This account is then only the lamport payout target,
    /// and it needs no signature. Program-keeper mode reaches
    /// `liquidate_perp_with_fill` alone, because the plain path rejects it.
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
    /// The fired market's crank conditions. Its reservoir pays the keeper in
    /// program-keeper mode, and the handler checks it against `market_index`.
    /// Program-keeper mode requires the account.
    #[account(mut)]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    /// CHECK: the instructions sysvar, locked by address. It is present only for
    /// a crank that wants its priority fee reimbursed. The transaction's own
    /// compute-budget instructions state the fee, and the handler reads it back
    /// from here. Without this account the crank takes the flat payment.
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct LiquidateSpot<'info> {
    pub state: AccountLoader<'info, State>,
    /// A spot liquidation settles by handing the liquidator the borrow and the
    /// collateral behind it, so whoever liquidates takes on that inventory and
    /// its price risk. A protocol keeper (and so relay) has no way to unwind
    /// it, so this path requires a signer: an executor names none.
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
