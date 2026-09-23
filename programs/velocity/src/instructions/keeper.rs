//! Permissionless cranks, one subject per module.
//!
//! Every handler here is work that somebody other than the account owner
//! performs: a keeper, a liquidator, or a relay executor. This root holds the
//! imports the subjects share and re-exports each of them, so a caller names
//! `instructions::keeper` and not the module a handler sits in.
//!
//! * [`fill`] fills a resting order through the router.
//! * [`signed_msg`] places a signed-message taker order, routes it, and rests
//!   the remainder on the book.
//! * [`trigger`] fires a trigger order and stages the relay crank for it.
//! * [`user_maintenance`] repairs a user account: force-cancel, idle flag,
//!   counter resync, and the equity breaker.
//! * [`force_delete`] retires a dormant, near-empty subaccount.
//! * [`settle_pnl`] settles perp pnl and funding for a user.
//! * [`liquidation`] holds the direct liquidation entrypoints.
//! * [`liquidation_swap`] holds the flash-loan liquidation pair.
//! * [`bankruptcy`] resolves a deficit or a bankruptcy against the insurance
//!   fund.
//! * [`funding`] cranks the funding rate and the bid/ask TWAP.
//! * [`prelaunch_oracle`] writes a prelaunch market's oracle.
//! * [`amm`] refreshes the AMMs and the AMM cache.
//! * [`spot_interest`] books spot lending interest and halts a short market.
//! * [`insurance_fund`] moves revenue into the insurance fund and resyncs a
//!   stake.
//! * [`perp_fees`] sweeps a perp market's accrued fee carveouts.
//! * [`revenue_share`] pays or writes off a builder or referrer row.
//!
//! CLOB cranks live in `crate::instructions::clob`.

use {
    super::optional_accounts::get_token_interface,
    crate::{
        auth::check_hot,
        controller::{
            self,
            insurance::update_user_stats_if_stake_amount,
            isolated_position::transfer_isolated_perp_position_deposit,
            liquidation::{liquidate_spot_with_swap_begin, liquidate_spot_with_swap_end},
            orders::cancel_orders,
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
                add_builder_order, get_revenue_share_escrow_account,
                load_escrow_owner_sub_accounts, load_maps, validate_and_load_builder, AccountMaps,
            },
            RouteFillAccounts, RoutedOrder,
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
            orders::{estimate_price_from_side, filter_bids_asks_by_oracle_divergence, Level},
            position::calculate_base_asset_value_and_pnl_with_oracle_price,
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
            prop_amm::{Direction, QuoterSlabExt, QuoterSlabV0},
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
            user::{
                MarketType, Order, OrderStatus, OrderTriggerCondition, OrderType, User, UserStats,
            },
            user_map::{load_user_maps, UserMap, UserStatsMap},
            zero_copy::{AccountZeroCopyMut, ZeroCopyLoader},
        },
        validate,
        validation::{
            sig_verification::{verify_and_decode_signed_msg, VerifiedMessage},
            user::{validate_user_deletion, validate_user_is_idle},
        },
        vlp::{amm::math::amm::calculate_net_user_pnl, amm_cache::CacheInfo},
        OracleSource, ID,
    },
    anchor_lang::{prelude::*, Discriminator},
    anchor_spl::{
        associated_token::{get_associated_token_address_with_program_id, AssociatedToken},
        token_interface::{Mint, TokenAccount, TokenInterface},
    },
    solana_program::{
        pubkey,
        sysvar::instructions::{self, ID as IX_ID},
    },
    std::convert::TryFrom,
};

mod amm;
mod bankruptcy;
mod force_delete;
mod funding;
mod insurance_fund;
mod liquidation;
mod liquidation_swap;
mod perp_fees;
mod prelaunch_oracle;
mod revenue_share;
mod settle_pnl;
mod signed_msg;
mod spot_interest;
mod user_maintenance;

pub use {
    amm::*, bankruptcy::*, force_delete::*, funding::*, insurance_fund::*, liquidation::*,
    liquidation_swap::*, perp_fees::*, prelaunch_oracle::*, revenue_share::*, settle_pnl::*,
    signed_msg::*, spot_interest::*, user_maintenance::*,
};

/// The leftover accounts of a router fill: the market maps, then the
/// sections [`RouteFillAccounts`] reads.
struct FillSections<'info> {
    maps: AccountMaps<'info>,
    route: RouteFillAccounts<'info>,
}

impl<'info> FillSections<'info> {
    fn load(
        remaining_accounts: &'info [AccountInfo<'info>],
        taker: &AccountLoader<'info, User>,
        market_index: u16,
        state: &State,
        slot: u64,
    ) -> Result<Self> {
        let iter = &mut remaining_accounts.iter().peekable();
        let maps = load_maps(
            iter,
            &get_writable_perp_market_set(market_index),
            &MarketSet::new(),
            slot,
            state.slot_clock(),
            Some(state.oracle_guard_rails),
        )?;

        Ok(Self {
            maps,
            route: RouteFillAccounts::read(remaining_accounts, iter, state, taker)?,
        })
    }
}

/// Bar a liquidator whose authority equity breaker is tripped: a
/// position-acquiring liquidation takes on the liquidatee's risk and earns a
/// fee, which is what the breaker freezes. PnL-settlement liquidations (no
/// new risk) and `liquidate_perp_with_fill` (no balance acquired) stay exempt.
fn require_liquidator_not_frozen(liquidator_stats: &UserStats) -> Result<()> {
    validate!(
        !liquidator_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "liquidator authority equity breaker is tripped"
    )?;
    Ok(())
}
