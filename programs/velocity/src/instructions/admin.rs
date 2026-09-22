use {
    crate::{
        auth::{check_cold, check_hot, check_pause, check_warm, require_pause_only_added},
        controller::{
            self,
            token::{close_vault, initialize_immutable_owner, initialize_token_account},
        },
        error::ErrorCode,
        get_then_update_id,
        instructions::{constraints::*, optional_accounts::load_maps},
        load_mut,
        math::{
            self, bn,
            casting::Cast,
            constants::{
                BANKRUPTCY_IF_FLOOR_DISABLED, BPS_PRECISION, DEFAULT_BANKRUPTCY_IF_FLOOR_PCT,
                DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO, FEE_ADJUSTMENT_MAX,
                FEE_POOL_TO_REVENUE_POOL_THRESHOLD, IF_FACTOR_PRECISION, INSURANCE_A_MAX,
                INSURANCE_B_MAX, INSURANCE_C_MAX, INSURANCE_SPECULATIVE_MAX,
                LIQUIDATION_FEE_PRECISION, MAX_CONCENTRATION_COEFFICIENT,
                MAX_TAKER_FEE_ADDON_TENTH_BPS, MM_ORACLE_MAX_SOURCE_AGE,
                MM_ORACLE_MAX_STEP_PCT_PRECISION, MM_ORACLE_MIN_WRITE_GAP, PERCENTAGE_PRECISION,
                PERCENTAGE_PRECISION_I128, PERCENTAGE_PRECISION_I64, PERCENTAGE_PRECISION_U32,
                PERP_FEE_TIER_MAX_INDEX, QUOTE_PRECISION_I64, QUOTE_SPOT_MARKET_INDEX,
                SPOT_BALANCE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_IMF_PRECISION,
                SPOT_WEIGHT_PRECISION, THIRTEEN_DAY,
            },
            margin::calculate_user_equity,
            orders::is_multiple_of_step_size,
            safe_math::SafeMath,
            spot_balance::get_token_amount,
            spot_withdraw::{
                validate_spot_market_vault_amount, DEFAULT_WITHDRAW_CIRCUIT_BREAKER_BPS,
            },
            time::{
                legacy_slot_duration_i64_raw, legacy_slot_duration_u8,
                slot_duration_transition_index, SlotClock, SLOT_DURATION_TRANSITION_MS,
            },
        },
        math_error, msg,
        optional_accounts::get_token_mint,
        safe_decrement, safe_increment,
        state::{
            events::{
                emit_accelerated_referral_status_changed, AcceleratedReferralStatusChange,
                DepositDirection, DepositExplanation, DepositRecord, SpotMarketVaultDepositRecord,
            },
            market_status::MarketStatus,
            oracle::{
                get_oracle_price, get_prelaunch_price, get_pyth_price, HistoricalIndexData,
                HistoricalOracleData, OraclePriceData, OracleSource, PrelaunchOracle,
                PrelaunchOracleParams, StrictOraclePrice,
            },
            oracle_map::OracleMap,
            paused_operations::{InsuranceFundOperation, PerpOperation, SpotOperation},
            perp_market::{
                ContractTier, ContractType, FeeLedger, HedgeConfig, InsuranceClaim,
                MarketConfigFlag, MarketStats, PerpMarket, PoolBalance, AMM,
            },
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            pyth_lazer_oracle::{PythLazerOracle, PYTH_LAZER_ORACLE_SEED},
            spot_market::{
                AssetTier, InsuranceFund, SpotBalanceType, SpotMarket, TokenProgramFlag,
            },
            spot_market_map::get_writable_spot_market_set,
            state::{
                ExchangeStatus, FeeStructure, HotRole, LpPoolFeatureBitFlags, OracleGuardRails,
                SolvencyStatus, State, TransactionFeeRails,
            },
            traits::Size,
            user::{MarketType, SpecialUserStatus, User, UserStats},
            user_map::load_user_map,
        },
        validate,
        validation::{
            fee_structure::validate_fee_structure,
            margin::{validate_margin, validate_margin_weights},
            spot_market::{validate_borrow_rate, validate_withdraw_guard_threshold},
        },
        vlp::{
            amm::math::amm,
            amm_cache::{AmmCache, AMM_POSITIONS_CACHE},
        },
        FeatureBitFlags,
    },
    anchor_lang::{prelude::*, Discriminator},
    anchor_spl::{
        token_2022::{
            spl_token_2022::{
                extension::{
                    transfer_hook::TransferHook, BaseStateWithExtensions, StateWithExtensions,
                },
                state::Mint as MintInner,
            },
            Token2022,
        },
        token_interface::{Mint, TokenAccount, TokenInterface},
    },
    std::convert::TryInto,
};

fn validate_supported_market_oracle_source(oracle_source: OracleSource) -> Result<()> {
    if matches!(
        oracle_source,
        OracleSource::PythPull
            | OracleSource::Pyth1KPull
            | OracleSource::Pyth1MPull
            | OracleSource::PythStableCoinPull
    ) {
        return Err(ErrorCode::InvalidOracle.into());
    }

    Ok(())
}

pub fn handle_initialize(ctx: Context<Initialize>) -> Result<()> {
    let (velocity_signer, velocity_signer_nonce) =
        Pubkey::find_program_address(&[b"velocity_signer".as_ref()], ctx.program_id);

    // Default warm_admin to the cold admin so warm-tier handlers work
    // immediately. All hot roles start as `Pubkey::default()` (unassigned)
    // until rotated via `update_hot_admin`.
    let mut state = ctx.accounts.state.load_init()?;
    *state = State {
        cold_admin: *ctx.accounts.admin.key,
        warm_admin: *ctx.accounts.admin.key,
        pause_admin: Pubkey::default(),
        hot_amm_crank: Pubkey::default(),
        hot_lp_cache: Pubkey::default(),
        hot_lp_swap: Pubkey::default(),
        hot_lp_settle: Pubkey::default(),
        hot_feature_flag: Pubkey::default(),
        hot_fuel: Pubkey::default(),
        hot_user_flag: Pubkey::default(),
        hot_vault_deposit: Pubkey::default(),
        hot_mm_oracle_crank: Pubkey::default(),
        hot_amm_spread_adjust: Pubkey::default(),
        hot_vamm_quote_management: Pubkey::default(),
        exchange_status: ExchangeStatus::active(),
        whitelist_mint: Pubkey::default(),
        discount_mint: Pubkey::default(),
        oracle_guard_rails: OracleGuardRails::default(),
        number_of_authorities: 0,
        number_of_sub_accounts: 0,
        number_of_markets: 0,
        number_of_spot_markets: 0,
        min_perp_auction_duration: legacy_slot_duration_u8(10),
        default_market_order_time_in_force: 60,
        default_spot_auction_duration: 10,
        liquidation_margin_buffer_ratio: DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO,
        settlement_duration: 0, // extra duration after market expiry to allow settlement
        signer: velocity_signer,
        signer_nonce: velocity_signer_nonce,
        srm_vault: Pubkey::default(),
        protocol_fee_recipient_perp: Pubkey::default(),
        hot_fee_withdraw: Pubkey::default(),
        hot_account_extension: Pubkey::default(),
        protocol_fee_recipient_spot: Pubkey::default(),
        perp_fee_structure: FeeStructure::perps_default(),
        spot_fee_structure: FeeStructure::spot_default(),
        liquidation_duration: legacy_slot_duration_u8(0),
        initial_pct_to_liquidate: 0,
        max_number_of_sub_accounts: 0,
        max_initialize_user_fee: 0,
        feature_bit_flags: 0,
        lp_pool_feature_bit_flags: 0,
        solvency_status: SolvencyStatus::active(),
        promo_fee_tier: 0,
        slot_duration_ms: 0,
        pending_slot_duration_ms: 0,
        slot_duration_pad: [0; 2],
        slot_duration_effective_slot: 0,
        slot_duration_transition_slots: [0; 4],
        hot_flow_authority: Pubkey::default(),
        transaction_fee_rails: TransactionFeeRails::FLAT_PER_SIGNATURE,
        // Five percent of a liquidation's value is the ceiling on what the
        // protocol spends to get it cranked. The field's docs give the detail.
        // The value stays low so a keeper that also builds the block cannot
        // bill much of the recovery back as its own priority fee.
        liquidation_crank_reimbursement_bps: 500,
        // The admin sets this once a SOL spot market exists. Until then the
        // liquidation crank pays its flat figure and nothing more.
        sol_spot_market_index: 0,
        padding_0: [0; 2],
        padding: [0; 110],
    };

    Ok(())
}

/// Names reserved to the quote spot market, index 0. Monitoring keys the
/// deposit-concentration stablecoin exemption off the decoded market name.
/// A second market named "USDT" would inherit that exemption and hide a
/// TVL concentration, so the name is reserved onchain to index 0.
const RESERVED_QUOTE_NAMES: &[&[u8]] = &[b"USDT"];

/// True if `name` decodes to a reserved quote name after trimming. The trim
/// is a superset of offchain `decodeName().trim()`: `char::is_whitespace`
/// plus U+FEFF and NUL, since JavaScript trims U+FEFF and Rust does not.
/// Invalid UTF-8 never matches, since it offchain decodes to U+FFFD instead.
fn name_is_reserved_quote(name: &[u8; 32]) -> bool {
    let is_trim = |c: char| c.is_whitespace() || c == '\0' || c == '\u{feff}';
    match core::str::from_utf8(name) {
        Ok(s) => RESERVED_QUOTE_NAMES.contains(&s.trim_matches(is_trim).as_bytes()),
        Err(_) => false,
    }
}

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
    let mut state = ctx.accounts.state.load_mut()?;
    let spot_market_pubkey = ctx.accounts.spot_market.key();

    validate_supported_market_oracle_source(oracle_source)?;

    let is_token_2022 = *ctx.accounts.spot_market_mint.to_account_info().owner == Token2022::id();
    if is_token_2022 {
        initialize_immutable_owner(&ctx.accounts.token_program, &ctx.accounts.spot_market_vault)?;

        initialize_immutable_owner(
            &ctx.accounts.token_program,
            &ctx.accounts.insurance_fund_vault,
        )?;
    }

    initialize_token_account(
        &ctx.accounts.token_program,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.velocity_signer,
        &ctx.accounts.spot_market_mint,
    )?;

    initialize_token_account(
        &ctx.accounts.token_program,
        &ctx.accounts.insurance_fund_vault,
        &ctx.accounts.velocity_signer,
        &ctx.accounts.spot_market_mint,
    )?;

    validate_borrow_rate(optimal_utilization, optimal_borrow_rate, max_borrow_rate, 0)?;

    let spot_market_index = get_then_update_id!(state, number_of_spot_markets);

    msg!("initializing spot market {}", spot_market_index);

    validate!(
        !name_is_reserved_quote(&name) || spot_market_index == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::ReservedSpotMarketName,
        "reserved quote name (USDT) may only be used by spot market {}",
        QUOTE_SPOT_MARKET_INDEX
    )?;

    if oracle_source == OracleSource::QuoteAsset {
        // catches inconsistent parameters
        validate!(
            ctx.accounts.oracle.key == &Pubkey::default(),
            ErrorCode::InvalidSpotMarketInitialization,
            "For OracleSource::QuoteAsset, oracle must be default public key"
        )?;
    } else {
        OracleMap::validate_oracle_account_info(&ctx.accounts.oracle)?;
    }

    let oracle_price_data = get_oracle_price(
        &oracle_source,
        &ctx.accounts.oracle,
        Clock::get()?.unix_timestamp.cast()?,
    );

    let (historical_oracle_data_default, historical_index_data_default) =
        if spot_market_index == QUOTE_SPOT_MARKET_INDEX {
            validate!(
                ctx.accounts.oracle.key == &Pubkey::default(),
                ErrorCode::InvalidSpotMarketInitialization,
                "For quote asset spot market, oracle must be default public key"
            )?;

            validate!(
                oracle_source == OracleSource::QuoteAsset,
                ErrorCode::InvalidSpotMarketInitialization,
                "For quote asset spot market, oracle source must be QuoteAsset"
            )?;

            validate!(
                ctx.accounts.spot_market_mint.decimals == 6,
                ErrorCode::InvalidSpotMarketInitialization,
                "For quote asset spot market, mint decimals must be 6"
            )?;

            (
                HistoricalOracleData::default_quote_oracle(),
                HistoricalIndexData::default_quote_oracle(),
            )
        } else {
            validate!(
                ctx.accounts.spot_market_mint.decimals >= 5,
                ErrorCode::InvalidSpotMarketInitialization,
                "Mint decimals must be greater than or equal to 5"
            )?;

            validate!(
                oracle_price_data.is_ok(),
                ErrorCode::InvalidSpotMarketInitialization,
                "Unable to read oracle price for {}",
                ctx.accounts.oracle.key,
            )?;

            (
                HistoricalOracleData::default_with_current_oracle(
                    oracle_price_data?,
                    Clock::get()?.unix_timestamp,
                ),
                HistoricalIndexData::default_with_current_oracle(oracle_price_data?)?,
            )
        };

    validate_margin_weights(
        spot_market_index,
        initial_asset_weight,
        maintenance_asset_weight,
        initial_liability_weight,
        maintenance_liability_weight,
        imf_factor,
    )?;

    let spot_market = &mut ctx.accounts.spot_market.load_init()?;
    let clock = Clock::get()?;
    let now = clock
        .unix_timestamp
        .cast()
        .or(Err(ErrorCode::UnableToCastUnixTime))?;

    let decimals = ctx.accounts.spot_market_mint.decimals.cast::<u32>()?;

    validate_withdraw_guard_threshold(
        withdraw_guard_threshold,
        decimals,
        oracle_price_data?.price,
    )?;

    let mut token_program = 0_u8;
    if ctx.accounts.token_program.key() == Token2022::id() {
        token_program |= TokenProgramFlag::Token2022 as u8;
    }

    let mint_account_info = ctx.accounts.spot_market_mint.to_account_info();
    let mint_data = mint_account_info.try_borrow_data()?;
    let mint_with_extension = StateWithExtensions::<MintInner>::unpack(&mint_data)?;
    if let Ok(transfer_hook) = mint_with_extension.get_extension::<TransferHook>() {
        let transfer_hook_program_id: Option<Pubkey> = transfer_hook.program_id.into();
        if transfer_hook_program_id.is_some() {
            token_program |= TokenProgramFlag::TransferHook as u8;
        }
    }

    if active_status {
        validate!(
            ctx.accounts.admin.key() == state.cold_admin,
            ErrorCode::DefaultError,
            "admin must be state admin"
        )?;
    }

    **spot_market = SpotMarket {
        market_index: spot_market_index,
        pubkey: spot_market_pubkey,
        status: if active_status {
            MarketStatus::Active
        } else {
            MarketStatus::Initialized
        },
        name,
        asset_tier,
        expiry_ts: 0,
        oracle: ctx.accounts.oracle.key(),
        oracle_source,
        historical_oracle_data: historical_oracle_data_default,
        historical_index_data: historical_index_data_default,
        mint: ctx.accounts.spot_market_mint.key(),
        vault: ctx.accounts.spot_market_vault.key(),
        revenue_pool: PoolBalance {
            scaled_balance: 0,
            market_index: spot_market_index,
            ..PoolBalance::default()
        }, // in base asset
        decimals,
        optimal_utilization,
        optimal_borrow_rate,
        max_borrow_rate,
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
        initial_asset_weight,
        maintenance_asset_weight,
        initial_liability_weight,
        maintenance_liability_weight,
        imf_factor,
        liquidator_fee,
        if_liquidation_fee, // 1%
        withdraw_guard_threshold,
        order_step_size,
        order_tick_size,
        min_order_size: order_step_size,
        max_position_size: 0,
        next_fill_record_id: 1,
        next_deposit_record_id: 1,
        padding_former_spot_fee_pool: [0; 32],
        total_spot_fee: 0,
        orders_enabled: spot_market_index != 0,
        paused_operations: 0,
        if_paused_operations: 0,
        fee_adjustment: 0,
        max_token_borrows_fraction: 0,
        flash_loan_amount: 0,
        flash_loan_initial_token_amount: 0,
        total_swap_fee: 0,
        scale_initial_asset_weight_start,
        min_borrow_rate: 0,
        token_program_flag: token_program,
        pool_id: 0,
        _padding_align_pfp: 0,
        protocol_fee_pool: PoolBalance {
            scaled_balance: 0,
            market_index: spot_market_index,
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
            vault: ctx.accounts.insurance_fund_vault.key(),
            unstaking_period: THIRTEEN_DAY,
            if_fee_factor: if_total_factor,
            revenue_settle_period: 3600,
            ..InsuranceFund::default()
        },
    };

    Ok(())
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
    msg!("perp market {}", market_index);
    let perp_market_pubkey = ctx.accounts.perp_market.to_account_info().key;
    let perp_market = &mut ctx.accounts.perp_market.load_init()?;

    // 0 means "unset" -> fall back to the launch defaults (5bps / 1.0x)
    let funding_clamp_threshold = if funding_clamp_threshold == 0 {
        5
    } else {
        funding_clamp_threshold
    };
    let funding_ramp_slope = if funding_ramp_slope == 0 {
        PERCENTAGE_PRECISION_U32
    } else {
        funding_ramp_slope
    };
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let clock_slot = clock.slot;

    validate_supported_market_oracle_source(oracle_source)?;

    if amm_base_asset_reserve != amm_quote_asset_reserve {
        return Err(ErrorCode::InvalidInitialPeg.into());
    }

    validate!(
        (0..=100).contains(&curve_update_intensity),
        ErrorCode::DefaultError,
        "invalid curve_update_intensity",
    )?;

    validate!(
        (0..=100).contains(&amm_jit_intensity),
        ErrorCode::DefaultError,
        "invalid amm_jit_intensity",
    )?;

    let init_reserve_price = amm::calculate_price(
        amm_quote_asset_reserve,
        amm_base_asset_reserve,
        amm_peg_multiplier,
    )?;

    assert_eq!(amm_peg_multiplier, init_reserve_price.cast::<u128>()?);

    let concentration_coef = MAX_CONCENTRATION_COEFFICIENT;

    // Verify there's no overflow
    let _k =
        bn::U192::from(amm_base_asset_reserve).safe_mul(bn::U192::from(amm_quote_asset_reserve))?;

    let (min_base_asset_reserve, max_base_asset_reserve) =
        amm::calculate_bid_ask_bounds(concentration_coef, amm_base_asset_reserve)?;

    OracleMap::validate_oracle_account_info(&ctx.accounts.oracle)?;

    // Verify oracle is readable
    let (oracle_price, oracle_delay, last_oracle_price_twap) = match oracle_source {
        OracleSource::Pyth => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(&ctx.accounts.oracle, clock_slot, &OracleSource::Pyth)?;
            let last_oracle_price_twap = perp_market
                .amm
                .get_pyth_twap(&ctx.accounts.oracle, &OracleSource::Pyth)?;
            (oracle_price, oracle_delay, last_oracle_price_twap)
        }
        OracleSource::Pyth1K => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(&ctx.accounts.oracle, clock_slot, &OracleSource::Pyth1K)?;
            let last_oracle_price_twap = perp_market
                .amm
                .get_pyth_twap(&ctx.accounts.oracle, &OracleSource::Pyth1K)?;
            (oracle_price, oracle_delay, last_oracle_price_twap)
        }
        OracleSource::Pyth1M => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(&ctx.accounts.oracle, clock_slot, &OracleSource::Pyth1M)?;
            let last_oracle_price_twap = perp_market
                .amm
                .get_pyth_twap(&ctx.accounts.oracle, &OracleSource::Pyth1M)?;
            (oracle_price, oracle_delay, last_oracle_price_twap)
        }
        OracleSource::PythStableCoin => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(
                &ctx.accounts.oracle,
                clock_slot,
                &OracleSource::PythStableCoin,
            )?;
            (oracle_price, oracle_delay, QUOTE_PRECISION_I64)
        }
        OracleSource::DeprecatedSwitchboard | OracleSource::DeprecatedSwitchboardOnDemand => {
            return Err(ErrorCode::InvalidOracle.into());
        }
        OracleSource::QuoteAsset => {
            msg!("Quote asset oracle cant be used for perp market");
            return Err(ErrorCode::InvalidOracle.into());
        }
        OracleSource::Prelaunch => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_prelaunch_price(&ctx.accounts.oracle, clock_slot)?;
            (oracle_price, oracle_delay, oracle_price)
        }
        OracleSource::PythPull
        | OracleSource::Pyth1KPull
        | OracleSource::Pyth1MPull
        | OracleSource::PythStableCoinPull => {
            return Err(ErrorCode::InvalidOracle.into());
        }
        OracleSource::PythLazer => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(&ctx.accounts.oracle, clock_slot, &OracleSource::PythLazer)?;
            let last_oracle_price_twap = perp_market
                .amm
                .get_pyth_twap(&ctx.accounts.oracle, &OracleSource::PythLazer)?;
            (oracle_price, oracle_delay, last_oracle_price_twap)
        }
        OracleSource::PythLazer1K => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(&ctx.accounts.oracle, clock_slot, &OracleSource::PythLazer1K)?;
            let last_oracle_price_twap = perp_market
                .amm
                .get_pyth_twap(&ctx.accounts.oracle, &OracleSource::PythLazer1K)?;
            (oracle_price, oracle_delay, last_oracle_price_twap)
        }
        OracleSource::PythLazer1M => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(&ctx.accounts.oracle, clock_slot, &OracleSource::PythLazer1M)?;
            let last_oracle_price_twap = perp_market
                .amm
                .get_pyth_twap(&ctx.accounts.oracle, &OracleSource::PythLazer1M)?;
            (oracle_price, oracle_delay, last_oracle_price_twap)
        }
        OracleSource::PythLazerStableCoin => {
            let OraclePriceData {
                price: oracle_price,
                delay: oracle_delay,
                ..
            } = get_pyth_price(
                &ctx.accounts.oracle,
                clock_slot,
                &OracleSource::PythLazerStableCoin,
            )?;
            (oracle_price, oracle_delay, QUOTE_PRECISION_I64)
        }
    };

    validate_margin(
        margin_ratio_initial,
        margin_ratio_maintenance,
        liquidator_fee,
        if_liquidation_fee,
        max_spread,
    )?;

    let mut state = ctx.accounts.state.load_mut()?;
    validate!(
        market_index == state.number_of_markets,
        ErrorCode::MarketIndexAlreadyInitialized,
        "market_index={} != state.number_of_markets={}",
        market_index,
        state.number_of_markets
    )?;

    if active_status {
        validate!(
            ctx.accounts.admin.key() == state.cold_admin,
            ErrorCode::DefaultError,
            "admin must be state admin"
        )?;
    }

    **perp_market = PerpMarket {
        contract_type: ContractType::Perpetual,
        contract_tier,
        status: if active_status {
            MarketStatus::Active
        } else {
            MarketStatus::Initialized
        },
        name,
        expiry_price: 0,
        expiry_ts: 0,
        pubkey: *perp_market_pubkey,
        market_index,
        number_of_users_with_base: 0,
        number_of_users: 0,
        margin_ratio_initial, // unit is 20% (+2 decimal places)
        margin_ratio_maintenance,
        imf_factor,
        next_fill_record_id: 1,
        next_funding_rate_record_id: 1,
        fee_ledger: FeeLedger::default(),
        pnl_pool: PoolBalance::default(),
        insurance_claim: InsuranceClaim {
            max_revenue_withdraw_per_period,
            quote_max_insurance,
            ..InsuranceClaim::default()
        },
        unrealized_pnl_initial_asset_weight: 0, // 100%
        unrealized_pnl_maintenance_asset_weight: SPOT_WEIGHT_PRECISION.cast()?, // 100%
        unrealized_pnl_imf_factor: 0,
        unrealized_pnl_max_imbalance: 0,
        liquidator_fee,
        if_liquidation_fee,
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
            pool_id: lp_pool_id,
            status: 0,
            paused_operations: 0,
            exchange_fee_exclusion_scalar: 0,
            fee_transfer_scalar: 1,
            padding: [0; 11],
        },
        oracle: *ctx.accounts.oracle.key,
        oracle_source,
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
        funding_clamp_threshold,
        funding_ramp_slope,
        order_step_size,
        order_tick_size,
        base_asset_amount_long: 0,
        base_asset_amount_short: 0,
        quote_asset_amount: 0,
        quote_entry_amount_long: 0,
        quote_entry_amount_short: 0,
        quote_break_even_amount_long: 0,
        quote_break_even_amount_short: 0,
        max_open_interest,
        bankruptcy_if_floor_pct: DEFAULT_BANKRUPTCY_IF_FLOOR_PCT,
        market_stats: MarketStats {
            last_oracle_normalised_price: oracle_price,
            last_mark_price_twap: init_reserve_price,
            last_mark_price_twap_5min: init_reserve_price,
            last_mark_price_twap_ts: now,
            last_bid_price_twap: init_reserve_price,
            last_ask_price_twap: init_reserve_price,
            last_trade_ts: now,
            last_24h_avg_funding_rate: 0,
            funding_period: amm_periodicity,
            min_order_size,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: oracle_price,
                last_oracle_delay: oracle_delay,
                last_oracle_price_twap,
                last_oracle_price_twap_5min: oracle_price,
                last_oracle_price_twap_ts: now,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        pending_revenue_share: 0,
        amm: AMM {
            base_asset_reserve: amm_base_asset_reserve,
            quote_asset_reserve: amm_quote_asset_reserve,
            terminal_quote_asset_reserve: amm_quote_asset_reserve,
            sqrt_k: amm_base_asset_reserve,
            concentration_coef,
            min_base_asset_reserve,
            max_base_asset_reserve,
            peg_multiplier: amm_peg_multiplier,
            total_fee: 0,
            total_fee_withdrawn: 0,
            total_fee_minus_distributions: 0,
            total_mm_fee: 0,
            net_revenue_since_last_funding: 0,
            max_slippage_ratio: 50,         // ~2%
            max_fill_reserve_fraction: 100, // moves price ~2%
            base_spread,
            max_spread,
            base_asset_amount_with_amm: 0,
            curve_update_intensity,
            fee_pool: PoolBalance::default(),
            last_update_slot: clock_slot,

            amm_jit_intensity,

            amm_spread_adjustment: 0,
            amm_inventory_spread_adjustment: 0,
            reference_price_offset_deadband_pct: 0,
            last_cumulative_funding_rate_long: 0,
            last_cumulative_funding_rate_short: 0,
            // Cached spread state: seed to a balanced no-spread snapshot
            // (ask/bid reserves == base/quote reserves, zero spreads). The
            // first `update_amms` keeper crank — or the first fill `setup` —
            // refreshes it with the real oracle-driven values.
            ask_base_asset_reserve: amm_base_asset_reserve,
            ask_quote_asset_reserve: amm_quote_asset_reserve,
            bid_base_asset_reserve: amm_base_asset_reserve,
            bid_quote_asset_reserve: amm_quote_asset_reserve,
            last_oracle_reserve_price_spread_pct: 0,
            last_spread_update_slot: clock_slot,
            long_spread: 0,
            short_spread: 0,
            reference_price_offset: 0,
            funding_bias_sensitivity: 0,
            padding_post_amm: [0; 2],
        },
        // protocol fees are quote-denominated; quote market is QUOTE_SPOT_MARKET_INDEX
        protocol_fee_pool: PoolBalance {
            market_index: QUOTE_SPOT_MARKET_INDEX,
            ..PoolBalance::default()
        },
        protocol_liquidation_fee: 0,
        taker_fee_addon_tenth_bps: 0,
        _padding_buffer: [0; 2],
        fee_pool_buffer_target: FEE_POOL_TO_REVENUE_POOL_THRESHOLD as u64,
        // `update_perp_market_clob_quoter` sets this once the CLOB registry
        // entry exists. The registry entry needs the market first.
        clob_market: Pubkey::default(),
        // The slab PDA is derivable here, so it is stored at creation. Every
        // accounts struct that names both binds them with `has_one`.
        quoter_slab: crate::state::pdas::quoter_slab(market_index),
        _padding_future: [0; 192],
    };

    safe_increment!(state.number_of_markets, 1);

    perp_market
        .amm
        .update_concentration_coef(concentration_coef_scale)?;
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

    crate::validation::perp_market::validate_perp_market(perp_market)?;

    Ok(())
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

