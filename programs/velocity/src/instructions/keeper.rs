use {
    super::optional_accounts::get_token_interface,
    crate::{
        auth::check_hot,
        controller::{
            self,
            insurance::update_user_stats_if_stake_amount,
            isolated_position::transfer_isolated_perp_position_deposit,
            liquidation::{liquidate_spot_with_swap_begin, liquidate_spot_with_swap_end},
            orders::{cancel_orders, validate_spot_dlob_trading_enabled_for_market_type},
            position::{get_position_index, PositionDirection},
            spot_balance::update_spot_balances,
            token::{receive, send_from_program_vault},
        },
        error::ErrorCode,
        ids::{
            dflow_mainnet_aggregator_4, jupiter_mainnet_3, jupiter_mainnet_4, jupiter_mainnet_6,
            serum_program, titan_mainnet_argos_v1,
        },
        instructions::{
            constraints::*,
            optional_accounts::{
                add_builder_order, get_referrer_accelerated_status,
                get_revenue_share_escrow_account, load_escrow_owner_sub_accounts, load_maps,
                validate_and_load_builder, AccountMaps,
            },
        },
        load, load_mut,
        math::{
            self,
            bankruptcy::perp_markets_with_forfeitable_claims,
            casting::Cast,
            constants::{
                BID_ASK_TWAP_MAX_ORACLE_DIVERGENCE_PERCENT, BID_ASK_TWAP_MIN_QUOTE_REST,
                QUOTE_PRECISION_I128, QUOTE_PRECISION_U64, QUOTE_SPOT_MARKET_INDEX,
            },
            margin::{
                calculate_user_equity, calculate_user_equity_for_trip,
                meets_settle_pnl_maintenance_margin_requirement,
            },
            orders::{
                estimate_price_from_side, filter_bids_asks_by_oracle_divergence,
                find_bids_and_asks_from_users,
            },
            position::calculate_base_asset_value_and_pnl_with_oracle_price,
            router::RouterFillInputs,
            safe_math::SafeMath,
            spot_withdraw::validate_spot_market_vault_amount,
            time::Millis,
        },
        math_error,
        optional_accounts::{get_token_mint, update_prelaunch_oracle},
        print_error, safe_decrement,
        state::{
            clob_crank::{
                ClobCrankConditionsV0, CrankPaymentsV0, LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE,
            },
            events::{DeleteUserRecord, OrderActionExplanation, SignedMsgOrderRecord},
            fill_mode::FillMode,
            insurance_fund_stake::InsuranceFundStake,
            market_status::MarketStatus,
            oracle_map::OracleMap,
            order_params::{OrderParams, PlaceOrderOptions},
            paused_operations::{PerpLpOperation, PerpOperation, SpotOperation},
            perp_market::PerpMarket,
            perp_market_map::{
                get_market_set_for_spot_positions, get_market_set_for_user_positions,
                get_market_set_from_list, get_writable_perp_market_set,
                get_writable_perp_market_set_from_vec, MarketSet, PerpMarketMap,
            },
            prop_amm::{Direction, QuoterSlabV0},
            revenue_share::{RevenueShareEscrowZeroCopyMut, REVENUE_SHARE_ESCROW_PDA_SEED},
            revenue_share_map::load_revenue_share_map,
            settle_pnl_mode::SettlePnlMode,
            signed_msg_user::{
                SignedMsgOrderId, SignedMsgUserOrdersLoader, SignedMsgUserOrdersZeroCopyMut,
                SIGNED_MSG_PDA_SEED,
            },
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::{
                get_writable_spot_market_set, get_writable_spot_market_set_from_many, SpotMarketMap,
            },
            state::{HotRole, State},
            user::{MarketType, OrderStatus, OrderTriggerCondition, OrderType, User, UserStats},
            user_conditions::{UserConditionsV0, USER_CONDITIONS_PDA_SEED},
            user_map::{load_user_map, load_user_maps, UserMap, UserStatsMap},
            zero_copy::{AccountZeroCopyMut, ZeroCopyLoader},
        },
        validate,
        validation::{
            sig_verification::verify_and_decode_signed_msg,
            user::{validate_user_deletion, validate_user_is_idle},
        },
        vlp::{amm::math::amm::calculate_net_user_pnl, amm_cache::CacheInfo},
        OracleSource, ID,
    },
    anchor_lang::{prelude::*, Discriminator},
    anchor_spl::{
        associated_token::{get_associated_token_address_with_program_id, AssociatedToken},
        token_interface::{TokenAccount, TokenInterface},
    },
    solana_program::{
        pubkey,
        sysvar::instructions::{self, ID as IX_ID},
    },
    std::{cell::RefMut, convert::TryFrom},
};

/// The router fill: one quote → split → execute sweep across the vAMM
/// ladder (with last look over the rival books), any DLOB makers, and any
/// external quoters.
///
/// `remaining_accounts`, beyond the usual market/oracle/user-map section:
/// the market's `QuoterSlabV0` plus the union of the consulted quoters'
/// registered CPI accounts (including the quoter programs and the velocity
/// signer PDA). A slab slot is consulted when its response account rides the
/// call; each live consulted slot is quoted via CPI into a book, and
/// allocations that land on a book execute through the same slot's
/// `execute_v0`. No slab = vAMM + DLOB routing only —
/// allowed only while the market names no canonical book (`clob_market`).
#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_legacy_fill_perp_order<'c: 'info, 'info>(
    ctx: Context<'info, FillOrder<'info>>,
    order_id: Option<u32>,
    signed_route: Vec<Pubkey>,
) -> Result<()> {
    let (order_id, market_index) = {
        let user = &load!(ctx.accounts.user)?;
        // if there is no order id, use the users last order id
        let order_id = order_id.unwrap_or_else(|| user.get_last_order_id());
        let market_index = match user.get_order(order_id) {
            Some(order) => order.market_index,
            None => {
                msg!("Order does not exist {}", order_id);
                return Ok(());
            }
        };
        (order_id, market_index)
    };

    let obligation = {
        let taker = load!(ctx.accounts.user)?;
        crate::math::router::FillerObligation {
            taker_signed: taker.authority == ctx.accounts.authority.key()
                || (taker.delegate == ctx.accounts.authority.key()
                    && taker.delegate != Pubkey::default()),
            tx_accounts: ctx
                .accounts
                .instructions_sysvar
                .as_ref()
                .map(|sysvar| {
                    crate::instructions::optional_accounts::tx_writable_lock_count(sysvar)
                })
                .transpose()?,
            // Set after the route is assembled: only then is it known which
            // entries the transaction carried.
            unrouted_quoters: 0,
        }
    };
    let user_key = &ctx.accounts.user.key();
    // A keeper fill is never attested flow: the attestation transports are
    // the flow authority signing a swift-built transaction, or a detached
    // attestation bound to a signed-message order — a legacy slot order has
    // neither. On a bumped book the route quotes the book as empty and the
    // restable remainder migrates into the auction.
    let taker_served_window = false;
    fill_order(
        FillAccounts {
            state: &ctx.accounts.state,
            filler: &ctx.accounts.filler,
            filler_stats: &ctx.accounts.filler_stats,
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
        },
        ctx.remaining_accounts,
        order_id,
        market_index,
        signed_route,
        obligation,
        taker_served_window,
        None,
    )
    .inspect_err(|_e| {
        msg!(
            "Err filling order id {} for user {} for market index {}",
            order_id,
            user_key,
            market_index
        );
    })?;

    Ok(())
}

/// The accounts a fill needs, borrowed so `fill_perp_order` and
/// `fill_legacy_dlob_order` — which have different `#[derive(Accounts)]`
/// shapes — share one body.
pub struct FillAccounts<'a, 'info> {
    pub state: &'a AccountLoader<'info, State>,
    pub filler: &'a AccountLoader<'info, User>,
    pub filler_stats: &'a AccountLoader<'info, UserStats>,
    pub user: &'a AccountLoader<'info, User>,
    pub user_stats: &'a AccountLoader<'info, UserStats>,
}

/// `fill_legacy_dlob_order`'s way in: `fill_order` is private, and this names
/// why it is being called with a CLOB route rather than exposing the whole
/// body.
#[allow(clippy::too_many_arguments)]
pub fn fill_legacy_dlob_order_entry<'c: 'info, 'info>(
    accounts: FillAccounts<'_, 'info>,
    remaining_accounts: &'c [AccountInfo<'info>],
    order_id: u32,
    market_index: u16,
    signed_route: Vec<Pubkey>,
    obligation: crate::math::router::FillerObligation,
    taker_served_window: bool,
    clob: Option<crate::instructions::ClobRemainderRoute<'_, 'info>>,
) -> Result<()> {
    fill_order(
        accounts,
        remaining_accounts,
        order_id,
        market_index,
        signed_route,
        obligation,
        taker_served_window,
        clob,
    )
}

