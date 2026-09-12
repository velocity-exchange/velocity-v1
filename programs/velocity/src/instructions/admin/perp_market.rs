//! The life of a perp market, from creation to deletion.
//!
//! [`handle_initialize_perp_market`] creates the market account, seeds its AMM
//! from the first oracle price, and validates the result.
//! [`handle_delete_initialized_perp_market`] removes a market that never went
//! live. The rest move one identity or trading setting: the name, the status,
//! the paused operations, the contract tier, the order size grid, and the LP
//! pool the market belongs to.
//!
//! Risk limits are in [`super::perp_risk`]. Oracle assignment is in
//! [`super::oracles`].

use super::*;

/// The settings a new perp market starts with.
///
/// The handler's argument list is fixed by the program ABI. It collects the
/// settings into one value, so every step below reads one parameter.
struct NewPerpMarket {
    market_index: u16,
    amm_base_asset_reserve: u128,
    amm_quote_asset_reserve: u128,
    amm_periodicity: i64,
    amm_peg_multiplier: u128,
    oracle_source: OracleSource,
    contract_tier: ContractTier,
    margin_ratio_initial: u32,
    margin_ratio_maintenance: u32,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    imf_factor: u32,
    active_status: bool,
    base_spread: u32,
    max_spread: u32,
    max_open_interest: u128,
    max_revenue_withdraw_per_period: u64,
    quote_max_insurance: u64,
    order_step_size: u64,
    order_tick_size: u64,
    min_order_size: u64,
    concentration_coef_scale: u128,
    curve_update_intensity: u8,
    amm_jit_intensity: u8,
    name: [u8; 32],
    lp_pool_id: u8,
    funding_clamp_threshold: u32,
    funding_ramp_slope: u32,
}

impl NewPerpMarket {
    /// Zero means "unset" for the two funding settings. It selects the launch
    /// defaults of 5 basis points and a slope of 1.0.
    fn with_funding_defaults(mut self) -> Self {
        if self.funding_clamp_threshold == 0 {
            self.funding_clamp_threshold = 5;
        }
        if self.funding_ramp_slope == 0 {
            self.funding_ramp_slope = PERCENTAGE_PRECISION_U32;
        }
        self
    }
}

/// The curve a new perp market opens on, derived from its reserves.
struct PerpAmmSeed {
    init_reserve_price: u64,
    concentration_coef: u128,
    min_base_asset_reserve: u128,
    max_base_asset_reserve: u128,
}

/// The first oracle reading a new perp market records.
struct InitialPerpOracle {
    price: i64,
    delay: i64,
    twap: i64,
}

#[allow(clippy::too_many_arguments)]
pub fn handle_initialize_perp_market(
    ctx: Context<InitializePerpMarket>,
    market_index: u16,
    amm_base_asset_reserve: u128,
    amm_quote_asset_reserve: u128,
    amm_periodicity: i64,
    amm_peg_multiplier: u128,
    oracle_source: OracleSource,
    contract_tier: ContractTier,
    margin_ratio_initial: u32,
    margin_ratio_maintenance: u32,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    imf_factor: u32,
    active_status: bool,
    base_spread: u32,
    max_spread: u32,
    max_open_interest: u128,
    max_revenue_withdraw_per_period: u64,
    quote_max_insurance: u64,
    order_step_size: u64,
    order_tick_size: u64,
    min_order_size: u64,
    concentration_coef_scale: u128,
    curve_update_intensity: u8,
    amm_jit_intensity: u8,
    name: [u8; 32],
    lp_pool_id: u8,
    funding_clamp_threshold: u32,
    funding_ramp_slope: u32,
) -> Result<()> {
    initialize_perp_market(
        ctx,
        NewPerpMarket {
            market_index,
            amm_base_asset_reserve,
            amm_quote_asset_reserve,
            amm_periodicity,
            amm_peg_multiplier,
            oracle_source,
            contract_tier,
            margin_ratio_initial,
            margin_ratio_maintenance,
            liquidator_fee,
            if_liquidation_fee,
            imf_factor,
            active_status,
            base_spread,
            max_spread,
            max_open_interest,
            max_revenue_withdraw_per_period,
            quote_max_insurance,
            order_step_size,
            order_tick_size,
            min_order_size,
            concentration_coef_scale,
            curve_update_intensity,
            amm_jit_intensity,
            name,
            lp_pool_id,
            funding_clamp_threshold,
            funding_ramp_slope,
        },
    )
}