pub fn handle_delete_initialized_spot_market(
    ctx: Context<DeleteInitializedSpotMarket>,
    market_index: u16,
) -> Result<()> {
    let spot_market = ctx.accounts.spot_market.load()?;
    msg!("spot market {}", spot_market.market_index);
    let mut state = ctx.accounts.state.load_mut()?;

    // to preserve all protocol invariants, can only remove the last market if it hasn't been "activated"

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

    safe_decrement!(state.number_of_spot_markets, 1);

    drop(spot_market);

    validate!(
        ctx.accounts.spot_market_vault.amount == 0,
        ErrorCode::InvalidMarketAccountforDeletion,
        "ctx.accounts.spot_market_vault.amount={}",
        ctx.accounts.spot_market_vault.amount
    )?;

    close_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.admin.to_account_info(),
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
    )?;

    validate!(
        ctx.accounts.insurance_fund_vault.amount == 0,
        ErrorCode::InvalidMarketAccountforDeletion,
        "ctx.accounts.insurance_fund_vault.amount={}",
        ctx.accounts.insurance_fund_vault.amount
    )?;

    close_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.insurance_fund_vault,
        &ctx.accounts.admin.to_account_info(),
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
    )?;

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_oracle(
    ctx: Context<AdminUpdateSpotMarketOracle>,
    oracle: Pubkey,
    oracle_source: OracleSource,
    skip_invariant_check: bool,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("updating spot market {} oracle", spot_market.market_index);
    let clock = Clock::get()?;

    validate_supported_market_oracle_source(oracle_source)?;

    OracleMap::validate_oracle_account_info(&ctx.accounts.oracle)?;

    validate!(
        ctx.accounts.oracle.key == &oracle,
        ErrorCode::DefaultError,
        "oracle account info ({:?}) and ix data ({:?}) must match",
        ctx.accounts.oracle.key,
        oracle
    )?;

    validate!(
        ctx.accounts.old_oracle.key == &spot_market.oracle,
        ErrorCode::DefaultError,
        "old oracle account info ({:?}) and spot market oracle ({:?}) must match",
        ctx.accounts.old_oracle.key,
        spot_market.oracle
    )?;

    // Verify oracle is readable
    let OraclePriceData {
        price: new_oracle_price,
        ..
    } = get_oracle_price(&oracle_source, &ctx.accounts.oracle, clock.slot)?;

    msg!(
        "spot_market.oracle {:?} -> {:?}",
        spot_market.oracle,
        oracle
    );

    msg!(
        "spot_market.oracle_source {:?} -> {:?}",
        spot_market.oracle_source,
        oracle_source
    );

    let OraclePriceData {
        price: old_oracle_price,
        ..
    } = get_oracle_price(
        &spot_market.oracle_source,
        &ctx.accounts.old_oracle,
        clock.slot,
    )?;

    msg!(
        "Oracle Price: {:?} -> {:?}",
        old_oracle_price,
        new_oracle_price
    );

    if !skip_invariant_check {
        validate!(
            new_oracle_price > 0,
            ErrorCode::DefaultError,
            "invalid oracle price, must be greater than 0"
        )?;

        let oracle_change_divergence = new_oracle_price
            .safe_sub(old_oracle_price)?
            .safe_mul(PERCENTAGE_PRECISION_I64)?
            .safe_div(old_oracle_price)?;

        validate!(
            oracle_change_divergence.abs() < (PERCENTAGE_PRECISION_I64 / 10),
            ErrorCode::DefaultError,
            "invalid new oracle price, more than 10% divergence"
        )?;
    }

    spot_market.oracle = oracle;
    spot_market.oracle_source = oracle_source;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_expiry(
    ctx: Context<AdminUpdateSpotMarket>,
    expiry_ts: i64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("updating spot market {} expiry", spot_market.market_index);
    let now = Clock::get()?.unix_timestamp;

    validate!(
        now < expiry_ts,
        ErrorCode::DefaultError,
        "Market expiry ts must later than current clock timestamp"
    )?;

    msg!(
        "spot_market.status {:?} -> {:?}",
        spot_market.status,
        MarketStatus::ReduceOnly
    );
    msg!(
        "spot_market.expiry_ts {} -> {}",
        spot_market.expiry_ts,
        expiry_ts
    );

    // automatically enter reduce only
    spot_market.status = MarketStatus::ReduceOnly;
    spot_market.expiry_ts = expiry_ts;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_expiry(
    ctx: Context<AdminUpdatePerpMarket>,
    expiry_ts: i64,
) -> Result<()> {
    let clock: Clock = Clock::get()?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("updating perp market {} expiry", perp_market.market_index);

    validate!(
        clock.unix_timestamp < expiry_ts,
        ErrorCode::DefaultError,
        "Market expiry ts must later than current clock timestamp"
    )?;

    msg!(
        "perp_market.status {:?} -> {:?}",
        perp_market.status,
        MarketStatus::ReduceOnly
    );
    msg!(
        "perp_market.expiry_ts {} -> {}",
        perp_market.expiry_ts,
        expiry_ts
    );

    // automatically enter reduce only
    perp_market.status = MarketStatus::ReduceOnly;
    perp_market.expiry_ts = expiry_ts;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_settle_expired_market_pools_to_revenue_pool(
    ctx: Context<SettleExpiredMarketPoolsToRevenuePool>,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let spot_market: &mut std::cell::RefMut<'_, SpotMarket> =
        &mut load_mut!(ctx.accounts.spot_market)?;
    let state = ctx.accounts.state.load()?;

    msg!(
        "settling expired market pools to revenue pool for perp market {}",
        perp_market.market_index
    );

    msg!(
        "settling expired market pools to revenue pool for spot market {}",
        spot_market.market_index
    );

    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        now,
        state.funding_paused()?,
    )?;

    validate!(
        spot_market.market_index == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::DefaultError,
        "spot_market must be perp market's quote asset"
    )?;

    validate!(
        perp_market.status == MarketStatus::Settlement,
        ErrorCode::DefaultError,
        "Market must in Settlement"
    )?;

    validate!(
        perp_market.base_asset_amount_long == 0
            && perp_market.base_asset_amount_short == 0
            && perp_market.number_of_users_with_base == 0,
        ErrorCode::DefaultError,
        "outstanding base_asset_amounts must be balanced {} {} {}",
        perp_market.base_asset_amount_long,
        perp_market.base_asset_amount_short,
        perp_market.number_of_users_with_base
    )?;

    validate!(
        crate::vlp::amm::math::amm::calculate_net_user_cost_basis(
            perp_market.quote_asset_amount,
            perp_market.net_unsettled_funding_pnl,
        )? == 0,
        ErrorCode::DefaultError,
        "outstanding quote_asset_amounts must be balanced"
    )?;

    // The wind-down checks above cannot see a booked bankruptcy claim. They sum the quote across
    // the market, so a latched bankrupt's settled debt of -X nets against another user's unsettled
    // claim of +X. Both positions hold no base, so all three checks pass with the debt still open.
    // The final sweep below runs with `force = true` and reserves nothing, so it would drain the
    // insurance tranche that backs that debt.
    //
    // The counter must therefore reach zero first. Two permissionless paths take it there.
    // `resolve_perp_bankruptcy` absorbs the debt through the bankruptcy waterfall. `settle_pnl`
    // releases the claim once the position's quote reaches zero. Neither needs the admin, and the
    // market stays in Settlement while they run.
    validate!(
        perp_market.pending_bankruptcy_claims == 0,
        ErrorCode::DefaultError,
        "perp market {} still holds {} unresolved bankruptcy claims; resolve them before delisting",
        perp_market.market_index,
        perp_market.pending_bankruptcy_claims
    )?;

    // With user base, AMM base, and net user cost basis all wound down,
    // net_user_pnl is identically 0 — no live user claim remains on the pnl
    // pool. This is what lets the final sweep below (and the full pnl-pool
    // drain to the revenue pool) reserve nothing for users without consulting
    // an oracle.
    validate!(
        perp_market.amm.base_asset_amount_with_amm == 0,
        ErrorCode::DefaultError,
        "amm base_asset_amount_with_amm must be balanced ({})",
        perp_market.amm.base_asset_amount_with_amm
    )?;

    // block when settlement_duration is default/unconfigured
    validate!(
        state.settlement_duration != 0,
        ErrorCode::DefaultError,
        "invalid state.settlement_duration (is 0)"
    )?;

    let escrow_period_before_transfer = state.escrow_period_before_transfer()?;

    validate!(
        now > perp_market
            .expiry_ts
            .safe_add(escrow_period_before_transfer)?,
        ErrorCode::DefaultError,
        "must be escrow_period_before_transfer={} after market.expiry_ts",
        escrow_period_before_transfer
    )?;

    // The pnl pool must still hold pending_revenue_share when it drains to
    // the revenue pool: the expiry solver prices winner claims against
    // `pnl_pool - pending_revenue_share` (OtterSec #147). A nonzero counter
    // here would give third-party fees to the revenue pool.
    validate!(
        perp_market.pending_revenue_share == 0,
        ErrorCode::UnsettledRevenueShareOnDelist,
        "perp market {} still owes {} of builder/referrer revenue share; run settle_revenue_share for every escrow still owed, and forfeit_revenue_share_order for any row that provably cannot be paid",
        perp_market.market_index,
        perp_market.pending_revenue_share
    )?;

    // Materialize accrued fees before draining the pnl pool to the revenue
    // pool. The pnl pool holds the un-swept fee value; without this sweep the
    // `pending_protocol_fee` carveout (which the waterfall routes to the
    // withdrawable `protocol_fee_pool`) would instead be dumped wholesale into
    // the revenue pool / insurance fund and lost to the protocol, since no
    // sweep can run once the market is Delisted. net_user_pnl is 0 here (see
    // the wind-down validations above), so the full pnl-pool surplus is
    // available to the waterfall. `force = true` overrides any standing
    // SettleRevPool pause — this is the last sweep the market will ever get.
    controller::perp_pools::sweep_market_fees(perp_market, spot_market, 0, now, true)?;

    let fee_pool_token_amount = perp_market.amm.fee_pool_token_amount(spot_market)?;
    let pnl_pool_token_amount = get_token_amount(
        perp_market.pnl_pool.scaled_balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    <crate::vlp::amm::AMM as crate::vlp::amm::quoter::AmmContract>::withdraw_from_fee_pool(
        &mut perp_market.amm,
        fee_pool_token_amount,
        spot_market,
        false,
    )?;

    controller::spot_balance::update_spot_balances(
        pnl_pool_token_amount,
        &SpotBalanceType::Borrow,
        spot_market,
        &mut perp_market.pnl_pool,
        false,
    )?;

    controller::spot_balance::update_revenue_pool_balances(
        pnl_pool_token_amount.safe_add(fee_pool_token_amount)?,
        &SpotBalanceType::Deposit,
        spot_market,
        false,
    )?;

    math::spot_withdraw::validate_spot_balances(spot_market)?;

    perp_market.status = MarketStatus::Delisted;

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_pnl_pool<'c: 'info, 'info>(
    ctx: Context<'info, UpdatePerpMarketPnlPool<'info>>,
    amount: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    controller::spot_balance::update_spot_balances(
        amount.cast::<u128>()?,
        &SpotBalanceType::Deposit,
        spot_market,
        &mut perp_market.pnl_pool,
        false,
    )?;

    validate_spot_market_vault_amount(spot_market, ctx.accounts.spot_market_vault.amount)?;

    msg!(
        "updating perp market {} pnl pool with amount {}",
        perp_market.market_index,
        amount
    );

    Ok(())
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_deposit_into_spot_market_vault<'c: 'info, 'info>(
    ctx: Context<'info, DepositIntoSpotMarketVault<'info>>,
    amount: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    validate!(
        !spot_market.is_operation_paused(SpotOperation::Deposit),
        ErrorCode::DefaultError,
        "spot market deposits paused"
    )?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();

    let mint = get_token_mint(remaining_accounts_iter)?;

    msg!(
        "depositing {} into spot market {} vault",
        amount,
        spot_market.market_index
    );

    let deposit_token_amount_before = spot_market.get_deposits()?;

    let deposit_token_amount_after = deposit_token_amount_before.safe_add(amount.cast()?)?;

    validate!(
        deposit_token_amount_after > deposit_token_amount_before,
        ErrorCode::DefaultError,
        "new_deposit_token_amount ({}) <= deposit_token_amount ({})",
        deposit_token_amount_after,
        deposit_token_amount_before
    )?;

    let token_precision = spot_market.get_precision();

    let cumulative_deposit_interest_before = spot_market.cumulative_deposit_interest;

    let cumulative_deposit_interest_after = deposit_token_amount_after
        .safe_mul(SPOT_CUMULATIVE_INTEREST_PRECISION)?
        .safe_div(spot_market.deposit_balance)?
        .safe_mul(SPOT_BALANCE_PRECISION)?
        .safe_div(token_precision.cast()?)?;

    validate!(
        cumulative_deposit_interest_after > cumulative_deposit_interest_before,
        ErrorCode::DefaultError,
        "cumulative_deposit_interest_after ({}) <= cumulative_deposit_interest_before ({})",
        cumulative_deposit_interest_after,
        cumulative_deposit_interest_before
    )?;

    spot_market.cumulative_deposit_interest = cumulative_deposit_interest_after;

    controller::token::receive(
        &ctx.accounts.token_program,
        &ctx.accounts.source_vault,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.admin.to_account_info(),
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    ctx.accounts.spot_market_vault.reload()?;
    validate_spot_market_vault_amount(spot_market, ctx.accounts.spot_market_vault.amount)?;

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    emit!(SpotMarketVaultDepositRecord {
        ts: Clock::get()?.unix_timestamp,
        market_index: spot_market.market_index,
        deposit_balance: spot_market.deposit_balance,
        cumulative_deposit_interest_before,
        cumulative_deposit_interest_after,
        deposit_token_amount_before: deposit_token_amount_before.cast()?,
        amount
    });

    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_margin_ratio(
    ctx: Context<AdminUpdatePerpMarket>,
    margin_ratio_initial: u32,
    margin_ratio_maintenance: u32,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating perp market {} margin ratio",
        perp_market.market_index
    );

    perp_market.amm.validate_compatible_with_margin_ratio(
        margin_ratio_initial,
        margin_ratio_maintenance,
        perp_market.liquidator_fee,
        perp_market.if_liquidation_fee,
    )?;

    msg!(
        "perp_market.margin_ratio_initial: {:?} -> {:?}",
        perp_market.margin_ratio_initial,
        margin_ratio_initial
    );

    msg!(
        "perp_market.margin_ratio_maintenance: {:?} -> {:?}",
        perp_market.margin_ratio_maintenance,
        margin_ratio_maintenance
    );

    perp_market.margin_ratio_initial = margin_ratio_initial;
    perp_market.margin_ratio_maintenance = margin_ratio_maintenance;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_funding_period(
    ctx: Context<AdminUpdatePerpMarket>,
    funding_period: i64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating funding period for perp market {}",
        perp_market.market_index
    );

    validate!(funding_period >= 0, ErrorCode::DefaultError)?;

    msg!(
        "perp_market.funding_period: {:?} -> {:?}",
        perp_market.market_stats.funding_period,
        funding_period
    );

    perp_market.market_stats.funding_period = funding_period;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_funding_dead_zone(
    ctx: Context<AdminUpdatePerpMarket>,
    funding_clamp_threshold: u32,
    funding_ramp_slope: u32,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating funding dead zone for perp market {}",
        perp_market.market_index
    );

    // threshold is a fraction of the oracle price; keep it well below 100%
    validate!(
        funding_clamp_threshold < BPS_PRECISION,
        ErrorCode::DefaultError
    )?;
    // a zero slope would flatten every premium past the band to the offset
    validate!(funding_ramp_slope > 0, ErrorCode::DefaultError)?;

    msg!(
        "perp_market.funding_clamp_threshold: {:?} -> {:?}",
        perp_market.funding_clamp_threshold,
        funding_clamp_threshold
    );

    msg!(
        "perp_market.funding_ramp_slope: {:?} -> {:?}",
        perp_market.funding_ramp_slope,
        funding_ramp_slope
    );

    perp_market.funding_clamp_threshold = funding_clamp_threshold;
    perp_market.funding_ramp_slope = funding_ramp_slope;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_max_imbalances(
    ctx: Context<AdminUpdatePerpMarket>,
    unrealized_max_imbalance: u64,
    max_revenue_withdraw_per_period: u64,
    quote_max_insurance: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating perp market {} max imbalances",
        perp_market.market_index
    );

    let max_insurance_for_tier = match perp_market.contract_tier {
        ContractTier::A => INSURANCE_A_MAX,
        ContractTier::B => INSURANCE_B_MAX,
        ContractTier::C => INSURANCE_C_MAX,
        ContractTier::Speculative => INSURANCE_SPECULATIVE_MAX,
        ContractTier::HighlySpeculative => INSURANCE_SPECULATIVE_MAX,
        ContractTier::Isolated => INSURANCE_SPECULATIVE_MAX,
    };

    validate!(
        max_revenue_withdraw_per_period
            <= max_insurance_for_tier.max(FEE_POOL_TO_REVENUE_POOL_THRESHOLD.cast()?)
            && unrealized_max_imbalance <= max_insurance_for_tier + 1
            && quote_max_insurance <= max_insurance_for_tier,
        ErrorCode::DefaultError,
        "all maxs must be less than max_insurance for ContractTier ={}",
        max_insurance_for_tier
    )?;

    validate!(
        perp_market.insurance_claim.quote_settled_insurance <= quote_max_insurance,
        ErrorCode::DefaultError,
        "quote_max_insurance must be above market.insurance_claim.quote_settled_insurance={}",
        perp_market.insurance_claim.quote_settled_insurance
    )?;

    msg!(
        "market.max_revenue_withdraw_per_period: {:?} -> {:?}",
        perp_market.insurance_claim.max_revenue_withdraw_per_period,
        max_revenue_withdraw_per_period
    );

    msg!(
        "market.unrealized_max_imbalance: {:?} -> {:?}",
        perp_market.unrealized_pnl_max_imbalance,
        unrealized_max_imbalance
    );

    msg!(
        "market.quote_max_insurance: {:?} -> {:?}",
        perp_market.insurance_claim.quote_max_insurance,
        quote_max_insurance
    );

    perp_market.insurance_claim.max_revenue_withdraw_per_period = max_revenue_withdraw_per_period;
    perp_market.unrealized_pnl_max_imbalance = unrealized_max_imbalance;
    perp_market.insurance_claim.quote_max_insurance = quote_max_insurance;

    // ensure altered max_revenue_withdraw_per_period doesn't break invariant check
    crate::validation::perp_market::validate_perp_market(perp_market)?;

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
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_liquidation_fee(
    ctx: Context<AdminUpdatePerpMarket>,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    protocol_liquidation_fee: u32,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;

    msg!(
        "updating perp market {} liquidation fee",
        perp_market.market_index
    );

    validate!(
        liquidator_fee
            .safe_add(if_liquidation_fee)?
            .safe_add(protocol_liquidation_fee)?
            < LIQUIDATION_FEE_PRECISION,
        ErrorCode::DefaultError,
        "Total liquidation fee must be less than 100%"
    )?;

    validate!(
        if_liquidation_fee < LIQUIDATION_FEE_PRECISION,
        ErrorCode::DefaultError,
        "If liquidation fee must be less than 100%"
    )?;

    validate!(
        protocol_liquidation_fee <= LIQUIDATION_FEE_PRECISION / 10,
        ErrorCode::DefaultError,
        "protocol_liquidation_fee must be <= 10%"
    )?;

    perp_market.amm.validate_compatible_with_liquidation_fee(
        perp_market.margin_ratio_initial,
        perp_market.margin_ratio_maintenance,
        liquidator_fee,
        if_liquidation_fee,
    )?;

    msg!(
        "perp_market.liquidator_fee: {:?} -> {:?}",
        perp_market.liquidator_fee,
        liquidator_fee
    );

    msg!(
        "perp_market.if_liquidation_fee: {:?} -> {:?}",
        perp_market.if_liquidation_fee,
        if_liquidation_fee
    );

    msg!(
        "perp_market.protocol_liquidation_fee: {:?} -> {:?}",
        perp_market.protocol_liquidation_fee,
        protocol_liquidation_fee
    );

    perp_market.liquidator_fee = liquidator_fee;
    perp_market.if_liquidation_fee = if_liquidation_fee;
    perp_market.protocol_liquidation_fee = protocol_liquidation_fee;
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
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_insurance_fund_unstaking_period(
    ctx: Context<AdminUpdateSpotMarket>,
    insurance_fund_unstaking_period: i64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    msg!("updating spot market {} IF unstaking period");
    msg!(
        "spot_market.insurance_fund.unstaking_period: {:?} -> {:?}",
        spot_market.insurance_fund.unstaking_period,
        insurance_fund_unstaking_period
    );

    spot_market.insurance_fund.unstaking_period = insurance_fund_unstaking_period;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_liquidation_fee(
    ctx: Context<AdminUpdateSpotMarket>,
    liquidator_fee: u32,
    if_liquidation_fee: u32,
    protocol_liquidation_fee: u32,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!(
        "updating spot market {} liquidation fee",
        spot_market.market_index
    );

    validate!(
        liquidator_fee
            .safe_add(if_liquidation_fee)?
            .safe_add(protocol_liquidation_fee)?
            < LIQUIDATION_FEE_PRECISION,
        ErrorCode::DefaultError,
        "Total liquidation fee must be less than 100%"
    )?;

    validate!(
        if_liquidation_fee <= LIQUIDATION_FEE_PRECISION / 10,
        ErrorCode::DefaultError,
        "if_liquidation_fee must be <= 10%"
    )?;

    validate!(
        protocol_liquidation_fee <= LIQUIDATION_FEE_PRECISION / 10,
        ErrorCode::DefaultError,
        "protocol_liquidation_fee must be <= 10%"
    )?;

    msg!(
        "spot_market.liquidator_fee: {:?} -> {:?}",
        spot_market.liquidator_fee,
        liquidator_fee
    );

    msg!(
        "spot_market.if_liquidation_fee: {:?} -> {:?}",
        spot_market.if_liquidation_fee,
        if_liquidation_fee
    );

    msg!(
        "spot_market.protocol_liquidation_fee: {:?} -> {:?}",
        spot_market.protocol_liquidation_fee,
        protocol_liquidation_fee
    );

    spot_market.liquidator_fee = liquidator_fee;
    spot_market.if_liquidation_fee = if_liquidation_fee;
    spot_market.protocol_liquidation_fee = protocol_liquidation_fee;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_withdraw_guard_threshold(
    ctx: Context<AdminUpdateSpotMarketWithdrawGuardThreshold>,
    withdraw_guard_threshold: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!(
        "updating spot market withdraw guard threshold {}",
        spot_market.market_index
    );

    let oracle_price = get_oracle_price(
        &spot_market.oracle_source,
        &ctx.accounts.oracle,
        Clock::get()?.slot,
    )?
    .price;

    // price the notional cap with the max of the live price and the 5min
    // twap so a momentarily manipulated-down oracle can't let an oversized
    // threshold through
    let strict_oracle_price = StrictOraclePrice::new(
        oracle_price,
        spot_market
            .historical_oracle_data
            .last_oracle_price_twap_5min,
        true,
    );
    strict_oracle_price.validate()?;

    validate_withdraw_guard_threshold(
        withdraw_guard_threshold,
        spot_market.decimals,
        strict_oracle_price.max(),
    )?;

    msg!(
        "spot_market.withdraw_guard_threshold: {:?} -> {:?}",
        spot_market.withdraw_guard_threshold,
        withdraw_guard_threshold
    );
    spot_market.withdraw_guard_threshold = withdraw_guard_threshold;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
/// Set the lending-gain carveouts: `if_fee_factor` (to the insurance fund) and
/// `protocol_fee_factor` (to the withdrawable protocol fee pool). Lenders receive
/// deposit interest net of both.
pub fn handle_update_spot_market_if_factor(
    ctx: Context<AdminUpdateSpotMarket>,
    spot_market_index: u16,
    if_fee_factor: u32,
    protocol_fee_factor: u32,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;

    msg!("spot market {}", spot_market.market_index);

    validate!(
        spot_market.market_index == spot_market_index,
        ErrorCode::DefaultError,
        "spot_market_index dne spot_market.index"
    )?;

    // The combined carveout stays below 100%, so lenders keep a configured share.
    // `split_deposit_interest` relies on this bound. It divides the deposit
    // interest by IF_FACTOR_PRECISION with the combined factor as the numerator.
    // A combined factor below IF_FACTOR_PRECISION keeps that quotient at or below
    // the interval gain, so the two cuts never take more than the market earned.
    //
    // The bound does not by itself keep the lender share above zero. A carried
    // remainder can raise the cuts to the whole gain on a short interval. The
    // accrual commits anyway in that case, so a zero lender share is safe.
    //
    // A lower pair can leave a carried remainder at or above the new combined
    // factor, which is the divisor of the insurance-fund-vs-protocol split.
    // `split_deposit_interest` reduces that remainder below the divisor in force, so
    // this handler does not have to settle or rescale it. The reduction costs less
    // than one index unit.
    validate!(
        if_fee_factor.safe_add(protocol_fee_factor)? < IF_FACTOR_PRECISION.cast()?,
        ErrorCode::DefaultError,
        "if_fee_factor + protocol_fee_factor must be < 100%"
    )?;

    msg!(
        "spot_market.if_fee_factor: {:?} -> {:?}",
        spot_market.insurance_fund.if_fee_factor,
        if_fee_factor
    );

    msg!(
        "spot_market.protocol_fee_factor: {:?} -> {:?}",
        spot_market.protocol_fee_factor,
        protocol_fee_factor
    );

    spot_market.insurance_fund.if_fee_factor = if_fee_factor;
    spot_market.protocol_fee_factor = protocol_fee_factor;

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_revenue_settle_period(
    ctx: Context<AdminUpdateSpotMarket>,
    revenue_settle_period: i64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate!(revenue_settle_period > 0, ErrorCode::DefaultError)?;
    msg!(
        "spot_market.revenue_settle_period: {:?} -> {:?}",
        spot_market.insurance_fund.revenue_settle_period,
        revenue_settle_period
    );
    spot_market.insurance_fund.revenue_settle_period = revenue_settle_period;
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
pub fn handle_update_spot_market_margin_weights(
    ctx: Context<AdminUpdateSpotMarket>,
    initial_asset_weight: u32,
    maintenance_asset_weight: u32,
    initial_liability_weight: u32,
    maintenance_liability_weight: u32,
    imf_factor: u32,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate_margin_weights(
        spot_market.market_index,
        initial_asset_weight,
        maintenance_asset_weight,
        initial_liability_weight,
        maintenance_liability_weight,
        imf_factor,
    )?;

    msg!(
        "spot_market.initial_asset_weight: {:?} -> {:?}",
        spot_market.initial_asset_weight,
        initial_asset_weight
    );

    msg!(
        "spot_market.maintenance_asset_weight: {:?} -> {:?}",
        spot_market.maintenance_asset_weight,
        maintenance_asset_weight
    );

    msg!(
        "spot_market.initial_liability_weight: {:?} -> {:?}",
        spot_market.initial_liability_weight,
        initial_liability_weight
    );

    msg!(
        "spot_market.maintenance_liability_weight: {:?} -> {:?}",
        spot_market.maintenance_liability_weight,
        maintenance_liability_weight
    );

    msg!(
        "spot_market.imf_factor: {:?} -> {:?}",
        spot_market.imf_factor,
        imf_factor
    );

    spot_market.initial_asset_weight = initial_asset_weight;
    spot_market.maintenance_asset_weight = maintenance_asset_weight;
    spot_market.initial_liability_weight = initial_liability_weight;
    spot_market.maintenance_liability_weight = maintenance_liability_weight;
    spot_market.imf_factor = imf_factor;

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_borrow_rate(
    ctx: Context<AdminUpdateSpotMarket>,
    optimal_utilization: u32,
    optimal_borrow_rate: u32,
    max_borrow_rate: u32,
    min_borrow_rate: Option<u8>,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    validate_borrow_rate(
        optimal_utilization,
        optimal_borrow_rate,
        max_borrow_rate,
        min_borrow_rate
            .unwrap_or(spot_market.min_borrow_rate)
            .cast::<u32>()?
            * ((PERCENTAGE_PRECISION / 200) as u32),
    )?;

    msg!(
        "spot_market.optimal_utilization: {:?} -> {:?}",
        spot_market.optimal_utilization,
        optimal_utilization
    );

    msg!(
        "spot_market.optimal_borrow_rate: {:?} -> {:?}",
        spot_market.optimal_borrow_rate,
        optimal_borrow_rate
    );

    msg!(
        "spot_market.max_borrow_rate: {:?} -> {:?}",
        spot_market.max_borrow_rate,
        max_borrow_rate
    );

    spot_market.optimal_utilization = optimal_utilization;
    spot_market.optimal_borrow_rate = optimal_borrow_rate;
    spot_market.max_borrow_rate = max_borrow_rate;

    if let Some(min_borrow_rate) = min_borrow_rate {
        msg!(
            "spot_market.min_borrow_rate: {:?} -> {:?}",
            spot_market.min_borrow_rate,
            min_borrow_rate
        );
        spot_market.min_borrow_rate = min_borrow_rate
    }

    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_max_token_deposits(
    ctx: Context<AdminUpdateSpotMarket>,
    max_token_deposits: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.max_token_deposits: {:?} -> {:?}",
        spot_market.max_token_deposits,
        max_token_deposits
    );

    spot_market.max_token_deposits = max_token_deposits;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_withdraw_circuit_breaker(
    ctx: Context<AdminUpdateSpotMarket>,
    withdraw_circuit_breaker_bps: u16,
) -> Result<()> {
    validate!(
        withdraw_circuit_breaker_bps <= BPS_PRECISION as u16,
        ErrorCode::DefaultError,
        "withdraw_circuit_breaker_bps ({} bps) must be <= 100% ({} bps)",
        withdraw_circuit_breaker_bps,
        BPS_PRECISION
    )?;

    // A higher pct loosens the breaker (allows a larger daily withdrawal). The
    // warm admin may only keep or tighten it relative to the 25% default;
    // loosening it past 25% is a riskier change reserved for the cold admin.
    // (`0` is the default-25% sentinel, so it stays within the warm cap.)
    if !check_cold(&ctx.accounts.admin.key(), &ctx.accounts.state)? {
        validate!(
            withdraw_circuit_breaker_bps <= DEFAULT_WITHDRAW_CIRCUIT_BREAKER_BPS,
            ErrorCode::Unauthorized,
            "warm admin cannot set withdraw_circuit_breaker_bps ({}) above the 25% default ({}); requires cold admin",
            withdraw_circuit_breaker_bps,
            DEFAULT_WITHDRAW_CIRCUIT_BREAKER_BPS
        )?;
    }

    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.withdraw_circuit_breaker_bps: {:?} -> {:?}",
        spot_market.withdraw_circuit_breaker_bps,
        withdraw_circuit_breaker_bps
    );

    spot_market.withdraw_circuit_breaker_bps = withdraw_circuit_breaker_bps;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_deposit_cap(
    ctx: Context<AdminUpdateSpotMarket>,
    deposit_guard_threshold: u64,
    max_deposit_bps_per_day: u16,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.deposit_guard_threshold: {:?} -> {:?}",
        spot_market.deposit_guard_threshold,
        deposit_guard_threshold
    );
    msg!(
        "spot_market.max_deposit_bps_per_day: {:?} -> {:?}",
        spot_market.max_deposit_bps_per_day,
        max_deposit_bps_per_day
    );

    spot_market.deposit_guard_threshold = deposit_guard_threshold;
    spot_market.max_deposit_bps_per_day = max_deposit_bps_per_day;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_max_token_borrows(
    ctx: Context<AdminUpdateSpotMarket>,
    max_token_borrows_fraction: u16,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.max_token_borrows_fraction: {:?} -> {:?}",
        spot_market.max_token_borrows_fraction,
        max_token_borrows_fraction
    );

    let current_spot_tokens_borrows: u64 = spot_market.get_borrows()?.cast()?;
    let new_max_token_borrows = spot_market
        .max_token_deposits
        .safe_mul(max_token_borrows_fraction.cast()?)?
        .safe_div(10000)?;

    validate!(
        current_spot_tokens_borrows <= new_max_token_borrows,
        ErrorCode::InvalidSpotMarketInitialization,
        "spot borrows {} > max_token_borrows {}",
        current_spot_tokens_borrows,
        max_token_borrows_fraction
    )?;

    spot_market.max_token_borrows_fraction = max_token_borrows_fraction;
    Ok(())
}

#[access_control(
spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_scale_initial_asset_weight_start(
    ctx: Context<AdminUpdateSpotMarket>,
    scale_initial_asset_weight_start: u64,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot_market.market_index);

    msg!(
        "spot_market.scale_initial_asset_weight_start: {:?} -> {:?}",
        spot_market.scale_initial_asset_weight_start,
        scale_initial_asset_weight_start
    );

    spot_market.scale_initial_asset_weight_start = scale_initial_asset_weight_start;
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
pub fn handle_update_spot_market_if_paused_operations(
    ctx: Context<PauseAdminUpdateSpotMarket>,
    paused_operations: u8,
) -> Result<()> {
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let signer = ctx.accounts.admin.key();
    let state = ctx.accounts.state.load()?;
    require_pause_only_added(
        &signer,
        &state,
        spot_market.if_paused_operations,
        paused_operations,
    )?;
    drop(state);
    spot_market.if_paused_operations = paused_operations;
    msg!("spot market {}", spot_market.market_index);
    InsuranceFundOperation::log_all_operations_paused(paused_operations);
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
pub fn handle_update_perp_market_imf_factor(
    ctx: Context<AdminUpdatePerpMarket>,
    imf_factor: u32,
    unrealized_pnl_imf_factor: u32,
) -> Result<()> {
    validate!(
        imf_factor <= SPOT_IMF_PRECISION,
        ErrorCode::DefaultError,
        "invalid imf factor",
    )?;
    validate!(
        unrealized_pnl_imf_factor <= SPOT_IMF_PRECISION,
        ErrorCode::DefaultError,
        "invalid unrealized pnl imf factor",
    )?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.imf_factor: {:?} -> {:?}",
        perp_market.imf_factor,
        imf_factor
    );

    msg!(
        "perp_market.unrealized_pnl_imf_factor: {:?} -> {:?}",
        perp_market.unrealized_pnl_imf_factor,
        unrealized_pnl_imf_factor
    );

    perp_market.imf_factor = imf_factor;
    perp_market.unrealized_pnl_imf_factor = unrealized_pnl_imf_factor;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_unrealized_asset_weight(
    ctx: Context<AdminUpdatePerpMarket>,
    unrealized_initial_asset_weight: u32,
    unrealized_maintenance_asset_weight: u32,
) -> Result<()> {
    validate!(
        unrealized_initial_asset_weight <= SPOT_WEIGHT_PRECISION.cast()?,
        ErrorCode::DefaultError,
        "invalid unrealized_initial_asset_weight",
    )?;
    validate!(
        unrealized_maintenance_asset_weight <= SPOT_WEIGHT_PRECISION.cast()?,
        ErrorCode::DefaultError,
        "invalid unrealized_maintenance_asset_weight",
    )?;
    validate!(
        unrealized_initial_asset_weight <= unrealized_maintenance_asset_weight,
        ErrorCode::DefaultError,
        "must enforce unrealized_initial_asset_weight <= unrealized_maintenance_asset_weight",
    )?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.unrealized_initial_asset_weight: {:?} -> {:?}",
        perp_market.unrealized_pnl_initial_asset_weight,
        unrealized_initial_asset_weight
    );

    msg!(
        "perp_market.unrealized_maintenance_asset_weight: {:?} -> {:?}",
        perp_market.unrealized_pnl_maintenance_asset_weight,
        unrealized_maintenance_asset_weight
    );

    perp_market.unrealized_pnl_initial_asset_weight = unrealized_initial_asset_weight;
    perp_market.unrealized_pnl_maintenance_asset_weight = unrealized_maintenance_asset_weight;
    Ok(())
}

pub fn handle_update_promo_fee_tier(
    ctx: Context<AdminUpdateState>,
    promo_fee_tier: u8,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;

    // The check uses the highest populated tier, not the length of the 10-slot
    // array. The tier lookup clamps to PERP_FEE_TIER_MAX_INDEX, so a higher
    // value would pass here and then mean a lower tier. Zero disables the
    // promo tier.
    validate!(
        (promo_fee_tier as usize) <= PERP_FEE_TIER_MAX_INDEX,
        ErrorCode::InvalidFeeStructure,
        "promo fee tier {} above max populated tier {}",
        promo_fee_tier,
        PERP_FEE_TIER_MAX_INDEX
    )?;

    msg!(
        "state.promo_fee_tier: {:?} -> {:?}",
        state.promo_fee_tier,
        promo_fee_tier
    );

    state.promo_fee_tier = promo_fee_tier;
    Ok(())
}

/// Re-price what the network charges to land a transaction. Every relay
/// crank pays its keeper enough to cover the keeper's own transaction, so
/// this is the one write that moves that cost when the network's fee model
/// changes, such as a new cost-unit rate or a different inclusion fee.
/// Payments already stored on a market's conditions account keep their old
/// figures until that market's attach runs again. A stored floor above
/// what the executor pays stops the crank at `assert_paid_v0`. A floor
/// below it only overpays.
pub fn handle_update_transaction_fee_rails(
    ctx: Context<AdminUpdateState>,
    rails: TransactionFeeRails,
) -> Result<()> {
    validate!(
        rails.resource_fee_denominator > 0 || rails.resource_fee_numerator == 0,
        ErrorCode::InvalidFeeStructure,
        "a resource fee rate needs a denominator"
    )?;

    msg!(
        "transaction_fee_rails: {:?} -> {:?}",
        ctx.accounts.state.load()?.transaction_fee_rails,
        rails
    );

    ctx.accounts.state.load_mut()?.transaction_fee_rails = rails;
    Ok(())
}

/// What the protocol spends to get a liquidation cranked, and the market
/// whose oracle prices that spend. The share repays a keeper's priority
/// fee as a fraction of what the liquidation recovers, leaving alone a
/// recovery too small to cover its own gas. Zero in either field is valid.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateLiquidationCrankReimbursementArgs {
    /// The liquidator fee share paid back to a cranked liquidation's payer,
    /// in basis points of the fee.
    pub share_bps: u16,
    /// The SOL spot market the reimbursement is priced through.
    pub sol_spot_market_index: u16,
}

pub fn handle_update_liquidation_crank_reimbursement(
    ctx: Context<AdminUpdateState>,
    args: UpdateLiquidationCrankReimbursementArgs,
) -> Result<()> {
    let UpdateLiquidationCrankReimbursementArgs {
        share_bps,
        sol_spot_market_index,
    } = args;

    validate!(
        share_bps <= 10_000,
        ErrorCode::InvalidFeeStructure,
        "a share of a liquidation cannot exceed the liquidation"
    )?;

    let mut state = ctx.accounts.state.load_mut()?;
    msg!(
        "liquidation crank reimbursement: {}bps market {} -> {}bps market {}",
        state.liquidation_crank_reimbursement_bps,
        state.sol_spot_market_index,
        share_bps,
        sol_spot_market_index
    );

    state.liquidation_crank_reimbursement_bps = share_bps;
    state.sol_spot_market_index = sol_spot_market_index;
    Ok(())
}

pub fn handle_update_user_accelerated_referral_status(
    ctx: Context<AdminUpdateUserStats>,
    accelerated: bool,
) -> Result<()> {
    let mut user_stats = ctx.accounts.user_stats.load_mut()?;
    let previous_status = user_stats.accelerated_referral_status;

    if user_stats.set_accelerated_referral_by_admin(accelerated) {
        emit_accelerated_referral_status_changed(
            Clock::get()?.unix_timestamp,
            user_stats.authority,
            previous_status,
            user_stats.accelerated_referral_status,
            if accelerated {
                AcceleratedReferralStatusChange::AdminGrant
            } else {
                AcceleratedReferralStatusChange::AdminRevoke
            },
        );
    }

    Ok(())
}

pub fn handle_update_perp_fee_structure(
    ctx: Context<AdminUpdateState>,
    fee_structure: FeeStructure,
) -> Result<()> {
    validate_fee_structure(&fee_structure)?;

    msg!(
        "perp_fee_structure: {:?} -> {:?}",
        ctx.accounts.state.load()?.perp_fee_structure,
        fee_structure
    );

    ctx.accounts.state.load_mut()?.perp_fee_structure = fee_structure;
    Ok(())
}

pub fn handle_update_spot_fee_structure(
    ctx: Context<AdminUpdateState>,
    fee_structure: FeeStructure,
) -> Result<()> {
    validate_fee_structure(&fee_structure)?;

    msg!(
        "spot_fee_structure: {:?} -> {:?}",
        ctx.accounts.state.load()?.spot_fee_structure,
        fee_structure
    );

    ctx.accounts.state.load_mut()?.spot_fee_structure = fee_structure;
    Ok(())
}

pub fn handle_update_initial_pct_to_liquidate(
    ctx: Context<AdminUpdateState>,
    initial_pct_to_liquidate: u16,
) -> Result<()> {
    msg!(
        "initial_pct_to_liquidate: {} -> {}",
        ctx.accounts.state.load()?.initial_pct_to_liquidate,
        initial_pct_to_liquidate
    );

    ctx.accounts.state.load_mut()?.initial_pct_to_liquidate = initial_pct_to_liquidate;
    Ok(())
}

pub fn handle_update_liquidation_duration(
    ctx: Context<AdminUpdateState>,
    liquidation_duration: u8,
) -> Result<()> {
    msg!(
        "liquidation_duration: {} -> {}",
        ctx.accounts.state.load()?.liquidation_duration,
        liquidation_duration
    );

    ctx.accounts.state.load_mut()?.liquidation_duration =
        legacy_slot_duration_u8(liquidation_duration);
    Ok(())
}

pub fn handle_update_liquidation_margin_buffer_ratio(
    ctx: Context<AdminUpdateState>,
    liquidation_margin_buffer_ratio: u32,
) -> Result<()> {
    msg!(
        "liquidation_margin_buffer_ratio: {} -> {}",
        ctx.accounts.state.load()?.liquidation_margin_buffer_ratio,
        liquidation_margin_buffer_ratio
    );

    ctx.accounts
        .state
        .load_mut()?
        .liquidation_margin_buffer_ratio = liquidation_margin_buffer_ratio;
    Ok(())
}

/// Ceiling on the margin oracle staleness window, in 400ms baseline units.
/// 1,000,000 units is about 4.6 days, far below where
/// `Millis::from_stored_units` saturates. The limit stops a mistyped
/// value from making the staleness gate accept every oracle.
const MAX_STALENESS_STORED_UNITS: i64 = 1_000_000;

/// A tighter ceiling on the AMM staleness window. `get_oracle_status` routes the
/// window through `oracle_validity`'s `i8` slot-delay override, which also
/// carries the AMM's immediate and low-risk delay overrides. A value above
/// `i8::MAX` would pass this setter and then fail with `CastingFailure` in every
/// funding path that calls `get_oracle_status`. `i8::MAX` units is about 50
/// seconds at the 400ms baseline, far above any real AMM freshness window. The
/// default is 10.
const MAX_AMM_STALENESS_STORED_UNITS: i64 = i8::MAX as i64;

pub fn handle_update_oracle_guard_rails(
    ctx: Context<AdminUpdateState>,
    oracle_guard_rails: OracleGuardRails,
) -> Result<()> {
    validate!(
        (0..=MAX_AMM_STALENESS_STORED_UNITS).contains(&legacy_slot_duration_i64_raw(
            oracle_guard_rails.validity.slots_before_stale_for_amm,
        )) && (0..=MAX_STALENESS_STORED_UNITS).contains(&legacy_slot_duration_i64_raw(
            oracle_guard_rails.validity.slots_before_stale_for_margin,
        )),
        ErrorCode::InvalidOracleStalenessWindow,
        "oracle staleness windows out of range: amm [0, {}], margin [0, {}]",
        MAX_AMM_STALENESS_STORED_UNITS,
        MAX_STALENESS_STORED_UNITS
    )?;

    msg!(
        "oracle_guard_rails: {:?} -> {:?}",
        ctx.accounts.state.load()?.oracle_guard_rails,
        oracle_guard_rails
    );

    ctx.accounts.state.load_mut()?.oracle_guard_rails = oracle_guard_rails;
    Ok(())
}

pub fn handle_update_state_settlement_duration(
    ctx: Context<AdminUpdateState>,
    settlement_duration: u16,
) -> Result<()> {
    msg!(
        "settlement_duration: {} -> {}",
        ctx.accounts.state.load()?.settlement_duration,
        settlement_duration
    );

    ctx.accounts.state.load_mut()?.settlement_duration = settlement_duration;
    Ok(())
}

/// Solana's feature-gate program. It owns every feature account.
const FEATURE_GATE_PROGRAM: Pubkey = pubkey!("Feature111111111111111111111111111111111111");
/// The IBRL feature gate whose activation drops the slot to `slot_duration_ms`.
/// Returns `None` for the 400ms baseline, which has no gate, and for any value
/// that is not on the schedule.
fn ibrl_feature_gate(slot_duration_ms: u16) -> Option<Pubkey> {
    Some(match slot_duration_ms {
        350 => pubkey!("iBRL5RuWhw4yqaAZu96RUULHckHTZAoe2b77qaV38JZ"),
        300 => pubkey!("iBRLL3k18HST852F1Mf3Lv83waTNQmmqvKDxvYGwQFL"),
        250 => pubkey!("iBRLMc81UjRa8fn8A6eE8bJTnRbgQoPTynM51akENCV"),
        200 => pubkey!("iBRLjhJnkmDZgNoZRDMW11d8ZV7HvsL3vAyRjZB5npW"),
        _ => return None,
    })
}

/// Target slot duration selected by an IBRL feature account.
fn ibrl_slot_duration_ms(feature_gate: &Pubkey) -> Option<u16> {
    SLOT_DURATION_TRANSITION_MS
        .iter()
        .find(|slot_duration_ms| {
            ibrl_feature_gate(**slot_duration_ms).as_ref() == Some(feature_gate)
        })
        .copied()
}

/// Returns the slot at which the IBRL feature's slot-time reduction becomes
/// effective, which is the activation slot plus a one-epoch warmup. The function
/// reads the feature-gate account layout: 9 bytes, `data[0] == 1`, and the
/// activation slot in little-endian `data[1..9]`. It errors when the account is
/// not activated or is malformed. [`SyncStateSlotDuration`] carries the key and
/// owner constraints. The following epoch does not have to begin first, because
/// recording the boundary in advance is the point.
fn feature_gate_effective_slot(
    account: &AccountInfo,
    epoch_schedule: &EpochSchedule,
) -> Result<u64> {
    let data = account.try_borrow_data()?;
    validate!(
        data.len() == 9,
        ErrorCode::InvalidFeatureGateAccount,
        "feature-gate account has the wrong data length"
    )?;

    // 0 means the feature is not activated. Any other value is malformed.
    validate!(
        data[0] == 1,
        ErrorCode::FeatureGateNotActive,
        "IBRL feature gate {} is not activated yet (data[0] = {})",
        account.key,
        data[0]
    )?;
    let activated_at = u64::from_le_bytes(data[1..9].try_into().unwrap());
    let activation_epoch = epoch_schedule.get_epoch(activated_at);
    Ok(epoch_schedule.get_first_slot_in_epoch(activation_epoch.saturating_add(1)))
}

fn validated_slot_duration_archive_update(
    state: &State,
    transition_index: usize,
    effective_slot: u64,
    now_slot: u64,
) -> Result<[u64; 4]> {
    let previous_slot = state.slot_duration_transition_slots[..transition_index]
        .iter()
        .rev()
        .find(|slot| **slot != 0)
        .copied();
    let next_slot = state.slot_duration_transition_slots[transition_index + 1..]
        .iter()
        .find(|slot| **slot != 0)
        .copied();

    validate!(
        previous_slot.is_none_or(|slot| effective_slot >= slot)
            && next_slot.is_none_or(|slot| effective_slot <= slot),
        ErrorCode::SlotDurationTransitionInvalid,
        "IBRL transition slots are not monotonic"
    )?;

    if let Some(highest) = state
        .slot_duration_transition_slots
        .iter()
        .rposition(|slot| *slot != 0)
    {
        if transition_index > highest {
            validate!(
                now_slot >= state.slot_duration_transition_slots[highest],
                ErrorCode::SlotDurationTransitionInvalid,
                "previous synchronized IBRL transition is not effective"
            )?;
        }
    }

    let current_duration_ms = state.slot_clock().slot_duration_at(now_slot).as_ms();
    let mut proposed_slots = state.slot_duration_transition_slots;
    proposed_slots[transition_index] = effective_slot;
    let proposed_clock = SlotClock::from_state_fields(
        proposed_slots,
        state.slot_duration_ms,
        state.pending_slot_duration_ms,
        state.slot_duration_effective_slot,
    );
    validate!(
        proposed_clock.slot_duration_at(now_slot).as_ms() <= current_duration_ms,
        ErrorCode::SlotDurationSyncRegresses,
        "IBRL synchronization cannot regress the active slot duration"
    )?;
    Ok(proposed_slots)
}

/// Synchronizes one IBRL transition from its feature account. The instruction
/// is permissionless. The feature key, the feature program ownership, the
/// serialized activation slot and the cluster `EpochSchedule` sysvar fix every
/// value it accepts.
pub fn handle_sync_state_slot_duration(ctx: Context<SyncStateSlotDuration>) -> Result<()> {
    let now_slot = Clock::get()?.slot;
    let epoch_schedule = EpochSchedule::get()?;
    let feature_account = ctx.accounts.feature_gate.to_account_info();
    let slot_duration_ms =
        ibrl_slot_duration_ms(feature_account.key).ok_or(ErrorCode::DefaultError)?;
    let transition_index =
        slot_duration_transition_index(slot_duration_ms).ok_or(ErrorCode::DefaultError)?;
    let effective_slot = feature_gate_effective_slot(&feature_account, &epoch_schedule)?;
    let mut state = ctx.accounts.state.load_mut()?;

    let recorded = state.slot_duration_transition_slots[transition_index];
    if recorded != 0 {
        validate!(
            recorded == effective_slot,
            ErrorCode::SlotDurationTransitionInvalid,
            "IBRL transition already recorded at {}, feature account resolves to {}",
            recorded,
            effective_slot
        )?;
        return Ok(());
    }

    // A permissionless caller may backfill a missing historical transition,
    // but adding it must never make the clock at the current slot slower. This
    // also closes migration from a legacy state already at 300/250/200ms: sync
    // the currently active (fastest) gate first, then backfill older gates.
    let proposed_slots =
        validated_slot_duration_archive_update(&state, transition_index, effective_slot, now_slot)?;
    let proposed_clock = SlotClock::from_state_fields(
        proposed_slots,
        state.slot_duration_ms,
        state.pending_slot_duration_ms,
        state.slot_duration_effective_slot,
    );
    msg!(
        "slot_duration_ms: synchronized {}ms effective at slot {}",
        slot_duration_ms,
        effective_slot
    );
    state.slot_duration_transition_slots = proposed_slots;

    // Keep the legacy staging fields coherent for old readers. Archive readers
    // apply every boundary themselves. Old readers get the active duration and
    // the earliest known future boundary. Stale legacy staging never blocks a
    // repair of the canonical archive.
    state.slot_duration_ms = proposed_clock.slot_duration_at(now_slot).as_ms() as u16;
    let next_future = proposed_slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| **slot > now_slot)
        .min_by_key(|(_, slot)| **slot);
    if let Some((index, next_slot)) = next_future {
        state.pending_slot_duration_ms = SLOT_DURATION_TRANSITION_MS[index];
        state.slot_duration_effective_slot = *next_slot;
    } else {
        state.pending_slot_duration_ms = 0;
        state.slot_duration_effective_slot = 0;
    }
    Ok(())
}

pub fn handle_update_state_max_number_of_sub_accounts(
    ctx: Context<AdminUpdateState>,
    max_number_of_sub_accounts: u16,
) -> Result<()> {
    msg!(
        "max_number_of_sub_accounts: {} -> {}",
        ctx.accounts.state.load()?.max_number_of_sub_accounts,
        max_number_of_sub_accounts
    );

    ctx.accounts.state.load_mut()?.max_number_of_sub_accounts = max_number_of_sub_accounts;
    Ok(())
}

pub fn handle_update_state_max_initialize_user_fee(
    ctx: Context<AdminUpdateState>,
    max_initialize_user_fee: u16,
) -> Result<()> {
    msg!(
        "max_initialize_user_fee: {} -> {}",
        ctx.accounts.state.load()?.max_initialize_user_fee,
        max_initialize_user_fee
    );

    ctx.accounts.state.load_mut()?.max_initialize_user_fee = max_initialize_user_fee;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_oracle(
    ctx: Context<AdminUpdatePerpMarketOracle>,
    oracle: Pubkey,
    oracle_source: OracleSource,
    skip_invariant_check: bool,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let amm_cache = &mut ctx.accounts.amm_cache;
    msg!("perp market {}", perp_market.market_index);

    let clock = Clock::get()?;

    validate_supported_market_oracle_source(oracle_source)?;

    OracleMap::validate_oracle_account_info(&ctx.accounts.oracle)?;

    validate!(
        ctx.accounts.oracle.key == &oracle,
        ErrorCode::DefaultError,
        "oracle account info ({:?}) and ix data ({:?}) must match",
        ctx.accounts.oracle.key,
        oracle
    )?;

    validate!(
        ctx.accounts.old_oracle.key == &perp_market.oracle,
        ErrorCode::DefaultError,
        "old oracle account info ({:?}) and perp market oracle ({:?}) must match",
        ctx.accounts.old_oracle.key,
        perp_market.oracle
    )?;

    // Verify new oracle is readable
    let OraclePriceData {
        price: new_oracle_price,
        delay: _oracle_delay,
        ..
    } = get_oracle_price(&oracle_source, &ctx.accounts.oracle, clock.slot)?;

    msg!(
        "perp_market.oracle: {:?} -> {:?}",
        perp_market.oracle,
        oracle
    );

    msg!(
        "perp_market.oracle_source: {:?} -> {:?}",
        perp_market.oracle_source,
        oracle_source
    );

    let OraclePriceData {
        price: old_oracle_price,
        ..
    } = get_oracle_price(
        &perp_market.oracle_source,
        &ctx.accounts.old_oracle,
        clock.slot,
    )?;

    msg!(
        "Oracle Price: {:?} -> {:?}",
        old_oracle_price,
        new_oracle_price
    );

    if !skip_invariant_check {
        validate!(
            new_oracle_price > 0,
            ErrorCode::DefaultError,
            "invalid oracle price, must be greater than 0"
        )?;

        let oracle_change_divergence = new_oracle_price
            .safe_sub(old_oracle_price)?
            .safe_mul(PERCENTAGE_PRECISION_I64)?
            .safe_div(old_oracle_price)?;

        validate!(
            oracle_change_divergence.abs() < (PERCENTAGE_PRECISION_I64 / 10),
            ErrorCode::DefaultError,
            "invalid new oracle price, more than 10% divergence"
        )?;
    }

    perp_market.oracle = oracle;
    perp_market.oracle_source = oracle_source;

    if amm_cache
        .cache
        .iter()
        .any(|cache_info| cache_info.market_index == perp_market.market_index)
    {
        amm_cache.update_perp_market_fields(perp_market)?;
    }

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

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_max_open_interest(
    ctx: Context<AdminUpdatePerpMarket>,
    max_open_interest: u128,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        is_multiple_of_step_size(
            max_open_interest.cast::<u64>()?,
            perp_market.order_step_size
        )?,
        ErrorCode::DefaultError,
        "max oi not a multiple of the step size"
    )?;

    msg!(
        "perp_market.max_open_interest: {:?} -> {:?}",
        perp_market.max_open_interest,
        max_open_interest
    );

    perp_market.max_open_interest = max_open_interest;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_fee_adjustment(
    ctx: Context<AdminUpdatePerpMarket>,
    fee_adjustment: i16,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        fee_adjustment.unsigned_abs().cast::<u64>()? <= FEE_ADJUSTMENT_MAX,
        ErrorCode::DefaultError,
        "fee adjustment {} greater than max {}",
        fee_adjustment,
        FEE_ADJUSTMENT_MAX
    )?;

    msg!(
        "perp_market.fee_adjustment: {:?} -> {:?}",
        perp_market.fee_adjustment,
        fee_adjustment
    );

    perp_market.fee_adjustment = fee_adjustment;
    Ok(())
}

pub fn handle_update_perp_market_taker_fee_addon(
    ctx: Context<AdminUpdatePerpMarket>,
    taker_fee_addon_tenth_bps: u16,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        taker_fee_addon_tenth_bps <= MAX_TAKER_FEE_ADDON_TENTH_BPS,
        ErrorCode::InvalidFeeStructure,
        "taker fee addon {} greater than max {}",
        taker_fee_addon_tenth_bps,
        MAX_TAKER_FEE_ADDON_TENTH_BPS
    )?;

    msg!(
        "perp_market.taker_fee_addon_tenth_bps: {:?} -> {:?}",
        perp_market.taker_fee_addon_tenth_bps,
        taker_fee_addon_tenth_bps
    );

    perp_market.taker_fee_addon_tenth_bps = taker_fee_addon_tenth_bps;
    Ok(())
}

pub fn handle_update_perp_market_fee_pool_buffer_target(
    ctx: Context<AdminUpdatePerpMarket>,
    fee_pool_buffer_target: u64,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.fee_pool_buffer_target: {:?} -> {:?}",
        perp_market.fee_pool_buffer_target,
        fee_pool_buffer_target
    );

    perp_market.fee_pool_buffer_target = fee_pool_buffer_target;
    Ok(())
}

/// Sets the market's `bankruptcy_if_floor_pct`. That is the fraction of
/// open-interest notional the fee sweep must leave in `pending_if_fee` as a
/// standing bankruptcy tranche, in `PERCENTAGE_PRECISION`. A value of `0`
/// selects `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT`. `BANKRUPTCY_IF_FLOOR_DISABLED`
/// turns the standing floor off. Turning it off does not expose a latched
/// bankruptcy, because `pending_bankruptcy_claims` still freezes the sweep
/// until the debt resolves.
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
pub fn handle_update_perp_market_oracle_low_risk_slot_delay_override(
    ctx: Context<AdminUpdatePerpMarket>,
    oracle_low_risk_slot_delay_override: i8,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.oracle_low_risk_slot_delay_override: {:?} -> {:?}",
        perp_market.oracle_low_risk_slot_delay_override,
        oracle_low_risk_slot_delay_override
    );

    perp_market.oracle_low_risk_slot_delay_override = oracle_low_risk_slot_delay_override;
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_oracle_slot_delay_override(
    ctx: Context<AdminUpdatePerpMarket>,
    oracle_slot_delay_override: i8,
) -> Result<()> {
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    msg!(
        "perp_market.oracle_slot_delay_override: {:?} -> {:?}",
        perp_market.oracle_slot_delay_override,
        oracle_slot_delay_override
    );

    perp_market.oracle_slot_delay_override = oracle_slot_delay_override;
    Ok(())
}

#[access_control(
    spot_market_valid(&ctx.accounts.spot_market)
)]
pub fn handle_update_spot_market_fee_adjustment(
    ctx: Context<AdminUpdateSpotMarket>,
    fee_adjustment: i16,
) -> Result<()> {
    let spot = &mut load_mut!(ctx.accounts.spot_market)?;
    msg!("spot market {}", spot.market_index);

    validate!(
        fee_adjustment.unsigned_abs().cast::<u64>()? <= FEE_ADJUSTMENT_MAX,
        ErrorCode::DefaultError,
        "fee adjustment {} greater than max {}",
        fee_adjustment,
        FEE_ADJUSTMENT_MAX
    )?;

    msg!(
        "spot_market.fee_adjustment: {:?} -> {:?}",
        spot.fee_adjustment,
        fee_adjustment
    );

    spot.fee_adjustment = fee_adjustment;
    Ok(())
}

pub fn handle_update_admin(ctx: Context<ColdAdminUpdateState>, admin: Pubkey) -> Result<()> {
    msg!(
        "admin: {:?} -> {:?}",
        ctx.accounts.state.load()?.cold_admin,
        admin
    );
    ctx.accounts.state.load_mut()?.cold_admin = admin;
    Ok(())
}

pub fn handle_update_whitelist_mint(
    ctx: Context<AdminUpdateState>,
    whitelist_mint: Pubkey,
) -> Result<()> {
    msg!(
        "whitelist_mint: {:?} -> {:?}",
        ctx.accounts.state.load()?.whitelist_mint,
        whitelist_mint
    );

    ctx.accounts.state.load_mut()?.whitelist_mint = whitelist_mint;
    Ok(())
}

pub fn handle_update_discount_mint(
    ctx: Context<AdminUpdateState>,
    discount_mint: Pubkey,
) -> Result<()> {
    msg!(
        "discount_mint: {:?} -> {:?}",
        ctx.accounts.state.load()?.discount_mint,
        discount_mint
    );

    ctx.accounts.state.load_mut()?.discount_mint = discount_mint;
    Ok(())
}

pub fn handle_update_exchange_status(
    ctx: Context<PauseAdminUpdateState>,
    exchange_status: u8,
) -> Result<()> {
    let signer = ctx.accounts.admin.key();
    let mut state = ctx.accounts.state.load_mut()?;
    require_pause_only_added(&signer, &state, state.exchange_status, exchange_status)?;
    msg!(
        "exchange_status: {:?} -> {:?}",
        state.exchange_status,
        exchange_status
    );
    state.exchange_status = exchange_status;
    Ok(())
}

pub fn handle_update_solvency_status(
    ctx: Context<ColdAdminUpdateState>,
    solvency_status: u8,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    msg!(
        "solvency_status: {:?} -> {:?}",
        state.solvency_status,
        solvency_status
    );
    state.solvency_status = solvency_status;
    Ok(())
}

pub fn handle_update_perp_auction_duration(
    ctx: Context<AdminUpdateState>,
    min_perp_auction_duration: u8,
) -> Result<()> {
    msg!(
        "min_perp_auction_duration: {:?} -> {:?}",
        ctx.accounts.state.load()?.min_perp_auction_duration,
        min_perp_auction_duration
    );

    ctx.accounts.state.load_mut()?.min_perp_auction_duration =
        legacy_slot_duration_u8(min_perp_auction_duration);
    Ok(())
}

pub fn handle_update_spot_auction_duration(
    ctx: Context<AdminUpdateState>,
    default_spot_auction_duration: u8,
) -> Result<()> {
    msg!(
        "default_spot_auction_duration: {:?} -> {:?}",
        ctx.accounts.state.load()?.default_spot_auction_duration,
        default_spot_auction_duration
    );

    ctx.accounts.state.load_mut()?.default_spot_auction_duration = default_spot_auction_duration;
    Ok(())
}

pub fn handle_admin_update_user_stats_paused_operations(
    ctx: Context<PauseAdminUpdateUserStats>,
    paused_operations: u8,
) -> Result<()> {
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;

    // Authority matrix for user_stats.paused_operations:
    //   * cold / warm / hot_user_flag — full control (pause + unpause)
    //   * pause_admin                 — pause-only (may not clear bits)
    //
    // `is_hot(.., UserFlag)` already returns true for cold/warm; the negation
    // therefore isolates pause_admin specifically.
    let signer = ctx.accounts.admin.key();
    let state = ctx.accounts.state.load()?;
    if !state.is_hot(&signer, HotRole::UserFlag) {
        validate!(
            (user_stats.paused_operations & paused_operations) == user_stats.paused_operations,
            ErrorCode::Unauthorized,
            "pause_admin may not clear pause bits",
        )?;
    }
    drop(state);

    msg!(
        "user_stats.paused_operations: {:?} -> {:?}",
        user_stats.paused_operations,
        paused_operations
    );

    user_stats.paused_operations = paused_operations;
    Ok(())
}

pub fn handle_initialize_prelaunch_oracle(
    ctx: Context<InitializePrelaunchOracle>,
    params: PrelaunchOracleParams,
) -> Result<()> {
    let mut oracle = ctx.accounts.prelaunch_oracle.load_init()?;
    msg!("perp market {}", params.perp_market_index);

    oracle.perp_market_index = params.perp_market_index;
    if let Some(price) = params.price {
        oracle.price = price;
    }
    if let Some(max_price) = params.max_price {
        oracle.max_price = max_price;
    }

    oracle.validate()?;

    Ok(())
}

pub fn handle_update_prelaunch_oracle_params(
    ctx: Context<UpdatePrelaunchOracleParams>,
    params: PrelaunchOracleParams,
) -> Result<()> {
    let mut oracle = ctx.accounts.prelaunch_oracle.load_mut()?;
    let mut perp_market = ctx.accounts.perp_market.load_mut()?;
    msg!("perp market {}", perp_market.market_index);

    let now = Clock::get()?.unix_timestamp;

    if let Some(price) = params.price {
        oracle.price = price;

        msg!("before mark twap ts = {:?} mark twap = {:?} mark twap 5min = {:?} bid twap = {:?} ask twap {:?}", perp_market.market_stats.last_mark_price_twap_ts, perp_market.market_stats.last_mark_price_twap, perp_market.market_stats.last_mark_price_twap_5min, perp_market.market_stats.last_bid_price_twap, perp_market.market_stats.last_ask_price_twap);

        perp_market.market_stats.last_mark_price_twap_ts = now;
        perp_market.market_stats.last_mark_price_twap = price.cast()?;
        perp_market.market_stats.last_mark_price_twap_5min = price.cast()?;
        perp_market.market_stats.last_bid_price_twap = perp_market
            .market_stats
            .last_bid_price_twap
            .min(price.cast()?);
        perp_market.market_stats.last_ask_price_twap = perp_market
            .market_stats
            .last_ask_price_twap
            .max(price.cast()?);

        msg!("after mark twap ts = {:?} mark twap = {:?} mark twap 5min = {:?} bid twap = {:?} ask twap {:?}", perp_market.market_stats.last_mark_price_twap_ts, perp_market.market_stats.last_mark_price_twap, perp_market.market_stats.last_mark_price_twap_5min, perp_market.market_stats.last_bid_price_twap, perp_market.market_stats.last_ask_price_twap);
    } else {
        msg!("mark twap ts, mark twap, mark twap 5min, bid twap, ask twap: unchanged");
    }

    if let Some(max_price) = params.max_price {
        msg!("max price: {:?} -> {:?}", oracle.max_price, max_price);
        oracle.max_price = max_price;
    } else {
        msg!("max price: unchanged")
    }

    oracle.validate()?;

    Ok(())
}

pub fn handle_delete_prelaunch_oracle(
    ctx: Context<DeletePrelaunchOracle>,
    _perp_market_index: u16,
) -> Result<()> {
    let perp_market = ctx.accounts.perp_market.load()?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        perp_market.oracle != ctx.accounts.prelaunch_oracle.key(),
        ErrorCode::DefaultError,
        "prelaunch oracle currently in use"
    )?;

    Ok(())
}

pub fn handle_initialize_pyth_lazer_oracle(
    ctx: Context<InitPythLazerOracle>,
    feed_id: u32,
) -> Result<()> {
    let pubkey = ctx.accounts.lazer_oracle.to_account_info().key;
    msg!(
        "Lazer price feed initted {} with feed_id {}",
        pubkey,
        feed_id
    );
    Ok(())
}

pub fn handle_settle_expired_market<'c: 'info, 'info>(
    ctx: Context<'info, AdminUpdatePerpMarket<'info>>,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let _now = clock.unix_timestamp;
    let state = ctx.accounts.state.load()?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &get_writable_perp_market_set(market_index),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // Refresh PerpMarket-level oracle stats only — settle_expired_market
    // reads `market.market_stats.historical_oracle_data` for the expiry
    // price, not AMM peg or reserves. The AMM refresh that used to fire
    // here was cargo-cult.
    {
        let mut perp_market = maps.perp_market_map.get_ref_mut(&market_index)?;
        let oracle_price_data = maps.oracle_map.get_price_data(&perp_market.oracle_id())?;
        let mm_oracle_price_data = perp_market.get_mm_oracle_price_data(
            *oracle_price_data,
            clock.slot,
            &state.oracle_guard_rails.validity,
            state.slot_clock(),
        )?;
        let validity = crate::vlp::amm::refresh::compute_amm_refresh_validity(
            &perp_market,
            &mm_oracle_price_data,
            &state,
            clock.slot,
        )?;
        perp_market.update_oracle_derived_stats(
            &mm_oracle_price_data,
            validity,
            clock.unix_timestamp,
            clock.slot,
            state.slot_clock(),
        )?;
    }

    crate::vlp::amm::refresh::settle_expired_market(market_index, &mut maps, &state, &clock)?;

    Ok(())
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
)]
pub fn handle_admin_deposit<'c: 'info, 'info>(
    ctx: Context<'info, AdminDeposit<'info>>,
    market_index: u16,
    amount: u64,
) -> Result<()> {
    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set(market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    if amount == 0 {
        return Err(ErrorCode::InsufficientDeposit.into());
    }

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    let mut spot_market = maps.spot_market_map.get_ref_mut(&market_index)?;
    let oracle_price_data = *maps.oracle_map.get_price_data(&spot_market.oracle_id())?;

    validate!(
        user.pool_id == spot_market.pool_id,
        ErrorCode::InvalidPoolId,
        "user pool id ({}) != market pool id ({})",
        user.pool_id,
        spot_market.pool_id
    )?;

    validate!(
        !matches!(spot_market.status, MarketStatus::Initialized),
        ErrorCode::MarketBeingInitialized,
        "Market is being initialized"
    )?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        &mut spot_market,
        Some(&oracle_price_data),
        now,
        state.funding_paused()?,
    )?;

    let position_index = user.force_get_spot_position_index(spot_market.market_index)?;

    // if reduce only, have to compare ix amount to current borrow amount
    let amount = if (spot_market.is_reduce_only())
        && user.spot_positions[position_index].balance_type == SpotBalanceType::Borrow
    {
        user.spot_positions[position_index]
            .get_token_amount(&spot_market)?
            .cast::<u64>()?
            .min(amount)
    } else {
        amount
    };

    let total_deposits_after = user.total_deposits;
    let total_withdraws_after = user.total_withdraws;

    let spot_position = &mut user.spot_positions[position_index];
    controller::spot_position::update_spot_balances_and_cumulative_deposits(
        amount as u128,
        &SpotBalanceType::Deposit,
        &mut spot_market,
        spot_position,
        false,
        None,
    )?;

    let token_amount = spot_position.get_token_amount(&spot_market)?;
    if token_amount == 0 {
        validate!(
            spot_position.scaled_balance == 0,
            ErrorCode::InvalidSpotPosition,
            "deposit left user with invalid position. scaled balance = {} token amount = {}",
            spot_position.scaled_balance,
            token_amount
        )?;
    }

    if spot_position.balance_type == SpotBalanceType::Deposit && spot_position.scaled_balance > 0 {
        validate!(
            matches!(spot_market.status, MarketStatus::Active),
            ErrorCode::MarketActionPaused,
            "spot_market not active",
        )?;
    }

    drop(spot_market);

    user.update_last_active_slot(slot);

    let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
    let user_token_amount_after = user.get_total_token_amount(spot_market)?;

    controller::token::receive(
        &ctx.accounts.token_program,
        &ctx.accounts.admin_token_account,
        &ctx.accounts.spot_market_vault,
        &ctx.accounts.admin,
        amount,
        &mint,
        if spot_market.has_transfer_hook() {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;
    ctx.accounts.spot_market_vault.reload()?;
    validate_spot_market_vault_amount(spot_market, ctx.accounts.spot_market_vault.amount)?;

    let deposit_record_id = get_then_update_id!(spot_market, next_deposit_record_id);
    let oracle_price = oracle_price_data.price;
    let deposit_record = DepositRecord {
        ts: now,
        deposit_record_id,
        user_authority: user.authority,
        user: user_key,
        direction: DepositDirection::Deposit,
        amount,
        oracle_price,
        market_deposit_balance: spot_market.deposit_balance,
        market_withdraw_balance: spot_market.borrow_balance,
        market_cumulative_deposit_interest: spot_market.cumulative_deposit_interest,
        market_cumulative_borrow_interest: spot_market.cumulative_borrow_interest,
        total_deposits_after,
        total_withdraws_after,
        market_index,
        explanation: DepositExplanation::Reward,
        transfer_user: None,
        signer: Some(ctx.accounts.admin.key()),
        user_token_amount_after,
    };
    emit!(deposit_record);

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    Ok(())
}

pub fn handle_zero_mm_oracle_fields(ctx: Context<AdminUpdatePerpMarket>) -> Result<()> {
    let mut perp_market = load_mut!(ctx.accounts.perp_market)?;
    perp_market.market_stats.mm_oracle_price = 0;
    perp_market.market_stats.mm_oracle_sequence_id = 0;
    perp_market.market_stats.mm_oracle_slot = 0;
    Ok(())
}

/// Byte offset of `State::feature_bit_flags` from the start of the account data
/// (including the 8-byte Anchor discriminator). The native handlers read it by
/// raw index rather than deserializing all of `State`. Guarded by
/// `state/traits/tests.rs::native_instruction_offsets`.
const STATE_FEATURE_BIT_FLAGS_OFFSET: usize = 1374;

/// Byte offset of `State::hot_mm_oracle_crank` (32 bytes) from the start of the
/// account data. Same guard as above. Only read outside `anchor-test`, which
/// compiles the signer checks out.
#[cfg_attr(feature = "anchor-test", allow(dead_code))]
const STATE_HOT_MM_ORACLE_CRANK_OFFSET: usize = 360;

/// Byte offset of `State::slot_duration_ms` (u16 LE) from the start of the
/// account data. Same guard as above. The native MM-oracle handlers read it to
/// scale the write-gap and source-age gates.
const STATE_SLOT_DURATION_MS_OFFSET: usize = 1506;
/// Byte offset of `State::pending_slot_duration_ms` (u16 LE). It holds the
/// staged next value, two bytes after `slot_duration_ms`.
const STATE_PENDING_SLOT_DURATION_MS_OFFSET: usize = 1508;
/// Byte offset of `State::slot_duration_effective_slot` (u64 LE). It holds the
/// slot the staged switch takes effect at. The field is 8-aligned, four bytes
/// after the pending u16.
const STATE_SLOT_DURATION_EFFECTIVE_SLOT_OFFSET: usize = 1512;
/// Byte offset of `State::slot_duration_transition_slots` (`[u64; 4]` LE).
const STATE_SLOT_DURATION_TRANSITION_SLOTS_OFFSET: usize = 1520;

/// Read the full clock from a raw, already discriminator-checked State account.
fn read_native_state_slot_clock(state_account: &AccountInfo) -> Result<SlotClock> {
    let state = state_account.try_borrow_data()?;
    let read_u16 = |off: usize| -> Result<u16> {
        let bytes: [u8; 2] = state
            .get(off..off + 2)
            .ok_or(ErrorCode::InvalidNativeStateAccount)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
        Ok(u16::from_le_bytes(bytes))
    };
    let base = read_u16(STATE_SLOT_DURATION_MS_OFFSET)?;
    let pending = read_u16(STATE_PENDING_SLOT_DURATION_MS_OFFSET)?;
    let effective_bytes: [u8; 8] = state
        .get(
            STATE_SLOT_DURATION_EFFECTIVE_SLOT_OFFSET
                ..STATE_SLOT_DURATION_EFFECTIVE_SLOT_OFFSET + 8,
        )
        .ok_or(ErrorCode::InvalidNativeStateAccount)?
        .try_into()
        .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
    let effective_slot = u64::from_le_bytes(effective_bytes);
    let mut transition_slots = [0u64; 4];
    for (i, transition_slot) in transition_slots.iter_mut().enumerate() {
        let off = STATE_SLOT_DURATION_TRANSITION_SLOTS_OFFSET + i * 8;
        let bytes: [u8; 8] = state
            .get(off..off + 8)
            .ok_or(ErrorCode::InvalidNativeStateAccount)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
        *transition_slot = u64::from_le_bytes(bytes);
    }
    Ok(SlotClock::from_state_fields(
        transition_slots,
        base,
        pending,
        effective_slot,
    ))
}

pub fn handle_update_mm_oracle_native(accounts: &[AccountInfo], data: &[u8]) -> Result<()> {
    // The slot comes from the Clock sysvar syscall. There is no clock account
    // for a caller to forge, and the transaction carries one account fewer.
    update_mm_oracle(accounts, data, Clock::get()?.slot)
}

/// Body of `handle_update_mm_oracle_native`, native dispatch opcode 0. It is
/// split from the syscall so tests can drive the slot directly.
///
/// This runs before Anchor, so it re-establishes the ownership and
/// discriminator guarantees Anchor would provide before it trusts any byte.
/// `crate::auth::require_native_account` does that work. The accounts are
/// `[0]` perp_market (mut), `[1]` signer, `[2]` state. The payload is
/// `i64 price | u64 sequence_id | u64 source_slot`, all little-endian, 24
/// bytes. The `STATE_*_OFFSET` constants above give the state byte offsets, and
/// `state/traits/tests.rs::native_instruction_offsets` guards them.
///
/// Every index is bounds-checked before use. A malformed instruction arrives
/// verbatim here, and a short account list or payload would panic. A panic
/// aborts the transaction with no identifiable error and burns the whole
/// compute budget.
///
/// After authentication the per-market gating is `apply_mm_oracle_update`, the
/// same core the batch handler at opcode 2 runs. The differences are all in
/// this prologue. A non-positive price is a hard `Err` here, where the batch
/// skips the entry, because a hard error there would destroy every other
/// market's write. The payload carries no market index to cross-check. Skips
/// are logged per reason, where the batch logs one reject mask.
fn update_mm_oracle(accounts: &[AccountInfo], data: &[u8], current_slot: u64) -> Result<()> {
    require!(accounts.len() >= 3, ErrorCode::InvalidNativeInstructionData);
    require!(data.len() >= 24, ErrorCode::InvalidNativeInstructionData);

    crate::auth::require_native_account(
        &accounts[2],
        State::DISCRIMINATOR,
        ErrorCode::InvalidNativeStateAccount,
    )?;
    crate::auth::require_native_account(
        &accounts[0],
        PerpMarket::DISCRIMINATOR,
        ErrorCode::InvalidNativePerpMarketAccount,
    )?;

    {
        let state = accounts[2].try_borrow_data()?;
        // The admin can disable this instruction through `feature_bit_flags`.
        // The check returns a typed error rather than panicking, so the reason
        // arrives as a code instead of "Program failed to complete".
        let feature_bit_flags = *state
            .get(STATE_FEATURE_BIT_FLAGS_OFFSET)
            .ok_or(ErrorCode::InvalidNativeStateAccount)?;
        require!(
            feature_bit_flags & (FeatureBitFlags::MmOracleUpdate as u8) > 0,
            ErrorCode::MmOracleUpdateDisabled
        );

        #[cfg(not(feature = "anchor-test"))]
        {
            let signer_account = &accounts[1];
            let hot_key_bytes: [u8; 32] = state
                .get(STATE_HOT_MM_ORACLE_CRANK_OFFSET..STATE_HOT_MM_ORACLE_CRANK_OFFSET + 32)
                .ok_or(ErrorCode::InvalidNativeStateAccount)?
                .try_into()
                .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
            let hot_key = anchor_lang::prelude::Pubkey::new_from_array(hot_key_bytes);
            require!(
                signer_account.is_signer && *signer_account.key == hot_key,
                ErrorCode::Unauthorized
            );
        }
    }

    // Non-positive prices are a hard error. A zero-only check would leave a
    // hole: the step cap clamps a negative target instead of skipping it, so
    // repeated negatives could walk the stored price to zero and reset the
    // bootstrap path and the step cap with it.
    let incoming_price = i64::from_le_bytes(data[0..8].try_into().unwrap());
    validate!(
        incoming_price > 0,
        ErrorCode::OracleNonPositive,
        "MM oracle price is non-positive, not updating"
    )?;
    let incoming_sequence_id = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let source_slot = u64::from_le_bytes(data[16..24].try_into().unwrap());

    match apply_mm_oracle_update(
        &accounts[0],
        None,
        current_slot,
        incoming_price,
        incoming_sequence_id,
        source_slot,
        read_native_state_slot_clock(&accounts[2])?,
    )? {
        MmOracleUpdateOutcome::Written { price } => {
            if price != incoming_price {
                msg!(
                    "mm oracle step clamped: incoming={} written={}",
                    incoming_price,
                    price
                );
            }
        }

        // A stale sequence id is the crank's ordinary redundant send, so it
        // stays silent. The other reasons are logged with their values. The
        // batch handler logs a mask instead.
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::StaleSequenceId) => {}
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::SlotNotAdvanced { stored_slot }) => {
            msg!(
                "mm oracle reject: stale slot {} <= {}",
                current_slot,
                stored_slot
            );
        }
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::RecrankGapTooSmall { gap, min_gap }) => {
            msg!(
                "mm oracle reject: re-crank gap {} slots < {} slots",
                gap,
                min_gap
            );
        }
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::SourceSlotOutOfRange {
            source_slot,
        }) => {
            msg!(
                "mm oracle reject: source slot {} out of range at slot {}",
                source_slot,
                current_slot
            );
        }

        // The hard error above makes this unreachable. The match stays
        // exhaustive so a new skip reason cannot pass unnoticed.
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::NonPositivePrice) => {
            msg!("MM oracle price is non-positive, not updating");
        }
    }

    Ok(())
}