#[allow(clippy::too_many_arguments)]
fn fill_order<'c: 'info, 'info>(
    accounts: FillAccounts<'_, 'info>,
    remaining_accounts: &'c [AccountInfo<'info>],
    order_id: u32,
    market_index: u16,
    signed_route: Vec<Pubkey>,
    mut obligation: crate::math::router::FillerObligation,
    // Whether the transaction is attested taker flow. A book with a speed
    // bump quotes no depth to an unattested taker, so an unattested keeper
    // fill reaches the vAMM and the DLOB makers only, and the remainder
    // migrates to the book to wait its window.
    taker_served_window: bool,
    clob: Option<crate::instructions::ClobRemainderRoute<'_, 'info>>,
) -> Result<()> {
    let clock = &Clock::get()?;
    let state = accounts.state.load()?;

    let remaining_accounts_iter = &mut remaining_accounts.iter().peekable();
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

    let builder_codes_enabled = state.builder_codes_enabled();
    let mut escrow = if builder_codes_enabled {
        get_revenue_share_escrow_account(remaining_accounts_iter, &load!(accounts.user)?.authority)?
    } else {
        None
    };
    let referrer_is_accelerated =
        get_referrer_accelerated_status(remaining_accounts_iter, escrow.as_ref())?;

    // No `update_amm` here: `fill_perp_order` snaps the AMM and refreshes
    // PerpMarket-level oracle stats internally before quoting.

    // ---- Quote external quoters from the leftover accounts. ----
    // Everything past the map/user/escrow sections is the quoter section:
    // the market's `QuoterSlabV0` plus the union of the consulted quoters'
    // registered CPI accounts (programs, response accounts, the CPI signer).
    // The tail as a subslice rather than a collected list: what the sections
    // above consumed is the difference in the iterator's remaining length, and
    // borrowing from there costs nothing where cloning every account did.
    let tail_from = remaining_accounts.len() - remaining_accounts_iter.len();
    let tail = &remaining_accounts[tail_from..];
    let (direction, unfilled, taker_ref, route_digest, quote_limit_price) = {
        let user = load!(accounts.user)?;
        let order = user
            .get_order(order_id)
            .ok_or(ErrorCode::OrderDoesNotExist)?;
        let position_base = user
            .get_perp_position(market_index)
            .map(|position| position.base_asset_amount)
            .ok();
        let direction = match order.direction {
            PositionDirection::Long => Direction::Long,
            PositionDirection::Short => Direction::Short,
        };
        (
            direction,
            order.get_base_asset_amount_unfilled(position_base)?,
            user.clob_user_ref(),
            // A DLOB order carries no route. Only a signed message names one,
            // and such an order routes at placement and rests any remainder on
            // the market's CLOB, so what a route binds is the fill of that
            // remainder. `crank_taker_origin_cross` reads it from the taker's
            // signed-message record.
            crate::state::order_params::NO_ROUTE_DIGEST,
            FillMode::Fill.quote_limit_price(
                order,
                clock.slot,
                maps.perp_market_map.get_ref(&market_index)?.order_tick_size,
                state.slot_clock(),
            ),
        )
    };
    let route_reference_price = {
        let oracle_id = maps.perp_market_map.get_ref(&market_index)?.oracle_id();
        maps.oracle_map.get_price_data(&oracle_id)?.price
    };
    let inputs =
        crate::instructions::QuoteInputs {
            // Filled in below: sizing the makers needs the inputs, and the
            // quote needs the sizes.
            caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
            market_index,
            direction,
            size: unfilled,
            users: &crate::state::prop_amm::quoter_wire_users(
                makers_and_referrer.user_ref_index()?.into_keys().map(
                    |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                        authority,
                        sub_account_id,
                    },
                ),
            )?,
            reference_price: route_reference_price,
            taker: taker_ref,
            limit_price: quote_limit_price,
            taker_served_window,
            consume_reservation: false,
        };
    // Before the quote, so a book never publishes depth standing on a maker
    // this fill would refuse to settle against.
    let inputs = crate::instructions::QuoteInputs {
        caps: crate::instructions::build_user_caps(
            tail,
            &inputs,
            &mut crate::instructions::CapInputs {
                makers_and_referrer: &makers_and_referrer,
                makers_and_referrer_stats: &makers_and_referrer_stats,
                maps: &mut maps,
                slot: clock.slot,
                now: clock.unix_timestamp,
            },
        )?,
        ..inputs
    };

    // One set of CPI buffers for the fill: the quote legs below and the
    // execute legs the router runs later all refill the same allocation,
    // because velocity's heap never gives a freed one back.
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let route = crate::instructions::QuotedRoute::assemble(tail, &inputs, &mut cpi_scratch)?;
    route.require_baseline(maps.perp_market_map.get_ref(&market_index)?.clob_market)?;
    route.require_signed_route(&signed_route, route_digest)?;
    // Countable only now: the route is what says which entries arrived, and
    // the obligation is only consulted if a book later withholds.
    obligation.unrouted_quoters = route.unrouted_quoters(&signed_route, route_digest);

    let mut book_storage =
        [crate::math::router::QuoterBook::default(); crate::state::prop_amm::MAX_ROUTE_QUOTERS];
    let books = route.books(&mut book_storage)?;
    let mut executor = route.executor(&inputs, clock.slot, clock.unix_timestamp, &mut cpi_scratch);
    let mut router_inputs = RouterFillInputs {
        books,
        executor: &mut executor,
        protocol_authority: state.signer,
        taker_exposure_closed_by_caller: false,
        obligation,
        worst_fill_price: None,
    };

    let (base_asset_amount_filled, _) = controller::orders::fill_perp_order(
        controller::orders::FillTarget::Slot(order_id),
        &*accounts.state.load()?,
        accounts.user,
        accounts.user_stats,
        &mut maps,
        accounts.filler,
        accounts.filler_stats,
        &makers_and_referrer,
        &makers_and_referrer_stats,
        clock,
        FillMode::Fill,
        &mut router_inputs,
        &mut escrow.as_mut(),
        referrer_is_accelerated,
    )?;

    // v1 route only: a restable remainder belongs on the book, not in
    // `User.orders`. This is the hole the taker-remainder design closes — a
    // signed-message taker order cannot be IOC, so without this its leftover
    // rests on the DLOB forever, and the DLOB is not where a restable order
    // lives any more.
    //
    // Restable means the same thing it means on the place-and-take route: a
    // fixed price, no oracle offset, not reduce-only, since the CLOB has
    // neither oracle-floating nor reduce-only semantics. A market order rests
    // at its `auction_end_price`. `restable_remainder_price` is the whole
    // rule, shared with the place-and-take route so a remainder's fate does
    // not depend on which one reached it.
    if let Some(clob) = clob {
        let remainder = {
            let user = load!(accounts.user)?;
            // Only the taker's own remainder migrates. A keeper fills any user's
            // order, so without this a keeper could cancel a resting order that
            // did not cross and re-place it on the book as taker_origin. Two
            // gates bound it to a genuine taker remainder: the fill must have
            // made progress or the order must be a taker-class order (one with
            // an auction — a market or auction-limit taker), and the owner must
            // not be under liquidation. `restable_remainder_price` (post-only,
            // reduce-only, oracle-offset) carries the rest.
            if user.is_being_liquidated() {
                return Ok(());
            }
            let Ok(order_index) = user.get_order_index(order_id) else {
                return Ok(());
            };
            let order = &user.orders[order_index];
            if base_asset_amount_filled == 0 && !order.has_auction() {
                return Ok(());
            }
            crate::instructions::restable_remainder(&user, order, market_index, None)
        };
        if let Some(remainder) = remainder {
            if remainder.unfilled > 0 {
                controller::orders::cancel_order_by_order_id(
                    order_id,
                    accounts.user,
                    &mut maps,
                    clock,
                )?;
                crate::instructions::try_place_remainder_on_clob(
                    accounts.user,
                    clob.quoter_slab,
                    clob.clob_market,
                    clob.clob_program,
                    &mut maps,
                    market_index,
                    remainder.direction,
                    remainder.price,
                    remainder.unfilled,
                    remainder.max_ts,
                    order_id,
                    true,
                    false,
                    remainder.reduce_only,
                    None,
                    clock,
                )?;
            }
        }
    }

    Ok(())
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_revert_fill<'info>(ctx: Context<RevertFill>) -> Result<()> {
    let filler = load_mut!(ctx.accounts.filler)?;
    let clock = Clock::get()?;

    validate!(
        filler.last_active_slot == clock.slot,
        ErrorCode::RevertFill,
        "filler last active slot ({}) != current slot ({})",
        filler.last_active_slot,
        clock.slot
    )?;

    Ok(())
}

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

    // Load the map under the live State guard rails so every oracle-validity
    // decision on this path, including the lazy breaker trip on the cancel
    // branch, uses the same policy as the permissionless trip.
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
        &ctx.accounts.user,
        &ctx.accounts.user_stats,
        &mut maps,
        &ctx.accounts.filler,
        &Clock::get()?,
    )?;

    // Only a trigger that placed the order did payable work. A cancel (a
    // failing account whose trigger condition is already met), an
    // already-triggered order, or a no-op must not draw the reservoir — the
    // cancel branch pays the user no flat reward, so paying the caller from
    // the reservoir for it would be free lamports. Mirrors trigger_limit_order_v1,
    // whose cancel branch returns before this call.
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
    // the trip's own: positions with invalid oracles are conceded a bounded
    // most-favorable value instead of vetoing the proof, so dust in a
    // dead-oracle market cannot keep a material breach untrippable.
    let trip_equity = calculate_user_equity_for_trip(&user, &mut maps)?;

    // An authority-wide freeze must not arm over exposure the program cannot
    // value: an invalid-oracle asset or long past the dust allowance (or one
    // whose twap cannot size it) blocks the proof. The floor gates on
    // withdrawals/fills still hold independently of the breaker. The two
    // validates decompose `TripNetEquity::proves_breach` so each failure
    // keeps its error code.
    validate!(
        trip_equity.provable,
        ErrorCode::InvalidOracle,
        "cannot trip equity floor breaker: invalid oracle on a position the dust test cannot bound"
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

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_signed_msg_taker_order<'c: 'info, 'info>(
    ctx: Context<'info, PlaceSignedMsgTakerOrder<'info>>,
    signed_msg_order_params_message_bytes: Vec<u8>,
    is_delegate_signer: bool,
    flow_attestation: Option<crate::validation::sig_verification::FlowAttestationV0>,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    // The taker's own signature: the first 64 bytes of the envelope, and
    // what a flow attestation binds to. Captured before the placement
    // consumes the bytes.
    let taker_order_signature: Option<[u8; 64]> = signed_msg_order_params_message_bytes
        .get(..64)
        .and_then(|sig| <[u8; 64]>::try_from(sig).ok());
    // The market comes off the quoter slab rather than an argument, because
    // the slab is what the crank-conditions seed already derives from and the
    // two must name the same market. The message is checked against it once
    // decoded.
    let market_index = ctx.accounts.quoter_slab.load()?.market;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    // TODO: generalize to support multiple market types
    let mut maps = load_maps(
        &mut remaining_accounts,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(&mut remaining_accounts, true)?;

    let taker_key = ctx.accounts.user.key();
    let escrow = if state.builder_codes_enabled() {
        get_revenue_share_escrow_account(
            &mut remaining_accounts,
            &load!(ctx.accounts.user)?.authority,
        )?
    } else {
        None
    };
    let referrer_is_accelerated =
        get_referrer_accelerated_status(&mut remaining_accounts, escrow.as_ref())?;

    // Everything past the sections above is the quoter tail: registry entries
    // plus the union of their registered CPI accounts.
    let tail_from = ctx.remaining_accounts.len() - remaining_accounts.len();
    let tail = &ctx.remaining_accounts[tail_from..];

    // ---- Placement. The taker's `User` is borrowed for this leg alone: the
    // router fill below takes the loader, not a live borrow. ----
    let (mut escrow, placed) = {
        let mut taker = load_mut!(ctx.accounts.user)?;
        let mut taker_stats = load_mut!(ctx.accounts.user_stats)?;
        let mut signed_msg_taker = ctx.accounts.signed_msg_user_orders.load_mut()?;
        place_signed_msg_taker_order(
            taker_key,
            &mut taker,
            &mut taker_stats,
            &mut signed_msg_taker,
            signed_msg_order_params_message_bytes,
            &mut maps,
            escrow,
            &state,
            is_delegate_signer,
        )?
    };

    // The message was stale, replayed, or past its placement deadline. Those
    // are no-ops rather than failures, so there is nothing to fill.
    let Some(mut placed) = placed else {
        return Ok(());
    };
    validate!(
        placed.market_index == market_index,
        ErrorCode::InvalidSignedMsgOrderParam,
        "signed message names market {} but the passed CLOB entry is for {}",
        placed.market_index,
        market_index
    )?;

    // Maker priority: on a book with a speed bump, only attested flow fills
    // synchronously. An unattested submission of a signed message rests the
    // whole order taker-origin through the activation window instead, and
    // the cross cranks fill it. The attestation is detached: swift signs
    // over the taker's own order signature after the hold, so the flow
    // authority never signs a keeper-built transaction and the fill pays no
    // second signature fee. The placement above already validated the
    // envelope, so the signature prefix is present.
    let taker_served_window = match flow_attestation {
        Some(ref attestation) => {
            crate::validation::sig_verification::verify_flow_attestation(
                attestation,
                &state.hot_key(crate::state::state::HotRole::FlowAuthority),
                &taker_order_signature.ok_or(ErrorCode::SigVerificationFailed)?,
                clock.unix_timestamp,
            )?;
            true
        }
        None => false,
    };
    let synchronous_take = crate::instructions::synchronous_take_allowed(
        taker_served_window,
        &ctx.accounts.quoter_slab,
        market_index,
    )?;

    // The fill mutates the ephemeral order's filled amounts in place; the rest
    // leg reads its remainder from there.
    let filled = if synchronous_take {
        fill_signed_msg_taker_order(
            &ctx,
            tail,
            &mut placed,
            &state,
            &mut maps,
            &makers_and_referrer,
            &makers_and_referrer_stats,
            &mut escrow,
            referrer_is_accelerated,
            taker_served_window,
            &clock,
        )?
    } else {
        msg!("unattested taker on a bumped book; the order rests whole");
        0
    };

    rest_signed_msg_remainder(&ctx, &placed, filled, &mut maps, &clock)?;

    if let Some(ref mut escrow) = escrow {
        let taker = load_mut!(ctx.accounts.user)?;
        escrow.revoke_completed_orders(&taker)?;
    }
    Ok(())
}

/// Route the freshly placed signed-message order and fill what the route
/// reaches at or better than its auction start price.
///
/// The taker did not sign this transaction, so the keeper is a filler and the
/// obligation rules apply: it owes the taker every maker it had room to carry,
/// and it must carry every quoter the message named. `require_signed_route`
/// states the second rule. The route is the message's own list here rather
/// than a claim the caller makes, because the message is in this transaction —
/// only a later fill of the rested remainder has to work from the digest.
#[allow(clippy::too_many_arguments)]
fn fill_signed_msg_taker_order<'c: 'info, 'info>(
    ctx: &Context<'info, PlaceSignedMsgTakerOrder<'info>>,
    tail: &'info [AccountInfo<'info>],
    placed: &mut PlacedSignedMsgOrder,
    state: &State,
    maps: &mut AccountMaps<'info>,
    makers_and_referrer: &UserMap<'info>,
    makers_and_referrer_stats: &UserStatsMap<'info>,
    escrow: &mut Option<RevenueShareEscrowZeroCopyMut<'info>>,
    referrer_is_accelerated: bool,
    taker_served_window: bool,
    clock: &Clock,
) -> Result<u64> {
    let market_index = placed.market_index;
    let (direction, unfilled, taker_ref, quote_limit_price) = {
        let user = load!(ctx.accounts.user)?;
        // The order lives on `placed`, not `user.orders`. It is the taker order
        // this leg fills detached.
        let order = &placed.order;
        let position_base = user
            .get_perp_position(market_index)
            .map(|position| position.base_asset_amount)
            .ok();
        (
            match order.direction {
                PositionDirection::Long => Direction::Long,
                PositionDirection::Short => Direction::Short,
            },
            order.get_base_asset_amount_unfilled(position_base)?,
            user.clob_user_ref(),
            // Zero progress prices the auction at its start. A signed-message
            // order takes only genuine improvement now and rests the rest, so
            // it never pays its own slippage bound to whoever lands first.
            FillMode::PlaceAndTake(placed.is_immediate_or_cancel, 0).quote_limit_price(
                order,
                clock.slot,
                maps.perp_market_map.get_ref(&market_index)?.order_tick_size,
                state.slot_clock(),
            ),
        )
    };
    if unfilled == 0 {
        return Ok(0);
    }

    let route_reference_price = {
        let oracle_id = maps.perp_market_map.get_ref(&market_index)?.oracle_id();
        maps.oracle_map.get_price_data(&oracle_id)?.price
    };
    let inputs =
        crate::instructions::QuoteInputs {
            caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
            market_index,
            direction,
            size: unfilled,
            users: &crate::state::prop_amm::quoter_wire_users(
                makers_and_referrer.user_ref_index()?.into_keys().map(
                    |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                        authority,
                        sub_account_id,
                    },
                ),
            )?,
            reference_price: route_reference_price,
            taker: taker_ref,
            limit_price: quote_limit_price,
            taker_served_window,
            consume_reservation: false,
        };
    // Before the quote, so a book never publishes depth standing on a maker
    // this fill would refuse to settle against.
    let inputs = crate::instructions::QuoteInputs {
        caps: crate::instructions::build_user_caps(
            tail,
            &inputs,
            &mut crate::instructions::CapInputs {
                makers_and_referrer,
                makers_and_referrer_stats,
                maps,
                slot: clock.slot,
                now: clock.unix_timestamp,
            },
        )?,
        ..inputs
    };

    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let route = crate::instructions::QuotedRoute::assemble(tail, &inputs, &mut cpi_scratch)?;
    route.require_baseline(maps.perp_market_map.get_ref(&market_index)?.clob_market)?;
    let digest = placed.route_digest;
    route.require_signed_route(&placed.route, digest)?;

    let obligation = crate::math::router::FillerObligation {
        // A signed message is not a signed transaction. The keeper chose the
        // account list, so it answers for what that list left out.
        taker_signed: false,
        tx_accounts: Some(
            crate::instructions::optional_accounts::tx_writable_lock_count(
                &ctx.accounts.ix_sysvar.to_account_info(),
            )?,
        ),
        unrouted_quoters: route.unrouted_quoters(&placed.route, digest),
    };

    let mut book_storage =
        [crate::math::router::QuoterBook::default(); crate::state::prop_amm::MAX_ROUTE_QUOTERS];
    let books = route.books(&mut book_storage)?;
    let mut executor = route.executor(&inputs, clock.slot, clock.unix_timestamp, &mut cpi_scratch);
    let mut router_inputs = RouterFillInputs {
        books,
        executor: &mut executor,
        protocol_authority: state.signer,
        taker_exposure_closed_by_caller: false,
        obligation,
        worst_fill_price: None,
    };

    let (base_asset_amount_filled, _) = controller::orders::fill_perp_order(
        // The taker order is ephemeral: it never reserved, so the fill unwinds
        // no exposure for it.
        controller::orders::FillTarget::Detached {
            order: &mut placed.order,
            reserved: false,
        },
        state,
        &ctx.accounts.user,
        &ctx.accounts.user_stats,
        maps,
        &ctx.accounts.filler,
        &ctx.accounts.filler_stats,
        makers_and_referrer,
        makers_and_referrer_stats,
        clock,
        FillMode::PlaceAndTake(placed.is_immediate_or_cancel, 0),
        &mut router_inputs,
        &mut escrow.as_mut(),
        referrer_is_accelerated,
    )?;
    Ok(base_asset_amount_filled)
}

/// Rest what the route could not fill, on the market's book.
///
/// A signed-message order never rests on the DLOB. Either it is
/// immediate-or-cancel and its residual is cancelled, or the residual migrates
/// to the CLOB as a taker-origin order and competes for price inside its
/// activation window. `restable_remainder_price` is the shared rule for which
/// residuals can rest at all.
///
/// The CLOB order id goes back onto the message's own record, which is how the
/// fill at the activation slot — a different transaction, built by somebody
/// else — finds the route this taker signed for.
#[allow(clippy::too_many_arguments)]
fn rest_signed_msg_remainder<'c: 'info, 'info>(
    ctx: &Context<'info, PlaceSignedMsgTakerOrder<'info>>,
    placed: &PlacedSignedMsgOrder,
    base_asset_amount_filled: u64,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> Result<()> {
    let market_index = placed.market_index;
    let remainder = {
        let user = load!(ctx.accounts.user)?;
        if user.is_being_liquidated() {
            return Ok(());
        }
        // The order lives on `placed`, not `user.orders`. Its filled amounts
        // were updated in place by the fill leg.
        let order = &placed.order;
        if base_asset_amount_filled == 0 && !order.has_auction() {
            return Ok(());
        }
        crate::instructions::restable_remainder(&user, order, market_index, None)
    };

    // Immediate-or-cancel asked for no residual. The order never persisted, so
    // dropping it is enough — nothing of it can fill after the window its signer
    // allowed.
    if placed.is_immediate_or_cancel {
        return Ok(());
    }

    let Some(remainder) = remainder else {
        return Ok(());
    };
    if remainder.unfilled == 0 {
        return Ok(());
    }

    // No slot to cancel: the order never entered `user.orders`. Its remainder
    // migrates straight onto the CLOB.
    let rested = crate::instructions::try_place_remainder_on_clob(
        &ctx.accounts.user,
        &ctx.accounts.quoter_slab,
        &ctx.accounts.clob_market.to_account_info(),
        &ctx.accounts.clob_program.to_account_info(),
        maps,
        market_index,
        remainder.direction,
        remainder.price,
        remainder.unfilled,
        remainder.max_ts,
        placed.order_id,
        true,
        false,
        remainder.reduce_only,
        None,
        clock,
    )?;

    if let Some(clob_order_id) = rested {
        ctx.accounts
            .signed_msg_user_orders
            .load_mut()?
            .set_resting_route(placed.uuid, clob_order_id, placed.route_digest);
    }
    Ok(())
}