fn initialize_perp_market(ctx: Context<InitializePerpMarket>, params: NewPerpMarket) -> Result<()> {
    msg!("perp market {}", params.market_index);
    let perp_market = &mut ctx.accounts.perp_market.load_init()?;

    let params = params.with_funding_defaults();

    let clock = Clock::get()?;
    let clock_slot = clock.slot;

    validate_new_perp_market(&params)?;

    let seed = perp_amm_seed(&params)?;

    OracleMap::validate_oracle_account_info(&ctx.accounts.oracle)?;

    let oracle = initial_perp_oracle(
        &perp_market.amm,
        &ctx.accounts.oracle,
        params.oracle_source,
        clock_slot,
    )?;

    validate_margin(
        params.margin_ratio_initial,
        params.margin_ratio_maintenance,
        params.liquidator_fee,
        params.if_liquidation_fee,
        params.max_spread,
    )?;

    let mut state = ctx.accounts.state.load_mut()?;
    validate_new_perp_market_index(ctx.accounts, &state, &params)?;

    **perp_market = new_perp_market(&params, ctx.accounts, &seed, &oracle, &clock)?;

    safe_increment!(state.number_of_markets, 1);

    perp_market
        .amm
        .update_concentration_coef(params.concentration_coef_scale)?;

    log_new_perp_market(perp_market, oracle.price)?;

    crate::validation::perp_market::validate_perp_market(perp_market)?;

    Ok(())
}

/// Holds a new perp market to the next free index, and an active launch to the
/// cold admin. A market that opens active takes orders at once, so only the
/// root key may do it.
fn validate_new_perp_market_index(
    accounts: &InitializePerpMarket,
    state: &State,
    params: &NewPerpMarket,
) -> Result<()> {
    validate!(
        params.market_index == state.number_of_markets,
        ErrorCode::MarketIndexAlreadyInitialized,
        "market_index={} != state.number_of_markets={}",
        params.market_index,
        state.number_of_markets
    )?;

    if params.active_status {
        validate!(
            accounts.admin.key() == state.cold_admin,
            ErrorCode::DefaultError,
            "admin must be state admin"
        )?;
    }

    Ok(())
}

/// Logs the opening state of a new perp market: the oracle price, the size the
/// AMM offers on each side, and the prices it offers them at.
fn log_new_perp_market(perp_market: &PerpMarket, oracle_price: i64) -> Result<()> {
    crate::dlog!(oracle_price);

    let (amm_bid_size, amm_ask_size) = amm::calculate_market_open_bids_asks(&perp_market.amm)?;
    crate::dlog!(amm_bid_size, amm_ask_size);

    // dlog the seeded (no-spread) bid/ask off the AMM's cached spread fields.
    let mrk = perp_market.amm.reserve_price()?;
    let (amm_bid_price, amm_ask_price) = perp_market.amm.bid_ask_price(
        mrk,
        perp_market.amm.long_spread,
        perp_market.amm.short_spread,
        perp_market.amm.reference_price_offset,
    )?;
    crate::dlog!(amm_bid_price, amm_ask_price);

    Ok(())
}

/// Holds a new perp market to the settings the AMM math can work with.
///
/// The reserves must open balanced, because the peg carries the price. Both
/// intensities are percentages.
fn validate_new_perp_market(params: &NewPerpMarket) -> Result<()> {
    validate_supported_market_oracle_source(params.oracle_source)?;

    if params.amm_base_asset_reserve != params.amm_quote_asset_reserve {
        return Err(ErrorCode::InvalidInitialPeg.into());
    }

    validate!(
        (0..=100).contains(&params.curve_update_intensity),
        ErrorCode::DefaultError,
        "invalid curve_update_intensity",
    )?;

    validate!(
        (0..=100).contains(&params.amm_jit_intensity),
        ErrorCode::DefaultError,
        "invalid amm_jit_intensity",
    )?;

    Ok(())
}