/// Maximum markets one batch may carry. It bounds the reject-mask width, which
/// is a `u64`, and the worst-case compute of a single instruction. The
/// transaction packet size and the runtime's 64 account-lock ceiling both bind
/// before this does.
const MM_ORACLE_BATCH_MAX_MARKETS: usize = 64;

// `rejected_mask` is a u64 indexed by entry position, so the batch can never
// carry more entries than the mask has bits.
static_assertions::const_assert!(MM_ORACLE_BATCH_MAX_MARKETS <= u64::BITS as usize);

/// Fixed (non-market) accounts at the head of the batch account list.
const MM_ORACLE_BATCH_FIXED_ACCOUNTS: usize = 2;

/// Bytes per market entry in the batch payload: `u16` market index + `i64` price
/// + `u64` sequence id + `u64` source slot.
const MM_ORACLE_BATCH_ENTRY_LEN: usize = 26;

/// Batches MM oracle price writes for many perp markets into one native instruction,
/// opcode 2, paying the auth prologue once instead of once per market.
///
/// allow-verbose: this is the wire format of a native entrypoint. A client builds the
/// bytes from this comment, and the dispatcher below is short because it delegates, so
/// the length of the body is not the measure of what the interface needs.
///
/// Accounts are `[0]` the signer, which must equal `State::hot_mm_oracle_crank`, `[1]`
/// state, then `[2..2+n]` the perp markets in payload order, writable. The payload after
/// the 5-byte native prefix is a `u8` entry count of 1 to
/// [`MM_ORACLE_BATCH_MAX_MARKETS`], then that many
/// [`MM_ORACLE_BATCH_ENTRY_LEN`]-byte entries.
///
/// Opcode 0 takes `[market, signer, state]` and this takes `[signer, state, markets..]`,
/// because the variable-length region has to sit last. Both confusions fail closed.
/// `bun run bench:native-cu` measures 1627 CU at one market, 2145 at two, 3181 at four.
/// [`update_mm_oracle_batch`] holds the accept, skip and clamp rules.
pub fn handle_update_mm_oracle_batch_native(accounts: &[AccountInfo], data: &[u8]) -> Result<()> {
    // The slot comes from the Clock sysvar syscall. There is no clock account
    // for a caller to forge, and one more market fits the transaction.
    let (rejected_mask, clamped_mask) = update_mm_oracle_batch(accounts, data, Clock::get()?.slot)?;

    // One log per non-zero mask, so a batch with nothing to report pays nothing
    // for logging. A formatted `msg!` measured about 700 CU on the single-market
    // handler's reject paths, which is why this is not emitted per market.
    if rejected_mask != 0 {
        msg!("mm oracle batch: rejected mask {:#x}", rejected_mask);
    }
    if clamped_mask != 0 {
        msg!("mm oracle batch: clamped mask {:#x}", clamped_mask);
    }

    Ok(())
}

