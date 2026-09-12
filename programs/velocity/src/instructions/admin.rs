//! Governance of the exchange, one subject per module.
//!
//! Every handler here writes configuration that only an admin key may move.
//! This root holds the imports the subjects share and re-exports each of them,
//! so a caller names `instructions::admin` and not the module a handler sits
//! in.
//!
//! * [`exchange`] holds the exchange-wide settings on `State`.
//! * [`authority`] holds the keys that may administer, and the tier each one
//!   unlocks.
//! * [`spot_market`] and [`perp_market`] create, configure, and retire a
//!   market.
//! * [`lending`] holds the deposit and borrow limits of a spot market.
//! * [`perp_risk`] holds the risk a perp market may carry.
//! * [`liquidation`] holds what a liquidation costs and how fast it runs.
//! * [`fees`] holds what the exchange charges.
//! * [`insurance_fund`] holds the insurance fund and the revenue that feeds it.
//! * [`oracles`], [`prelaunch_oracle`], and [`mm_oracle`] select and write the
//!   price a market reads.
//! * [`slot_duration`] tracks the cluster slot time the program measures
//!   against.
//! * [`market_settlement`] retires a market and drains its pools.
//! * [`vault_deposits`] moves tokens into the protocol.
//! * [`user_flags`] holds the per-user flags an admin sets.
//! * [`feature_flags`] holds the kill switches on `State`.
//! * [`contexts`] holds the accounts structs that several subjects share.
//!
//! AMM repeg and spread instructions live in `crate::vlp::amm::admin`. CLOB
//! admin instructions live in `crate::instructions::clob::admin`.

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

mod authority;
mod contexts;
#[cfg(not(feature = "mainnet-beta"))]
mod devnet_wipe;
mod exchange;
mod feature_flags;
mod fees;
mod insurance_fund;
mod lending;
mod liquidation;
mod market_settlement;
mod mm_oracle;
mod oracles;
mod perp_market;
mod perp_risk;
mod prelaunch_oracle;
mod slot_duration;
mod spot_market;
mod user_flags;
mod vault_deposits;

#[cfg(not(feature = "mainnet-beta"))]
pub use devnet_wipe::*;
pub use {
    authority::*, contexts::*, exchange::*, feature_flags::*, fees::*, insurance_fund::*,
    lending::*, liquidation::*, market_settlement::*, mm_oracle::*, oracles::*, perp_market::*,
    perp_risk::*, prelaunch_oracle::*, slot_duration::*, spot_market::*, user_flags::*,
    vault_deposits::*,
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
