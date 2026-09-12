//! The life of a spot market, from creation to deletion.
//!
//! [`handle_initialize_spot_market`] creates the market account and its two
//! token vaults. [`handle_delete_initialized_spot_market`] removes a market
//! that never went live. The rest move one identity or trading setting: the
//! name, the status, the paused operations, the asset tier, and the order
//! size grid.
//!
//! Deposit and borrow limits are in [`super::lending`]. Insurance settings are
//! in [`super::insurance_fund`].

use super::*;

/// Names reserved to the quote spot market (index 0). Monitoring keys the
/// stablecoin exemption in the deposit-concentration alert off the decoded
/// market name (`decodeName` = utf8 + trim), so if any *other* market could be
/// named "USDT" it would silently inherit that exemption and hide TVL
/// concentration. Reserving the name on-chain makes the name↔index binding
/// trustworthy: only market 0 can ever be "USDT".
const RESERVED_QUOTE_NAMES: &[&[u8]] = &[b"USDT"];

/// True if `name` decodes to one of the reserved quote names after trimming
/// leading/trailing whitespace. Trims a superset of what the off-chain
/// `decodeName().trim()` strips: all Unicode whitespace (`char::is_whitespace`)
/// plus U+FEFF (BOM, trimmed by JS but not Rust) and NUL. Invalid UTF-8 is
/// never reserved: it decodes to U+FFFD off-chain, which `trim()` keeps, so it
/// cannot decode to a reserved name.
fn name_is_reserved_quote(name: &[u8; 32]) -> bool {
    let is_trim = |c: char| c.is_whitespace() || c == '\0' || c == '\u{feff}';
    match core::str::from_utf8(name) {
        Ok(s) => RESERVED_QUOTE_NAMES.contains(&s.trim_matches(is_trim).as_bytes()),
        Err(_) => false,
    }
}

/// The settings a new spot market starts with.
///
/// The handler's argument list is fixed by the program ABI. It collects the
/// settings into one value, so every step below reads one parameter.
struct NewSpotMarket {
    optimal_utilization: u32,
    optimal_borrow_rate: u32,
    max_borrow_rate: u32,
    oracle_source: OracleSource,
    initial_asset_weight: u32,
    maintenance_asset_weight: u32,
    initial_liability_weight: u32,
    maintenance_liability_weight: u32,
    imf_factor: u32,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    active_status: bool,
    asset_tier: AssetTier,
    scale_initial_asset_weight_start: u64,
    withdraw_guard_threshold: u64,
    order_tick_size: u64,
    order_step_size: u64,
    if_total_factor: u32,
    name: [u8; 32],
}

/// What the accounts and the clock supply that the settings do not.
struct SpotMarketSeed {
    market_index: u16,
    decimals: u32,
    now: u64,
    token_program_flag: u8,
    historical_oracle_data: HistoricalOracleData,
    historical_index_data: HistoricalIndexData,
}

#[allow(clippy::too_many_arguments)]
pub fn handle_initialize_spot_market(
    ctx: Context<InitializeSpotMarket>,
    optimal_utilization: u32,
    optimal_borrow_rate: u32,
    max_borrow_rate: u32,
    oracle_source: OracleSource,
    initial_asset_weight: u32,
    maintenance_asset_weight: u32,
    initial_liability_weight: u32,
    maintenance_liability_weight: u32,
    imf_factor: u32,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    active_status: bool,
    asset_tier: AssetTier,
    scale_initial_asset_weight_start: u64,
    withdraw_guard_threshold: u64,
    order_tick_size: u64,
    order_step_size: u64,
    if_total_factor: u32,
    name: [u8; 32],
) -> Result<()> {
    initialize_spot_market(
        ctx,
        NewSpotMarket {
            optimal_utilization,
            optimal_borrow_rate,
            max_borrow_rate,
            oracle_source,
            initial_asset_weight,
            maintenance_asset_weight,
            initial_liability_weight,
            maintenance_liability_weight,
            imf_factor,
            liquidator_fee,
            if_liquidation_fee,
            active_status,
            asset_tier,
            scale_initial_asset_weight_start,
            withdraw_guard_threshold,
            order_tick_size,
            order_step_size,
            if_total_factor,
            name,
        },
    )
}

