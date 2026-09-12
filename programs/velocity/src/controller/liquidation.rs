//! Liquidation: reducing a failing account, and resolving what is left when
//! reduction cannot.
//!
//! Every path here opens on a margin shortage and closes when the shortage is
//! gone. They differ in what they move and who takes it on:
//!
//! * [`context`] names the parties, the request and the terms every path
//!   speaks.
//! * [`margin`] holds the margin arithmetic they repeat, and the keeper entry
//!   point that only latches the account.
//! * [`perp_entry`] holds the opening steps the two perp paths share.
//! * [`perp`] hands a failing perp position to a liquidator.
//! * [`perp_fill`] sells that position to the book instead.
//! * [`spot_side`] names one side of a spot exchange.
//! * [`spot`] repays a failing borrow out of the account's deposit.
//! * [`spot_swap`] repays it through an external swap, in two halves.
//! * [`borrow_for_perp_pnl`] pays a borrow with the account's positive perp
//!   pnl.
//! * [`perp_pnl_for_deposit`] pays negative perp pnl with the account's
//!   deposit.
//! * [`estate`] realizes what a bankrupt estate owns before anyone else pays.
//! * [`perp_bankruptcy`] and [`spot_bankruptcy`] run the tranches that cover
//!   what the estate cannot.
//!
//! The instruction handlers are in `crate::instructions::keeper`. The pricing
//! and sizing math is in `crate::math::liquidation`.

use {
    crate::{
        controller::{
            funding::settle_funding_payment,
            orders::{
                self, cancel_order, fill_perp_order_without_external_books, place_perp_order,
            },
            position::{
                get_position_index, update_position_and_market, update_quote_asset_amount,
                update_quote_asset_and_break_even_amount, update_settled_pnl, PositionDelta,
                PositionDirection,
            },
            spot_balance::{
                check_spot_oracle_validity, transfer_spot_balances,
                update_protocol_fee_pool_balances, update_revenue_pool_balances,
                update_spot_balances, update_spot_market_and_check_validity,
                update_spot_market_cumulative_interest,
            },
            spot_position::update_spot_balances_and_cumulative_deposits,
        },
        error::{ErrorCode, VelocityResult},
        get_then_update_id,
        instructions::optional_accounts::AccountMaps,
        load_mut,
        math::{
            bankruptcy::{
                has_pending_cross_margin_perp_bankruptcy, has_realizable_spot_assets_for_setoff,
                is_cross_margin_bankrupt, perp_markets_with_forfeitable_claims,
            },
            casting::Cast,
            constants::{
                LIQUIDATION_FEE_PRECISION, LIQUIDATION_FEE_PRECISION_U128,
                LIQUIDATION_PCT_PRECISION, LST_POOL_ID, QUOTE_PRECISION, QUOTE_PRECISION_I128,
                QUOTE_PRECISION_U64, QUOTE_SPOT_MARKET_INDEX, SPOT_WEIGHT_PRECISION,
            },
            liquidation::{
                calculate_asset_transfer_for_liability_transfer,
                calculate_asset_transfer_for_liability_transfer_exact,
                calculate_base_asset_amount_to_cover_margin_shortage,
                calculate_cumulative_deposit_interest_delta_to_resolve_bankruptcy,
                calculate_funding_rate_deltas_to_resolve_bankruptcy,
                calculate_liability_transfer_implied_by_asset_amount,
                calculate_liability_transfer_to_cover_margin_shortage,
                calculate_liquidation_multiplier, calculate_max_pct_to_liquidate,
                calculate_perp_if_fee, calculate_spot_if_fee,
                calculate_user_protective_asset_price, calculate_user_protective_liability_price,
                get_liquidation_fee, get_liquidation_order_params,
                validate_swap_within_liquidation_boundaries,
                validate_transfer_satisfies_limit_price, LiquidationMultiplierType,
            },
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, meets_initial_margin_requirement,
                MarginRequirementType,
            },
            oracle::{is_oracle_valid_for_action, LogMode, VelocityAction},
            orders::{
                calculate_existing_position_fields_for_order_action, get_position_delta_for_fill,
                is_multiple_of_step_size, is_oracle_too_divergent_with_twap_5min,
                standardize_base_asset_amount, standardize_base_asset_amount_ceil,
            },
            position::calculate_base_asset_value_with_oracle_price,
            safe_math::SafeMath,
            spot_balance::{get_token_amount, get_token_value},
            time::Millis,
        },
        msg,
        state::{
            events::{
                LiquidateBorrowForPerpPnlRecord, LiquidatePerpPnlForDepositRecord,
                LiquidatePerpRecord, LiquidateSpotRecord, LiquidationRecord, LiquidationType,
                OrderAction, OrderActionExplanation, OrderActionRecord, OrderRecord,
                PerpBankruptcyRecord, SpotBankruptcyRecord,
            },
            fill_mode::FillMode,
            liquidation_mode::{get_perp_liquidation_mode, LiquidatePerpMode},
            margin_calculation::{MarginCalculation, MarginContext, MarketIdentifier},
            market_status::MarketStatus,
            order_params::{OrderParams, PlaceOrderOptions},
            paused_operations::{PerpOperation, SpotOperation},
            perp_market::{ContractTier, PerpMarket},
            perp_market_map::PerpMarketMap,
            spot_market::{AssetTier, SpotBalance, SpotBalanceType},
            spot_market_map::SpotMarketMap,
            state::State,
            user::{MarketType, Order, OrderStatus, OrderType, User, UserStats},
            user_map::{UserMap, UserStatsMap},
        },
        validate,
        vlp::amm::{controller::get_fee_pool_tokens, refresh::update_amm_and_check_validity},
    },
    anchor_lang::prelude::*,
    std::ops::{Deref, DerefMut},
};

mod borrow_for_perp_pnl;
mod context;
mod estate;
mod margin;
mod perp;
mod perp_bankruptcy;
mod perp_entry;
mod perp_fill;
mod perp_pnl_for_deposit;
mod spot;
mod spot_bankruptcy;
mod spot_side;
mod spot_swap;

pub use {
    borrow_for_perp_pnl::*,
    context::*,
    margin::{calculate_margin_freed, set_user_status_to_being_liquidated},
    perp::*,
    perp_bankruptcy::*,
    perp_fill::*,
    perp_pnl_for_deposit::*,
    spot::*,
    spot_bankruptcy::*,
    spot_swap::*,
};
pub(crate) use {estate::*, margin::*, perp_entry::*, spot_side::*};

#[cfg(test)]
mod tests;