pub fn place_signed_msg_taker_order<'c: 'info, 'info>(
    taker_key: Pubkey,
    taker: &mut RefMut<User>,
    taker_stats: &mut RefMut<UserStats>,
    signed_msg_account: &mut SignedMsgUserOrdersZeroCopyMut,
    taker_order_params_message_bytes: Vec<u8>,
    maps: &mut AccountMaps,
    escrow: Option<RevenueShareEscrowZeroCopyMut<'info>>,
    state: &State,
    is_delegate_signer: bool,
) -> Result<(
    Option<RevenueShareEscrowZeroCopyMut<'info>>,
    Option<PlacedSignedMsgOrder>,
)> {
    // Authenticate the signed msg order param message. The taker's signature
    // is verified in-program over the message the argument carries, so no
    // preceding ed25519 precompile instruction is required.
    let signer = if is_delegate_signer {
        taker.delegate.to_bytes()
    } else {
        taker.authority.to_bytes()
    };
    let verified_message_and_signature = verify_and_decode_signed_msg(
        &taker_order_params_message_bytes[..],
        &signer,
        is_delegate_signer,
    )?;

    let (mut escrow_zc, builder_fee_bps) = validate_and_load_builder(
        escrow,
        &taker.authority,
        verified_message_and_signature.builder_idx,
        verified_message_and_signature.builder_fee_tenth_bps,
        state,
    )?;

    if is_delegate_signer {
        validate!(
            verified_message_and_signature.delegate_signed_taker_pubkey == Some(taker_key),
            ErrorCode::SignedMsgUserContextUserMismatch,
            "Delegate signed msg for taker pubkey different than supplied pubkey"
        )?;
    } else {
        // Verify taker passed to the ix matches pda derived from subaccount id + authority
        let taker_pda = Pubkey::find_program_address(
            &[
                "user".as_bytes(),
                &taker.authority.to_bytes(),
                &verified_message_and_signature
                    .sub_account_id
                    .unwrap()
                    .to_le_bytes(),
            ],
            &ID,
        );
        validate!(
            taker_pda.0 == taker_key,
            ErrorCode::SignedMsgUserContextUserMismatch,
            "Taker key does not match pda"
        )?;
    };

    let signature = verified_message_and_signature.signature;
    let clock = &Clock::get()?;

    // First order must be a taker order
    let matching_taker_order_params = &verified_message_and_signature.signed_msg_order_params;
    if matching_taker_order_params.market_type != MarketType::Perp
        || !matching_taker_order_params.has_valid_auction_params()?
    {
        msg!("First order must be a perp taker order");
        return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
    }

    // Immediate-or-cancel is allowed here. The hazard it used to carry was a
    // stored IOC limit order resting forever, because a limit order defaults
    // `max_ts` to 0 and residual cancellation only existed in the take and make
    // fill modes. This instruction now routes and fills in the same
    // transaction, and cancels the residual rather than storing it, so nothing
    // of an IOC order survives the call.

    // Set max slot for the order early so we set correct signed msg order id
    let order_slot = verified_message_and_signature.slot;
    if order_slot > clock.slot {
        msg!(
            "SignedMsg order slot {} is ahead of current slot {}",
            order_slot,
            clock.slot
        );
        return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
    }
    // ~200s of wall clock age, integrated per slot duration regime
    let max_order_age = Millis::from_secs(200);
    if state.slot_clock().elapsed(order_slot, clock.slot) > max_order_age {
        msg!(
            "SignedMsg order slot {} is too old: must be within {}ms of current slot {}",
            order_slot,
            max_order_age.as_ms(),
            clock.slot
        );
        return Err(print_error!(ErrorCode::InvalidSignedMsgOrderParam)().into());
    }
    let market_index = matching_taker_order_params.market_index;
    // `auction_duration` is in wall clock 400ms units. Resolve the first slot
    // reaching that duration across every known future transition so placement
    // expiry cannot disagree with auction completion at a gate boundary.
    let auction_duration_units = if matching_taker_order_params.order_type == OrderType::Limit {
        matching_taker_order_params.auction_duration.unwrap_or(0)
    } else {
        matching_taker_order_params.auction_duration.unwrap()
    };
    let max_slot = state.slot_clock().slot_at_or_after_duration(
        order_slot,
        Millis::from_stored_units(auction_duration_units as u64),
    );

    // Dont place order if max slot already passed
    if max_slot < clock.slot {
        msg!(
            "SignedMsg order max_slot {} < current slot {}",
            max_slot,
            clock.slot
        );
        return Ok((escrow_zc, None));
    }

    // Dont place order if signed msg order already exists
    let mut signed_msg_order_id =
        SignedMsgOrderId::new(verified_message_and_signature.uuid, max_slot, 0);
    if signed_msg_account.check_exists_and_prune_stale_signed_msg_order_ids(
        signed_msg_order_id,
        clock.slot,
        state.slot_clock(),
    ) {
        msg!("SignedMsg order already exists for taker {:?}", taker_key);
        return Ok((escrow_zc, None));
    }

    if let Some(max_margin_ratio) = verified_message_and_signature.max_margin_ratio {
        taker.update_perp_position_max_margin_ratio(market_index, max_margin_ratio)?;
    }

    #[cfg(feature = "isolated-position")]
    if let Some(isolated_position_deposit) =
        verified_message_and_signature.isolated_position_deposit
    {
        maps.spot_market_map.update_writable_spot_market(0)?;
        transfer_isolated_perp_position_deposit(
            taker,
            Some(taker_stats),
            maps,
            clock.slot,
            clock.unix_timestamp,
            0,
            market_index,
            isolated_position_deposit.cast::<i64>()?,
            state.funding_paused()?,
        )?;
    }
    #[cfg(not(feature = "isolated-position"))]
    {
        let _ = &taker_stats;
        validate!(
            verified_message_and_signature
                .isolated_position_deposit
                .is_none(),
            ErrorCode::IsolatedPositionDisabled,
            "signed msg isolated position deposit not enabled in this build"
        )?;
    }

    // #84: if the main taker order would soft-skip on an already-expired
    // `max_ts`, place NOTHING. The reduce-only TP/SL sidecars below are trigger
    // orders, which are exempt from `max_ts` expiry, so without this pre-check
    // they'd be installed as standalone triggers even though the main entry
    // never existed — breaking the bundle's atomicity. Checked up front (rather
    // than placing the main first) so the sidecars keep their order ids and the
    // main keeps the trailing id that clients and the SignedMsgOrderRecord rely
    // on. (`place_perp_order`'s only other soft-skip, a `TryPostOnly` that would
    // cross, does not apply to a signed-msg taker order — takers are not
    // post-only.)
    if let Some(max_ts) = matching_taker_order_params.max_ts {
        if max_ts != 0 && max_ts < clock.unix_timestamp {
            msg!(
                "signed msg main order max_ts {} expired (< now {}); skipping bundle",
                max_ts,
                clock.unix_timestamp
            );
            return Ok((escrow_zc, None));
        }
    }

    // Good to place orders, do stop loss and take profit orders first. Each
    // builder row is keyed to `taker.next_order_id`, the id `place_perp_order`
    // will assign; the main order below therefore takes the trailing id.
    if let Some(stop_loss_order_params) = verified_message_and_signature.stop_loss_order_params {
        let stop_loss_order = OrderParams {
            order_type: OrderType::TriggerMarket,
            direction: matching_taker_order_params.direction.opposite(),
            trigger_price: Some(stop_loss_order_params.trigger_price),
            base_asset_amount: stop_loss_order_params.base_asset_amount,
            trigger_condition: if matching_taker_order_params.direction == PositionDirection::Long {
                OrderTriggerCondition::Below
            } else {
                OrderTriggerCondition::Above
            },
            market_index,
            market_type: MarketType::Perp,
            reduce_only: true,
            ..OrderParams::default()
        };

        let mut builder_order = add_builder_order(
            &mut escrow_zc,
            taker,
            verified_message_and_signature.builder_idx,
            builder_fee_bps,
            taker.next_order_id,
            market_index,
        )?;

        controller::orders::place_perp_order(
            state,
            taker,
            taker_key,
            maps,
            clock,
            stop_loss_order,
            PlaceOrderOptions {
                enforce_margin_check: false,
                existing_position_direction_override: Some(matching_taker_order_params.direction),
                ..PlaceOrderOptions::default()
            },
            &mut builder_order,
        )?;
    }

    if let Some(take_profit_order_params) = verified_message_and_signature.take_profit_order_params
    {
        let take_profit_order = OrderParams {
            order_type: OrderType::TriggerMarket,
            direction: matching_taker_order_params.direction.opposite(),
            trigger_price: Some(take_profit_order_params.trigger_price),
            base_asset_amount: take_profit_order_params.base_asset_amount,
            trigger_condition: if matching_taker_order_params.direction == PositionDirection::Long {
                OrderTriggerCondition::Above
            } else {
                OrderTriggerCondition::Below
            },
            market_index,
            market_type: MarketType::Perp,
            reduce_only: true,
            ..OrderParams::default()
        };

        let mut builder_order = add_builder_order(
            &mut escrow_zc,
            taker,
            verified_message_and_signature.builder_idx,
            builder_fee_bps,
            taker.next_order_id,
            market_index,
        )?;

        controller::orders::place_perp_order(
            state,
            taker,
            taker_key,
            maps,
            clock,
            take_profit_order,
            PlaceOrderOptions {
                enforce_margin_check: false,
                existing_position_direction_override: Some(matching_taker_order_params.direction),
                ..PlaceOrderOptions::default()
            },
            &mut builder_order,
        )?;
    }

    // Carry the taker's chosen route on the message's own record. A keeper
    // builds the fill transaction, so without this the route is a suggestion it
    // can ignore, and the whole point of the taker signing one is that they
    // pick which quoters compete. The record outlives the message, which is
    // what lets a fill in a later transaction still be held to it.
    signed_msg_order_id.order_id = taker.next_order_id;
    signed_msg_order_id.route_digest = verified_message_and_signature
        .route
        .as_deref()
        .map(crate::state::order_params::route_digest)
        .unwrap_or(crate::state::order_params::NO_ROUTE_DIGEST);
    signed_msg_account.add_signed_msg_order_id(signed_msg_order_id)?;

    let mut builder_order = add_builder_order(
        &mut escrow_zc,
        taker,
        verified_message_and_signature.builder_idx,
        builder_fee_bps,
        taker.next_order_id,
        market_index,
    )?;

    // Sweep expired slot orders first: their reservations release, which
    // can be what lets the new order pass the margin gate. The create never
    // touches `user.orders`, so the sweep is the caller's.
    controller::orders::expire_orders(taker, &taker_key, maps, clock.unix_timestamp, clock.slot)?;

    // The taker order never enters `user.orders`. It is built, margin-checked,
    // routed straight to the book, and only its remainder rests on the CLOB.
    let Some(ephemeral_order) = controller::orders::create_ephemeral_perp_order(
        state,
        taker,
        taker_key,
        maps,
        clock,
        *matching_taker_order_params,
        PlaceOrderOptions {
            enforce_margin_check: true,
            signed_msg_taker_order_slot: Some(order_slot),
            ..PlaceOrderOptions::default()
        },
        &mut builder_order,
    )?
    else {
        // The order soft-skipped its build (expired `max_ts`, or a `TryPostOnly`
        // that would cross). There is nothing to fill or rest.
        return Ok((escrow_zc, None));
    };

    // `signature` is `[u8; 64]`; borsh serializes it as its raw bytes, so hash
    // the array directly rather than allocating an identical copy.
    let order_params_hash = base64::encode(solana_program::hash::hash(&signature).as_ref());

    emit!(SignedMsgOrderRecord {
        user: taker_key,
        signed_msg_order_max_slot: signed_msg_order_id.max_slot,
        signed_msg_order_uuid: signed_msg_order_id.uuid,
        user_order_id: signed_msg_order_id.order_id,
        matching_order_params: *matching_taker_order_params,
        hash: order_params_hash,
        ts: clock.unix_timestamp,
    });

    // `revoke_completed_orders` is deliberately not run here. The fill leg
    // follows in the same instruction and completes orders of its own, so the
    // caller revokes once, after it.
    Ok((
        escrow_zc,
        Some(PlacedSignedMsgOrder {
            order_id: signed_msg_order_id.order_id,
            order: ephemeral_order,
            uuid: signed_msg_order_id.uuid,
            market_index,
            route_digest: signed_msg_order_id.route_digest,
            route: verified_message_and_signature.route.unwrap_or_default(),
            is_immediate_or_cancel: matching_taker_order_params.is_immediate_or_cancel(),
        }),
    ))
}