fn initialize_spot_market(ctx: Context<InitializeSpotMarket>, params: NewSpotMarket) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    let spot_market_pubkey = ctx.accounts.spot_market.key();

    validate_supported_market_oracle_source(params.oracle_source)?;

    initialize_spot_market_vaults(ctx.accounts)?;

    validate_borrow_rate(
        params.optimal_utilization,
        params.optimal_borrow_rate,
        params.max_borrow_rate,
        0,
    )?;

    let spot_market_index = get_then_update_id!(state, number_of_spot_markets);

    msg!("initializing spot market {}", spot_market_index);

    validate!(
        !name_is_reserved_quote(&params.name) || spot_market_index == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::ReservedSpotMarketName,
        "reserved quote name (USDT) may only be used by spot market {}",
        QUOTE_SPOT_MARKET_INDEX
    )?;

    let (oracle_price_data, history) =
        spot_market_oracle_start(ctx.accounts, &params, spot_market_index)?;

    validate_margin_weights(
        spot_market_index,
        params.initial_asset_weight,
        params.maintenance_asset_weight,
        params.initial_liability_weight,
        params.maintenance_liability_weight,
        params.imf_factor,
    )?;

    let spot_market = &mut ctx.accounts.spot_market.load_init()?;

    let seed = spot_market_seed(
        ctx.accounts,
        &params,
        spot_market_index,
        history,
        oracle_price_data,
    )?;

    if params.active_status {
        validate!(
            ctx.accounts.admin.key() == state.cold_admin,
            ErrorCode::DefaultError,
            "admin must be state admin"
        )?;
    }

    **spot_market = new_spot_market(&params, ctx.accounts, seed, spot_market_pubkey);

    Ok(())
}

/// Reads the oracle a new spot market will use, and the history it starts from.
///
/// A quote market names no oracle at all, so the two cases are checked apart.
/// The price is returned as it was read, because the caller decides whether an
/// unreadable one is fatal.
fn spot_market_oracle_start(
    accounts: &InitializeSpotMarket,
    params: &NewSpotMarket,
    spot_market_index: u16,
) -> Result<(
    crate::error::VelocityResult<OraclePriceData>,
    (HistoricalOracleData, HistoricalIndexData),
)> {
    if params.oracle_source == OracleSource::QuoteAsset {
        // catches inconsistent parameters
        validate!(
            accounts.oracle.key == &Pubkey::default(),
            ErrorCode::InvalidSpotMarketInitialization,
            "For OracleSource::QuoteAsset, oracle must be default public key"
        )?;
    } else {
        OracleMap::validate_oracle_account_info(&accounts.oracle)?;
    }

    let oracle_price_data = get_oracle_price(
        &params.oracle_source,
        &accounts.oracle,
        Clock::get()?.unix_timestamp.cast()?,
    );

    let history =
        spot_market_oracle_history(accounts, params, spot_market_index, oracle_price_data)?;

    Ok((oracle_price_data, history))
}

/// Collects what the accounts and the clock supply to a new spot market.
fn spot_market_seed(
    accounts: &InitializeSpotMarket,
    params: &NewSpotMarket,
    market_index: u16,
    history: (HistoricalOracleData, HistoricalIndexData),
    oracle_price_data: crate::error::VelocityResult<OraclePriceData>,
) -> Result<SpotMarketSeed> {
    let clock = Clock::get()?;
    let now = clock
        .unix_timestamp
        .cast()
        .or(Err(ErrorCode::UnableToCastUnixTime))?;

    let decimals = accounts.spot_market_mint.decimals.cast::<u32>()?;

    validate_withdraw_guard_threshold(
        params.withdraw_guard_threshold,
        decimals,
        oracle_price_data?.price,
    )?;

    let (historical_oracle_data, historical_index_data) = history;

    Ok(SpotMarketSeed {
        market_index,
        decimals,
        now,
        token_program_flag: spot_market_token_program_flag(accounts)?,
        historical_oracle_data,
        historical_index_data,
    })
}

/// Creates the market vault and the insurance fund vault.
///
/// A Token-2022 mint gets the immutable owner extension first. The extension
/// must exist before the account holds a balance.
fn initialize_spot_market_vaults(accounts: &InitializeSpotMarket) -> Result<()> {
    let is_token_2022 = *accounts.spot_market_mint.to_account_info().owner == Token2022::id();
    if is_token_2022 {
        initialize_immutable_owner(&accounts.token_program, &accounts.spot_market_vault)?;

        initialize_immutable_owner(&accounts.token_program, &accounts.insurance_fund_vault)?;
    }

    initialize_token_account(
        &accounts.token_program,
        &accounts.spot_market_vault,
        &accounts.velocity_signer,
        &accounts.spot_market_mint,
    )?;

    initialize_token_account(
        &accounts.token_program,
        &accounts.insurance_fund_vault,
        &accounts.velocity_signer,
        &accounts.spot_market_mint,
    )
}