/// Derives the curve a new perp market opens on.
///
/// The opening price must equal the peg, because the reserves open balanced.
/// The bounds come from the concentration coefficient, which fixes how far the
/// reserves may move before the market refuses a fill.
fn perp_amm_seed(params: &NewPerpMarket) -> Result<PerpAmmSeed> {
    let init_reserve_price = amm::calculate_price(
        params.amm_quote_asset_reserve,
        params.amm_base_asset_reserve,
        params.amm_peg_multiplier,
    )?;

    assert_eq!(
        params.amm_peg_multiplier,
        init_reserve_price.cast::<u128>()?
    );

    let concentration_coef = MAX_CONCENTRATION_COEFFICIENT;

    // Verify there's no overflow
    let _k = bn::U192::from(params.amm_base_asset_reserve)
        .safe_mul(bn::U192::from(params.amm_quote_asset_reserve))?;

    let (min_base_asset_reserve, max_base_asset_reserve) =
        amm::calculate_bid_ask_bounds(concentration_coef, params.amm_base_asset_reserve)?;

    Ok(PerpAmmSeed {
        init_reserve_price,
        concentration_coef,
        min_base_asset_reserve,
        max_base_asset_reserve,
    })
}

/// Reads the first oracle price a new perp market records.
///
/// A Pyth feed carries its own TWAP, so the market starts from it. A stablecoin
/// feed starts from the quote price instead, and a prelaunch feed starts from
/// its own spot price. Every other source cannot price a perp market.
fn initial_perp_oracle(
    amm: &AMM,
    oracle_account: &AccountInfo,
    oracle_source: OracleSource,
    clock_slot: u64,
) -> Result<InitialPerpOracle> {
    let price_data = |source: OracleSource| get_pyth_price(oracle_account, clock_slot, &source);

    match oracle_source {
        OracleSource::Pyth
        | OracleSource::Pyth1K
        | OracleSource::Pyth1M
        | OracleSource::PythLazer
        | OracleSource::PythLazer1K
        | OracleSource::PythLazer1M => {
            let OraclePriceData { price, delay, .. } = price_data(oracle_source)?;
            let twap = amm.get_pyth_twap(oracle_account, &oracle_source)?;
            Ok(InitialPerpOracle { price, delay, twap })
        }
        OracleSource::PythStableCoin | OracleSource::PythLazerStableCoin => {
            let OraclePriceData { price, delay, .. } = price_data(oracle_source)?;
            Ok(InitialPerpOracle {
                price,
                delay,
                twap: QUOTE_PRECISION_I64,
            })
        }
        OracleSource::Prelaunch => {
            let OraclePriceData { price, delay, .. } =
                get_prelaunch_price(oracle_account, clock_slot)?;
            Ok(InitialPerpOracle {
                price,
                delay,
                twap: price,
            })
        }
        OracleSource::QuoteAsset => {
            msg!("Quote asset oracle cant be used for perp market");
            Err(ErrorCode::InvalidOracle.into())
        }
        OracleSource::DeprecatedSwitchboard
        | OracleSource::DeprecatedSwitchboardOnDemand
        | OracleSource::PythPull
        | OracleSource::Pyth1KPull
        | OracleSource::Pyth1MPull
        | OracleSource::PythStableCoinPull => Err(ErrorCode::InvalidOracle.into()),
    }
}

/// The market statistics a new perp market starts with. Every price twap starts
/// at the opening price, so the first crank has a reading to move from.
fn initial_market_stats(
    params: &NewPerpMarket,
    oracle: &InitialPerpOracle,
    init_reserve_price: u64,
    now: i64,
) -> MarketStats {
    MarketStats {
        last_oracle_normalised_price: oracle.price,
        last_mark_price_twap: init_reserve_price,
        last_mark_price_twap_5min: init_reserve_price,
        last_mark_price_twap_ts: now,
        last_bid_price_twap: init_reserve_price,
        last_ask_price_twap: init_reserve_price,
        last_trade_ts: now,
        last_24h_avg_funding_rate: 0,
        funding_period: params.amm_periodicity,
        min_order_size: params.min_order_size,
        historical_oracle_data: HistoricalOracleData {
            last_oracle_price: oracle.price,
            last_oracle_delay: oracle.delay,
            last_oracle_price_twap: oracle.twap,
            last_oracle_price_twap_5min: oracle.price,
            last_oracle_price_twap_ts: now,
            ..HistoricalOracleData::default()
        },
        ..MarketStats::default()
    }
}