/// What the placement leg hands the fill leg.
///
/// The placement holds the taker's `User` borrowed for its whole body, and the
/// router fill takes the loader instead, so the two cannot run inside one
/// borrow. This is the state that has to cross that boundary.
pub struct PlacedSignedMsgOrder {
    pub order_id: u32,
    /// The ephemeral taker order. It never enters `user.orders`: the fill leg
    /// routes it detached and mutates its filled amounts here, and the rest leg
    /// reads its remainder from here to migrate onto the CLOB.
    pub order: crate::state::user::Order,
    pub uuid: [u8; 8],
    pub market_index: u16,
    /// The custom quoters the taker's message named. The fill must carry every
    /// one of them; the CLOB and the vAMM are the baseline and are not listed.
    pub route: Vec<Pubkey>,
    /// The digest of `route`, computed once at placement. The fill and the rest
    /// both hold the quoters they carry against it, so it is cached rather than
    /// re-hashed at each.
    pub route_digest: crate::state::order_params::RouteDigest,
    /// The taker asked for no remainder to rest.
    pub is_immediate_or_cancel: bool,
}

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

    let (mut builder_escrow, maybe_rev_share_map) = if state.builder_codes_enabled() {
        (
            get_revenue_share_escrow_account(&mut remaining_accounts, &user.authority)?,
            load_revenue_share_map(&mut remaining_accounts).ok(),
        )
    } else {
        (None, None)
    };

    let market_in_settlement =
        maps.perp_market_map.get_ref(&market_index)?.status == MarketStatus::Settlement;

    // Whether settlement actually happened this call. The revenue-share sweep
    // moves builder/referrer fees out of the market's pnl pool, so it must only
    // run when settlement truly happened. Two calls settle nothing and must not
    // drain the pool. A `settle_pnl` under TrySettle turns a pause or a degraded
    // oracle into a no-op. A `settle_expired_position` for a user with no
    // position returns before the market's SettlePnl pause checks.
    let settled = if market_in_settlement {
        amm_not_paused(&ctx.accounts.state)?;

        let settled = controller::pnl::settle_expired_position(
            market_index,
            user,
            &user_key,
            &mut maps,
            &clock,
            &state,
        )?;

        user.update_last_active_slot(clock.slot);
        settled
    } else {
        // No `update_amm` here: settle_pnl reads the live oracle and falls
        // back to the AMM's slot-fresh check only when the live oracle is
        // degraded. Either path is satisfied without an in-ix AMM refresh;
        // the keeper's `update_amms` crank or any prior fill in the same
        // slot provides the freshness when needed.

        controller::pnl::settle_pnl(
            market_index,
            user,
            ctx.accounts.authority.key,
            &user_key,
            &mut maps,
            &clock,
            &state,
            None,
            SettlePnlMode::MustSettle,
        )?
    };

    if state.builder_codes_enabled() {
        if let Some(ref mut escrow) = builder_escrow {
            escrow.revoke_completed_orders(user)?;
            // Only sweep the market's pnl pool when settlement actually
            // happened; a soft-skipped settle must not move builder/referrer
            // fees out of a market that never settled.
            if settled {
                if let Some(ref builder_map) = maybe_rev_share_map {
                    // Oracle price for this market, validity-gated in-slot by the
                    // settle_pnl/settle_expired_position above; used to reserve
                    // max(net_user_pnl, 0) so the sweep can't pay revenue share out
                    // of tokens backing a user's positive PnL.
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
                        clock.unix_timestamp,
                        oracle_price,
                        state.builder_codes_enabled(),
                        state.funding_paused()?,
                    )?;
                } else {
                    msg!("Builder Users not provided, but RevenueEscrow was provided");
                }
            }
        }
    }

    if let Ok(position_index) = get_position_index(&user.perp_positions, market_index) {
        if user.perp_positions[position_index].can_transfer_isolated_position_deposit() {
            transfer_isolated_perp_position_deposit(
                user,
                None,
                &mut maps,
                clock.slot,
                clock.unix_timestamp,
                QUOTE_SPOT_MARKET_INDEX,
                market_index,
                i64::MIN,
                state.funding_paused()?,
            )?;
        }
    }

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

    let (mut builder_escrow, maybe_rev_share_map) = if state.builder_codes_enabled() {
        (
            get_revenue_share_escrow_account(&mut remaining_accounts, &user.authority)?,
            load_revenue_share_map(&mut remaining_accounts).ok(),
        )
    } else {
        (None, None)
    };

    let meets_margin_requirement =
        meets_settle_pnl_maintenance_margin_requirement(user, &mut maps)?;

    for market_index in market_indexes.iter() {
        let market_in_settlement =
            maps.perp_market_map.get_ref(market_index)?.status == MarketStatus::Settlement;

        // Whether settlement actually happened for this market. Under
        // `TrySettle`, `settle_pnl` soft-skips a paused or degraded-oracle
        // market into `Ok(false)`. `settle_expired_position` returns `Ok(false)`
        // for a user with no position, which is a no-op that runs before the
        // market's SettlePnl pause checks. The revenue-share sweep below must be
        // tied to real settlement. Otherwise it moves builder/referrer fees out
        // of a market that never settled.
        let settled = if market_in_settlement {
            amm_not_paused(&ctx.accounts.state)?;

            let settled = controller::pnl::settle_expired_position(
                *market_index,
                user,
                &user_key,
                &mut maps,
                &clock,
                &state,
            )?;

            user.update_last_active_slot(clock.slot);
            settled
        } else {
            // See `handle_settle_pnl` for the no-refresh rationale.

            controller::pnl::settle_pnl(
                *market_index,
                user,
                ctx.accounts.authority.key,
                &user_key,
                &mut maps,
                &clock,
                &state,
                Some(meets_margin_requirement),
                mode,
            )?
        };

        if state.builder_codes_enabled() {
            if let Some(ref mut escrow) = builder_escrow {
                escrow.revoke_completed_orders(user)?;
                // Only sweep the market's pnl pool when settlement actually
                // happened; a soft-skipped settle must not move
                // builder/referrer fees out of a market that never settled.
                if settled {
                    if let Some(ref builder_map) = maybe_rev_share_map {
                        // Oracle price for this market, validity-gated in-slot by the
                        // settle above; used to reserve max(net_user_pnl, 0) so the
                        // sweep can't pay revenue share out of tokens backing a user's
                        // positive PnL.
                        let oracle_price = {
                            let perp_market = maps.perp_market_map.get_ref(market_index)?;
                            maps.oracle_map
                                .get_price_data(&perp_market.oracle_id())?
                                .price
                        };
                        let _ =
                            controller::revenue_share::sweep_completed_revenue_share_for_market(
                                *market_index,
                                escrow,
                                &maps.perp_market_map,
                                &maps.spot_market_map,
                                builder_map,
                                clock.unix_timestamp,
                                oracle_price,
                                state.builder_codes_enabled(),
                                state.funding_paused()?,
                            )?;
                    } else {
                        msg!("Builder Users not provided, but RevenueEscrow was provided");
                    }
                }
            }
        }

        if let Ok(position_index) = get_position_index(&user.perp_positions, *market_index) {
            if user.perp_positions[position_index].can_transfer_isolated_position_deposit() {
                transfer_isolated_perp_position_deposit(
                    user,
                    None,
                    &mut maps,
                    clock.slot,
                    clock.unix_timestamp,
                    QUOTE_SPOT_MARKET_INDEX,
                    *market_index,
                    i64::MIN,
                    state.funding_paused()?,
                )?;
            }
        }
    }

    let spot_market = maps.spot_market_map.get_quote_spot_market()?;
    validate_spot_market_vault_amount(&spot_market, ctx.accounts.spot_market_vault.amount)?;

    Ok(())
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

    // #82: a position-acquiring liquidation both takes on the liquidatee's risk
    // and earns a liquidation fee — exactly the risk-taking the authority-wide
    // equity breaker freezes. Bar a tripped authority from liquidating out of a
    // healthy sibling subaccount. (PnL-settlement liquidations stay allowed;
    // they are protocol-protective and acquire no new risk.)
    validate!(
        !liquidator_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "liquidator authority equity breaker is tripped"
    )?;

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
/// flat payment still stands. That includes an unusable SOL price — this
/// converts quote to lamports, so it is a value transfer driven by an oracle
/// and takes the same validity gate as any other.
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
    let Ok(sol_market) = spot_market_map.get_ref(&state.sol_spot_market_index) else {
        return Ok(0);
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
        return Ok(0);
    }
    let sol_price = oracle_data.price;
    drop(sol_market);

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

    let filled_quote = controller::liquidation::liquidate_perp_with_fill(
        market_index,
        &ctx.accounts.user,
        &user_key,
        &ctx.accounts.user_stats,
        &ctx.accounts.liquidator,
        &liquidator_key,
        &ctx.accounts.liquidator_stats,
        &makers_and_referrer,
        &makers_and_referrer_stats,
        &mut maps,
        &clock,
        &state,
    )?;

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
    let program_keeper_mode =
        filled_quote > 0 && ctx.accounts.liquidator.load()?.authority == state.signer;
    if program_keeper_mode {
        let reservoir = ctx.accounts.crank_conditions.as_ref().ok_or_else(
            || -> anchor_lang::error::Error {
                msg!("program-keeper liquidation requires the market's conditions account");
                ErrorCode::DefaultError.into()
            },
        )?;
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
                &state,
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
    }

    Ok(())
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

    // #82: a position-acquiring liquidation both takes on the liquidatee's risk
    // and earns a liquidation fee — exactly the risk-taking the authority-wide
    // equity breaker freezes. Bar a tripped authority from liquidating out of a
    // healthy sibling subaccount. (PnL-settlement liquidations stay allowed;
    // they are protocol-protective and acquire no new risk.)
    validate!(
        !liquidator_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "liquidator authority equity breaker is tripped"
    )?;

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
pub fn handle_liquidate_spot_with_swap_begin<'c: 'info, 'info>(
    ctx: Context<'info, LiquidateSpotWithSwap<'info>>,
    asset_market_index: u16,
    liability_market_index: u16,
    swap_amount: u64,
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
    let liquidator_stats = load!(ctx.accounts.liquidator_stats)?;

    // A swap-backed liquidation earns the liquidation fee like the other
    // liquidator routes, the same value capture the authority-wide equity
    // breaker freezes, even though the tokens flow through the authority's
    // wallet accounts rather than the liquidator subaccount. Bar a tripped
    // authority here too, before any flash-loan state opens; `end` runs in
    // the same transaction, so checking `begin` covers the pair.
    //
    // Only the breaker, deliberately: the four direct routes additionally
    // require the liquidator subaccount to clear its own buffered floor
    // (`validate_clears_buffered_floor`), because the liquidation moves the
    // liquidatee's position onto that subaccount. This route moves nothing
    // onto it (both `update_spot_balances_and_cumulative_deposits` calls in
    // `liquidate_spot_with_swap_end` target the liquidatee, and the fees go
    // to the revenue and protocol pools), so there is no exposure for a
    // per-subaccount floor to gate.
    validate!(
        !liquidator_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "liquidator authority equity breaker is tripped"
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![asset_market_index, liability_market_index]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let _token_interface = get_token_interface(remaining_accounts_iter)?;
    let mint = get_token_mint(remaining_accounts_iter)?;

    let mut asset_spot_market = maps.spot_market_map.get_ref_mut(&asset_market_index)?;
    validate!(
        asset_spot_market.flash_loan_initial_token_amount == 0
            && asset_spot_market.flash_loan_amount == 0,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "begin_swap ended in invalid state"
    )?;

    // Accrue interest and advance the deposit/borrow/utilization TWAPs, but pass
    // `None` so this liquidation does NOT advance the markets' *oracle* TWAPs.
    // `liquidate_spot_with_swap_begin` gates itself on
    // `is_oracle_too_divergent_with_twap_5min` against the liability market's
    // `last_oracle_price_twap_5min`; refreshing it first — in this same
    // instruction — pulls it toward the live oracle price and lets a liquidation
    // the band check would reject proceed and transfer collateral
    // (OtterSec #111).
    //
    // The direct `liquidate_spot` lane already runs that same check with no
    // pre-refresh, so this only brings the swap-backed lane in line with it; a
    // band-blocked swap liquidation can still be routed through the direct path.
    // The refresh is moved, not dropped: `liquidate_spot_with_swap_end` advances
    // both markets' oracle TWAPs once every check in the lane is done.
    controller::spot_balance::update_spot_market_cumulative_interest(
        &mut asset_spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    let mut liability_spot_market = maps.spot_market_map.get_ref_mut(&liability_market_index)?;

    validate!(
        liability_spot_market.flash_loan_initial_token_amount == 0
            && liability_spot_market.flash_loan_amount == 0,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "begin_swap ended in invalid state"
    )?;

    // `None` for the same reason as the asset market above (OtterSec #111) — this
    // is the market whose 5-minute TWAP the divergence check actually reads.
    controller::spot_balance::update_spot_market_cumulative_interest(
        &mut liability_spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    // The swap sends asset-vault tokens out and pulls liability tokens in — the
    // same egress/ingress the direct spot withdraw/deposit paths gate. `liq_not_paused`
    // alone doesn't cover them, so mirror `end_swap`: reject when the global
    // Deposit/Withdraw status is paused, when the asset market's Withdraw is
    // paused, or when the liability market's Deposit is paused. Gating the begin
    // ix is sufficient — a matching end ix is required in the same atomic tx.
    validate!(
        !(state.deposit_paused()? || state.withdraw_paused()?),
        ErrorCode::ExchangePaused
    )?;

    validate!(
        !asset_spot_market.is_operation_paused(SpotOperation::Withdraw),
        ErrorCode::MarketWithdrawPaused,
        "asset spot market {} withdraws paused",
        asset_market_index
    )?;

    validate!(
        !liability_spot_market.is_operation_paused(SpotOperation::Deposit),
        ErrorCode::MarketActionPaused,
        "liability spot market {} deposits paused",
        liability_market_index
    )?;

    drop(liability_spot_market);
    drop(asset_spot_market);

    validate!(
        asset_market_index != liability_market_index,
        ErrorCode::InvalidSwap,
        "asset and liability market the same"
    )?;

    validate!(
        swap_amount != 0,
        ErrorCode::InvalidSwap,
        "swap_amount cannot be zero"
    )?;

    liquidate_spot_with_swap_begin(
        asset_market_index,
        liability_market_index,
        swap_amount,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        clock.slot,
        &state,
    )?;

    let mut asset_spot_market = maps.spot_market_map.get_ref_mut(&asset_market_index)?;
    let mut liability_spot_market = maps.spot_market_map.get_ref_mut(&liability_market_index)?;

    let asset_vault = &ctx.accounts.asset_spot_market_vault;
    let asset_token_account = &ctx.accounts.asset_token_account;

    asset_spot_market.flash_loan_amount = swap_amount;
    asset_spot_market.flash_loan_initial_token_amount = asset_token_account.amount;

    let liability_token_account = &ctx.accounts.liability_token_account;

    liability_spot_market.flash_loan_initial_token_amount = liability_token_account.amount;

    let asset_spot_has_transfer_hook = asset_spot_market.has_transfer_hook();
    let liability_spot_has_transfer_hook = liability_spot_market.has_transfer_hook();

    validate!(
        !(asset_spot_has_transfer_hook && liability_spot_has_transfer_hook),
        ErrorCode::InvalidSwap,
        "both asset and liability spot markets cannot both have transfer hooks"
    )?;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        asset_vault,
        &ctx.accounts.asset_token_account,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        swap_amount,
        &mint,
        if asset_spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    let ixs = ctx.accounts.instructions.as_ref();
    let current_index = instructions::load_current_index_checked(ixs)? as usize;

    let current_ix = instructions::load_instruction_at_checked(current_index, ixs)?;
    validate!(
        current_ix.program_id == *ctx.program_id,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "LiquidateSpotWithSwapBegin must be a top-level instruction (cant be cpi)"
    )?;

    let mut index = current_index + 1;
    let mut found_end = false;
    loop {
        let ix = match instructions::load_instruction_at_checked(index, ixs) {
            Ok(ix) => ix,
            Err(ProgramError::InvalidArgument) => break,
            Err(e) => return Err(e.into()),
        };

        // Check that the velocity program key is not used
        if ix.program_id == crate::id() {
            // must be the last ix -- this could possibly be relaxed
            validate!(
                !found_end,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the transaction must not contain a Velocity instruction after FlashLoanEnd"
            )?;
            found_end = true;

            // must be the SwapEnd instruction
            let discriminator = crate::instruction::LiquidateSpotWithSwapEnd::DISCRIMINATOR;
            validate!(
                &ix.data[0..8] == discriminator,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "last velocity ix must be end of swap"
            )?;

            validate!(
                ctx.accounts.authority.key() == ix.accounts[1].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the authority passed to SwapBegin and End must match"
            )?;

            validate!(
                ctx.accounts.liquidator.key() == ix.accounts[2].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the liquidator passed to SwapBegin and End must match"
            )?;

            validate!(
                ctx.accounts.user.key() == ix.accounts[3].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the user passed to SwapBegin and End must match"
            )?;

            validate!(
                ctx.accounts.liability_spot_market_vault.key() == ix.accounts[4].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the liability_spot_market_vault passed to SwapBegin and End must match"
            )?;

            validate!(
                ctx.accounts.asset_spot_market_vault.key() == ix.accounts[5].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the asset_spot_market_vault passed to SwapBegin and End must match"
            )?;

            validate!(
                ctx.accounts.liability_token_account.key() == ix.accounts[6].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the liability_token_account passed to SwapBegin and End must match"
            )?;

            validate!(
                ctx.accounts.asset_token_account.key() == ix.accounts[7].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the asset_token_account passed to SwapBegin and End must match"
            )?;

            validate!(
                ctx.accounts.liquidator_stats.key() == ix.accounts[11].pubkey,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the liquidator_stats passed to SwapBegin and End must match"
            )?;

            // `LiquidateSpotWithSwap` has 12 fixed accounts (indexes 0..=11);
            // remaining (swap) accounts start at index 12 and must match between
            // begin and end.
            validate!(
                ctx.remaining_accounts.len() == ix.accounts.len() - 12,
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "begin and end ix must have the same number of accounts"
            )?;

            for i in 12..ix.accounts.len() {
                validate!(
                    *ctx.remaining_accounts[i - 12].key == ix.accounts[i].pubkey,
                    ErrorCode::InvalidLiquidateSpotWithSwap,
                    "begin and end ix must have the same accounts. {}th account mismatch. begin: {}, end: {}",
                    i,
                    ctx.remaining_accounts[i - 12].key,
                    ix.accounts[i].pubkey
                )?;
            }
        } else if found_end {
            for meta in ix.accounts.iter() {
                validate!(
                    !meta.is_writable,
                    ErrorCode::InvalidLiquidateSpotWithSwap,
                    "instructions after swap end must not have writable accounts"
                )?;
            }
        } else {
            let whitelisted_programs = [
                serum_program::id(),
                AssociatedToken::id(),
                jupiter_mainnet_3::ID,
                jupiter_mainnet_4::ID,
                jupiter_mainnet_6::ID,
                dflow_mainnet_aggregator_4::ID,
                titan_mainnet_argos_v1::ID,
            ];
            validate!(
                whitelisted_programs.contains(&ix.program_id),
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "only allowed to pass in ixs to ATA, openbook, Jupiter v3/v4/v6, dflow, or titan programs"
            )?;

            for meta in ix.accounts.iter() {
                validate!(
                    meta.pubkey != crate::id(),
                    ErrorCode::InvalidLiquidateSpotWithSwap,
                    "instructions between begin and end must not be velocity instructions"
                )?;
            }
        }

        index += 1;
    }

    validate!(
        found_end,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "found no LiquidateSpotWithSwapEnd instruction in transaction"
    )?;

    Ok(())
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_spot_with_swap_end<'c: 'info, 'info>(
    ctx: Context<'info, LiquidateSpotWithSwap<'info>>,
    asset_market_index: u16,
    liability_market_index: u16,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let slot = clock.slot;
    let now = clock.unix_timestamp;

    let remaining_accounts = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts,
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![asset_market_index, liability_market_index]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;
    let liability_token_program = get_token_interface(remaining_accounts)?;

    let asset_mint = get_token_mint(remaining_accounts)?;
    let liability_mint = get_token_mint(remaining_accounts)?;

    let user_key = ctx.accounts.user.key();
    let mut user = load_mut!(&ctx.accounts.user)?;

    let liquidator_key = ctx.accounts.liquidator.key();

    let mut asset_spot_market = maps.spot_market_map.get_ref_mut(&asset_market_index)?;

    validate!(
        asset_spot_market.flash_loan_amount != 0,
        ErrorCode::InvalidSwap,
        "the asset_spot_market must have a flash loan amount set"
    )?;

    let mut liability_spot_market = maps.spot_market_map.get_ref_mut(&liability_market_index)?;

    let asset_vault = &mut ctx.accounts.asset_spot_market_vault;
    let asset_token_account = &mut ctx.accounts.asset_token_account;

    let mut amount_in = asset_spot_market.flash_loan_amount;
    if asset_token_account.amount > asset_spot_market.flash_loan_initial_token_amount {
        let residual = asset_token_account
            .amount
            .safe_sub(asset_spot_market.flash_loan_initial_token_amount)?;

        controller::token::receive(
            &ctx.accounts.token_program,
            asset_token_account,
            asset_vault,
            &ctx.accounts.authority,
            residual,
            &asset_mint,
            if asset_spot_market.has_transfer_hook() {
                Some(remaining_accounts)
            } else {
                None
            },
        )?;
        asset_token_account.reload()?;
        asset_vault.reload()?;

        amount_in = amount_in.safe_sub(residual)?;
    }

    asset_spot_market.flash_loan_initial_token_amount = 0;
    asset_spot_market.flash_loan_amount = 0;

    let liability_vault = &mut ctx.accounts.liability_spot_market_vault;
    let liability_token_account = &mut ctx.accounts.liability_token_account;

    let mut amount_out = 0_u64;
    if liability_token_account.amount > liability_spot_market.flash_loan_initial_token_amount {
        amount_out = liability_token_account
            .amount
            .safe_sub(liability_spot_market.flash_loan_initial_token_amount)?;

        if let Some(token_interface) = liability_token_program {
            controller::token::receive(
                &token_interface,
                liability_token_account,
                liability_vault,
                &ctx.accounts.authority,
                amount_out,
                &liability_mint,
                if liability_spot_market.has_transfer_hook() {
                    Some(remaining_accounts)
                } else {
                    None
                },
            )?;
        } else {
            controller::token::receive(
                &ctx.accounts.token_program,
                liability_token_account,
                liability_vault,
                &ctx.accounts.authority,
                amount_out,
                &liability_mint,
                if liability_spot_market.has_transfer_hook() {
                    Some(remaining_accounts)
                } else {
                    None
                },
            )?;
        }

        liability_vault.reload()?;
    }

    validate!(
        amount_out != 0,
        ErrorCode::InvalidSwap,
        "amount_out must be greater than 0"
    )?;

    liability_spot_market.flash_loan_initial_token_amount = 0;
    liability_spot_market.flash_loan_amount = 0;

    drop(liability_spot_market);
    drop(asset_spot_market);

    liquidate_spot_with_swap_end(
        asset_market_index,
        liability_market_index,
        &mut user,
        &user_key,
        &liquidator_key,
        &mut maps,
        now,
        slot,
        &state,
        amount_in.cast()?,
        amount_out.cast()?,
    )?;

    let liability_spot_market = maps.spot_market_map.get_ref_mut(&liability_market_index)?;

    validate!(
        liability_spot_market.flash_loan_initial_token_amount == 0
            && liability_spot_market.flash_loan_amount == 0,
        ErrorCode::InvalidSwap,
        "end_swap ended in invalid state"
    )?;

    math::spot_withdraw::validate_spot_market_vault_amount(
        &liability_spot_market,
        liability_vault.amount,
    )?;

    let mut asset_spot_market = maps.spot_market_map.get_ref_mut(&asset_market_index)?;

    validate!(
        asset_spot_market.flash_loan_initial_token_amount == 0
            && asset_spot_market.flash_loan_amount == 0,
        ErrorCode::InvalidSwap,
        "end_swap ended in invalid state"
    )?;

    math::spot_withdraw::validate_spot_market_vault_amount(&asset_spot_market, asset_vault.amount)?;

    // Advance the oracle TWAPs last, for the same reason as `end_swap`: the begin
    // instruction passes `None` so it cannot refresh the anchor its own
    // divergence check reads (OtterSec #111), and both this lane's checks are
    // done by here. The begin instruction left `last_oracle_price_twap_ts` alone,
    // so this update still weights the full elapsed interval.
    let asset_oracle_data = *maps
        .oracle_map
        .get_price_data(&asset_spot_market.oracle_id())?;
    controller::spot_balance::update_spot_market_twap_stats(
        &mut asset_spot_market,
        Some(&asset_oracle_data),
        now,
    )?;

    let mut liability_spot_market = maps.spot_market_map.get_ref_mut(&liability_market_index)?;
    let liability_oracle_data = *maps
        .oracle_map
        .get_price_data(&liability_spot_market.oracle_id())?;
    controller::spot_balance::update_spot_market_twap_stats(
        &mut liability_spot_market,
        Some(&liability_oracle_data),
        now,
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
    let liquidator_stats = load!(ctx.accounts.liquidator_stats)?;

    // #82: taking over the user's borrow (in exchange for positive pnl) both acquires balance-sheet risk and
    // earns a liquidation fee — the same risk-taking the authority-wide equity
    // breaker freezes, and the same shape as `liquidate_spot` (which is barred).
    // Bar a tripped authority here too. (`liquidate_perp_with_fill` stays
    // ungated: its liquidator routes the position to the book and never
    // acquires a balance.)
    validate!(
        !liquidator_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "liquidator authority equity breaker is tripped"
    )?;

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
    let liquidator_stats = load!(ctx.accounts.liquidator_stats)?;

    // #82: taking over the user's deposit (in exchange for negative pnl) both acquires balance-sheet risk and
    // earns a liquidation fee — the same risk-taking the authority-wide equity
    // breaker freezes, and the same shape as `liquidate_spot` (which is barred).
    // Bar a tripped authority here too. (`liquidate_perp_with_fill` stays
    // ungated: its liquidator routes the position to the book and never
    // acquires a balance.)
    validate!(
        !liquidator_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "liquidator authority equity breaker is tripped"
    )?;

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

#[access_control(
    solvency_repair_not_paused(&ctx.accounts.state)
)]
pub fn handle_resolve_perp_pnl_deficit<'c: 'info, 'info>(
    ctx: Context<'info, ResolvePerpPnlDeficit<'info>>,
    spot_market_index: u16,
    perp_market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    validate!(spot_market_index == 0, ErrorCode::InvalidSpotMarketAccount)?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(perp_market_index),
        &get_writable_spot_market_set(spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    // No `update_amm` here: this handler moves spot/IF balances and does
    // not read perp AMM peg or reserves. Refreshing the AMM was cargo-cult.

    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;
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

        // reload the spot market vault balance so it's up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        ctx.accounts.insurance_fund_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    let insurance_vault_amount = ctx.accounts.insurance_fund_vault.amount;
    let spot_market_vault_amount = ctx.accounts.spot_market_vault.amount;

    let pay_from_insurance = {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;
        let perp_market = &mut maps.perp_market_map.get_ref_mut(&perp_market_index)?;

        let oracle_price_data = *maps.oracle_map.get_price_data(&perp_market.oracle_id())?;

        if perp_market.amm.is_curve_update_enabled() {
            validate!(
                perp_market.market_stats.last_oracle_valid,
                ErrorCode::InvalidOracle,
                "Oracle Price detected as invalid"
            )?;

            validate!(
                perp_market.amm.is_fresh_at(maps.oracle_map.slot),
                ErrorCode::AMMNotUpdatedInSameSlot,
                "AMM must be updated in a prior instruction within same slot"
            )?;

            // The cached verdict only covers the sample the AMM update
            // validated; a later oracle write in the same slot replaces the
            // sample without touching `last_oracle_valid`.
            validate!(
                perp_market.is_validated_oracle_sample(&oracle_price_data),
                ErrorCode::InvalidOracle,
                "Oracle rewritten after same-slot AMM update; sample no longer matches the validated one"
            )?;
        }

        validate!(
            !perp_market.is_in_settlement(now),
            ErrorCode::MarketActionPaused,
            "Market is in settlement mode",
        )?;

        let oracle_price = oracle_price_data.price;
        controller::orders::validate_market_within_price_band(perp_market, &state, oracle_price)?;

        controller::insurance::resolve_perp_pnl_deficit(
            spot_market_vault_amount,
            insurance_vault_amount,
            spot_market,
            perp_market,
            clock.unix_timestamp,
            state.funding_paused()?,
        )?
    };

    if pay_from_insurance > 0 {
        validate!(
            pay_from_insurance < ctx.accounts.insurance_fund_vault.amount,
            ErrorCode::InsufficientCollateral,
            "Insurance Fund balance InsufficientCollateral for payment: !{} < {}",
            pay_from_insurance,
            ctx.accounts.insurance_fund_vault.amount
        )?;

        let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;
        controller::token::send_from_program_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.insurance_fund_vault,
            &ctx.accounts.spot_market_vault,
            &ctx.accounts.velocity_signer,
            state.signer_nonce,
            pay_from_insurance,
            &mint,
            if spot_market.has_transfer_hook() {
                Some(remaining_accounts_iter)
            } else {
                None
            },
        )?;

        validate!(
            ctx.accounts.insurance_fund_vault.amount > 0,
            ErrorCode::InvalidIFDetected,
            "insurance_fund_vault.amount must remain > 0"
        )?;

        controller::insurance::record_insurance_fund_outflow(
            spot_market,
            insurance_vault_amount,
            pay_from_insurance,
        );
    }

    // todo: validate amounts transfered and spot_market before and after are zero-sum

    Ok(())
}

#[access_control(
    solvency_repair_not_paused(&ctx.accounts.state)
)]
pub fn handle_resolve_perp_bankruptcy<'c: 'info, 'info>(
    ctx: Context<'info, ResolveBankruptcy<'info>>,
    quote_spot_market_index: u16,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;

    validate!(
        quote_spot_market_index == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::InvalidSpotMarketAccount
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;
    let state = ctx.accounts.state.load()?;

    // OtterSec #145: the resolver forfeits unfundable claims to their own markets' insurance
    // tranches, so every market holding such a claim is written to, not just `market_index`.
    // Declaring them here makes a caller that passes one read-only fail at load with
    // `MarketWrongMutability` instead of deep inside the resolver.
    let mut writable_perp_markets = vec![market_index];
    writable_perp_markets.extend(perp_markets_with_forfeitable_claims(user));

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set_from_vec(&writable_perp_markets),
        &get_writable_spot_market_set(quote_spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&quote_spot_market_index)?;
        let mut transfer_hook_remaining_accounts_iter = remaining_accounts_iter.clone();
        let remaining_accounts = if spot_market.has_transfer_hook() {
            Some(&mut transfer_hook_remaining_accounts_iter)
        } else {
            None
        };
        controller::insurance::attempt_settle_revenue_to_insurance_fund(
            &ctx.accounts.spot_market_vault,
            &ctx.accounts.insurance_fund_vault,
            spot_market,
            now,
            &ctx.accounts.token_program,
            &ctx.accounts.velocity_signer,
            &state,
            &mint,
            remaining_accounts,
        )?;

        // reload the spot market vault balance so it's up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        ctx.accounts.insurance_fund_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    let insurance_vault_amount = ctx.accounts.insurance_fund_vault.amount;

    let pay_from_insurance = controller::liquidation::resolve_perp_bankruptcy(
        market_index,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        insurance_vault_amount,
        state.funding_paused()?,
    )?;

    if pay_from_insurance > 0 {
        validate!(
            pay_from_insurance < ctx.accounts.insurance_fund_vault.amount,
            ErrorCode::InsufficientCollateral,
            "Insurance Fund balance InsufficientCollateral for payment: !{} < {}",
            pay_from_insurance,
            ctx.accounts.insurance_fund_vault.amount
        )?;

        let spot_market = &maps.spot_market_map.get_ref(&quote_spot_market_index)?;
        let mut transfer_hook_remaining_accounts_iter = remaining_accounts_iter.clone();
        let remaining_accounts = if spot_market.has_transfer_hook() {
            Some(&mut transfer_hook_remaining_accounts_iter)
        } else {
            None
        };

        controller::token::send_from_program_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.insurance_fund_vault,
            &ctx.accounts.spot_market_vault,
            &ctx.accounts.velocity_signer,
            state.signer_nonce,
            pay_from_insurance,
            &mint,
            remaining_accounts,
        )?;

        validate!(
            ctx.accounts.insurance_fund_vault.amount > 0,
            ErrorCode::InvalidIFDetected,
            "insurance_fund_vault.amount must remain > 0"
        )?;
    }

    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&quote_spot_market_index)?;
        controller::insurance::record_insurance_fund_outflow(
            spot_market,
            insurance_vault_amount,
            pay_from_insurance,
        );
        // reload the spot market vault balance so it's up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    Ok(())
}