/// Seeds the oracle history a new market starts from.
///
/// The quote market has no oracle, so it starts from the fixed quote history
/// and the handler holds it to that shape. Every other market must have a
/// readable price, because the history it starts from is that price.
fn spot_market_oracle_history(
    accounts: &InitializeSpotMarket,
    params: &NewSpotMarket,
    spot_market_index: u16,
    oracle_price_data: crate::error::VelocityResult<OraclePriceData>,
) -> Result<(HistoricalOracleData, HistoricalIndexData)> {
    if spot_market_index == QUOTE_SPOT_MARKET_INDEX {
        validate!(
            accounts.oracle.key == &Pubkey::default(),
            ErrorCode::InvalidSpotMarketInitialization,
            "For quote asset spot market, oracle must be default public key"
        )?;

        validate!(
            params.oracle_source == OracleSource::QuoteAsset,
            ErrorCode::InvalidSpotMarketInitialization,
            "For quote asset spot market, oracle source must be QuoteAsset"
        )?;

        validate!(
            accounts.spot_market_mint.decimals == 6,
            ErrorCode::InvalidSpotMarketInitialization,
            "For quote asset spot market, mint decimals must be 6"
        )?;

        return Ok((
            HistoricalOracleData::default_quote_oracle(),
            HistoricalIndexData::default_quote_oracle(),
        ));
    }

    validate!(
        accounts.spot_market_mint.decimals >= 5,
        ErrorCode::InvalidSpotMarketInitialization,
        "Mint decimals must be greater than or equal to 5"
    )?;

    validate!(
        oracle_price_data.is_ok(),
        ErrorCode::InvalidSpotMarketInitialization,
        "Unable to read oracle price for {}",
        accounts.oracle.key,
    )?;

    Ok((
        HistoricalOracleData::default_with_current_oracle(
            oracle_price_data?,
            Clock::get()?.unix_timestamp,
        ),
        HistoricalIndexData::default_with_current_oracle(oracle_price_data?)?,
    ))
}

/// Records which token program owns the mint, and whether the mint runs a
/// transfer hook. Every transfer the market makes must pass the hook the
/// accounts it needs, so the market stores the answer once.
fn spot_market_token_program_flag(accounts: &InitializeSpotMarket) -> Result<u8> {
    let mut token_program = 0_u8;
    if accounts.token_program.key() == Token2022::id() {
        token_program |= TokenProgramFlag::Token2022 as u8;
    }

    let mint_account_info = accounts.spot_market_mint.to_account_info();
    let mint_data = mint_account_info.try_borrow_data()?;
    let mint_with_extension = StateWithExtensions::<MintInner>::unpack(&mint_data)?;
    if let Ok(transfer_hook) = mint_with_extension.get_extension::<TransferHook>() {
        let transfer_hook_program_id: Option<Pubkey> = transfer_hook.program_id.into();
        if transfer_hook_program_id.is_some() {
            token_program |= TokenProgramFlag::TransferHook as u8;
        }
    }

    Ok(token_program)
}