/// The AMM a new perp market opens with.
fn initial_amm(params: &NewPerpMarket, seed: &PerpAmmSeed, clock_slot: u64) -> AMM {
    AMM {
        base_asset_reserve: params.amm_base_asset_reserve,
        quote_asset_reserve: params.amm_quote_asset_reserve,
        terminal_quote_asset_reserve: params.amm_quote_asset_reserve,
        sqrt_k: params.amm_base_asset_reserve,
        concentration_coef: seed.concentration_coef,
        min_base_asset_reserve: seed.min_base_asset_reserve,
        max_base_asset_reserve: seed.max_base_asset_reserve,
        peg_multiplier: params.amm_peg_multiplier,
        total_fee: 0,
        total_fee_withdrawn: 0,
        total_fee_minus_distributions: 0,
        total_mm_fee: 0,
        net_revenue_since_last_funding: 0,
        max_slippage_ratio: 50,         // ~2%
        max_fill_reserve_fraction: 100, // moves price ~2%
        base_spread: params.base_spread,
        max_spread: params.max_spread,
        base_asset_amount_with_amm: 0,
        curve_update_intensity: params.curve_update_intensity,
        fee_pool: PoolBalance::default(),
        last_update_slot: clock_slot,

        amm_jit_intensity: params.amm_jit_intensity,

        amm_spread_adjustment: 0,
        amm_inventory_spread_adjustment: 0,
        reference_price_offset_deadband_pct: 0,
        last_cumulative_funding_rate_long: 0,
        last_cumulative_funding_rate_short: 0,
        // Cached spread state: seed to a balanced no-spread snapshot
        // (ask/bid reserves == base/quote reserves, zero spreads). The
        // first `update_amms` keeper crank — or the first fill `setup` —
        // refreshes it with the real oracle-driven values.
        ask_base_asset_reserve: params.amm_base_asset_reserve,
        ask_quote_asset_reserve: params.amm_quote_asset_reserve,
        bid_base_asset_reserve: params.amm_base_asset_reserve,
        bid_quote_asset_reserve: params.amm_quote_asset_reserve,
        last_oracle_reserve_price_spread_pct: 0,
        last_spread_update_slot: clock_slot,
        long_spread: 0,
        short_spread: 0,
        reference_price_offset: 0,
        funding_bias_sensitivity: 0,
        padding_post_amm: [0; 2],
    }
}