/// Body of `handle_update_mm_oracle_batch_native`. It is split from the syscall
/// so tests can drive the slot directly and assert the exact bitmasks instead of
/// inferring them from market state. It returns `(rejected_mask, clamped_mask)`.
/// Bit `i` of the first is set when entry `i` was skipped. Bit `i` of the second
/// is set when entry `i` landed and its price was clamped to the step cap.
fn update_mm_oracle_batch(
    accounts: &[AccountInfo],
    data: &[u8],
    current_slot: u64,
) -> Result<(u64, u64)> {
    // Payload framing. Validate before indexing anything. Opcode 0 slices its
    // payload without a length check and panics on malformed input. A handler
    // whose loop bound comes from the caller must not repeat that.
    let n = *data
        .first()
        .ok_or(ErrorCode::InvalidNativeInstructionData)? as usize;
    require!(
        n > 0 && n <= MM_ORACLE_BATCH_MAX_MARKETS,
        ErrorCode::InvalidNativeInstructionData
    );
    require!(
        data.len() == 1 + n * MM_ORACLE_BATCH_ENTRY_LEN,
        ErrorCode::InvalidNativeInstructionData
    );
    require!(
        accounts.len() >= MM_ORACLE_BATCH_FIXED_ACCOUNTS + n,
        ErrorCode::InvalidNativeInstructionData
    );

    // Fixed prologue, paid once for the whole batch. Authenticate the state
    // account before any raw byte read, which is the rule `auth.rs` states.
    let state_account = &accounts[1];
    crate::auth::require_native_account(
        state_account,
        State::DISCRIMINATOR,
        ErrorCode::InvalidNativeStateAccount,
    )?;
    let slot_clock = read_native_state_slot_clock(state_account)?;

    {
        let state = state_account.try_borrow_data()?;

        // Kill switch. Typed error so the failure is identifiable by code.
        let feature_bit_flags = *state
            .get(STATE_FEATURE_BIT_FLAGS_OFFSET)
            .ok_or(ErrorCode::InvalidNativeStateAccount)?;
        require!(
            feature_bit_flags & (FeatureBitFlags::MmOracleUpdate as u8) > 0,
            ErrorCode::MmOracleUpdateDisabled
        );

        #[cfg(not(feature = "anchor-test"))]
        {
            let signer_account = &accounts[0];
            let hot_key_bytes: [u8; 32] = state
                .get(STATE_HOT_MM_ORACLE_CRANK_OFFSET..STATE_HOT_MM_ORACLE_CRANK_OFFSET + 32)
                .ok_or(ErrorCode::InvalidNativeStateAccount)?
                .try_into()
                .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
            require!(
                signer_account.is_signer
                    && *signer_account.key
                        == anchor_lang::prelude::Pubkey::new_from_array(hot_key_bytes),
                ErrorCode::Unauthorized
            );
        }
    }

    let mut rejected_mask: u64 = 0;
    let mut clamped_mask: u64 = 0;

    for i in 0..n {
        let market_account = &accounts[MM_ORACLE_BATCH_FIXED_ACCOUNTS + i];

        crate::auth::require_native_account(
            market_account,
            PerpMarket::DISCRIMINATOR,
            ErrorCode::InvalidNativePerpMarketAccount,
        )?;
        // A read-only market would make the runtime fail the whole transaction
        // at the end with an opaque "readonly data modified" error. Catch the
        // builder mistake here instead, with a code that names the account.
        require!(
            market_account.is_writable,
            ErrorCode::InvalidNativePerpMarketAccount
        );

        // In range: `data.len() == 1 + n * ENTRY_LEN` was checked above and
        // `i < n`.
        let entry = 1 + i * MM_ORACLE_BATCH_ENTRY_LEN;
        let market_index = u16::from_le_bytes(data[entry..entry + 2].try_into().unwrap());
        let incoming_price = i64::from_le_bytes(data[entry + 2..entry + 10].try_into().unwrap());
        let incoming_sequence_id =
            u64::from_le_bytes(data[entry + 10..entry + 18].try_into().unwrap());
        let source_slot = u64::from_le_bytes(data[entry + 18..entry + 26].try_into().unwrap());

        // `i < n <= MM_ORACLE_BATCH_MAX_MARKETS`, which a `const_assert!`
        // bounds to at most `u64::BITS`, so the shifts are in range.
        match apply_mm_oracle_update(
            market_account,
            Some(market_index),
            current_slot,
            incoming_price,
            incoming_sequence_id,
            source_slot,
            slot_clock,
        )? {
            MmOracleUpdateOutcome::Written { price } => {
                if price != incoming_price {
                    clamped_mask |= 1u64 << i;
                }
            }
            MmOracleUpdateOutcome::Skipped(_) => {
                rejected_mask |= 1u64 << i;
            }
        }
    }

    Ok((rejected_mask, clamped_mask))
}