#[access_control(
    solvency_repair_not_paused(&ctx.accounts.state)
)]
pub fn handle_resolve_spot_bankruptcy<'c: 'info, 'info>(
    ctx: Context<'info, ResolveBankruptcy<'info>>,
    market_index: u16,
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

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        // OtterSec #145: this resolver also recovers and winds up the estate's perp claims, so the
        // markets holding them are written to even though the bankruptcy being resolved is a spot
        // borrow.
        &get_writable_perp_market_set_from_vec(&perp_markets_with_forfeitable_claims(user)),
        // The quote market is written too: a recovered claim lands in the estate's quote deposit,
        // and the borrow being resolved may be in another market entirely. It was already a required
        // account here, because the claim passes read it, but only as read-only.
        &get_writable_spot_market_set_from_many(vec![market_index, QUOTE_SPOT_MARKET_INDEX]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
        let mut transfer_hook_remaining_accounts_iter = remaining_accounts_iter.clone();
        let remaining_accounts = if spot_market.has_transfer_hook() {
            Some(&mut transfer_hook_remaining_accounts_iter)
        } else {
            None
        };
        controller::insurance::attempt_settle_revenue_to_insurance_fund(
            &ctx.accounts.spot_market_vault,
            &ctx.accounts.insurance_fund_vault,
            spot_market,
            now,
            &ctx.accounts.token_program,
            &ctx.accounts.velocity_signer,
            &state,
            &mint,
            remaining_accounts,
        )?;

        // reload the spot market vault balance so it's up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        ctx.accounts.insurance_fund_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    let insurance_vault_amount = ctx.accounts.insurance_fund_vault.amount;

    let pay_from_insurance = controller::liquidation::resolve_spot_bankruptcy(
        market_index,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        insurance_vault_amount,
        state.funding_paused()?,
    )?;

    if pay_from_insurance > 0 {
        let spot_market = &maps.spot_market_map.get_ref(&market_index)?;
        let mut transfer_hook_remaining_accounts_iter = remaining_accounts_iter.clone();
        let remaining_accounts = if spot_market.has_transfer_hook() {
            Some(&mut transfer_hook_remaining_accounts_iter)
        } else {
            None
        };
        controller::token::send_from_program_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.insurance_fund_vault,
            &ctx.accounts.spot_market_vault,
            &ctx.accounts.velocity_signer,
            ctx.accounts.state.load()?.signer_nonce,
            pay_from_insurance,
            &mint,
            remaining_accounts,
        )?;

        validate!(
            ctx.accounts.insurance_fund_vault.amount > 0,
            ErrorCode::InvalidIFDetected,
            "insurance_fund_vault.amount must remain > 0"
        )?;
    }

    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
        controller::insurance::record_insurance_fund_outflow(
            spot_market,
            insurance_vault_amount,
            pay_from_insurance,
        );
        // reload the spot market vault balance so it's up-to-date
        ctx.accounts.spot_market_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            ctx.accounts.spot_market_vault.amount,
        )?;
    }

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
    funding_not_paused(&ctx.accounts.state)
    valid_oracle_for_perp_market(&ctx.accounts.oracle, &ctx.accounts.perp_market)
)]
pub fn handle_update_funding_rate(
    ctx: Context<UpdateFundingRate>,
    perp_market_index: u16,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let clock_slot = clock.slot;
    let state = ctx.accounts.state.load()?;
    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock_slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let oracle_price_data = oracle_map.get_price_data(&perp_market.oracle_id())?;
    let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
        *oracle_price_data,
        clock_slot,
        &state.oracle_guard_rails.validity,
        state.slot_clock(),
    )?;
    // Refresh PerpMarket-level oracle stats. AMM refresh happens inside
    // `update_funding_rate` via the AmmQuoter's setup phase — not here.
    //
    // Deliberately the TWAP-free half. `update_funding_rate`'s gate
    // (`oracle::block_operation` -> `get_oracle_status`) reads
    // `last_oracle_price_twap` for the too-volatile check and
    // `last_oracle_price_twap_5min` for the mark-divergence check. Advancing
    // either one here would pull it toward the live price and let a too-volatile
    // or too-divergent oracle clear its own gate inside this same instruction,
    // then go on to mutate cumulative funding (OtterSec #109).
    //
    // Nothing is lost by skipping it: on the path where funding actually
    // updates, `update_funding_rate` advances the TWAPs itself, and on every
    // path where it does not this handler returns `FundingWasNotUpdated`, which
    // reverts the whole instruction. The TWAPs also keep advancing independently
    // via `update_amms`, perp fills, and `update_perp_bid_ask_twap`, so a market
    // whose oracle is genuinely too volatile still recovers — relaxing its own
    // gate is not this crank's job.
    let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity(
        perp_market,
        &mm_oracle_price_data,
        &state,
        clock_slot,
    )?;
    perp_market.refresh_amm_quote_state(
        &mm_oracle_price_data,
        validity,
        clock_slot,
        state.slot_clock(),
    )?;

    validate!(
        matches!(
            perp_market.status,
            MarketStatus::Active | MarketStatus::ReduceOnly
        ),
        ErrorCode::MarketActionPaused,
        "Market funding is paused",
    )?;

    let funding_paused =
        state.funding_paused()? || perp_market.is_operation_paused(PerpOperation::UpdateFunding);

    let is_updated = controller::funding::update_funding_rate(
        perp_market_index,
        perp_market,
        &mut oracle_map,
        now,
        clock_slot,
        &state.oracle_guard_rails,
        funding_paused,
        None,
    )?;

    if !is_updated {
        let time_until_next_update = crate::math::helpers::on_the_hour_update(
            now,
            perp_market.last_funding_rate_ts,
            perp_market.market_stats.funding_period,
        )?;
        msg!(
            "time_until_next_update = {:?} seconds",
            time_until_next_update
        );
        return Err(ErrorCode::FundingWasNotUpdated.into());
    }

    Ok(())
}