/// A perp market at birth.
///
/// Every field is named, because a zero-copy account has no default to fall
/// back on. The curve and the statistics are built first, so this reads as the
/// market's own settings.
fn new_perp_market(
    params: &NewPerpMarket,
    accounts: &InitializePerpMarket,
    seed: &PerpAmmSeed,
    oracle: &InitialPerpOracle,
    clock: &Clock,
) -> Result<PerpMarket> {
    let now = clock.unix_timestamp;

    Ok(PerpMarket {
        contract_type: ContractType::Perpetual,
        contract_tier: params.contract_tier,
        status: if params.active_status {
            MarketStatus::Active
        } else {
            MarketStatus::Initialized
        },
        name: params.name,
        expiry_price: 0,
        expiry_ts: 0,
        pubkey: accounts.perp_market.key(),
        market_index: params.market_index,
        number_of_users_with_base: 0,
        number_of_users: 0,
        margin_ratio_initial: params.margin_ratio_initial, // unit is 20% (+2 decimal places)
        margin_ratio_maintenance: params.margin_ratio_maintenance,
        imf_factor: params.imf_factor,
        next_fill_record_id: 1,
        next_funding_rate_record_id: 1,
        fee_ledger: FeeLedger::default(),
        pnl_pool: PoolBalance::default(),
        insurance_claim: InsuranceClaim {
            max_revenue_withdraw_per_period: params.max_revenue_withdraw_per_period,
            quote_max_insurance: params.quote_max_insurance,
            ..InsuranceClaim::default()
        },
        unrealized_pnl_initial_asset_weight: 0, // 100%
        unrealized_pnl_maintenance_asset_weight: SPOT_WEIGHT_PRECISION.cast()?, // 100%
        unrealized_pnl_imf_factor: 0,
        unrealized_pnl_max_imbalance: 0,
        liquidator_fee: params.liquidator_fee,
        if_liquidation_fee: params.if_liquidation_fee,
        paused_operations: 0,
        quote_spot_market_index: QUOTE_SPOT_MARKET_INDEX,
        fee_adjustment: 0,
        pending_bankruptcy_claims: 0,
        _padding_align_lfp: [0; 4],
        pool_id: 0,
        _padding_pmm: [0; 2],
        _padding_hedge: [0; 5],
        last_fill_price: 0,
        market_config: 0,
        hedge_config: HedgeConfig {
            pool_id: params.lp_pool_id,
            status: 0,
            paused_operations: 0,
            exchange_fee_exclusion_scalar: 0,
            fee_transfer_scalar: 1,
            padding: [0; 11],
        },
        oracle: *accounts.oracle.key,
        oracle_source: params.oracle_source,
        oracle_slot_delay_override: -1,
        oracle_low_risk_slot_delay_override: 0,
        cumulative_funding_rate_long: 0,
        cumulative_funding_rate_short: 0,
        total_social_loss: 0,
        last_funding_rate: 0,
        last_funding_rate_long: 0,
        last_funding_rate_short: 0,
        last_funding_rate_ts: now,
        net_unsettled_funding_pnl: 0,
        funding_clamp_threshold: params.funding_clamp_threshold,
        funding_ramp_slope: params.funding_ramp_slope,
        order_step_size: params.order_step_size,
        order_tick_size: params.order_tick_size,
        base_asset_amount_long: 0,
        base_asset_amount_short: 0,
        quote_asset_amount: 0,
        quote_entry_amount_long: 0,
        quote_entry_amount_short: 0,
        quote_break_even_amount_long: 0,
        quote_break_even_amount_short: 0,
        max_open_interest: params.max_open_interest,
        bankruptcy_if_floor_pct: DEFAULT_BANKRUPTCY_IF_FLOOR_PCT,
        market_stats: initial_market_stats(params, oracle, seed.init_reserve_price, now),
        pending_revenue_share: 0,
        amm: initial_amm(params, seed, clock.slot),
        // protocol fees are quote-denominated; quote market is QUOTE_SPOT_MARKET_INDEX
        protocol_fee_pool: PoolBalance {
            market_index: QUOTE_SPOT_MARKET_INDEX,
            ..PoolBalance::default()
        },
        protocol_liquidation_fee: 0,
        taker_fee_addon_tenth_bps: 0,
        _padding_buffer: [0; 2],
        fee_pool_buffer_target: FEE_POOL_TO_REVENUE_POOL_THRESHOLD as u64,
        // Set post-init via `update_perp_market_clob_quoter` once the CLOB's
        // registry entry exists (the entry itself needs the market first).
        clob_market: Pubkey::default(),
        // The slab PDA is derivable now, so it is stored at birth: every
        // accounts struct that names both binds them with `has_one`.
        quoter_slab: crate::state::pdas::quoter_slab(params.market_index),
        _padding_future: [0; 192],
    })
}