/// Why one per-market update was skipped. [`MmOracleUpdateOutcome::Skipped`]
/// carries it. The batch handler folds it into the reject bitmask, and opcode 0
/// logs it with its values.
enum MmOracleSkipReason {
    /// `oracle_validity` classifies a stored non-positive price as
    /// `NonPositive` on read, so storing one has no value. Opcode 0 checks this
    /// first with a hard error, so it only reaches a mask in the batch.
    NonPositivePrice,
    /// Sequence id not strictly greater than the stored one. This is the
    /// crank's ordinary redundant send.
    StaleSequenceId,
    /// Current slot not strictly greater than the stored slot.
    SlotNotAdvanced { stored_slot: u64 },
    /// Fewer slots since the last accepted write than `MM_ORACLE_MIN_WRITE_GAP` allows.
    RecrankGapTooSmall { gap: u64, min_gap: u64 },
    /// Source slot more than `MM_ORACLE_MAX_SOURCE_AGE` from the current
    /// slot in either direction.
    SourceSlotOutOfRange { source_slot: u64 },
}

/// Result of one per-market update attempt. `Written::price` is the price that
/// landed. It differs from the incoming price when the step cap clamped it.
/// Opcode 0 logs that price, and the batch handler sets the clamped bitmask from
/// it.
enum MmOracleUpdateOutcome {
    Written { price: i64 },
    Skipped(MmOracleSkipReason),
}

fn mm_oracle_source_slot_out_of_range(
    slot_clock: SlotClock,
    current_slot: u64,
    source_slot: u64,
) -> bool {
    if source_slot <= current_slot {
        slot_clock.elapsed(source_slot, current_slot) > MM_ORACLE_MAX_SOURCE_AGE
    } else {
        source_slot.saturating_sub(current_slot)
            > MM_ORACLE_MAX_SOURCE_AGE.to_slots(slot_clock.slot_duration_at(current_slot))
    }
}

/// Applies one MM oracle update to an already-authenticated perp market
/// account. This is the single copy of the per-market gating. Opcode 0, which is
/// `update_mm_oracle`, and the batch handler at opcode 2 both call it, so the
/// two cannot drift apart.
///
/// It returns [`MmOracleUpdateOutcome::Written`] with the price that landed,
/// which the step cap may have clamped, or [`MmOracleUpdateOutcome::Skipped`]
/// with the reason. A skip never aborts a batch. The failure model on
/// `handle_update_mm_oracle_batch_native` states the rule.
///
/// `expected_market_index` is `Some` for batch entries, whose payload names the
/// market it expects at each account position. A mismatch is a hard error rather
/// than a skip, because it means the account list and the payload are
/// misaligned. Opcode 0's payload carries no index and passes `None`.
///
/// # Safety contract
///
/// The caller must already have passed `market_account` through
/// `crate::auth::require_native_account(.., PerpMarket::DISCRIMINATOR, ..)`.
/// This function `bytemuck`-casts the account data. Without that check it would
/// reinterpret caller-chosen bytes as a `PerpMarket`.
///
/// The mutable borrow is scoped to this call, so the same market passed twice in
/// one batch cannot alias. The second occurrence re-borrows cleanly and then
/// stops at the slot check, because the first occurrence already advanced
/// `mm_oracle_slot` to `current_slot`.
fn apply_mm_oracle_update(
    market_account: &AccountInfo,
    expected_market_index: Option<u16>,
    current_slot: u64,
    incoming_price: i64,
    incoming_sequence_id: u64,
    source_slot: u64,
    slot_clock: SlotClock,
) -> Result<MmOracleUpdateOutcome> {
    use {MmOracleSkipReason as Skip, MmOracleUpdateOutcome as Outcome};

    let mut market_data = market_account.try_borrow_mut_data()?;
    let market_bytes = market_data
        .get_mut(8..8 + std::mem::size_of::<PerpMarket>())
        .ok_or(ErrorCode::InvalidNativePerpMarketAccount)?;
    let perp_market: &mut PerpMarket = bytemuck::from_bytes_mut(market_bytes);

    // Structural, so it runs before any skip condition. When the caller names
    // the market an entry is for, the account paired with that entry must agree.
    // A mismatch means the account list and the payload are misaligned. That is
    // a caller bug, and the handler must not absorb it.
    if let Some(expected) = expected_market_index {
        require!(
            perp_market.market_index == expected,
            ErrorCode::InvalidNativePerpMarketAccount
        );
    }

    let stats = &mut perp_market.market_stats;

    if incoming_price <= 0 {
        return Ok(Outcome::Skipped(Skip::NonPositivePrice));
    }

    if incoming_sequence_id <= stats.mm_oracle_sequence_id {
        return Ok(Outcome::Skipped(Skip::StaleSequenceId));
    }

    // Ordered before the subtraction below so the slot gap cannot underflow.
    if current_slot <= stats.mm_oracle_slot {
        return Ok(Outcome::Skipped(Skip::SlotNotAdvanced {
            stored_slot: stats.mm_oracle_slot,
        }));
    }

    // Both gates are wall-clock durations expressed in actual slots, so the
    // write rate limit and the source-age bound keep their width at any slot
    // duration. They must stay consistent with the `MM_ORACLE_MIN_WRITE_GAP`
    // fallback inside `oracle_validity`.
    let gap = current_slot - stats.mm_oracle_slot;
    // The rate limiter rounds the minimum accepted interval up, so the wall
    // clock gap is never shorter than intended. The immediate-fill staleness
    // fallback in `oracle_validity` rounds the same constant up, so its
    // accept threshold matches this write gate.
    let slot_duration = slot_clock.slot_duration_at(current_slot);
    let min_gap = MM_ORACLE_MIN_WRITE_GAP.to_slots_ceil(slot_duration);
    if gap < min_gap {
        return Ok(Outcome::Skipped(Skip::RecrankGapTooSmall { gap, min_gap }));
    }

    // Freshness of the source observation around the landing slot, bounded
    // in both directions. A late-landing update would read an old
    // observation as fresh without the lower bound. A wrong-unit value, such
    // as a millisecond timestamp, would silently disable the upper bound.
    if mm_oracle_source_slot_out_of_range(slot_clock, current_slot, source_slot) {
        return Ok(Outcome::Skipped(Skip::SourceSlotOutOfRange { source_slot }));
    }

    // The step cap is measured against the last accepted price. A step
    // beyond the cap is clamped rather than skipped, so a feed gap larger
    // than the cap converges over a few writes instead of freezing the
    // oracle at its pre-gap price. The cap floors at one price unit, so a
    // small enough price still makes progress. A stored price of zero
    // means the market is still bootstrapping, and the cap does not apply.
    let mut incoming_price = incoming_price;
    if stats.mm_oracle_price != 0 {
        let prev = stats.mm_oracle_price as i128;
        let max_step = MM_ORACLE_MAX_STEP_PCT_PRECISION
            .saturating_mul(prev.abs())
            .saturating_div(PERCENTAGE_PRECISION_I128)
            .max(1);
        let diff = (incoming_price as i128).saturating_sub(prev);
        if diff.abs() > max_step {
            // Between `prev` and `incoming_price`, so always i64-representable.
            let clamped = prev.saturating_add(max_step.saturating_mul(diff.signum()));
            incoming_price = clamped.cast::<i64>()?;
        }
    }

    stats.mm_oracle_slot = current_slot;
    stats.mm_oracle_price = incoming_price;
    stats.mm_oracle_sequence_id = incoming_sequence_id;

    Ok(Outcome::Written {
        price: incoming_price,
    })
}