#[access_control(
    valid_oracle_for_perp_market(&ctx.accounts.oracle, &ctx.accounts.perp_market)
)]
pub fn handle_update_prelaunch_oracle(ctx: Context<UpdatePrelaunchOracle>) -> Result<()> {
    let clock = Clock::get()?;
    let clock_slot = clock.slot;
    let oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock_slot,
        ctx.accounts.state.load()?.slot_clock(),
        None,
    )?;

    let perp_market = &load!(ctx.accounts.perp_market)?;

    validate!(
        perp_market.oracle_source == OracleSource::Prelaunch,
        ErrorCode::DefaultError,
        "wrong oracle source"
    )?;

    update_prelaunch_oracle(perp_market, &oracle_map, clock_slot)?;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
    funding_not_paused(&ctx.accounts.state)
    valid_oracle_for_perp_market(&ctx.accounts.oracle, &ctx.accounts.perp_market)
)]
pub fn handle_update_perp_bid_ask_twap<'c: 'info, 'info>(
    ctx: Context<'info, UpdatePerpBidAskTwap<'info>>,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    // Stop this crank while the market's funding is paused. The `funding_not_paused`
    // access control already blocks the exchange-wide pause.
    //
    // The crank estimates the book from `User` accounts that the caller supplies. The
    // estimate moves the bid, ask and mark TWAPs.
    // `OrderParams::get_perp_baseline_start_price_offset` reads those TWAPs to set the
    // auction band for a different user's triggered stop-loss order (OtterSec #146).
    // A paused market is one the administrator does not trust, so the caller-supplied
    // input stops here. Perp fills still write the same TWAPs, because a fill is a
    // trade with capital at risk.
    if perp_market.is_operation_paused(PerpOperation::UpdateFunding) {
        return Ok(());
    }

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let state = ctx.accounts.state.load()?;
    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let keeper_stats = load!(ctx.accounts.keeper_stats)?;
    validate!(
        keeper_stats.can_update_bid_ask_twap(),
        ErrorCode::CantUpdatePerpBidAskTwap,
        "Keeper stats can_update_bid_ask_twap is false"
    )?;

    let min_if_stake = 1000 * QUOTE_PRECISION_U64;
    validate!(
        keeper_stats.if_staked_quote_asset_amount >= min_if_stake,
        ErrorCode::CantUpdatePerpBidAskTwap,
        "Keeper doesnt have min if stake. stake = {} min if stake = {}",
        keeper_stats.if_staked_quote_asset_amount,
        min_if_stake
    )?;

    let oracle_price_data = oracle_map.get_price_data(&perp_market.oracle_id())?;
    let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
        *oracle_price_data,
        slot,
        &state.oracle_guard_rails.validity,
        state.slot_clock(),
    )?;
    // PerpMarket-level oracle stats only — this ix walks DLOB makers to
    // estimate bid/ask TWAP and does not read AMM peg or reserves. The
    // AMM snap_to_oracle that used to fire here was cargo-cult and is
    // dropped; oracle TWAP / reference-price-offset bookkeeping still
    // happens via refresh_perp_market_stats_from_oracle.
    let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity(
        perp_market,
        &mm_oracle_price_data,
        &state,
        slot,
    )?;
    perp_market.update_oracle_derived_stats(
        &mm_oracle_price_data,
        validity,
        now,
        slot,
        state.slot_clock(),
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let makers = load_user_map(remaining_accounts_iter, false)?;

    let depth = perp_market.get_market_depth_for_funding_rate()?;

    let (bids, asks) = find_bids_and_asks_from_users(
        perp_market,
        oracle_price_data,
        &makers,
        slot,
        now,
        BID_ASK_TWAP_MIN_QUOTE_REST,
        state.slot_clock(),
    )?;
    let (bids, asks) = filter_bids_asks_by_oracle_divergence(
        bids,
        asks,
        oracle_price_data.price,
        BID_ASK_TWAP_MAX_ORACLE_DIVERGENCE_PERCENT,
    )?;
    let estimated_bid = estimate_price_from_side(&bids, depth)?;
    let estimated_ask = estimate_price_from_side(&asks, depth)?;

    msg!(
        "estimated_bid = {:?} estimated_ask = {:?}",
        estimated_bid,
        estimated_ask
    );

    let before_bid_price_twap = perp_market.market_stats.last_bid_price_twap;
    let before_ask_price_twap = perp_market.market_stats.last_ask_price_twap;
    let before_mark_twap_ts = perp_market.market_stats.last_mark_price_twap_ts;

    let sanitize_clamp_denominator = perp_market.get_sanitize_clamp_denominator()?;
    {
        let reserve_price = perp_market.amm.reserve_price()?;
        let crate::state::perp_market::PerpMarket {
            amm, market_stats, ..
        } = &mut **perp_market;
        // Refresh the AMM's cached spread state against this slot's oracle,
        // then fold it (plus DLOB liquidity) into the mark TWAP.
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            &mm_oracle_price_data,
            reserve_price,
            slot,
            state.slot_clock(),
        )?;
        market_stats.update_mark_twap_crank(
            amm,
            now,
            oracle_price_data,
            estimated_bid,
            estimated_ask,
            sanitize_clamp_denominator,
        )?;
    }

    msg!(
        "after amm bid twap = {} -> {}
        ask twap = {} -> {}
        ts = {} -> {}",
        before_bid_price_twap,
        perp_market.market_stats.last_bid_price_twap,
        before_ask_price_twap,
        perp_market.market_stats.last_ask_price_twap,
        before_mark_twap_ts,
        perp_market.market_stats.last_mark_price_twap_ts
    );

    if perp_market.market_stats.last_bid_price_twap == before_bid_price_twap
        || perp_market.market_stats.last_ask_price_twap == before_ask_price_twap
    {
        validate!(
            perp_market
                .market_stats
                .last_mark_price_twap_ts
                .safe_sub(before_mark_twap_ts)?
                >= 60
                || estimated_bid.unwrap_or(0) == before_bid_price_twap
                || estimated_ask.unwrap_or(0) == before_ask_price_twap,
            ErrorCode::CantUpdatePerpBidAskTwap,
            "bid or ask twap unchanged from small ts delta update",
        )?;
    }

    // Funding is intentionally decoupled from this crank: refreshing the mark
    // TWAP from caller-supplied DLOB depth and applying funding in the same
    // instruction let a caller stamp `last_mark_price_twap_ts = now` and then
    // have funding read that just-written TWAP back at zero elapsed time.
    // Funding runs via its own `update_funding_rate` crank (and on fills).

    Ok(())
}

#[access_control(
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_settle_revenue_to_insurance_fund<'c: 'info, 'info>(
    ctx: Context<'info, SettleRevenueToInsuranceFund<'info>>,
    spot_market_index: u16,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mint = get_token_mint(remaining_accounts_iter)?;

    validate!(
        spot_market_index == spot_market.market_index,
        ErrorCode::InvalidSpotMarketAccount,
        "invalid spot_market passed"
    )?;

    // Moving revenue out of the spot vault into the IF vault is an egress from
    // the market: gate it on the market-scoped Withdraw pause, not just the
    // global `withdraw_not_paused` access control.
    validate!(
        !spot_market.is_operation_paused(SpotOperation::Withdraw),
        ErrorCode::MarketWithdrawPaused,
        "spot market {} withdraws paused",
        spot_market.market_index
    )?;

    validate!(
        spot_market.insurance_fund.revenue_settle_period > 0,
        ErrorCode::RevenueSettingsCannotSettleToIF,
        "invalid revenue_settle_period settings on spot market"
    )?;

    let spot_vault_amount = ctx.accounts.spot_market_vault.amount;
    let insurance_vault_amount = ctx.accounts.insurance_fund_vault.amount;

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let time_until_next_update = math::helpers::on_the_hour_update(
        now,
        spot_market.insurance_fund.last_revenue_settle_ts,
        spot_market.insurance_fund.revenue_settle_period,
    )?;

    validate!(
        time_until_next_update == 0,
        ErrorCode::RevenueSettingsCannotSettleToIF,
        "Must wait {} seconds until next available settlement time",
        time_until_next_update
    )?;

    // uses proportion of revenue pool allocated to insurance fund
    let token_amount = controller::insurance::settle_revenue_to_insurance_fund(
        spot_vault_amount,
        insurance_vault_amount,
        spot_market,
        now,
        true,
        state.funding_paused()?,
    )?;

    spot_market.insurance_fund.last_revenue_settle_ts = now;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.insurance_fund_vault,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        token_amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    // reload the spot market vault balance so it's up-to-date
    ctx.accounts.spot_market_vault.reload()?;
    math::spot_withdraw::validate_spot_market_vault_amount(
        spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    Ok(())
}

/// Permissionless streaming sweep: materialize a perp market's accrued
/// pending fee carveouts out of the pnl pool — `pending_protocol_fee` to the
/// market's `protocol_fee_pool` (buffer-exempt, runs first), then
/// `pending_if_fee` to the quote spot market's `revenue_pool` and
/// `pending_amm_provision` tokenized into `amm.fee_pool` (both leave
/// `fee_pool_buffer_target` behind). Every drain reserves
/// `max(net_user_pnl, 0)` so user claims stay backed. The same sweep runs
/// inline on every pnl settle (`update_pool_balances`); this instruction lets
/// keepers run it on demand without settling anyone's pnl.
#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_sweep_perp_market_fees(
    ctx: Context<SweepPerpMarketFees>,
    perp_market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let state = ctx.accounts.state.load()?;

    // account identities (market index, quote spot market, oracle) are
    // enforced by the SweepPerpMarketFees constraints
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // The reserve keeps the live user claim in the pnl pool. `get_pnl_pool_drain_reserve_price`
    // picks the price and validates the oracle. The revenue-share sweep uses the same function, so
    // both drains value the same claim at the same price.
    let reserve_price = controller::perp_pools::get_pnl_pool_drain_reserve_price(
        perp_market,
        &state,
        &mut oracle_map,
    )?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    let net_user_pnl = calculate_net_user_pnl(
        &perp_market.amm,
        reserve_price,
        perp_market.quote_asset_amount,
        perp_market.net_unsettled_funding_pnl,
    )?;

    let (if_swept, protocol_swept, amm_provision_tokenized) =
        controller::perp_pools::sweep_market_fees(
            perp_market,
            spot_market,
            net_user_pnl,
            now,
            false,
        )?;

    msg!(
        "swept perp market {} fees: if={} protocol={} amm_provision_tokenized={}",
        perp_market_index,
        if_swept,
        protocol_swept,
        amm_provision_tokenized
    );

    Ok(())
}