pub fn handle_delete_initialized_perp_market(
    ctx: Context<DeleteInitializedPerpMarket>,
    market_index: u16,
) -> Result<()> {
    let perp_market = &mut ctx.accounts.perp_market.load()?;
    msg!("perp market {}", perp_market.market_index);
    let mut state = ctx.accounts.state.load_mut()?;

    // to preserve all protocol invariants, can only remove the last market if it hasn't been "activated"

    validate!(
        state.number_of_markets - 1 == market_index,
        ErrorCode::InvalidMarketAccountforDeletion,
        "state.number_of_markets={} != market_index={}",
        state.number_of_markets,
        market_index
    )?;
    validate!(
        perp_market.status == MarketStatus::Initialized,
        ErrorCode::InvalidMarketAccountforDeletion,
        "perp_market.status != Initialized",
    )?;
    validate!(
        perp_market.number_of_users == 0,
        ErrorCode::InvalidMarketAccountforDeletion,
        "perp_market.number_of_users={} != 0",
        perp_market.number_of_users,
    )?;
    validate!(
        perp_market.market_index == market_index,
        ErrorCode::InvalidMarketAccountforDeletion,
        "market_index={} != perp_market.market_index={}",
        market_index,
        perp_market.market_index
    )?;

    safe_decrement!(state.number_of_markets, 1);

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_name(
    ctx: Context<AdminUpdatePerpMarket>,
    name: [u8; 32],
) -> Result<()> {
    let mut perp_market = load_mut!(ctx.accounts.perp_market)?;
    msg!("perp_market.name: {:?} -> {:?}", perp_market.name, name);
    perp_market.name = name;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_lp_pool_id(
    ctx: Context<AdminUpdatePerpMarket>,
    lp_pool_id: u8,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating perp market {} lp pool id: {} -> {}",
        perp_market.market_index,
        perp_market.hedge_config.pool_id,
        lp_pool_id
    );
    perp_market.hedge_config.pool_id = lp_pool_id;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_status(
    ctx: Context<AdminUpdatePerpMarket>,
    status: MarketStatus,
) -> Result<()> {
    validate!(
        !matches!(status, MarketStatus::Delisted | MarketStatus::Settlement),
        ErrorCode::DefaultError,
        "must set settlement/delist through another instruction",
    )?;

    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.status: {:?} -> {:?}",
        perp_market.status,
        status
    );

    perp_market.status = status;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_paused_operations(
    ctx: Context<PauseAdminUpdatePerpMarket>,
    paused_operations: u8,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    // Authority matrix for perp paused_operations:
    //   * cold       — may set any value (full unpause + pause)
    //   * warm       — may only flip the UpdateFunding / SettleRevPool bits;
    //                  all other pause bits must be preserved
    //   * pause_admin — may set any bit but only *add* bits (no unpause)
    let signer = ctx.accounts.admin.key();
    let state = ctx.accounts.state.load()?;
    let is_cold = state.is_cold(&signer);
    let is_pause_admin = state.pause_admin != Pubkey::default() && state.pause_admin == signer;
    if !is_cold && !is_pause_admin {
        validate!(
            PerpOperation::is_warm_update_allowed(perp_market.paused_operations, paused_operations),
            ErrorCode::DefaultError,
            "warm admin may only change the UpdateFunding / SettleRevPool pause bits",
        )?;
    }
    require_pause_only_added(
        &signer,
        &state,
        perp_market.paused_operations,
        paused_operations,
    )?;
    drop(state);

    perp_market.paused_operations = paused_operations;

    PerpOperation::log_all_operations_paused(perp_market.paused_operations);

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_contract_tier(
    ctx: Context<AdminUpdatePerpMarket>,
    contract_tier: ContractTier,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.contract_tier: {:?} -> {:?}",
        perp_market.contract_tier,
        contract_tier
    );

    perp_market.contract_tier = contract_tier;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_step_size_and_tick_size(
    ctx: Context<AdminUpdatePerpMarket>,
    step_size: u64,
    tick_size: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(step_size > 0 && tick_size > 0, ErrorCode::DefaultError)?;
    validate!(step_size <= 2000000000, ErrorCode::DefaultError)?; // below i32 max for lp's remainder_base_asset

    msg!(
        "perp_market.order_step_size: {:?} -> {:?}",
        perp_market.order_step_size,
        step_size
    );

    msg!(
        "perp_market.order_tick_size: {:?} -> {:?}",
        perp_market.order_tick_size,
        tick_size
    );

    perp_market.order_step_size = step_size;
    perp_market.order_tick_size = tick_size;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_min_order_size(
    ctx: Context<AdminUpdatePerpMarket>,
    order_size: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(order_size > 0, ErrorCode::DefaultError)?;

    msg!(
        "perp_market.min_order_size: {:?} -> {:?}",
        perp_market.market_stats.min_order_size,
        order_size
    );

    perp_market.market_stats.min_order_size = order_size;
    Ok(())
}

/// Set the market's `bankruptcy_if_floor_pct` — the fraction of open-interest
/// notional the fee sweep must leave behind in `pending_if_fee` as a standing
/// bankruptcy tranche (PERCENTAGE_PRECISION). `0` selects
/// `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT`; `BANKRUPTCY_IF_FLOOR_DISABLED` turns the
/// standing floor off. Turning it off does not expose a latched bankruptcy:
/// `pending_bankruptcy_claims` still freezes the sweep until the debt
/// resolves.
pub fn handle_update_perp_market_bankruptcy_if_floor_pct(
    ctx: Context<AdminUpdatePerpMarket>,
    bankruptcy_if_floor_pct: u32,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        bankruptcy_if_floor_pct <= PERCENTAGE_PRECISION_U32
            || bankruptcy_if_floor_pct == BANKRUPTCY_IF_FLOOR_DISABLED,
        ErrorCode::DefaultError,
        "bankruptcy_if_floor_pct must be <= PERCENTAGE_PRECISION (100%) or BANKRUPTCY_IF_FLOOR_DISABLED"
    )?;

    msg!(
        "perp_market.bankruptcy_if_floor_pct: {:?} -> {:?}",
        perp_market.bankruptcy_if_floor_pct,
        bankruptcy_if_floor_pct
    );

    perp_market.bankruptcy_if_floor_pct = bankruptcy_if_floor_pct;
    Ok(())
}

pub fn handle_update_perp_market_number_of_users(
    ctx: Context<AdminUpdatePerpMarket>,
    number_of_users: Option<u32>,
    number_of_users_with_base: Option<u32>,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    if let Some(number_of_users) = number_of_users {
        msg!(
            "perp_market.number_of_users: {:?} -> {:?}",
            perp_market.number_of_users,
            number_of_users
        );
        perp_market.number_of_users = number_of_users;
    } else {
        msg!("perp_market.number_of_users: unchanged");
    }

    if let Some(number_of_users_with_base) = number_of_users_with_base {
        msg!(
            "perp_market.number_of_users_with_base: {:?} -> {:?}",
            perp_market.number_of_users_with_base,
            number_of_users_with_base
        );
        perp_market.number_of_users_with_base = number_of_users_with_base;
    } else {
        msg!("perp_market.number_of_users_with_base: unchanged");
    }

    validate!(
        perp_market.number_of_users >= perp_market.number_of_users_with_base,
        ErrorCode::DefaultError,
        "number_of_users must be >= number_of_users_with_base "
    )?;

    Ok(())
}

pub fn handle_update_perp_market_lp_pool_paused_operations(
    ctx: Context<PauseAdminUpdatePerpMarket>,
    lp_paused_operations: u8,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);
    let signer = ctx.accounts.admin.key();
    let state = ctx.accounts.state.load()?;
    require_pause_only_added(
        &signer,
        &state,
        perp_market.hedge_config.paused_operations,
        lp_paused_operations,
    )?;
    drop(state);
    perp_market.hedge_config.paused_operations = lp_paused_operations;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_config(
    ctx: Context<HotAdminUpdatePerpMarket>,
    market_config: u8,
) -> Result<()> {
    let allowed_bits = MarketConfigFlag::DisableFormulaicKUpdate as u8;

    validate!(
        market_config & !allowed_bits == 0,
        ErrorCode::InvalidPerpMarketConfig,
        "unknown bits set in market_config: {:?}",
        market_config
    )?;

    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    if *ctx.accounts.admin.key != ctx.accounts.state.load()?.cold_admin {
        validate!(
            market_config == 0,
            ErrorCode::DefaultError,
            "signer must be state admin to enable market config flags",
        )?;
    }

    msg!(
        "perp_market.market_config: {:?} -> {:?}",
        perp_market.market_config,
        market_config
    );

    perp_market.market_config = market_config;

    Ok(())
}

#[derive(Accounts)]
pub struct InitializePerpMarket<'info> {
    #[account(
        mut,
        constraint = check_warm(&admin.key(), &state)?
    )]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    #[account(
        init,
        seeds = [b"perp_market", state.load()?.number_of_markets.to_le_bytes().as_ref()],
        space = PerpMarket::SIZE,
        bump,
        payer = admin
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: checked in `initialize_perp_market`
    pub oracle: UncheckedAccount<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DeleteInitializedPerpMarket<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    #[account(mut, close = admin)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}