pub fn handle_update_feature_bit_flags_mm_oracle(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting first bit to 1, enabling mm oracle update");
        state.feature_bit_flags |= FeatureBitFlags::MmOracleUpdate as u8;
    } else {
        msg!("Setting first bit to 0, disabling mm oracle update");
        state.feature_bit_flags &= !(FeatureBitFlags::MmOracleUpdate as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_median_trigger_price(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting second bit to 1, enabling median trigger price");
        state.feature_bit_flags |= FeatureBitFlags::MedianTriggerPrice as u8;
    } else {
        msg!("Setting second bit to 0, disabling median trigger price");
        state.feature_bit_flags &= !(FeatureBitFlags::MedianTriggerPrice as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_builder_codes(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::Unauthorized,
            "Only state admin can enable feature bit flags"
        )?;

        msg!("Setting 3rd bit to 1, enabling builder codes");
        state.feature_bit_flags |= FeatureBitFlags::BuilderCodes as u8;
    } else {
        msg!("Setting 3rd bit to 0, disabling builder codes");
        state.feature_bit_flags &= !(FeatureBitFlags::BuilderCodes as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_vamm_maker_rebate(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can enable feature bit flags"
        )?;

        msg!("Setting 4th bit to 1, enabling vamm maker rebate");
        state.feature_bit_flags |= FeatureBitFlags::VammMakerRebate as u8;
    } else {
        msg!("Setting 4th bit to 0, disabling vamm maker rebate");
        state.feature_bit_flags &= !(FeatureBitFlags::VammMakerRebate as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_settle_lp_pool(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting first bit to 1, enabling settle LP pool");
        state.lp_pool_feature_bit_flags |= LpPoolFeatureBitFlags::SettleLpPool as u8;
    } else {
        msg!("Setting first bit to 0, disabling settle LP pool");
        state.lp_pool_feature_bit_flags &= !(LpPoolFeatureBitFlags::SettleLpPool as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_swap_lp_pool(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::Unauthorized,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting second bit to 1, enabling swapping with LP pool");
        state.lp_pool_feature_bit_flags |= LpPoolFeatureBitFlags::SwapLpPool as u8;
    } else {
        msg!("Setting second bit to 0, disabling swapping with LP pool");
        state.lp_pool_feature_bit_flags &= !(LpPoolFeatureBitFlags::SwapLpPool as u8);
    }
    Ok(())
}

pub fn handle_update_feature_bit_flags_mint_redeem_lp_pool(
    ctx: Context<HotAdminUpdateState>,
    enable: bool,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    if enable {
        validate!(
            ctx.accounts.admin.key().eq(&state.cold_admin),
            ErrorCode::DefaultError,
            "Only state admin can re-enable after kill switch"
        )?;

        msg!("Setting third bit to 1, enabling minting and redeeming with LP pool");
        state.lp_pool_feature_bit_flags |= LpPoolFeatureBitFlags::MintRedeemLpPool as u8;
    } else {
        msg!("Setting third bit to 0, disabling minting and redeeming with LP pool");
        state.lp_pool_feature_bit_flags &= !(LpPoolFeatureBitFlags::MintRedeemLpPool as u8);
    }
    Ok(())
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_config(
    ctx: Context<AdminUpdatePerpMarket>,
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

pub fn handle_update_special_user_status(
    ctx: Context<UpdateSpecialUserStatus>,
    status: u8,
) -> Result<()> {
    let allowed_bits = SpecialUserStatus::VammHedger as u8;

    validate!(
        status & !allowed_bits == 0,
        ErrorCode::DefaultError,
        "unknown bits set in user's special_user_status: {:?}",
        status
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;

    if *ctx.accounts.admin.key != ctx.accounts.state.load()?.cold_admin {
        validate!(
            status == 0,
            ErrorCode::DefaultError,
            "signer must be state admin to enable special user status flags",
        )?;
    }

    msg!(
        "special_user_status for {:?}: {:?} -> {:?}",
        user.authority,
        user.special_user_status,
        status
    );

    user.special_user_status = status;

    Ok(())
}

/// Clears the authority-wide equity breaker set by the permissionless
/// `trip_equity_floor_breaker`. The warm admin is the only caller. It runs after
/// a person reviews why the breaker fired.
///
/// The clear verifies itself at execution time. `remaining_accounts` must carry
/// every live subaccount of the authority, followed by the markets and oracles
/// their positions reference. `UserStats.number_of_sub_accounts` pins the count,
/// so no subaccount can be omitted or passed twice. Every floored subaccount
/// must show net equity at or above its floor plus buffer, with all oracles
/// valid. An approval that went stale therefore fails instead of unfreezing a
/// breached authority. That happens when a subaccount drifts back into breach
/// after the review.
///
/// The validity requirement here stays all-or-nothing. The trip proves equity
/// below the floor, and it can use a zero upper bound for invalid-oracle
/// liabilities and shorts. The reset proves the opposite direction, where an
/// invalid price cannot establish health. A dead oracle on any position
/// therefore blocks the reset until the feed recovers.
/// `update_user_equity_floor` is the way out, here and whenever resumption is
/// the business decision. It lowers the floors first, in the open and on the
/// record.
pub fn handle_reset_equity_floor_breaker<'c: 'info, 'info>(
    ctx: Context<'info, ResetEquityFloorBreaker<'info>>,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let mut user_stats = load_mut!(ctx.accounts.user_stats)?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let user_map = load_user_map(remaining_accounts_iter, false)?;
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        Clock::get()?.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // The set must be exactly the authority's live subaccounts. The map is
    // keyed by pubkey, so a duplicate collapses and fails the count. Every entry
    // must belong to the authority. Two distinct user accounts of one authority
    // are distinct subaccounts, because the PDA is unique. No subaccount can
    // therefore be omitted or counted twice.
    validate!(
        user_map.0.len() == user_stats.number_of_sub_accounts as usize,
        ErrorCode::InvalidEquityBreakerReset,
        "expected all {} subaccounts of the authority, got {}",
        user_stats.number_of_sub_accounts,
        user_map.0.len()
    )?;

    for user_account_loader in user_map.0.values() {
        let user = user_account_loader.load()?;

        validate!(
            user.authority == user_stats.authority,
            ErrorCode::InvalidEquityBreakerReset,
            "subaccount {} does not belong to authority {}",
            user.sub_account_id,
            user_stats.authority
        )?;

        if user.equity_floor == 0 {
            continue;
        }

        let (net_equity, all_oracles_valid) = calculate_user_equity(&user, &mut maps)?;

        // An unfreeze must not rest on an invalid price. The trip requires
        // oracle validity for the same reason.
        validate!(
            all_oracles_valid,
            ErrorCode::InvalidOracle,
            "cannot reset equity floor breaker with an invalid oracle"
        )?;

        validate!(
            !user.is_below_buffered_equity_floor(net_equity),
            ErrorCode::InvalidEquityBreakerReset,
            "subaccount {} net equity {} below equity floor {} + buffer {}",
            user.sub_account_id,
            net_equity,
            user.equity_floor,
            user.equity_floor_buffer
        )?;
    }

    msg!(
        "equity floor breaker reset for authority {:?}",
        user_stats.authority
    );

    user_stats.set_equity_breaker_tripped(false);

    Ok(())
}

pub fn handle_update_user_equity_floor(
    ctx: Context<AdminUpdateUserEquityFloor>,
    equity_floor: u64,
    equity_floor_buffer: u64,
) -> Result<()> {
    let user = &mut load_mut!(ctx.accounts.user)?;

    msg!(
        "equity_floor for {:?}: {:?} -> {:?}, buffer: {:?} -> {:?}",
        user.authority,
        user.equity_floor,
        equity_floor,
        user.equity_floor_buffer,
        equity_floor_buffer
    );

    user.equity_floor = equity_floor;
    user.equity_floor_buffer = equity_floor_buffer;

    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    // Only the designated `state_init_authority` may create the singleton
    // `State` account, so the one-time init cannot be front-run. This lock is
    // active *only* on a real mainnet build (`mainnet-beta` on, `anchor-test`
    // off). Every other build leaves `initialize` open to any admin key:
    //   - devnet/localnet (`mainnet-beta` off) — free setup, and
    //   - the integration-test build, which keeps the default `mainnet-beta`
    //     feature on but adds `anchor-test`, so each test's bankrun wallet can
    //     still initialize.
    // The two arms below are exact complements, so exactly one applies per
    // build. This mirrors the three-way build split used by the keys in
    // `ids.rs`.
    //
    // Anchor honors a single `#[account]` per field, so `mut` is repeated in
    // both arms rather than shared.
    #[cfg_attr(
        any(not(feature = "mainnet-beta"), feature = "anchor-test"),
        account(mut)
    )]
    #[cfg_attr(
        all(feature = "mainnet-beta", not(feature = "anchor-test")),
        account(mut, address = crate::ids::state_init_authority::id())
    )]
    pub admin: Signer<'info>,
    #[account(
        init,
        seeds = [b"velocity_state".as_ref()],
        space = State::SIZE,
        bump,
        payer = admin
    )]
    pub state: AccountLoader<'info, State>,
    pub quote_asset_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: checked in `initialize`
    pub velocity_signer: UncheckedAccount<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
    pub token_program: Interface<'info, TokenInterface>,
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

#[derive(Accounts)]
pub struct AdminUpdatePerpMarket<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

#[derive(Accounts)]
pub struct HotAdminUpdatePerpMarket<'info> {
    // The active-management authority for the scoped vAMM quoting controls.
    // `check_hot` tiers additively, so the cold and warm admins stay valid.
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::VammQuoteManagement)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

#[derive(Accounts)]
pub struct SettleExpiredMarketPoolsToRevenuePool<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [b"spot_market", 0_u16.to_le_bytes().as_ref()],
        bump,
        mut
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

#[derive(Accounts)]
pub struct UpdatePerpMarketPnlPool<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [b"spot_market", 0_u16.to_le_bytes().as_ref()],
        bump,
        mut
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), 0_u16.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

#[derive(Accounts)]
pub struct DepositIntoSpotMarketVault<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::VaultDeposit)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        token::authority = admin
    )]
    pub source_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = spot_market.load()?.vault == spot_market_vault.key()
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct AdminUpdateState<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct SyncStateSlotDuration<'info> {
    #[account(mut, seeds = [b"velocity_state".as_ref()], bump)]
    pub state: AccountLoader<'info, State>,
    /// CHECK: constrained to the feature program and the four scheduled IBRL
    /// keys. The handler validates the serialized activation state.
    #[account(
        owner = FEATURE_GATE_PROGRAM @ ErrorCode::DefaultError,
        constraint = ibrl_slot_duration_ms(&feature_gate.key()).is_some() @ ErrorCode::DefaultError
    )]
    pub feature_gate: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct AdminUpdateUserStats<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
pub struct HotAdminUpdateState<'info> {
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::FeatureFlag)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct AdminUpdateSpotMarket<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
}

#[derive(Accounts)]
pub struct AdminUpdateSpotMarketWithdrawGuardThreshold<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        has_one = oracle @ ErrorCode::InvalidOracle,
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: validated against `spot_market.oracle` by the `has_one` constraint
    pub oracle: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct AdminUpdateSpotMarketOracle<'info> {
    // cold-only: a lesser admin swapping the oracle could re-price the
    // withdraw guard threshold notional cap (and all margin math) at will
    #[account(constraint = check_cold(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// CHECK: checked in `initialize_spot_market`
    pub oracle: UncheckedAccount<'info>,
    /// CHECK: checked in `admin_update_spot_market_oracle` ix constraint
    pub old_oracle: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct AdminUpdatePerpMarketOracle<'info> {
    // cold-only: see AdminUpdateSpotMarketOracle
    #[account(constraint = check_cold(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: checked in `admin_update_perp_market_oracle` ix constraint
    pub oracle: UncheckedAccount<'info>,
    /// CHECK: checked in `admin_update_perp_market_oracle` ix constraint
    pub old_oracle: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [AMM_POSITIONS_CACHE.as_bytes()],
        bump = amm_cache.bump,
    )]
    pub amm_cache: Box<Account<'info, AmmCache>>,
}

#[derive(Accounts)]
pub struct AdminDisableBidAskTwapUpdate<'info> {
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::UserFlag)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}

#[derive(Accounts)]
#[instruction(params: PrelaunchOracleParams,)]
pub struct InitializePrelaunchOracle<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        init,
        seeds = [b"prelaunch_oracle".as_ref(), params.perp_market_index.to_le_bytes().as_ref()],
        space = PrelaunchOracle::SIZE,
        bump,
        payer = admin
    )]
    pub prelaunch_oracle: AccountLoader<'info, PrelaunchOracle>,
    pub state: AccountLoader<'info, State>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(params: PrelaunchOracleParams,)]
pub struct UpdatePrelaunchOracleParams<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"prelaunch_oracle".as_ref(), params.perp_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub prelaunch_oracle: AccountLoader<'info, PrelaunchOracle>,
    #[account(
        mut,
        constraint = perp_market.load()?.market_index == params.perp_market_index
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
#[instruction(perp_market_index: u16,)]
pub struct DeletePrelaunchOracle<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"prelaunch_oracle".as_ref(), perp_market_index.to_le_bytes().as_ref()],
        bump,
        close = admin
    )]
    pub prelaunch_oracle: AccountLoader<'info, PrelaunchOracle>,
    #[account(
        constraint = perp_market.load()?.market_index == perp_market_index
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
#[instruction(feed_id: u32)]
pub struct InitPythLazerOracle<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(init, seeds = [PYTH_LAZER_ORACLE_SEED, &feed_id.to_le_bytes()],
        space=PythLazerOracle::SIZE,
        bump,
        payer=admin
    )]
    pub lazer_oracle: AccountLoader<'info, PythLazerOracle>,
    pub state: AccountLoader<'info, State>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(market_index: u16,)]
pub struct AdminDeposit<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(mut, constraint = check_hot(&admin.key(), &state, HotRole::VaultDeposit)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &spot_market_vault.mint.eq(&admin_token_account.mint),
        token::authority = admin.key()
    )]
    pub admin_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct UpdateSpecialUserStatus<'info> {
    #[account(constraint = check_hot(&admin.key(), &state, HotRole::UserFlag)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct AdminUpdateUserEquityFloor<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
}

#[derive(Accounts)]
pub struct ResetEquityFloorBreaker<'info> {
    #[account(constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}

// ----- Tiered admin authority handlers -----
//
// cold/warm/hot pubkeys now live directly on `State`. `handle_initialize`
// seeds `cold_admin = warm_admin = signer` at deploy time; the handlers below
// rotate `warm_admin` (cold-only), `pause_admin` (cold-only), and individual
// hot-role keys (warm-only).

pub fn handle_update_warm_admin(
    ctx: Context<UpdateWarmAdmin>,
    new_warm_admin: Pubkey,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    msg!("warm_admin: {:?} -> {:?}", state.warm_admin, new_warm_admin);
    state.warm_admin = new_warm_admin;
    Ok(())
}

pub fn handle_update_pause_admin(
    ctx: Context<UpdatePauseAdmin>,
    new_pause_admin: Pubkey,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    msg!(
        "pause_admin: {:?} -> {:?}",
        state.pause_admin,
        new_pause_admin
    );
    state.pause_admin = new_pause_admin;
    Ok(())
}

pub fn handle_update_hot_admin(
    ctx: Context<UpdateHotAdmin>,
    role: HotRole,
    new_pubkey: Pubkey,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    let prev = state.hot_key(role);
    state.set_hot_key(role, new_pubkey);
    msg!("hot_admin[{:?}]: {:?} -> {:?}", role, prev, new_pubkey);
    Ok(())
}

/// Cold-only. Sets the treasury that protocol fees can be withdrawn to —
/// perp (quote-denominated) and spot (per-market tokens) recipients are
/// configured independently via `market_type`.
pub fn handle_update_protocol_fee_recipient(
    ctx: Context<ColdAdminUpdateState>,
    protocol_fee_recipient: Pubkey,
    market_type: MarketType,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    match market_type {
        MarketType::Perp => {
            msg!(
                "protocol_fee_recipient_perp: {:?} -> {:?}",
                state.protocol_fee_recipient_perp,
                protocol_fee_recipient
            );
            state.protocol_fee_recipient_perp = protocol_fee_recipient;
        }
        MarketType::Spot => {
            msg!(
                "protocol_fee_recipient_spot: {:?} -> {:?}",
                state.protocol_fee_recipient_spot,
                protocol_fee_recipient
            );
            state.protocol_fee_recipient_spot = protocol_fee_recipient;
        }
    }
    Ok(())
}

/// Cold-only state mutation. Constraint enforces `state.cold_admin == admin.key()`.
#[derive(Accounts)]
pub struct ColdAdminUpdateState<'info> {
    #[account(mut, constraint = state.load()?.cold_admin == admin.key() @ ErrorCode::Unauthorized)]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}

/// Cold-only mutation of `warm_admin`.
#[derive(Accounts)]
pub struct UpdateWarmAdmin<'info> {
    #[account(mut, constraint = state.load()?.cold_admin == admin.key() @ ErrorCode::Unauthorized)]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}

/// Cold-only mutation of `pause_admin`. The pause admin is the no-timelock
/// emergency-pause key; only the root (cold) authority can rotate it.
#[derive(Accounts)]
pub struct UpdatePauseAdmin<'info> {
    #[account(mut, constraint = state.load()?.cold_admin == admin.key() @ ErrorCode::Unauthorized)]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}

/// Warm-or-cold gated mutation of an individual hot-role key.
#[derive(Accounts)]
pub struct UpdateHotAdmin<'info> {
    #[account(
        mut,
        constraint = state.load()?.is_warm(&admin.key()) @ ErrorCode::Unauthorized
    )]
    pub state: AccountLoader<'info, State>,
    pub admin: Signer<'info>,
}

// ----- Pause-admin gated contexts -----
//
// Pause flags can be flipped by cold, warm, or the dedicated `pause_admin`
// (which has no on-chain timelock). pause_admin is restricted *inside* the
// handlers to bit-additions only — it can never clear a pause bit.

#[derive(Accounts)]
pub struct PauseAdminUpdateState<'info> {
    #[account(constraint = check_pause(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
pub struct PauseAdminUpdateSpotMarket<'info> {
    #[account(constraint = check_pause(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub spot_market: AccountLoader<'info, SpotMarket>,
}

#[derive(Accounts)]
pub struct PauseAdminUpdatePerpMarket<'info> {
    #[account(constraint = check_pause(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
}

/// Per-user pause flips are reachable by cold/warm, the existing
/// `HotRole::UserFlag` bot, or the pause_admin (pause-only — see handler).
#[derive(Accounts)]
pub struct PauseAdminUpdateUserStats<'info> {
    #[account(
        constraint =
            check_pause(&admin.key(), &state)?
                || check_hot(&admin.key(), &state, HotRole::UserFlag)?
    )]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub user_stats: AccountLoader<'info, UserStats>,
}

// ----- Force wipe (non-mainnet only) -----
//
// One-shot escape hatch for devnet: closes velocity-owned PDAs whose on-chain
// layout no longer matches the program (e.g. after a layout-breaking upgrade).
// Bypasses `AccountLoader::try_from` size checks by reading State's admin
// pubkey directly from raw bytes — the first pubkey field lives at offset
// 8..40 in both the legacy `#[account]` State (admin) and the new zero-copy
// State (cold_admin), so this admin gate works across layouts.
//
// Compiled out of mainnet builds via `cfg(not(feature = "mainnet-beta"))`.

#[cfg(not(feature = "mainnet-beta"))]
pub fn handle_force_wipe_accounts_devnet<'info>(
    ctx: Context<'info, ForceWipeAccountsDevnet<'info>>,
    velocity_signer_nonce: u8,
) -> Result<()> {
    use {anchor_lang::solana_program::system_program, anchor_spl::token_interface};

    let state_ai = ctx.accounts.state.to_account_info();
    require_keys_eq!(*state_ai.owner, crate::ID, ErrorCode::DefaultError);
    {
        let data = state_ai.try_borrow_data()?;
        require!(data.len() >= 40, ErrorCode::DefaultError);
        let mut admin_bytes = [0u8; 32];
        admin_bytes.copy_from_slice(&data[8..40]);
        let stored_admin = Pubkey::from(admin_bytes);
        require_keys_eq!(
            stored_admin,
            ctx.accounts.admin.key(),
            ErrorCode::Unauthorized
        );
    }

    let admin_ai = ctx.accounts.admin.to_account_info();
    let token_program_id = ctx.accounts.token_program.key();
    let signer_seeds = crate::signer::get_signer_seeds(&velocity_signer_nonce);
    let cpi_signers = &[&signer_seeds[..]];

    // PASS 1: close token vaults. Remaining accounts must come in pairs:
    //   (vault, mint), (vault, mint), ...
    // For each vault: if it holds a non-zero balance, CPI burn first (mint is
    // the next account in the pair), then CPI close_account.
    // Velocity-owned PDAs come AFTER all the (vault, mint) pairs.
    let mut i = 0;
    while i < ctx.remaining_accounts.len() {
        let target = &ctx.remaining_accounts[i];
        if *target.owner != token_program_id {
            break; // start of velocity-owned section
        }
        if target.lamports() == 0 {
            i += 1;
            continue;
        }
        // pair: next account is the mint
        let mint_ai = ctx
            .remaining_accounts
            .get(i + 1)
            .ok_or(ErrorCode::DefaultError)?;
        require_keys_eq!(*mint_ai.owner, token_program_id, ErrorCode::DefaultError);

        // read current token amount (offset 64..72 in SPL token account layout)
        let amount = {
            let data = target.try_borrow_data()?;
            require!(data.len() >= 72, ErrorCode::DefaultError);
            u64::from_le_bytes(data[64..72].try_into().unwrap())
        };

        if amount > 0 {
            let burn_accounts = token_interface::Burn {
                mint: mint_ai.clone(),
                from: target.clone(),
                authority: ctx.accounts.velocity_signer.to_account_info(),
            };
            let burn_ctx =
                CpiContext::new_with_signer(token_program_id, burn_accounts, cpi_signers);
            token_interface::burn(burn_ctx, amount)?;
            msg!("burned {} from {}", amount, target.key());
        }

        let close_accounts = token_interface::CloseAccount {
            account: target.clone(),
            destination: admin_ai.clone(),
            authority: ctx.accounts.velocity_signer.to_account_info(),
        };
        let close_ctx = CpiContext::new_with_signer(token_program_id, close_accounts, cpi_signers);
        token_interface::close_account(close_ctx)?;
        msg!("closed token vault {}", target.key());

        i += 2; // skip past the mint
    }
    let velocity_section_start = i;

    // PASS 2: drain velocity-owned PDAs by zeroing lamports; runtime GCs at EOT.
    for target in ctx.remaining_accounts.iter().skip(velocity_section_start) {
        if *target.owner == system_program::ID || target.lamports() == 0 {
            msg!("skip {} (already empty)", target.key());
            continue;
        }
        if *target.owner != crate::ID {
            msg!(
                "skip {} (owner {} not velocity)",
                target.key(),
                target.owner,
            );
            continue;
        }
        let take = target.lamports();
        **admin_ai.try_borrow_mut_lamports()? = admin_ai
            .lamports()
            .checked_add(take)
            .ok_or_else(math_error!())?;
        **target.try_borrow_mut_lamports()? = 0;
        msg!("wiped {} (reclaimed {} lamports)", target.key(), take);
    }
    Ok(())
}

#[cfg(not(feature = "mainnet-beta"))]
#[derive(Accounts)]
pub struct ForceWipeAccountsDevnet<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    /// CHECK: read raw bytes manually; both old and new State layouts have the
    /// (cold-)admin pubkey at offset 8..40.
    pub state: UncheckedAccount<'info>,
    /// CHECK: PDA seeded by [b"velocity_signer", nonce]. Verified by Token Program
    /// at CPI time when closing token vaults; ignored otherwise.
    pub velocity_signer: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    // Targets are passed via `remaining_accounts` so a single call can wipe
    // many accounts in one tx. Velocity-owned PDAs are drained; token-owned vaults
    // are closed via CPI (rent → admin).
}

#[cfg(test)]
mod native_auth_tests {
    //! Negative tests for the pre-Anchor native dispatch authentication on
    //! `handle_update_mm_oracle_native`. These run under `cargo test` with
    //! default features and without `anchor-test`, so the signer check is
    //! compiled in. The structural account checks are compiled in under every
    //! feature set.
    use {
        super::*,
        crate::{
            create_anchor_account_info,
            math::time::SlotDuration,
            state::{
                perp_market::PerpMarket,
                state::{FeatureBitFlags, State},
            },
            test_utils::get_anchor_account_bytes,
        },
        anchor_lang::prelude::{AccountInfo, Pubkey},
    };