/// Writes off one revenue-share row that the program cannot pay. Anyone can call this.
///
/// `settle_expired_market_pools_to_revenue_pool` refuses to delist a market that still owes
/// revenue share. That value belongs to the beneficiaries, not to the revenue pool. A row that
/// nobody can collect would block the delist forever. This instruction ends such a row.
///
/// The program requires proof that it cannot pay the row. One of these must be true:
///
///   * The beneficiary has no payout `User` account. The handler derives the address of that
///     account from the row, so the caller cannot substitute or omit it.
///   * The market is closed and the pool is smaller than the row.
///   * The row names no beneficiary that the program can reach.
///
/// This moves no tokens. Only the row and the counter change.
#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_forfeit_revenue_share_order(
    ctx: Context<ForfeitRevenueShareOrder>,
    args: ForfeitRevenueShareOrderArgs,
) -> Result<()> {
    let ForfeitRevenueShareOrderArgs {
        market_index,
        order_index,
    } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let escrow_authority = ctx.accounts.escrow_authority.key();

    // The proof below compares the pool against the amount that the row owes. `get_token_amount`
    // scales the pool by the cumulative deposit interest of the market. That interest only grows.
    // An old value therefore makes the pool look too small, and the program could write off a row
    // that it can pay. Accrue the interest first. The sweep does the same.
    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        clock.unix_timestamp,
        state.funding_paused()?,
    )?;

    let escrow_account_info = ctx.accounts.revenue_share_escrow.to_account_info();
    let mut escrow: RevenueShareEscrowZeroCopyMut =
        crate::state::revenue_share::RevenueShareEscrowLoader::load_zc_mut(&escrow_account_info)?;
    validate!(
        escrow.fixed.authority == escrow_authority,
        ErrorCode::RevenueShareEscrowAuthorityMismatch,
        "escrow header authority {} does not match the seed authority {}",
        escrow.fixed.authority,
        escrow_authority
    )?;

    // The proof that the program cannot pay the row. It reads the beneficiary from the escrow and
    // derives the payout address itself, so the caller cannot substitute or omit that account.
    let reason = controller::revenue_share::resolve_revenue_share_forfeit_reason(
        perp_market,
        spot_market,
        &mut escrow,
        market_index,
        order_index,
        &ctx.accounts.beneficiary_user.key(),
        ctx.accounts.beneficiary_user.data_is_empty()
            && *ctx.accounts.beneficiary_user.owner == anchor_lang::system_program::ID,
        clock.unix_timestamp,
        state.escrow_period_before_transfer()?,
    )?;

    controller::revenue_share::forfeit_revenue_share_order(
        perp_market,
        &mut escrow,
        order_index,
        reason,
    )?;

    Ok(())
}

/// Pays the accrued builder and referrer fees in one escrow for one perp market. Anyone can call
/// this.
///
/// A pnl settle runs the same sweep, but only after it settles pnl. A row is therefore payable
/// only while the escrow owner still has pnl to settle on the market. After the owner closes the
/// position and stops trading, nobody can collect the fee. `PerpMarket.pending_revenue_share` then
/// holds pnl-pool value against that claim forever, and the fee sweeps cannot use it. This
/// instruction lets a beneficiary or a keeper collect without the owner.
///
/// `remaining_accounts` holds three groups, in this order:
///   1. The oracle, spot market and perp market accounts that `load_maps` reads.
///   2. `num_owner_sub_accounts` read-only `User` accounts of the escrow authority. The handler
///      uses them to complete rows whose orders are closed. A builder row needs `Completed`.
///   3. The `User` and `RevenueShare` accounts of the beneficiaries, writable. These go to
///      `load_revenue_share_map`.
#[access_control(
    exchange_not_paused(&ctx.accounts.state)
    settle_pnl_not_paused(&ctx.accounts.state)
)]
pub fn handle_settle_revenue_share<'c: 'info, 'info>(
    ctx: Context<'info, SettleRevenueShare<'info>>,
    args: SettleRevenueShareArgs,
) -> Result<()> {
    let SettleRevenueShareArgs {
        market_index,
        num_owner_sub_accounts,
    } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    validate!(
        state.builder_codes_enabled(),
        ErrorCode::DefaultError,
        "builder codes feature is disabled"
    )?;

    let escrow_authority = ctx.accounts.escrow_authority.key();

    // Bind the account info to a local first. `load_zc_mut` borrows from it, and a temporary
    // value from `to_account_info()` does not live long enough.
    let escrow_account_info = ctx.accounts.revenue_share_escrow.to_account_info();
    // Fully qualified: `ZeroCopyLoader` also defines `load_zc_mut` for account infos.
    let mut escrow: RevenueShareEscrowZeroCopyMut =
        crate::state::revenue_share::RevenueShareEscrowLoader::load_zc_mut(&escrow_account_info)?;
    validate!(
        escrow.fixed.authority == escrow_authority,
        ErrorCode::RevenueShareEscrowAuthorityMismatch,
        "escrow header authority {} does not match the seed authority {}",
        escrow.fixed.authority,
        escrow_authority
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

    // The sweep pays from the market that `get_quote_spot_market_mut` returns. A perp market with
    // a different quote market would find no balance.
    validate!(
        maps.perp_market_map
            .get_ref(&market_index)?
            .quote_spot_market_index
            == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::DefaultError,
        "perp market {} is not quoted in the quote spot market",
        market_index
    )?;

    // Complete the rows whose orders are closed. This is the only way that a builder row gets the
    // `Completed` flag that the sweep needs. Payment clears a row. If the program paid an `Open`
    // row, it would remove the `order_id` and `sub_account_id` that `find_builder_order_index`
    // reads. A third party could then stop the fees of a builder on a live order.
    let owner_sub_accounts = load_escrow_owner_sub_accounts(
        &mut remaining_accounts,
        &escrow_authority,
        num_owner_sub_accounts,
    )?;
    for loader in owner_sub_accounts.iter() {
        let user = load!(loader)?;
        escrow.revoke_completed_orders(&user)?;
    }
    drop(owner_sub_accounts);

    // This uses `?`, not `.ok()`. The settle handlers process a batch and must continue. This
    // instruction has one job, so a bad beneficiary account must fail the transaction.
    let revenue_share_map = load_revenue_share_map(&mut remaining_accounts)?;

    // The price that sets the `max(net_user_pnl, 0)` reserve. The settle handlers get this check
    // from the `settle_pnl` that runs before their sweep. This instruction must do the check
    // itself. It uses the same checks as `handle_sweep_perp_market_fees`, which values the same
    // reserve.
    let reserve_price = {
        let perp_market = maps.perp_market_map.get_ref(&market_index)?;

        // A delist requires a zero liability and moves the pnl pool to the revenue pool. A
        // delisted market therefore owes nothing and holds nothing. Report this. A silent success
        // would look like a completed settle.
        validate!(
            perp_market.status != MarketStatus::Delisted,
            ErrorCode::MarketDelisted,
            "perp market {} is delisted; its pnl pool is drained and it owes nothing",
            market_index
        )?;

        controller::perp_pools::get_pnl_pool_drain_reserve_price(
            &perp_market,
            &state,
            &mut maps.oracle_map,
        )?
    };

    let discharged = controller::revenue_share::sweep_completed_revenue_share_for_market(
        market_index,
        &mut escrow,
        &maps.perp_market_map,
        &maps.spot_market_map,
        &revenue_share_map,
        clock.unix_timestamp,
        reserve_price,
        state.builder_codes_enabled(),
        state.funding_paused()?,
    )?;

    msg!(
        "settled revenue share for market {} escrow {}: {}",
        market_index,
        escrow_authority,
        discharged
    );

    let spot_market = maps.spot_market_map.get_quote_spot_market()?;
    validate_spot_market_vault_amount(&spot_market, ctx.accounts.spot_market_vault.amount)?;

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
    exchange_not_paused(&ctx.accounts.state)
    valid_oracle_for_spot_market(&ctx.accounts.oracle, &ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_cumulative_interest(
    ctx: Context<UpdateSpotMarketCumulativeInterest>,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let clock_slot = clock.slot;

    let mut oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock_slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let oracle_price_data = oracle_map.get_price_data(&spot_market.oracle_id())?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        Some(oracle_price_data),
        now,
        state.funding_paused()?,
    )?;

    math::spot_withdraw::validate_spot_market_vault_amount(
        spot_market,
        ctx.accounts.spot_market_vault.amount,
    )?;

    Ok(())
}

/// Permissionless batch refresh: book the lending interest of several spot markets in one
/// instruction. Markets and their indexes arrive through `remaining_accounts`, and each one goes
/// through the same `update_spot_market_cumulative_interest` as the single-market crank above.
///
/// Written for a caller that must value several markets in one transaction, such as a program
/// that prices a share against the markets a user holds. The single-market crank stays the
/// instruction that keeps a market's oracle EMA fresh.
///
/// This instruction moves no tokens and passes no oracle, which is why it drops two of the crank's
/// guards and its spot-vault assertion, and keeps the third:
///
/// - No oracle means `update_spot_market_twap_stats` leaves `historical_oracle_data` alone. A
///   caller that reads a market's oracle TWAP after this call therefore reads a value this call
///   did not move, and no caller can pick the sampling instant of an oracle EMA.
/// - A market status of `Delisted` is not rejected. `deposit` and `force_delete_user` already
///   book interest on a delisted market, so refusing here would block callers without stopping
///   the accrual.
/// - `exchange_not_paused` is kept. A full halt sets every `ExchangeStatus` bit, `FundingPaused`
///   included, so no interest can accrue and the only work left is stamping the clock and the
///   balance TWAPs. Those TWAPs size the withdraw and borrow circuit breakers, and a halt freezes
///   them for a reason. Without this guard a caller could re-baseline a breaker mid-halt, or stamp
///   the halted interval away so nobody is charged for it.
/// - The spot vault holds the same tokens after this call as before it, and booking interest can
///   only lower the depositors' claim, never raise it. Asserting the vault invariant here would
///   let one market that is already short abort the refresh of every other market in the batch.
#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_refresh_spot_market_interest<'c: 'info, 'info>(
    ctx: Context<'info, RefreshSpotMarketInterest<'info>>,
    args: RefreshSpotMarketInterestArgs,
) -> Result<()> {
    let RefreshSpotMarketInterestArgs { market_indexes } = args;
    // A user holds eight spot positions, and every perp market quotes the same spot market
    // (`initialize_perp_market` hardcodes it and no setter exists), so ten markets cover every
    // market one user's equity can read. The cap keeps one call inside a compute budget.
    validate!(
        market_indexes.len() <= 16,
        ErrorCode::DefaultError,
        "too many markets passed, max 16, got {}",
        market_indexes.len()
    )?;

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;

    let writable_spot_markets = get_writable_spot_market_set_from_many(market_indexes);

    let maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &writable_spot_markets,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    controller::spot_balance::refresh_spot_market_interest(
        &maps.spot_market_map,
        None,
        &writable_spot_markets,
        clock.unix_timestamp,
        state.funding_paused()?,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_update_amms<'c: 'info, 'info>(
    ctx: Context<'info, UpdateAMM<'info>>,
    market_indexes: Vec<u16>,
) -> Result<()> {
    if market_indexes.len() > 5 {
        msg!("Too many markets passed, max 5");
        return Err(ErrorCode::DefaultError.into());
    }
    // up to ~60k compute units (per amm) worst case

    let clock = Clock::get()?;

    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_market_set_from_list(market_indexes),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    crate::vlp::amm::refresh::update_amms(
        &mut maps.perp_market_map,
        &mut maps.oracle_map,
        &state,
        &clock,
    )?;

    Ok(())
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn view_amm_liquidity<'c: 'info, 'info>(
    ctx: Context<'info, UpdateAMM<'info>>,
    market_indexes: Vec<u16>,
) -> Result<()> {
    if market_indexes.len() > 5 {
        msg!("Too many markets passed, max 5");
        return Err(ErrorCode::DefaultError.into());
    }
    // up to ~60k compute units (per amm) worst case

    let clock = Clock::get()?;

    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let oracle_map = &mut OracleMap::load(
        remaining_accounts_iter,
        clock.slot,
        state.slot_clock(),
        None,
    )?;
    let market_map = &mut PerpMarketMap::load(
        &get_market_set_from_list(market_indexes),
        remaining_accounts_iter,
    )?;

    crate::vlp::amm::refresh::update_amms(market_map, oracle_map, &state, &clock)?;

    for (_key, market_account_loader) in market_map.0.iter_mut() {
        let market = &mut load_mut!(market_account_loader)?;
        let oracle_price_data = &oracle_map.get_price_data(&market.oracle_id())?;

        // `update_amms` above refreshed each AMM's cached spread state; read
        // it back for the dlog.
        let reserve_price = market.amm.reserve_price()?;
        let (bid, ask) = market.amm.bid_ask_price(
            reserve_price,
            market.amm.long_spread,
            market.amm.short_spread,
            market.amm.reference_price_offset,
        )?;
        crate::dlog!(bid, ask, oracle_price_data.price);
    }

    Ok(())
}

pub fn handle_update_user_quote_asset_insurance_stake(
    ctx: Context<UpdateUserQuoteAssetInsuranceStake>,
) -> Result<()> {
    let insurance_fund_stake = &mut load_mut!(ctx.accounts.insurance_fund_stake)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    validate!(
        insurance_fund_stake.market_index == 0,
        ErrorCode::IncorrectSpotMarketAccountPassed,
        "insurance_fund_stake is not for quote market"
    )?;

    if insurance_fund_stake.market_index == 0 && spot_market.market_index == 0 {
        update_user_stats_if_stake_amount(
            0,
            ctx.accounts.insurance_fund_vault.amount,
            insurance_fund_stake,
            user_stats,
            spot_market,
        )?;
    }

    Ok(())
}

pub fn handle_force_delete_user<'c: 'info, 'info>(
    ctx: Context<'info, ForceDeleteUser<'info>>,
) -> Result<()> {
    // Pyra accounts are exempt from force_delete_user

    let pyra_program = pubkey!("6JjHXLheGSNvvexgzMthEcgjkcirDrGduc3HAKB2P1v2");
    validate!(
        *ctx.accounts.authority.owner != pyra_program,
        ErrorCode::DefaultError,
        "pyra accounts are exempt from force_delete_user"
    )?;

    let state = ctx.accounts.state.load()?;

    let keeper_key = *ctx.accounts.keeper.key;

    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;

    let slot = Clock::get()?.slot;
    let now = Clock::get()?.unix_timestamp;
    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_market_set_for_spot_positions(&user.spot_positions),
        slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // check the user equity

    let (user_equity, all_oracles_valid) = calculate_user_equity(user, &mut maps)?;

    // Deletion sends the user's remaining deposits to the keeper's own token
    // account, so this must fail closed. A stale-low price understates the
    // equity and makes a funded account look like dust.
    validate!(
        all_oracles_valid,
        ErrorCode::InvalidOracle,
        "cannot force delete user with an invalid oracle"
    )?;

    let max_equity = QUOTE_PRECISION_I128 / 20;
    validate!(
        user_equity <= max_equity,
        ErrorCode::DefaultError,
        "user equity must be less than {}",
        max_equity
    )?;

    #[cfg(not(feature = "anchor-test"))]
    {
        let time_since_last_active = state.slot_clock().elapsed(user.last_active_slot, slot);

        validate!(
            // ~3 months (12 weeks)
            time_since_last_active >= Millis::from_secs(7_257_600),
            ErrorCode::DefaultError,
            "user not inactive for long enough: {} ms",
            time_since_last_active.as_ms()
        )?;
    }

    // cancel all open orders
    cancel_orders(
        user,
        &user_key,
        Some(&keeper_key),
        &mut maps,
        now,
        slot,
        OrderActionExplanation::None,
        None,
        None,
        None,
        false,
    )?;

    validate!(
        !user.perp_positions.iter().any(|p| !p.is_available()),
        ErrorCode::DefaultError,
        "user must have no perp positions"
    )?;

    // Book the interest of every market the user still holds before the transfers below read a
    // token amount. Cancelling the orders above can free a position that only open orders kept
    // alive, so the set is taken here rather than reused from the account load.
    //
    // The dust gate earlier in this handler still values the user through the stored indexes. An
    // account close to the cap can therefore read below it and be deleted, which sends its
    // deposits to the keeper. Moving that gate after this call is a separate change.
    controller::spot_balance::refresh_spot_market_interest(
        &maps.spot_market_map,
        Some(&mut maps.oracle_map),
        &get_market_set_for_spot_positions(&user.spot_positions),
        now,
        state.funding_paused()?,
    )?;

    for spot_position in user.spot_positions.iter_mut() {
        if spot_position.is_available() {
            continue;
        }

        let spot_market = &mut maps
            .spot_market_map
            .get_ref_mut(&spot_position.market_index)?;

        let token_amount = spot_position.get_token_amount(spot_market)?;
        let balance_type = spot_position.balance_type;

        let token_program_pubkey = spot_market.get_token_program();

        let token_program = &ctx
            .remaining_accounts
            .iter()
            .find(|acc| acc.key() == token_program_pubkey)
            .map(Interface::try_from)
            .unwrap()
            .unwrap();

        let spot_market_mint = &spot_market.mint;
        let mint_account_info = ctx
            .remaining_accounts
            .iter()
            .find(|acc| acc.key() == spot_market_mint.key())
            .map(|acc| InterfaceAccount::try_from(acc).unwrap());

        let keeper_vault = get_associated_token_address_with_program_id(
            &keeper_key,
            spot_market_mint,
            &token_program_pubkey,
        );
        let keeper_vault_account_info = ctx
            .remaining_accounts
            .iter()
            .find(|acc| acc.key() == keeper_vault.key())
            .map(InterfaceAccount::try_from)
            .unwrap()
            .unwrap();

        let spot_market_vault = spot_market.vault;
        let mut spot_market_vault_account_info = ctx
            .remaining_accounts
            .iter()
            .find(|acc| acc.key() == spot_market_vault.key())
            .map(InterfaceAccount::try_from)
            .unwrap()
            .unwrap();

        if balance_type == SpotBalanceType::Deposit {
            update_spot_balances(
                token_amount,
                &SpotBalanceType::Borrow,
                spot_market,
                spot_position,
                true,
            )?;

            // TODO: support transfer hook tokens
            send_from_program_vault(
                token_program,
                &spot_market_vault_account_info,
                &keeper_vault_account_info,
                &ctx.accounts.velocity_signer,
                state.signer_nonce,
                token_amount.cast()?,
                &mint_account_info,
                None,
            )?;
        } else {
            update_spot_balances(
                token_amount,
                &SpotBalanceType::Deposit,
                spot_market,
                spot_position,
                false,
            )?;

            // TODO: support transfer hook tokens
            receive(
                token_program,
                &keeper_vault_account_info,
                &spot_market_vault_account_info,
                &ctx.accounts.keeper.to_account_info(),
                token_amount.cast()?,
                &mint_account_info,
                None,
            )?;
        }

        spot_market_vault_account_info.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            spot_market_vault_account_info.amount,
        )?;
    }

    validate_user_deletion(
        user,
        user_stats,
        &*ctx.accounts.state.load()?,
        Clock::get()?.unix_timestamp,
    )?;

    // OtterSec #128: settle this subaccount's revenue-share rows before the id goes
    // away for good. This path retires the id exactly as `delete_user` does, so it
    // orphans a fee-bearing row the same way. See `handle_delete_user` for why the row
    // then becomes unreachable.
    //
    // `cancel_orders` above closed every order of this subaccount, so each row for it
    // becomes `Completed` (or is cleared when it carries no fees). That is the state
    // the permissionless sweep pays out of.
    //
    // The escrow is pinned to the authority's PDA by `seeds`, so an empty account
    // proves this authority has no escrow rather than signalling an omitted account.
    if !ctx.accounts.revenue_share_escrow.data_is_empty() {
        // `ZeroCopyLoader` is in scope for this module and also has a `load_zc_mut`, so
        // name the trait to pick the escrow's loader.
        use crate::state::revenue_share::RevenueShareEscrowLoader;

        let mut escrow =
            RevenueShareEscrowLoader::load_zc_mut(&*ctx.accounts.revenue_share_escrow)?;
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

    // Release the shared `State` borrow taken at the top of this handler. `Ref` implements `Drop`,
    // so the borrow lives to the end of the scope and a shadowing `let` does not end it. Without
    // this the `load_mut` below fails with `AccountBorrowFailed`, and it fails after the user's
    // tokens have already moved to the keeper.
    drop(state);

    let mut state = ctx.accounts.state.load_mut()?;
    safe_decrement!(state.number_of_sub_accounts, 1);

    emit!(DeleteUserRecord {
        ts: now,
        user_authority: *ctx.accounts.authority.key,
        user: user_key,
        sub_account_id: user.sub_account_id,
        keeper: Some(*ctx.accounts.keeper.key),
    });

    Ok(())
}

pub fn handle_pause_spot_market_deposit_withdraw(
    ctx: Context<PauseSpotMarketDepositWithdraw>,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    let result =
        validate_spot_market_vault_amount(spot_market, ctx.accounts.spot_market_vault.amount);

    validate!(
        matches!(result, Err(ErrorCode::SpotMarketVaultInvariantViolated)),
        ErrorCode::DefaultError,
        "spot market vault amount is valid"
    )?;

    spot_market.paused_operations |= SpotOperation::Deposit as u8;
    spot_market.paused_operations |= SpotOperation::Withdraw as u8;

    Ok(())
}

pub fn handle_update_amm_cache<'c: 'info, 'info>(
    ctx: Context<'info, UpdateAmmCache<'info>>,
) -> Result<()> {
    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut amm_cache: AccountZeroCopyMut<'_, CacheInfo, _> =
        ctx.accounts.amm_cache.load_zc_mut()?;

    let state = ctx.accounts.state.load()?;
    let quote_market = ctx.accounts.quote_market.load()?;

    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        state.slot_clock(),
        None,
    )?;
    let slot = Clock::get()?.slot;

    for (_, perp_market_loader) in maps.perp_market_map.0.iter() {
        let perp_market = perp_market_loader.load()?;
        if perp_market.hedge_config.status == 0 {
            continue;
        }
        let cached_info = amm_cache.get_for_market_index_mut(perp_market.market_index)?;

        validate!(
            perp_market.oracle_id() == cached_info.oracle_id()?,
            ErrorCode::DefaultError,
            "oracle id mismatch between amm cache and perp market"
        )?;

        let oracle_data = maps.oracle_map.get_price_data(&perp_market.oracle_id())?;
        let validity = ctx.accounts.state.load()?.oracle_guard_rails.validity;
        let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
            *oracle_data,
            slot,
            &validity,
            state.slot_clock(),
        )?;

        cached_info.update_perp_market_fields(&perp_market)?;
        cached_info.try_update_oracle_info(
            slot,
            &mm_oracle_price_data,
            &perp_market,
            &state.oracle_guard_rails,
            state.slot_clock(),
        )?;

        if perp_market.hedge_config.status != 0
            && !PerpLpOperation::is_operation_paused(
                perp_market.hedge_config.paused_operations,
                PerpLpOperation::TrackAmmRevenue,
            )
        {
            amm_cache.update_amount_owed_from_lp_pool(&perp_market, &quote_market)?;
        }
    }

    Ok(())
}