/// A spot market at birth.
///
/// Every field is named, because a zero-copy account has no default to fall
/// back on.
fn new_spot_market(
    params: &NewSpotMarket,
    accounts: &InitializeSpotMarket,
    seed: SpotMarketSeed,
    spot_market_pubkey: Pubkey,
) -> SpotMarket {
    let SpotMarketSeed {
        market_index,
        decimals,
        now,
        token_program_flag,
        historical_oracle_data,
        historical_index_data,
    } = seed;

    SpotMarket {
        market_index,
        pubkey: spot_market_pubkey,
        status: if params.active_status {
            MarketStatus::Active
        } else {
            MarketStatus::Initialized
        },
        name: params.name,
        asset_tier: params.asset_tier,
        expiry_ts: 0,
        oracle: accounts.oracle.key(),
        oracle_source: params.oracle_source,
        historical_oracle_data,
        historical_index_data,
        mint: accounts.spot_market_mint.key(),
        vault: accounts.spot_market_vault.key(),
        revenue_pool: PoolBalance {
            scaled_balance: 0,
            market_index,
            ..PoolBalance::default()
        }, // in base asset
        decimals,
        optimal_utilization: params.optimal_utilization,
        optimal_borrow_rate: params.optimal_borrow_rate,
        max_borrow_rate: params.max_borrow_rate,
        deposit_balance: 0,
        borrow_balance: 0,
        max_token_deposits: 0,
        deposit_token_twap: 0,
        borrow_token_twap: 0,
        utilization_twap: 0,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        total_social_loss: 0,
        total_quote_social_loss: 0,
        last_interest_ts: now,
        last_twap_ts: now,
        initial_asset_weight: params.initial_asset_weight,
        maintenance_asset_weight: params.maintenance_asset_weight,
        initial_liability_weight: params.initial_liability_weight,
        maintenance_liability_weight: params.maintenance_liability_weight,
        imf_factor: params.imf_factor,
        liquidator_fee: params.liquidator_fee,
        if_liquidation_fee: params.if_liquidation_fee, // 1%
        withdraw_guard_threshold: params.withdraw_guard_threshold,
        order_step_size: params.order_step_size,
        order_tick_size: params.order_tick_size,
        min_order_size: params.order_step_size,
        max_position_size: 0,
        next_fill_record_id: 1,
        next_deposit_record_id: 1,
        padding_former_spot_fee_pool: [0; 32],
        total_spot_fee: 0,
        orders_enabled: market_index != 0,
        paused_operations: 0,
        if_paused_operations: 0,
        fee_adjustment: 0,
        max_token_borrows_fraction: 0,
        flash_loan_amount: 0,
        flash_loan_initial_token_amount: 0,
        total_swap_fee: 0,
        scale_initial_asset_weight_start: params.scale_initial_asset_weight_start,
        min_borrow_rate: 0,
        token_program_flag,
        pool_id: 0,
        _padding_align_pfp: 0,
        protocol_fee_pool: PoolBalance {
            scaled_balance: 0,
            market_index,
            ..PoolBalance::default()
        },
        protocol_liquidation_fee: 0,
        protocol_fee_factor: 0,
        if_last_settle_vault_amount: 0,
        _padding_future: [0; 256],
        deposit_guard_threshold: 0,
        withdraw_circuit_breaker_bps: 0, // 0 => default 25%
        max_deposit_bps_per_day: 0,      // disabled
        insurance_fund: InsuranceFund {
            vault: accounts.insurance_fund_vault.key(),
            unstaking_period: THIRTEEN_DAY,
            if_fee_factor: params.if_total_factor,
            revenue_settle_period: 3600,
            ..InsuranceFund::default()
        },
    }
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_pool_id(
    ctx: Context<AdminUpdateSpotMarket>,
    pool_id: u8,
) -> Result<()> {
    let mut spot_market = load_mut!(ctx.accounts.spot_market)?;
    msg!(
        "updating spot market {} pool id to {}",
        spot_market.market_index,
        pool_id
    );

    validate!(
        spot_market.status == MarketStatus::Initialized,
        ErrorCode::DefaultError,
        "Market must be just initialized to update pool"
    )?;

    spot_market.pool_id = pool_id;

    Ok(())
}

pub fn handle_delete_initialized_spot_market(
    ctx: Context<DeleteInitializedSpotMarket>,
    market_index: u16,
) -> Result<()> {
    let spot_market = ctx.accounts.spot_market.load()?;
    msg!("spot market {}", spot_market.market_index);
    let mut state = ctx.accounts.state.load_mut()?;

    validate_spot_market_deletable(&spot_market, &state, market_index)?;

    safe_decrement!(state.number_of_spot_markets, 1);

    drop(spot_market);

    close_empty_spot_vault(
        ctx.accounts,
        &ctx.accounts.spot_market_vault,
        state.signer_nonce,
    )?;

    close_empty_spot_vault(
        ctx.accounts,
        &ctx.accounts.insurance_fund_vault,
        state.signer_nonce,
    )
}

/// Holds a deletion to the last market that never went live.
///
/// The protocol indexes a market by its position, so only the last one can go.
/// A market that took a deposit or a borrow holds user value, so it stays.
fn validate_spot_market_deletable(
    spot_market: &SpotMarket,
    state: &State,
    market_index: u16,
) -> Result<()> {
    validate!(
        state.number_of_spot_markets - 1 == market_index,
        ErrorCode::InvalidMarketAccountforDeletion,
        "state.number_of_spot_markets={} != market_index={}",
        state.number_of_markets,
        market_index
    )?;
    validate!(
        spot_market.status == MarketStatus::Initialized,
        ErrorCode::InvalidMarketAccountforDeletion,
        "spot_market.status != Initialized",
    )?;
    validate!(
        spot_market.deposit_balance == 0,
        ErrorCode::InvalidMarketAccountforDeletion,
        "spot_market.number_of_users={} != 0",
        spot_market.deposit_balance,
    )?;
    validate!(
        spot_market.borrow_balance == 0,
        ErrorCode::InvalidMarketAccountforDeletion,
        "spot_market.borrow_balance={} != 0",
        spot_market.borrow_balance,
    )?;
    validate!(
        spot_market.market_index == market_index,
        ErrorCode::InvalidMarketAccountforDeletion,
        "market_index={} != spot_market.market_index={}",
        market_index,
        spot_market.market_index
    )?;

    Ok(())
}

/// Closes one vault of a deleted market and refunds the rent to the admin. The
/// token program refuses to close a vault that still holds a balance, so the
/// handler checks the balance first and names the vault in the error.
fn close_empty_spot_vault<'info>(
    accounts: &DeleteInitializedSpotMarket<'info>,
    vault: &InterfaceAccount<'info, TokenAccount>,
    signer_nonce: u8,
) -> Result<()> {
    validate!(
        vault.amount == 0,
        ErrorCode::InvalidMarketAccountforDeletion,
        "vault {} still holds {}",
        vault.key(),
        vault.amount
    )?;

    close_vault(
        &accounts.token_program,
        vault,
        &accounts.admin.to_account_info(),
        &accounts.velocity_signer,
        signer_nonce,
    )
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_name(
    ctx: Context<AdminUpdateSpotMarket>,
    name: [u8; 32],
) -> Result<()> {
    let mut spot_market = load_mut!(ctx.accounts.spot_market)?;
    validate!(
        !name_is_reserved_quote(&name) || spot_market.market_index == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::ReservedSpotMarketName,
        "reserved quote name (USDT) may only be used by spot market {}",
        QUOTE_SPOT_MARKET_INDEX
    )?;
    msg!("spot_market.name: {:?} -> {:?}", spot_market.name, name);
    spot_market.name = name;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_status(
    ctx: Context<AdminUpdateSpotMarket>,
    status: MarketStatus,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.status: {:?} -> {:?}",
        spot_market.status,
        status
    );

    spot_market.status = status;
    Ok(())
}

#[access_control(
spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_paused_operations(
    ctx: Context<PauseAdminUpdateSpotMarket>,
    paused_operations: u8,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    let signer = ctx.accounts.admin.key();
    let state = ctx.accounts.state.load()?;
    require_pause_only_added(
        &signer,
        &state,
        spot_market.paused_operations,
        paused_operations,
    )?;
    drop(state);

    spot_market.paused_operations = paused_operations;

    SpotOperation::log_all_operations_paused(spot_market.paused_operations);

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_asset_tier(
    ctx: Context<AdminUpdateSpotMarket>,
    asset_tier: AssetTier,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    if spot_market.initial_asset_weight > 0 {
        validate!(
            matches!(asset_tier, AssetTier::Collateral | AssetTier::Protected),
            ErrorCode::DefaultError,
            "initial_asset_weight > 0 so AssetTier must be collateral or protected"
        )?;
    }

    msg!(
        "spot_market.asset_tier: {:?} -> {:?}",
        spot_market.asset_tier,
        asset_tier
    );

    spot_market.asset_tier = asset_tier;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_orders_enabled(
    ctx: Context<AdminUpdateSpotMarket>,
    orders_enabled: bool,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.orders_enabled: {:?} -> {:?}",
        spot_market.orders_enabled,
        orders_enabled
    );

    spot_market.orders_enabled = orders_enabled;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_step_size_and_tick_size(
    ctx: Context<AdminUpdateSpotMarket>,
    step_size: u64,
    tick_size: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate!(
        spot_market.market_index == 0 || step_size > 0 && tick_size > 0,
        ErrorCode::DefaultError
    )?;

    msg!(
        "spot_market.order_step_size: {:?} -> {:?}",
        spot_market.order_step_size,
        step_size
    );

    msg!(
        "spot_market.order_tick_size: {:?} -> {:?}",
        spot_market.order_tick_size,
        tick_size
    );

    spot_market.order_step_size = step_size;
    spot_market.order_tick_size = tick_size;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_min_order_size(
    ctx: Context<AdminUpdateSpotMarket>,
    order_size: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate!(
        spot_market.market_index == 0 || order_size > 0,
        ErrorCode::DefaultError
    )?;

    msg!(
        "spot_market.min_order_size: {:?} -> {:?}",
        spot_market.min_order_size,
        order_size
    );

    spot_market.min_order_size = order_size;
    Ok(())
}

#[derive(Accounts)]
pub struct InitializeSpotMarket<'info> {
    #[account(
        init,
        seeds = [b"spot_market", state.load()?.number_of_spot_markets.to_le_bytes().as_ref()],
        space = SpotMarket::SIZE,
        bump,
        payer = admin
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mint::token_program = token_program,
    )]
    pub spot_market_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        init,
        seeds = [b"spot_market_vault".as_ref(), state.load()?.number_of_spot_markets.to_le_bytes().as_ref()],
        bump,
        payer = admin,
        space = get_vault_len(&spot_market_mint)?,
        owner = token_program.key()
    )]
    /// CHECK: checked in `initialize_spot_market`
    pub spot_market_vault: AccountInfo<'info>,
    #[account(
        init,
        seeds = [b"insurance_fund_vault".as_ref(), state.load()?.number_of_spot_markets.to_le_bytes().as_ref()],
        bump,
        payer = admin,
        space = get_vault_len(&spot_market_mint)?,
        owner = token_program.key()
    )]
    /// CHECK: checked in `initialize_spot_market`
    pub insurance_fund_vault: AccountInfo<'info>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: program signer
    pub velocity_signer: UncheckedAccount<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    /// CHECK: checked in `initialize_spot_market`
    pub oracle: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = check_warm(&admin.key(), &state)?
    )]
    pub admin: Signer<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct DeleteInitializedSpotMarket<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    #[account(mut, close = admin)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
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
    /// CHECK: program signer
    pub velocity_signer: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[cfg(test)]