    // The mm-oracle payload is an 8-byte price, an 8-byte sequence id and an
    // 8-byte source slot. The price and the sequence are non-zero, and the
    // source slot matches the slot the tests drive, so an accepted update would
    // pass every early check.
    fn mm_payload() -> [u8; 24] {
        let mut d = [0u8; 24];
        d[0..8].copy_from_slice(&100_i64.to_le_bytes());
        d[8..16].copy_from_slice(&1_u64.to_le_bytes());
        d[16..24].copy_from_slice(&100_u64.to_le_bytes());
        d
    }

    fn signer_info<'a>(
        key: &'a Pubkey,
        is_signer: bool,
        lamports: &'a mut u64,
        data: &'a mut [u8],
        owner: &'a Pubkey,
    ) -> AccountInfo<'a> {
        AccountInfo::new(key, is_signer, false, lamports, data, owner, false)
    }

    #[test]
    fn mm_oracle_native_rejects_forged_state() {
        // State account with the attacker's key at the hot-key field but owned by
        // a foreign program — the pre-fix bug authenticated against exactly this.
        let attacker = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = attacker;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        let mut state_bytes = get_anchor_account_bytes(&mut state);
        let foreign_owner = Pubkey::new_unique();
        let state_key = Pubkey::new_unique();
        let mut state_lamports = 0u64;
        let forged_state = AccountInfo::new(
            &state_key,
            false,
            false,
            &mut state_lamports,
            &mut state_bytes[..],
            &foreign_owner, // NOT crate::ID
            false,
        );

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(
            &attacker,
            true,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
        );

        let accounts = [perp_market_info, signer, forged_state];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeStateAccount.into());
    }

    #[test]
    fn mm_oracle_native_rejects_non_perp_market_in_market_slot() {
        // Genuine state, but the "market" slot holds a non-PerpMarket account
        // (here a second State) — the pre-fix bug bytemuck-cast it blindly.
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [not_a_market_info, signer, state_info];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn mm_oracle_native_rejects_unauthorized_signer() {
        // Genuine state + market, but the signer is not the configured hot key.
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let attacker = Pubkey::new_unique();
        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(
            &attacker,
            true,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
        );

        let accounts = [perp_market_info, signer, state_info];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::Unauthorized.into());
    }

    /// The handler runs before Anchor, so a malformed instruction reaches it
    /// verbatim. A short account list or a short payload must return an error.
    /// A panic on the indexing aborts the transaction with no identifiable
    /// error.
    #[test]
    fn mm_oracle_native_rejects_malformed_shape_without_panicking() {
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [perp_market_info, signer, state_info];

        // Too few accounts. Zero of them, so nothing can be indexed at all.
        let err = update_mm_oracle(&[], &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeInstructionData.into());

        // Two accounts where three are required: the state slot is `accounts[2]`.
        let err = update_mm_oracle(&accounts[..2], &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeInstructionData.into());

        // Payload shorter than the 24 bytes the handler slices.
        for len in 0..24usize {
            let err = update_mm_oracle(&accounts, &mm_payload()[..len], 100).unwrap_err();
            assert_eq!(
                err,
                ErrorCode::InvalidNativeInstructionData.into(),
                "payload of {len} bytes did not return a clean error"
            );
        }
    }

    #[test]
    fn mm_oracle_native_kill_switch_returns_typed_error() {
        // A panic here would surface as "Program failed to complete", which no
        // caller can tell apart from any other abort.
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = 0; // MmOracleUpdate clear
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [perp_market_info, signer, state_info];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::MmOracleUpdateDisabled.into());
    }

    /// Drives the handler repeatedly against a fixed target price and returns
    /// the stored price after each accepted write.
    fn walk_price(start: i64, target: i64, writes: usize) -> Vec<i64> {
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        perp_market.market_stats.mm_oracle_price = start;
        perp_market.market_stats.mm_oracle_slot = 0;
        perp_market.market_stats.mm_oracle_sequence_id = 0;
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [perp_market_info.clone(), signer, state_info];

        let mut observed = Vec::with_capacity(writes);
        for i in 0..writes {
            // Advance the slot past the MM-oracle write gap for each write.
            let gap =
                crate::math::constants::MM_ORACLE_MIN_WRITE_GAP.to_slots(SlotDuration::BASELINE);
            let slot = ((i as u64) + 1) * (gap + 1);

            let mut payload = [0u8; 24];
            payload[0..8].copy_from_slice(&target.to_le_bytes());
            payload[8..16].copy_from_slice(&((i as u64) + 1).to_le_bytes());
            payload[16..24].copy_from_slice(&slot.to_le_bytes()); // fresh source
            update_mm_oracle(&accounts, &payload, slot).unwrap();

            let data = perp_market_info.try_borrow_data().unwrap();
            let market: &PerpMarket =
                bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<PerpMarket>()]);
            observed.push(market.market_stats.mm_oracle_price);
        }
        observed
    }

    /// A move beyond the cap is written at the cap and keeps closing the gap.
    /// Rejecting it instead would leave the stored price where it is, so the
    /// next update would still be beyond the cap against the same stale value
    /// and the oracle would never recover.
    #[test]
    fn mm_oracle_native_clamps_and_converges_upward() {
        let start = 1_000_000i64;
        let target = start * 105 / 100; // 5% away, cap is 1%

        let observed = walk_price(start, target, 6);

        assert_eq!(observed[0], 1_010_000, "first write must land at the cap");
        // Monotonic toward the target, and strictly moving until it arrives.
        for pair in observed.windows(2) {
            assert!(pair[1] >= pair[0], "price moved backwards: {observed:?}");
            assert!(
                pair[0] == target || pair[1] > pair[0],
                "price stalled before reaching the target: {observed:?}"
            );
        }
        assert_eq!(
            *observed.last().unwrap(),
            target,
            "must converge on the target: {observed:?}"
        );
        assert!(
            observed.iter().all(|p| *p <= target),
            "must never overshoot: {observed:?}"
        );
    }

    /// The cap is symmetric, so the same must hold downward.
    #[test]
    fn mm_oracle_native_clamps_and_converges_downward() {
        let start = 1_000_000i64;
        let target = start * 95 / 100;

        let observed = walk_price(start, target, 6);

        assert_eq!(observed[0], 990_000, "first write must land at the cap");
        for pair in observed.windows(2) {
            assert!(pair[1] <= pair[0], "price moved backwards: {observed:?}");
            assert!(
                pair[0] == target || pair[1] < pair[0],
                "price stalled before reaching the target: {observed:?}"
            );
        }
        assert_eq!(*observed.last().unwrap(), target);
        assert!(observed.iter().all(|p| *p >= target));
    }

    /// A move inside the cap is written verbatim.
    #[test]
    fn mm_oracle_native_leaves_in_range_steps_alone() {
        let start = 1_000_000i64;
        let target = start + 5_000; // 0.5%, inside the 1% cap
        assert_eq!(walk_price(start, target, 1)[0], target);
    }

    /// The cap is a percentage, so integer division rounds it to zero for very
    /// small prices. The floor of one unit keeps those markets moving instead
    /// of freezing the oracle.
    #[test]
    fn mm_oracle_native_makes_progress_at_prices_below_the_cap_resolution() {
        // 1% of 50 rounds to 0.
        let observed = walk_price(50, 60, 3);
        assert_eq!(observed, vec![51, 52, 53], "must advance by at least one");
    }

    /// Every non-positive price is a hard error, not only exact zero. A check
    /// for zero alone leaves a hole, because the step cap clamps instead of
    /// skipping. A negative target would be clamped against the stored price and
    /// written. For example, -1 against 1,000,000 lands as 990,000 and consumes
    /// the sequence id. Repeated negatives could walk the price to zero, which
    /// resets the bootstrap path and the step cap with it. At bootstrap, where
    /// the stored price is 0, a negative would be written verbatim.
    #[test]
    fn mm_oracle_native_rejects_non_positive_price() {
        // (stored price, incoming price)
        let cases: [(i64, i64); 5] = [
            (1_000_000, 0),
            (1_000_000, -1),
            (1_000_000, -1_000_000),
            (0, -1), // bootstrap: previously written verbatim
            (0, i64::MIN),
        ];

        for (stored, incoming) in cases {
            let hot_key = Pubkey::new_unique();
            let mut state = State::default();
            state.hot_mm_oracle_crank = hot_key;
            state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
            create_anchor_account_info!(state, State, state_info);

            let mut perp_market = PerpMarket::default();
            perp_market.market_stats.mm_oracle_price = stored;
            create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

            let mut sig_lamports = 0u64;
            let mut sig_data: [u8; 0] = [];
            let sig_owner = Pubkey::new_unique();
            let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

            let mut payload = [0u8; 24];
            payload[0..8].copy_from_slice(&incoming.to_le_bytes());
            payload[8..16].copy_from_slice(&1u64.to_le_bytes());
            payload[16..24].copy_from_slice(&100u64.to_le_bytes());

            let accounts = [perp_market_info.clone(), signer, state_info];
            let err = update_mm_oracle(&accounts, &payload, 100).unwrap_err();
            assert_eq!(
                err,
                ErrorCode::OracleNonPositive.into(),
                "price {incoming} against stored {stored} must be a hard error"
            );

            let data = perp_market_info.try_borrow_data().unwrap();
            let market: &PerpMarket =
                bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<PerpMarket>()]);
            assert_eq!(
                market.market_stats.mm_oracle_price, stored,
                "stored price must be untouched"
            );
            assert_eq!(market.market_stats.mm_oracle_sequence_id, 0);
        }
    }
}

#[cfg(test)]
mod native_batch_tests {
    //! Tests for `handle_update_mm_oracle_batch_native`, native dispatch
    //! opcode 2. They come in three groups.
    //!
    //! 1. Negative tests that mirror `native_auth_tests`. The batch handler
    //!    re-implements the same pre-Anchor authentication and must hold every
    //!    case that motivated `auth::require_native_account`: a forged state
    //!    account, a blind `bytemuck` cast of an untyped account, and a signer
    //!    bypass. It adds the framing checks that only a variable-length handler
    //!    needs. There is no clock to forge, because the slot comes from the
    //!    Clock sysvar syscall, and the tests drive it through the split handler
    //!    bodies.
    //! 2. Functional tests that pin every accept and skip decision. They assert
    //!    on the returned reject bitmask as well as on written state.
    //! 3. An equivalence test against `handle_update_mm_oracle_native`, so the
    //!    two copies of the gating logic cannot drift apart.
    //!
    //! These run under `cargo test` with default features, so the hot-key signer
    //! check is compiled in. The structural checks are compiled in under every
    //! feature set.
    use {
        super::*,
        crate::{
            create_anchor_account_info,
            math::time::SlotDuration,
            state::{
                perp_market::PerpMarket,
                state::{FeatureBitFlags, State},
            },
            test_utils::get_anchor_account_bytes,
        },
        anchor_lang::prelude::{AccountInfo, Pubkey},
    };

    /// `(price, slot, sequence_id)` triple of a market's stored MM oracle fields.
    type MmStats = (i64, u64, u64);

    const BASE_PRICE: i64 = 1_000_000;
    /// Slot every fixture's clock reports, chosen far enough above the fixtures'
    /// stored slots that the gap check is never the accidental reason a test
    /// passes.
    const SLOT: u64 = 100;

    /// Binds a valid prologue for the batch handler: a program-owned `State`
    /// with the kill switch on and `$hot` as the MM-oracle crank key, and
    /// `$hot` as a signing account. The slot is passed straight to the split
    /// handler bodies; there is no clock account since the handlers read the
    /// Clock sysvar via syscall.
    macro_rules! valid_prologue {
        ($hot:ident, $state:ident, $signer:ident) => {
            let mut state_struct = State::default();
            state_struct.hot_mm_oracle_crank = $hot;
            state_struct.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
            create_anchor_account_info!(state_struct, State, $state);

            let mut sig_lamports = 0u64;
            let mut sig_data: [u8; 0] = [];
            let sig_owner = Pubkey::new_unique();
            let $signer = AccountInfo::new(
                &$hot,
                true,
                false,
                &mut sig_lamports,
                &mut sig_data,
                &sig_owner,
                false,
            );
        };
    }

    /// Binds `$name` to a writable, program-owned `PerpMarket` account carrying
    /// `market_index = $index` and the MM oracle fields in `$stats`.
    macro_rules! market_account {
        ($index:expr, $stats:expr, $name:ident) => {
            let market_key = Pubkey::new_unique();
            let mut market_struct = market_with($index, $stats);
            create_anchor_account_info!(market_struct, &market_key, PerpMarket, $name);
        };
    }

    /// Batch payload: count byte then `(market_index, price, sequence_id,
    /// source_slot)` per entry, little-endian, matching
    /// `MM_ORACLE_BATCH_ENTRY_LEN`.
    fn batch_payload_with_source(entries: &[(u16, i64, u64, u64)]) -> Vec<u8> {
        let mut data = Vec::with_capacity(1 + entries.len() * MM_ORACLE_BATCH_ENTRY_LEN);
        data.push(entries.len() as u8);
        for (market_index, price, sequence_id, source_slot) in entries {
            data.extend_from_slice(&market_index.to_le_bytes());
            data.extend_from_slice(&price.to_le_bytes());
            data.extend_from_slice(&sequence_id.to_le_bytes());
            data.extend_from_slice(&source_slot.to_le_bytes());
        }
        data
    }

    /// `batch_payload_with_source` with every entry's source slot pinned to
    /// `SLOT`, i.e. observed in the landing slot, so the source-age gate is
    /// never the accidental reason a test passes.
    fn batch_payload(entries: &[(u16, i64, u64)]) -> Vec<u8> {
        let with_source: Vec<(u16, i64, u64, u64)> = entries
            .iter()
            .map(|&(market_index, price, sequence_id)| (market_index, price, sequence_id, SLOT))
            .collect();
        batch_payload_with_source(&with_source)
    }

    /// Payload for the single-market handler (opcode 0): price, sequence id,
    /// source slot; no count byte and no market index.
    fn single_payload_with_source(price: i64, sequence_id: u64, source_slot: u64) -> [u8; 24] {
        let mut data = [0u8; 24];
        data[0..8].copy_from_slice(&price.to_le_bytes());
        data[8..16].copy_from_slice(&sequence_id.to_le_bytes());
        data[16..24].copy_from_slice(&source_slot.to_le_bytes());
        data
    }

    /// `single_payload_with_source` with the source slot pinned to `SLOT`.
    fn single_payload(price: i64, sequence_id: u64) -> [u8; 24] {
        single_payload_with_source(price, sequence_id, SLOT)
    }