#[derive(Accounts)]
pub struct UpdateAmmCache<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    /// CHECK: checked in AmmCacheZeroCopy checks
    #[account(mut)]
    pub amm_cache: UncheckedAccount<'info>,
    #[account(
        owner = crate::ID,
        seeds = [b"spot_market", QUOTE_SPOT_MARKET_INDEX.to_le_bytes().as_ref()],
        bump,
    )]
    pub quote_market: AccountLoader<'info, SpotMarket>,
}

#[derive(Accounts)]
pub struct FillOrder<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// CHECK: address-locked to the instructions sysvar. See `FillLegacyDlobOrder` for
    /// what it is read for and why it is optional.
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: Option<UncheckedAccount<'info>>,
}

#[derive(Accounts)]
pub struct RevertFill<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
pub struct TriggerOrder<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler`; in
    /// program-keeper mode (protocol `User` as filler, relay turners) it is
    /// only the lamport payout target and no signature is required.
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
    /// The user's relay trigger conditions: the fired slot is released so
    /// its level-triggered wake goes quiet. Optional — keepers on markets
    /// (or users) without relay plumbing crank exactly as before.
    #[account(
        mut,
        seeds = [USER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        bump
    )]
    pub trigger_conditions: Option<AccountLoader<'info, UserConditionsV0>>,
    /// The fired market's crank conditions — the reservoir that pays the
    /// keeper in program-keeper mode (validated against the order's market
    /// in the handler). Required in program-keeper mode.
    #[account(mut)]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
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
pub struct PlaceSignedMsgTakerOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(
        mut,
        seeds = [SIGNED_MSG_PDA_SEED.as_bytes(), user.load()?.authority.as_ref()],
        bump,
    )]
    /// CHECK: checked in SignedMsgUserOrdersZeroCopy checks
    pub signed_msg_user_orders: UncheckedAccount<'info>,
    pub authority: Signer<'info>,
    /// CHECK: The address check is needed because otherwise
    /// the supplied Sysvar could be anything else.
    /// The Instruction Sysvar has not been implemented
    /// in the Anchor framework yet, so this is the safe approach.
    #[account(address = IX_ID)]
    pub ix_sysvar: UncheckedAccount<'info>,
    /// The keeper's own `User`, credited for the fill it lands. The taker did
    /// not sign this transaction, so the keeper is a filler and owes the taker
    /// every maker it had room to carry.
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The market's quoter slab. The remainder only ever rests on the vetted
    /// book its `Clob` slot names, and that book is the mandatory baseline of
    /// a router fill.
    #[account(has_one = clob_market)]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct SettleFunding<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
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

#[derive(Accounts)]
#[instruction(asset_market_index: u16, liability_market_index: u16, )]
pub struct LiquidateSpotWithSwap<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&liquidator, &authority)?
    )]
    pub liquidator: AccountLoader<'info, User>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), liability_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub liability_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), asset_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub asset_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &liability_spot_market_vault.mint.eq(&liability_token_account.mint),
        token::authority = authority
    )]
    pub liability_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &asset_spot_market_vault.mint.eq(&asset_token_account.mint),
        token::authority = authority
    )]
    pub asset_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    /// Instructions Sysvar for instruction introspection
    /// CHECK: fixed instructions sysvar account
    #[account(address = instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    /// The liquidator's `UserStats`, read by `begin` to bar an authority whose
    /// equity breaker is tripped.
    ///
    /// It sits last, not beside `liquidator` where the direct liquidation
    /// contexts carry it, because this pair is addressed by position rather
    /// than by name: `begin` introspects the matching `end` and compares the
    /// two account lists index by index, and the swap accounts both forward
    /// begin where this fixed block ends. Taking the last slot renumbered
    /// nothing. Slotting it beside `liquidator` would have moved `user`, both
    /// vaults and both token accounts down one, silently invalidating every
    /// hand-built transaction that still filled the old order.
    #[account(
        constraint = is_stats_for_user(&liquidator, &liquidator_stats)?
    )]
    pub liquidator_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
#[instruction(spot_market_index: u16,)]
pub struct ResolveBankruptcy<'info> {
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
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()], // todo: market_index=0 hardcode for perps?
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
#[instruction(spot_market_index: u16,)]
pub struct ResolvePerpPnlDeficit<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()], // todo: market_index=0 hardcode for perps?
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
pub struct SettleRevenueToInsuranceFund<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(perp_market_index: u16,)]
pub struct SweepPerpMarketFees<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"perp_market", perp_market_index.to_le_bytes().as_ref()],
        bump,
        has_one = oracle @ ErrorCode::InvalidOracle,
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The perp market's quote spot market (enforced by the PDA derivation)
    #[account(
        mut,
        seeds = [b"spot_market", perp_market.load()?.quote_spot_market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: must be `perp_market.oracle` (enforced by `has_one` above)
    pub oracle: UncheckedAccount<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct ForfeitRevenueShareOrderArgs {
    pub market_index: u16,
    /// Index of the escrow order row to forfeit.
    pub order_index: u32,
}

#[derive(Accounts)]
#[instruction(args: ForfeitRevenueShareOrderArgs)]
pub struct ForfeitRevenueShareOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The quote spot market of the perp market. The PDA seeds enforce this. The handler values
    /// the pnl pool against it. It is writable because the handler accrues interest first.
    #[account(
        mut,
        seeds = [b"spot_market", perp_market.load()?.quote_spot_market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// The owner of the escrow that holds the row.
    /// CHECK: the PDA seeds below bind this key to the escrow. The handler also compares it with the authority in the escrow header.
    pub escrow_authority: UncheckedAccount<'info>,
    /// The escrow that holds the row to write off.
    /// CHECK: `load_zc_mut` reads this account and validates the owner and the discriminator. The seeds fix the address.
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), escrow_authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
    /// Sub-account 0 of the beneficiary of the row. This is the payout account. The handler proves
    /// that it does not exist.
    /// CHECK: the handler derives the required address from the beneficiary of the row and rejects any other address. Anchor `seeds` cannot express this, because the address depends on `builder_idx` and on `approved_builders`, which the handler reads at run time.
    pub beneficiary_user: UncheckedAccount<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct SettleRevenueShareArgs {
    pub market_index: u16,
    /// How many of the owner's sub-accounts ride the remaining accounts.
    pub num_owner_sub_accounts: u8,
}

#[derive(Accounts)]
pub struct SettleRevenueShare<'info> {
    pub state: AccountLoader<'info, State>,
    /// The owner of the escrow to settle.
    /// CHECK: the PDA seeds below bind this key to the escrow. The handler also compares it with the authority in the escrow header.
    pub escrow_authority: UncheckedAccount<'info>,
    /// The escrow that holds the accrued builder and referrer rows.
    /// CHECK: `load_zc_mut` reads this account and validates the owner and the discriminator. The seeds fix the address.
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), escrow_authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), 0_u16.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct UpdateSpotMarketCumulativeInterest<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: checked in `update_spot_market_cumulative_interest` ix constraint
    pub oracle: UncheckedAccount<'info>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), spot_market.load()?.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

/// The markets to refresh arrive as writable spot market accounts in `remaining_accounts`.
/// `SpotMarketMap` reads each market's index out of the account it loads, so a market is refreshed
/// only when its own account is passed.
#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct RefreshSpotMarketInterestArgs {
    pub market_indexes: Vec<u16>,
}

#[derive(Accounts)]
pub struct RefreshSpotMarketInterest<'info> {
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct UpdateAMM<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct UpdateFundingRate<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: checked in `update_funding_rate` ix constraint
    pub oracle: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct UpdatePerpBidAskTwap<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: checked in `update_funding_rate` ix constraint
    pub oracle: UncheckedAccount<'info>,
    #[account(has_one = authority)]
    pub keeper_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct UpdateUserQuoteAssetInsuranceStake<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market", 0_u16.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        constraint = is_stats_for_if_stake(&insurance_fund_stake, &user_stats)?
    )]
    pub insurance_fund_stake: AccountLoader<'info, InsuranceFundStake>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub signer: Signer<'info>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), 0_u16.to_le_bytes().as_ref()],
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[derive(Accounts)]
pub struct UpdatePrelaunchOracle<'info> {
    pub state: AccountLoader<'info, State>,
    pub perp_market: AccountLoader<'info, PerpMarket>,
    #[account(mut)]
    /// CHECK: checked in ix
    pub oracle: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ForceDeleteUser<'info> {
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
    /// CHECK: authority
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = check_hot(&keeper.key(), &state, HotRole::UserFlag)?
    )]
    pub keeper: Signer<'info>,
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    /// CHECK: the authority's `RevenueShareEscrow`. It may legitimately not exist,
    /// because most users never create one. It carries the same contract as
    /// `DeleteUser::revenue_share_escrow`: an `UncheckedAccount` pinned by `seeds`, so
    /// the handler can tell "this authority has no escrow" (`data_is_empty()`) from "the
    /// keeper omitted the account to skip the check". It is required rather than
    /// `Option` for that second reason (OtterSec #128).
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct PauseSpotMarketDepositWithdraw<'info> {
    pub state: AccountLoader<'info, State>,
    pub keeper: Signer<'info>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), spot_market.load()?.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

/// The relay resolver for `trigger_order` (`Resolve<EndpointName>`):
/// simulation-only, staged from the user's synced trigger conditions.
#[derive(Accounts)]
pub struct ResolveTriggerOrder<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account, not
    /// into the block they read.
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