mod reserved_quote_name_tests {
    //! The "USDT" name is reserved to spot market 0 so the monitoring
    //! stablecoin-exemption (keyed off decodeName) can't be inherited by any
    //! other market. These lock the trim semantics (must be at least as
    //! aggressive as off-chain `decodeName().trim()`).
    use super::name_is_reserved_quote;

    fn padded(s: &str) -> [u8; 32] {
        let mut n = [b' '; 32];
        n[..s.len()].copy_from_slice(s.as_bytes());
        n
    }

    #[test]
    fn reserved_variants_match() {
        // exact + the SDK's space padding
        assert!(name_is_reserved_quote(&padded("USDT")));
        // leading/trailing whitespace + NUL padding all still decode to USDT
        assert!(name_is_reserved_quote(&padded("  USDT")));
        let mut nul = [0u8; 32];
        nul[..4].copy_from_slice(b"USDT");
        assert!(name_is_reserved_quote(&nul));
        let mut mixed = [0u8; 32];
        mixed[..6].copy_from_slice(b"\tUSDT\n");
        assert!(name_is_reserved_quote(&mixed));
    }

    #[test]
    fn unicode_whitespace_padding_is_reserved() {
        // JS trim() strips these, so they decode to "USDT" off-chain and must
        // be reserved on-chain: NBSP, ogham space, en quad, line/paragraph
        // separators, narrow NBSP, medium math space, ideographic space, BOM
        for pad in [
            "\u{00a0}", "\u{1680}", "\u{2000}", "\u{200a}", "\u{2028}", "\u{2029}", "\u{202f}",
            "\u{205f}", "\u{3000}", "\u{feff}",
        ] {
            let s = format!("{pad}USDT{pad}");
            assert!(
                name_is_reserved_quote(&padded(&s)),
                "{:?} padding not reserved",
                pad
            );
        }
    }

    #[test]
    fn non_reserved_names_pass() {
        // devnet stable, other stable, and volatile tokens must NOT be reserved
        assert!(!name_is_reserved_quote(&padded("dUSDT")));
        assert!(!name_is_reserved_quote(&padded("USDC")));
        assert!(!name_is_reserved_quote(&padded("SOL")));
        assert!(!name_is_reserved_quote(&padded("USDT.e")));
        assert!(!name_is_reserved_quote(&padded("USD")));
        assert!(!name_is_reserved_quote(&[b' '; 32])); // all blank
                                                       // ZWSP is not trimmed by JS trim(), so "\u{200b}USDT" does not decode
                                                       // to "USDT" off-chain and must not be reserved
        assert!(!name_is_reserved_quote(&padded("\u{200b}USDT")));
        // invalid UTF-8 decodes with U+FFFD, which trim() keeps
        let mut invalid = [b' '; 32];
        invalid[..5].copy_from_slice(&[0xff, b'U', b'S', b'D', b'T']);
        assert!(!name_is_reserved_quote(&invalid));
    }
}