    fn read_stats(info: &AccountInfo) -> MmStats {
        let data = info.try_borrow_data().unwrap();
        let market: &PerpMarket =
            bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<PerpMarket>()]);
        (
            market.market_stats.mm_oracle_price,
            market.market_stats.mm_oracle_slot,
            market.market_stats.mm_oracle_sequence_id,
        )
    }

    fn market_with(market_index: u16, stats: MmStats) -> PerpMarket {
        let mut market = PerpMarket {
            market_index,
            ..PerpMarket::default()
        };
        market.market_stats.mm_oracle_price = stats.0;
        market.market_stats.mm_oracle_slot = stats.1;
        market.market_stats.mm_oracle_sequence_id = stats.2;
        market
    }

    // framing

    /// Framing is validated before any account is touched, so these cases need
    /// no account fixtures at all. Passing an empty `accounts` slice also proves
    /// the handler never indexes an account before checking lengths.
    #[test]
    fn batch_rejects_malformed_framing() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty payload", vec![]),
            ("zero count", vec![0u8]),
            ("count above max", {
                let mut d = vec![(MM_ORACLE_BATCH_MAX_MARKETS + 1) as u8];
                d.extend(std::iter::repeat_n(
                    0u8,
                    (MM_ORACLE_BATCH_MAX_MARKETS + 1) * MM_ORACLE_BATCH_ENTRY_LEN,
                ));
                d
            }),
            ("count says 2, one entry supplied", {
                let mut d = batch_payload(&[(0, BASE_PRICE, 1)]);
                d[0] = 2;
                d
            }),
            ("count says 1, trailing byte", {
                let mut d = batch_payload(&[(0, BASE_PRICE, 1)]);
                d.push(0);
                d
            }),
            ("entry truncated by one byte", {
                let mut d = batch_payload(&[(0, BASE_PRICE, 1)]);
                d.pop();
                d
            }),
        ];

        for (label, data) in cases {
            let err = update_mm_oracle_batch(&[], &data, SLOT).unwrap_err();
            assert_eq!(
                err,
                ErrorCode::InvalidNativeInstructionData.into(),
                "framing case did not reject: {label}"
            );
        }
    }

    #[test]
    fn batch_rejects_too_few_accounts_for_declared_count() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info);

        // Declares two markets, supplies one.
        let payload = batch_payload(&[(0, BASE_PRICE, 1), (1, BASE_PRICE, 2)]);
        let accounts = [signer, state_info, market_info];
        let err = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeInstructionData.into());
    }

    // authentication

    #[test]
    fn batch_rejects_forged_state() {
        // State carrying the attacker's key at the hot-key offset, but owned by a
        // foreign program. This is the exact shape of the original finding.
        let attacker = Pubkey::new_unique();
        let mut state_struct = State::default();
        state_struct.hot_mm_oracle_crank = attacker;
        state_struct.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        let mut state_bytes = get_anchor_account_bytes(&mut state_struct);
        let foreign_owner = Pubkey::new_unique();
        let state_key = Pubkey::new_unique();
        let mut state_lamports = 0u64;
        let forged_state = AccountInfo::new(
            &state_key,
            false,
            false,
            &mut state_lamports,
            &mut state_bytes[..],
            &foreign_owner, // NOT crate::ID
            false,
        );

        market_account!(0, (0, 0, 0), market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = AccountInfo::new(
            &attacker,
            true,
            false,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
            false,
        );

        let accounts = [signer, forged_state, market_info];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeStateAccount.into());
    }

    #[test]
    fn batch_rejects_non_state_account_in_state_slot() {
        // Program-owned, but a PerpMarket rather than State: the discriminator
        // arm of require_native_account, which the forged-state test (foreign
        // owner) does not reach.
        let hot_key = Pubkey::new_unique();
        market_account!(0, (0, 0, 0), not_a_state_info);
        market_account!(0, (0, 0, 0), market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = AccountInfo::new(
            &hot_key,
            true,
            false,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
            false,
        );

        let accounts = [signer, not_a_state_info, market_info];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeStateAccount.into());
    }

    #[test]
    fn batch_rejects_non_perp_market_in_market_slot() {
        // Genuine state, but a second State account sits in the market region.
        // Without the discriminator check this would be `bytemuck`-cast blindly.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let accounts = [signer, state_info, not_a_market_info];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn batch_rejects_foreign_owned_market() {
        // Correct PerpMarket discriminator but owned by another program, i.e. the
        // owner arm of require_native_account on the market side. Anyone can
        // create an account with arbitrary bytes under a program they control.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut market_struct = market_with(0, (0, 0, 0));
        let mut market_bytes = get_anchor_account_bytes(&mut market_struct);
        let foreign_owner = Pubkey::new_unique();
        let market_key = Pubkey::new_unique();
        let mut market_lamports = 0u64;
        let foreign_market = AccountInfo::new(
            &market_key,
            false,
            true,
            &mut market_lamports,
            &mut market_bytes[..],
            &foreign_owner, // NOT crate::ID
            false,
        );

        let accounts = [signer, state_info, foreign_market];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn batch_rejects_truncated_market_account() {
        // Program-owned and correctly discriminated, but too short to hold a
        // PerpMarket. Opcode 0 panics on this input; the batch must not.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut truncated = [0u8; 64];
        truncated[..8].copy_from_slice(PerpMarket::DISCRIMINATOR);
        let market_key = Pubkey::new_unique();
        let mut market_lamports = 0u64;
        let market_owner = crate::ID;
        let truncated_market = AccountInfo::new(
            &market_key,
            false,
            true,
            &mut market_lamports,
            &mut truncated,
            &market_owner,
            false,
        );

        let accounts = [signer, state_info, truncated_market];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    /// A batch must authenticate every market, not just the first. Market 0 is
    /// genuine and market 1 is not, so the error can only come from index 1.
    #[test]
    fn batch_authenticates_every_market_not_just_the_first() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let payload = batch_payload(&[(0, BASE_PRICE, 1), (1, BASE_PRICE, 2)]);
        let accounts = [signer, state_info, market_info, not_a_market_info];
        let err = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    /// The handler is not internally transactional: entries before the failing
    /// one have already been written when the error returns. That is safe only
    /// because the runtime discards every account mutation when an instruction
    /// returns `Err`. Pinned here so the assumption is explicit rather than
    /// incidental.
    #[test]
    fn batch_is_not_internally_transactional() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let payload = batch_payload(&[(0, BASE_PRICE, 1), (1, BASE_PRICE, 2)]);
        let accounts = [signer, state_info, market_info.clone(), not_a_market_info];
        assert!(update_mm_oracle_batch(&accounts, &payload, SLOT).is_err());
        assert_eq!(
            read_stats(&market_info),
            (BASE_PRICE, SLOT, 1),
            "entry 0 is written before entry 1 fails; the runtime, not the \
             handler, is what rolls this back"
        );
    }

    #[test]
    fn batch_rejects_market_index_mismatch() {
        // The account list and the payload disagree about which market entry 0
        // is for. Without the market-index field this would silently write one
        // market's price onto another.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(7, (0, 0, 0), market_info);

        let payload = batch_payload(&[(3, BASE_PRICE, 1)]); // account says 7
        let accounts = [signer, state_info, market_info.clone()];
        let err = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
        assert_eq!(
            read_stats(&market_info),
            (0, 0, 0),
            "nothing may be written"
        );
    }

    #[test]
    fn batch_rejects_read_only_market() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut market_struct = market_with(0, (0, 0, 0));
        let mut market_bytes = get_anchor_account_bytes(&mut market_struct);
        let market_key = Pubkey::new_unique();
        let mut market_lamports = 0u64;
        let market_owner = crate::ID;
        let read_only_market = AccountInfo::new(
            &market_key,
            false,
            false, // not writable
            &mut market_lamports,
            &mut market_bytes[..],
            &market_owner,
            false,
        );

        let accounts = [signer, state_info, read_only_market];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn batch_rejects_unauthorized_and_non_signing_hot_key() {
        // Two cases: the wrong key that signs, and the right key that does not.
        for wrong_key in [true, false] {
            let hot_key = Pubkey::new_unique();
            let attacker = Pubkey::new_unique();
            let mut state_struct = State::default();
            state_struct.hot_mm_oracle_crank = hot_key;
            state_struct.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
            create_anchor_account_info!(state_struct, State, state_info);

            market_account!(0, (0, 0, 0), market_info);

            let signer_key = if wrong_key { attacker } else { hot_key };
            let mut sig_lamports = 0u64;
            let mut sig_data: [u8; 0] = [];
            let sig_owner = Pubkey::new_unique();
            let signer = AccountInfo::new(
                &signer_key,
                wrong_key, // signs only in the wrong-key case
                false,
                &mut sig_lamports,
                &mut sig_data,
                &sig_owner,
                false,
            );

            let accounts = [signer, state_info, market_info];
            let err =
                update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
                    .unwrap_err();
            assert_eq!(err, ErrorCode::Unauthorized.into());
        }
    }

    #[test]
    fn batch_rejects_when_kill_switch_is_off() {
        let hot_key = Pubkey::new_unique();
        let mut state_struct = State::default();
        state_struct.hot_mm_oracle_crank = hot_key;
        state_struct.feature_bit_flags = 0; // MmOracleUpdate clear
        create_anchor_account_info!(state_struct, State, state_info);

        market_account!(0, (0, 0, 0), market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = AccountInfo::new(
            &hot_key,
            true,
            false,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
            false,
        );

        let accounts = [signer, state_info, market_info.clone()];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::MmOracleUpdateDisabled.into());
        assert_eq!(
            read_stats(&market_info),
            (0, 0, 0),
            "nothing may be written"
        );
    }

    // functional

    /// The core property: four markets in one call, three of which must be
    /// skipped for a different reason each. The skips must not disturb the
    /// healthy market, the call must succeed, and the reject mask must name
    /// exactly the skipped positions. Prices are distinct per entry so a
    /// positional mix-up between entries and accounts would be visible.
    #[test]
    fn batch_skips_bad_entries_and_still_writes_the_good_one() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        // 0: cranked one slot ago, below MM_ORACLE_MIN_SLOT_GAP.
        market_account!(0, (BASE_PRICE, SLOT - 1, 5), rate_limited_info);
        // 1: healthy.
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), healthy_info);
        // 2: feed produced a zero price.
        market_account!(2, (BASE_PRICE, SLOT - 10, 5), zero_priced_info);
        // 3: sequence id has not advanced.
        market_account!(3, (BASE_PRICE, SLOT - 10, 9), stale_sequence_info);

        let payload = batch_payload(&[
            (0, BASE_PRICE + 1_000, 6),
            (1, BASE_PRICE + 2_000, 6),
            (2, 0, 6),
            (3, BASE_PRICE + 4_000, 9),
        ]);
        let accounts = [
            signer,
            state_info,
            rate_limited_info.clone(),
            healthy_info.clone(),
            zero_priced_info.clone(),
            stale_sequence_info.clone(),
        ];

        let (mask, _) = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap();
        assert_eq!(mask, 0b1101, "entries 0, 2 and 3 must be reported rejected");

        assert_eq!(
            read_stats(&rate_limited_info),
            (BASE_PRICE, SLOT - 1, 5),
            "rate-limited market must be untouched"
        );
        assert_eq!(
            read_stats(&healthy_info),
            (BASE_PRICE + 2_000, SLOT, 6),
            "healthy market must be written despite its neighbours failing"
        );
        assert_eq!(
            read_stats(&zero_priced_info),
            (BASE_PRICE, SLOT - 10, 5),
            "zero-priced market must be untouched"
        );
        assert_eq!(
            read_stats(&stale_sequence_info),
            (BASE_PRICE, SLOT - 10, 9),
            "stale-sequence market must be untouched"
        );
    }

    #[test]
    fn batch_reports_empty_mask_when_everything_lands() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), a_info);
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), b_info);

        let payload = batch_payload(&[(0, BASE_PRICE + 1, 6), (1, BASE_PRICE + 2, 6)]);
        let accounts = [signer, state_info, a_info.clone(), b_info.clone()];

        assert_eq!(
            update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap(),
            (0, 0)
        );
        assert_eq!(read_stats(&a_info), (BASE_PRICE + 1, SLOT, 6));
        assert_eq!(read_stats(&b_info), (BASE_PRICE + 2, SLOT, 6));
    }

    /// Skip conditions the equivalence matrix does not reach: a strictly
    /// decreasing sequence id and a strictly decreasing slot. The slot case is
    /// what stops the gap subtraction underflowing.
    #[test]
    fn batch_skips_strictly_regressing_sequence_id_and_slot() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 50), older_sequence_info);
        market_account!(1, (BASE_PRICE, SLOT + 10, 5), future_slot_info);

        let payload = batch_payload(&[(0, BASE_PRICE + 1, 6), (1, BASE_PRICE + 1, 6)]);
        let accounts = [
            signer,
            state_info,
            older_sequence_info.clone(),
            future_slot_info.clone(),
        ];

        assert_eq!(
            update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap(),
            (0b11, 0)
        );
        assert_eq!(
            read_stats(&older_sequence_info),
            (BASE_PRICE, SLOT - 10, 50)
        );
        assert_eq!(read_stats(&future_slot_info), (BASE_PRICE, SLOT + 10, 5));
    }

    /// The step cap is symmetric, a move of exactly 1% is written verbatim, and
    /// a move beyond the cap is clamped to the cap rather than skipped, so it
    /// counts as accepted in the reject mask.
    #[test]
    fn batch_step_cap_is_symmetric_and_clamps_beyond_the_cap() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        // BASE_PRICE is 1_000_000, so exactly 1% is 10_000.
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), down_over_info);
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), down_at_cap_info);
        market_account!(2, (BASE_PRICE, SLOT - 10, 5), up_at_cap_info);
        market_account!(3, (BASE_PRICE, SLOT - 10, 5), up_over_info);

        let payload = batch_payload(&[
            (0, BASE_PRICE - 50_000, 6),
            (1, BASE_PRICE - 10_000, 6),
            (2, BASE_PRICE + 10_000, 6),
            (3, BASE_PRICE + 50_000, 6),
        ]);
        let accounts = [
            signer,
            state_info,
            down_over_info.clone(),
            down_at_cap_info.clone(),
            up_at_cap_info.clone(),
            up_over_info.clone(),
        ];

        assert_eq!(
            update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap(),
            (0, 0b1001),
            "a clamped write is accepted (empty reject mask) and reported in \
             the clamped mask"
        );
        assert_eq!(
            read_stats(&down_over_info),
            (BASE_PRICE - 10_000, SLOT, 6),
            "beyond-cap move must be clamped to the cap"
        );
        assert_eq!(
            read_stats(&down_at_cap_info),
            (BASE_PRICE - 10_000, SLOT, 6)
        );
        assert_eq!(read_stats(&up_at_cap_info), (BASE_PRICE + 10_000, SLOT, 6));
        assert_eq!(
            read_stats(&up_over_info),
            (BASE_PRICE + 10_000, SLOT, 6),
            "beyond-cap move must be clamped to the cap"
        );
    }

    #[test]
    fn batch_skips_negative_price() {
        // `oracle_validity` classifies a stored non-positive price as
        // `NonPositive` on read, so storing one buys nothing.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info); // bootstrap: stored price 0

        let accounts = [signer, state_info, market_info.clone()];
        let (mask, _) =
            update_mm_oracle_batch(&accounts, &batch_payload(&[(0, -BASE_PRICE, 1)]), SLOT)
                .unwrap();
        assert_eq!(mask, 0b1);
        assert_eq!(read_stats(&market_info), (0, 0, 0));
    }

    /// The same market listed twice must not alias its `RefCell` borrow, and must
    /// not be written twice: the first entry advances `mm_oracle_slot` to the
    /// current slot, so the second falls out on the slot check. Run at the
    /// maximum batch size, which also exercises the top bit of the reject mask.
    #[test]
    fn batch_tolerates_duplicate_market_accounts_at_max_size() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), market_info);

        // Every entry targets the same market with a strictly increasing
        // sequence id, so only the slot check can stop entries 1..n.
        let entries: Vec<(u16, i64, u64)> = (0..MM_ORACLE_BATCH_MAX_MARKETS)
            .map(|i| (0u16, BASE_PRICE + 1 + i as i64, 6 + i as u64))
            .collect();
        let payload = batch_payload(&entries);

        let mut accounts = vec![signer, state_info];
        accounts.extend(std::iter::repeat_n(
            market_info.clone(),
            MM_ORACLE_BATCH_MAX_MARKETS,
        ));

        let (mask, _) = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap();
        assert_eq!(
            mask, !1u64,
            "only the first entry for a duplicated market may land"
        );
        assert_eq!(read_stats(&market_info), (BASE_PRICE + 1, SLOT, 6));
    }

    #[test]
    fn batch_ignores_trailing_accounts() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), market_a_info);
        // Declared count is 1, so this must never be touched even though it is a
        // perfectly valid perp market sitting in the account list.
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), market_b_info);

        let accounts = [
            signer,
            state_info,
            market_a_info.clone(),
            market_b_info.clone(),
        ];
        update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE + 1, 6)]), SLOT).unwrap();

        assert_eq!(read_stats(&market_a_info), (BASE_PRICE + 1, SLOT, 6));
        assert_eq!(
            read_stats(&market_b_info),
            (BASE_PRICE, SLOT - 10, 5),
            "account beyond the declared count must be untouched"
        );
    }

    // equivalence with the single-market handler

    fn run_single_with_source(
        initial: MmStats,
        price: i64,
        sequence_id: u64,
        source_slot: u64,
    ) -> MmStats {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, initial, market_info);

        // Opcode 0 takes [market, signer, state].
        let accounts = [market_info.clone(), signer, state_info];
        update_mm_oracle(
            &accounts,
            &single_payload_with_source(price, sequence_id, source_slot),
            SLOT,
        )
        .unwrap();
        read_stats(&market_info)
    }

    fn run_single(initial: MmStats, price: i64, sequence_id: u64) -> MmStats {
        run_single_with_source(initial, price, sequence_id, SLOT)
    }

    fn run_batch_with_source(
        initial: MmStats,
        price: i64,
        sequence_id: u64,
        source_slot: u64,
    ) -> MmStats {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, initial, market_info);

        let accounts = [signer, state_info, market_info.clone()];
        update_mm_oracle_batch(
            &accounts,
            &batch_payload_with_source(&[(0, price, sequence_id, source_slot)]),
            SLOT,
        )
        .unwrap();
        read_stats(&market_info)
    }

    fn run_batch(initial: MmStats, price: i64, sequence_id: u64) -> MmStats {
        run_batch_with_source(initial, price, sequence_id, SLOT)
    }

    /// Pins opcode 2 to opcode 0 at the wire level. The per-market gating is
    /// shared (`apply_mm_oracle_update`), so this now guards the wrappers: the
    /// payload parsing, the prologue differences, and any future divergence.
    /// Zero and negative prices are excluded because their divergence is
    /// deliberate and is asserted separately below.
    ///
    /// Each case also asserts the expected result outright, so a bug in the
    /// shared core cannot pass by agreeing with itself.
    #[test]
    fn batch_matches_single_market_handler() {
        // (label, initial, price, sequence_id, expected)
        let cases: [(&str, MmStats, i64, u64, MmStats); 8] = [
            (
                "bootstrap from zero",
                (0, 0, 0),
                BASE_PRICE,
                1,
                (BASE_PRICE, SLOT, 1),
            ),
            (
                "accepts after a wide enough gap",
                (BASE_PRICE, SLOT - 10, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE + 1_000, SLOT, 6),
            ),
            (
                "accepts at exactly MM_ORACLE_MIN_SLOT_GAP",
                (BASE_PRICE, SLOT - 2, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE + 1_000, SLOT, 6),
            ),
            (
                "rejects one slot below the gap",
                (BASE_PRICE, SLOT - 1, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT - 1, 5),
            ),
            (
                "rejects an equal sequence id",
                (BASE_PRICE, SLOT - 10, 6),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT - 10, 6),
            ),
            (
                "rejects a lower sequence id",
                (BASE_PRICE, SLOT - 10, 7),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT - 10, 7),
            ),
            (
                "rejects a slot that is not in the future",
                (BASE_PRICE, SLOT, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT, 5),
            ),
            (
                "clamps a step above the cap and consumes the sequence id",
                (BASE_PRICE, SLOT - 10, 5),
                BASE_PRICE + 20_000,
                6,
                (BASE_PRICE + 10_000, SLOT, 6),
            ),
        ];

        for (label, initial, price, sequence_id, expected) in cases {
            let single = run_single(initial, price, sequence_id);
            let batch = run_batch(initial, price, sequence_id);
            assert_eq!(single, expected, "single handler wrong: {label}");
            assert_eq!(batch, expected, "batch handler wrong: {label}");
        }
    }

    /// The one documented behavioural divergence: opcode 0 returns `Err` on a
    /// non-positive price, which in a batch would destroy every other market's
    /// write, so opcode 2 skips it instead. Both reject; only the failure mode
    /// differs.
    #[test]
    fn non_positive_price_divergence_is_deliberate() {
        let initial = (BASE_PRICE, SLOT - 10, 5);

        for price in [0i64, -1, -BASE_PRICE] {
            // Batch: skipped, call succeeds, market untouched.
            assert_eq!(run_batch(initial, price, 6), initial);

            // Single: hard error, market untouched.
            let hot_key = Pubkey::new_unique();
            valid_prologue!(hot_key, state_info, signer);
            market_account!(0, initial, market_info);

            let accounts = [market_info.clone(), signer, state_info];
            assert!(
                update_mm_oracle(&accounts, &single_payload(price, 6), SLOT).is_err(),
                "price {price} must be a hard error on opcode 0"
            );
            assert_eq!(read_stats(&market_info), initial);
        }
    }

    /// Source-observation freshness on both handlers, symmetric around the
    /// landing slot. An update whose source slot is more than
    /// `MM_ORACLE_MAX_SOURCE_AGE` away in either direction is skipped. Behind
    /// the bound means it landed too late to be fresh. Ahead of the bound means
    /// a wrong-unit or wrong-scale source value that must not disable the gate.
    /// Exactly at the bound lands on both sides, so a crank may still estimate
    /// its landing slot.
    #[test]
    fn stale_source_slot_is_skipped_by_both_handlers() {
        let initial = (BASE_PRICE, SLOT - 10, 5);
        let written = (BASE_PRICE + 1_000, SLOT, 6);
        let max_age =
            crate::math::constants::MM_ORACLE_MAX_SOURCE_AGE.to_slots(SlotDuration::BASELINE);

        // (label, source_slot, expected)
        let cases: [(&str, u64, MmStats); 5] = [
            (
                "one slot beyond the bound is skipped",
                SLOT - max_age - 1,
                initial,
            ),
            ("exactly at the bound lands", SLOT - max_age, written),
            (
                "a landing-slot estimate at the forward bound lands",
                SLOT + max_age,
                written,
            ),
            (
                "one slot beyond the forward bound is skipped",
                SLOT + max_age + 1,
                initial,
            ),
            (
                "a wrong-unit source value is skipped, not accepted",
                1_700_000_000_000, // a millisecond timestamp
                initial,
            ),
        ];

        for (label, source_slot, expected) in cases {
            let single = run_single_with_source(initial, BASE_PRICE + 1_000, 6, source_slot);
            let batch = run_batch_with_source(initial, BASE_PRICE + 1_000, 6, source_slot);
            assert_eq!(single, expected, "single handler wrong: {label}");
            assert_eq!(batch, expected, "batch handler wrong: {label}");
        }

        // The skip is reported in the batch reject mask.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, initial, market_info);
        let accounts = [signer, state_info, market_info.clone()];
        let stale = SLOT - max_age - 1;
        let (mask, clamped) = update_mm_oracle_batch(
            &accounts,
            &batch_payload_with_source(&[(0, BASE_PRICE + 1_000, 6, stale)]),
            SLOT,
        )
        .unwrap();
        assert_eq!(mask, 0b1);
        assert_eq!(clamped, 0);
        assert_eq!(read_stats(&market_info), initial);
    }

    #[test]
    fn source_age_integrates_across_slot_duration_transition() {
        let clock = SlotClock::from_state_fields([1, 1, 1, 100], 0, 0, 0);
        // Four slots priced only at the 200ms endpoint would look exactly 800ms
        // old. Two of them were actually 250ms, so the observation is 900ms old.
        assert!(mm_oracle_source_slot_out_of_range(clock, 102, 98));
        assert!(!mm_oracle_source_slot_out_of_range(clock, 102, 99));
    }
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

#[cfg(test)]
mod feature_gate_tests {
    //! The permissionless slot duration sync reads the target IBRL gate's
    //! activation slot from its feature account and derives the effective slot
    //! from the cluster `EpochSchedule` (first slot of the epoch after the
    //! activation epoch, mirroring Agave). These pin the account parse, the
    //! epoch arithmetic, and every rejection path; the key/owner checks live as
    //! constraints on `SyncStateSlotDuration`.
    use {
        super::{
            feature_gate_effective_slot, ibrl_feature_gate, ibrl_slot_duration_ms,
            read_native_state_slot_clock, validated_slot_duration_archive_update,
            FEATURE_GATE_PROGRAM,
        },
        crate::state::state::State,
        anchor_lang::prelude::*,
    };

    fn activated(slot: u64) -> [u8; 9] {
        let mut d = [0u8; 9];
        d[0] = 1;
        d[1..9].copy_from_slice(&slot.to_le_bytes());
        d
    }

    fn account<'a>(
        key: &'a Pubkey,
        owner: &'a Pubkey,
        lamports: &'a mut u64,
        data: &'a mut [u8],
    ) -> AccountInfo<'a> {
        AccountInfo::new(key, false, false, lamports, data, owner, false)
    }

    #[test]
    fn gate_pubkeys_only_for_schedule_values() {
        for ms in [350, 300, 250, 200] {
            assert!(ibrl_feature_gate(ms).is_some());
        }
        // baseline, unset, and non-schedule values have no gate
        for ms in [400, 0, 375] {
            assert!(ibrl_feature_gate(ms).is_none());
        }
    }

    #[test]
    fn gate_keys_map_back_to_their_slot_duration() {
        for ms in [350u16, 300, 250, 200] {
            let key = ibrl_feature_gate(ms).unwrap();
            assert_eq!(ibrl_slot_duration_ms(&key), Some(ms));
        }
        assert_eq!(ibrl_slot_duration_ms(&Pubkey::new_unique()), None);
    }

    #[test]
    fn archive_sync_never_regresses_legacy_live_duration() {
        let mut state = State::default();
        state.slot_duration_ms = 200;

        // Starting catch-up with an old slower gate would make the archive
        // authoritative at 350ms and is rejected.
        assert!(validated_slot_duration_archive_update(&state, 0, 100, 200).is_err());

        // Sync the currently active fastest gate first, then historical gates
        // may be backfilled without changing the live 200ms duration.
        let slots = validated_slot_duration_archive_update(&state, 3, 100, 200).unwrap();
        state.slot_duration_transition_slots = slots;
        assert!(validated_slot_duration_archive_update(&state, 0, 10, 200).is_ok());
    }

    #[test]
    fn archive_sync_allows_skips_but_not_early_forward_advances() {
        let state = State::default();
        // No predecessor is required, so an abandoned 350ms gate cannot brick
        // synchronization of a verified 300ms transition.
        assert!(validated_slot_duration_archive_update(&state, 1, 100, 200).is_ok());

        let mut state = State::default();
        state.slot_duration_transition_slots[0] = 300;
        assert!(validated_slot_duration_archive_update(&state, 2, 400, 299).is_err());
        assert!(validated_slot_duration_archive_update(&state, 2, 400, 300).is_ok());
        state.slot_duration_transition_slots[2] = 400;
        assert!(validated_slot_duration_archive_update(&state, 1, 301, 300).is_ok());
        assert!(validated_slot_duration_archive_update(&state, 1, 401, 300).is_err());
    }

    #[test]
    fn effective_slot_is_first_slot_of_the_following_epoch() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let schedule = EpochSchedule::without_warmup();
        let mut lamports = 1u64;

        // mid epoch activation rounds up to the next epoch boundary, never
        // accelerating accounting for the rest of the activation epoch
        let mut data = activated(500_000);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            2 * 432_000
        );

        // epoch aligned activation still waits one full epoch
        let mut data = activated(432_000);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            2 * 432_000
        );

        let mut data = activated(1_000);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            432_000
        );
    }

    #[test]
    fn effective_slot_respects_warmup_epochs() {
        // With warmup the early epochs are shorter than 432,000 slots; the
        // boundary must come from the schedule, not a hardcoded epoch length.
        let key = ibrl_feature_gate(350).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let schedule = EpochSchedule::default();
        let mut lamports = 1u64;
        let activation = 1_000u64;
        let mut data = activated(activation);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        let expected =
            schedule.get_first_slot_in_epoch(schedule.get_epoch(activation).saturating_add(1));
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            expected
        );
    }

    #[test]
    fn inactive_is_rejected() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let mut lamports = 1u64;
        let mut data = [0u8; 9]; // data[0] == 0 => not activated by Anza yet
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert!(feature_gate_effective_slot(&acct, &EpochSchedule::without_warmup()).is_err());
    }

    #[test]
    fn malformed_flag_is_rejected() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let mut lamports = 1u64;
        let mut data = [2u8; 9]; // data[0] not in {0, 1}
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert!(feature_gate_effective_slot(&acct, &EpochSchedule::without_warmup()).is_err());
    }

    #[test]
    fn wrong_length_is_rejected() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let mut lamports = 1u64;
        let mut data = [1u8; 8]; // not the 9-byte feature layout
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert!(feature_gate_effective_slot(&acct, &EpochSchedule::without_warmup()).is_err());
    }

    // Exercises `State::slot_duration_from_account_info`. That is the validated
    // reader a foreign program such as vaults uses to read velocity's live slot
    // duration from a bare AccountInfo. AccountLoader needs a `'info` borrow
    // that such a program does not have.
    #[test]
    fn foreign_state_reader_validates_and_switches() {
        let (key, _) = Pubkey::find_program_address(&[b"velocity_state"], &crate::id());
        let mut lamports = 1u64;
        // 8-byte discriminator + zeroed State, with the staging fields written at
        // their real offsets
        let mut data = vec![0u8; 8 + std::mem::size_of::<State>()];
        data[..8].copy_from_slice(&State::DISCRIMINATOR);
        let put_u16 = |d: &mut [u8], off: usize, v: u16| {
            d[8 + off..8 + off + 2].copy_from_slice(&v.to_le_bytes())
        };
        put_u16(
            &mut data,
            std::mem::offset_of!(State, slot_duration_ms),
            350,
        );
        put_u16(
            &mut data,
            std::mem::offset_of!(State, pending_slot_duration_ms),
            300,
        );
        let eff_off = 8 + std::mem::offset_of!(State, slot_duration_effective_slot);
        data[eff_off..eff_off + 8].copy_from_slice(&1_000u64.to_le_bytes());

        let velocity_id = crate::id();
        {
            let acct = account(&key, &velocity_id, &mut lamports, &mut data);
            // before the effective slot: base 350; at/after: staged 300
            assert_eq!(
                State::slot_duration_from_account_info(&acct, 999)
                    .unwrap()
                    .as_ms(),
                350
            );
            assert_eq!(
                State::slot_duration_from_account_info(&acct, 1_000)
                    .unwrap()
                    .as_ms(),
                300
            );
        }
        // a correctly-owned State-shaped account at the wrong address is rejected
        let wrong_key = Pubkey::new_unique();
        {
            let acct = account(&wrong_key, &velocity_id, &mut lamports, &mut data);
            assert!(State::slot_duration_from_account_info(&acct, 0).is_err());
        }
        // wrong owner is rejected
        let not_velocity = Pubkey::new_unique();
        {
            let acct = account(&key, &not_velocity, &mut lamports, &mut data);
            assert!(State::slot_duration_from_account_info(&acct, 0).is_err());
        }
        // wrong discriminator is rejected
        data[0] ^= 0xff;
        {
            let acct = account(&key, &velocity_id, &mut lamports, &mut data);
            assert!(State::slot_duration_from_account_info(&acct, 0).is_err());
        }
    }

    // The native fast-path reader parses the slot-duration fields by raw byte
    // offset; verify it decodes the staged switch (not merely that the offset
    // constants match `offset_of!`, which the traits test covers).
    #[test]
    fn native_reader_switches_at_effective_slot() {
        let key = Pubkey::new_unique();
        let owner = crate::id();
        let mut lamports = 1u64;
        let mut data = vec![0u8; 8 + std::mem::size_of::<State>()];
        data[..8].copy_from_slice(&State::DISCRIMINATOR);
        let put_u16 = |d: &mut [u8], off: usize, v: u16| {
            d[8 + off..8 + off + 2].copy_from_slice(&v.to_le_bytes())
        };
        put_u16(
            &mut data,
            std::mem::offset_of!(State, slot_duration_ms),
            350,
        );
        put_u16(
            &mut data,
            std::mem::offset_of!(State, pending_slot_duration_ms),
            300,
        );
        let eff_off = 8 + std::mem::offset_of!(State, slot_duration_effective_slot);
        data[eff_off..eff_off + 8].copy_from_slice(&1_000u64.to_le_bytes());
        let acct = account(&key, &owner, &mut lamports, &mut data);
        // before the effective slot: base 350; at/after: staged 300
        assert_eq!(
            read_native_state_slot_clock(&acct)
                .unwrap()
                .slot_duration_at(999)
                .as_ms(),
            350
        );
        assert_eq!(
            read_native_state_slot_clock(&acct)
                .unwrap()
                .slot_duration_at(1_000)
                .as_ms(),
            300
        );
    }

    // Once any transition archive entry exists it is authoritative for both
    // validated readers: the legacy staging fields are ignored and the duration
    // switches exactly at each recorded transition slot.
    #[test]
    fn readers_prefer_the_transition_archive() {
        let (key, _) = Pubkey::find_program_address(&[b"velocity_state"], &crate::id());
        let mut lamports = 1u64;
        let mut data = vec![0u8; 8 + std::mem::size_of::<State>()];
        data[..8].copy_from_slice(&State::DISCRIMINATOR);
        // stale legacy staging fields that must lose to the archive
        let base_off = 8 + std::mem::offset_of!(State, slot_duration_ms);
        data[base_off..base_off + 2].copy_from_slice(&300u16.to_le_bytes());
        let transitions_off = 8 + std::mem::offset_of!(State, slot_duration_transition_slots);
        for (i, transition_slot) in [1_000u64, 2_000, 0, 0].iter().enumerate() {
            let off = transitions_off + i * 8;
            data[off..off + 8].copy_from_slice(&transition_slot.to_le_bytes());
        }

        let velocity_id = crate::id();
        let acct = account(&key, &velocity_id, &mut lamports, &mut data);
        for (slot, expected_ms) in [(999u64, 400u64), (1_000, 350), (1_999, 350), (2_000, 300)] {
            assert_eq!(
                State::slot_duration_from_account_info(&acct, slot)
                    .unwrap()
                    .as_ms(),
                expected_ms
            );
            assert_eq!(
                read_native_state_slot_clock(&acct)
                    .unwrap()
                    .slot_duration_at(slot)
                    .as_ms(),
                expected_ms
            );
        }
    }
}
