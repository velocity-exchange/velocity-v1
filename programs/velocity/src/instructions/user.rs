//! What a user does with their own account, one subject per module.
//!
//! Every handler here acts for the account owner or their delegate. This root
//! holds the imports the subjects share and re-exports each of them, so a
//! caller names `instructions` and not the module a handler sits in.
//!
//! * [`lifecycle`] creates, deletes, and reclaims rent from an account.
//! * [`settings`] holds the per-account flags and the delegate key.
//! * [`signed_msg`] holds the signed-message order account and its websocket
//!   delegates.
//! * [`revenue_share`] holds the referrer name, the builder escrow, and the
//!   builders an account approves.
//! * [`deposit`] moves tokens between a token account and a spot market.
//! * [`transfer_deposit`] moves a spot balance between two subaccounts of one
//!   authority.
//! * [`transfer_pools`] moves a deposit and a borrow between two pools.
//! * [`transfer_position`] moves a perp position to another subaccount or to
//!   the vAMM.
//! * [`isolated_position`] funds and drains the collateral of an isolated perp
//!   position.
//! * [`orders`] places, cancels, and modifies orders.
//! * [`place_and_take`] holds the two taker routes that place and fill in one
//!   instruction.
//! * [`swap`] holds the two halves of a spot swap.
//!
//! Keeper work on another user's account is in `crate::instructions::keeper`.
//! The CLOB-aware endpoints are in `crate::instructions::clob`.

// Only the mainnet build gates external depositors on the allowlist.
#[cfg(feature = "mainnet-beta")]
use crate::ids::WHITELISTED_EXTERNAL_DEPOSITORS;
use {
    crate::{
        controller::{
            self,
            funding::settle_funding_payment,
            orders::{
                cancel_orders, validate_spot_dlob_trading_enabled_for_market_type, ModifyOrderId,
                PlaceOrderResult,
            },
            position::{update_position_and_market, PositionDelta, PositionDirection},
            spot_balance::update_revenue_pool_balances,
            spot_position::{
                update_spot_balances_and_cumulative_deposits,
                update_spot_balances_and_cumulative_deposits_with_limits,
            },
        },
        error::ErrorCode,
        get_then_update_id,
        ids::{lighthouse, marinade_mainnet, WHITELISTED_SWAP_PROGRAMS},
        instructions::{
            constraints::*,
            optional_accounts::{
                add_builder_order, get_referrer_accelerated_status,
                get_referrer_and_referrer_stats, get_revenue_share_escrow_account,
                get_whitelist_token, load_maps, validate_and_load_builder, validate_builder_fee,
                AccountMaps,
            },
        },
        load, load_mut,
        math::{
            self,
            casting::Cast,
            constants::{
                EQUITY_FLOOR_SWAP_MAX_VALUE_LOSS_BPS, MAX_BASE_ASSET_AMOUNT_WITH_AMM,
                ONE_BPS_DENOMINATOR, THIRTEEN_DAY,
            },
            liquidation::is_cross_margin_being_liquidated,
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_max_withdrawable_amount, calculate_net_equity_for_floor,
                calculate_user_equity, meets_initial_margin_requirement,
                meets_place_order_margin_requirement, validate_spot_margin_trading,
                MarginRequirementType,
            },
            oracle::{is_oracle_valid_for_action, LogMode, OracleValidity, VelocityAction},
            orders::{
                calculate_existing_position_fields_for_order_action, get_position_delta_for_fill,
                is_multiple_of_step_size, standardize_price_i64,
            },
            position::calculate_base_asset_value_with_oracle_price,
            safe_math::SafeMath,
            spot_balance::get_token_value,
            spot_swap::{self, calculate_swap_price, validate_price_bands_for_swap},
        },
        math_error,
        optional_accounts::{get_token_interface, get_token_mint},
        print_error, safe_decrement, safe_increment,
        state::{
            events::{
                emit_stack, DepositDirection, DepositExplanation, DepositRecord, NewUserRecord,
                OrderAction, OrderActionExplanation, OrderActionRecord, OrderRecord, SwapRecord,
            },
            fill_mode::FillMode,
            margin_calculation::MarginContext,
            market_status::MarketStatus,
            oracle::{OraclePriceData, StrictOraclePrice},
            oracle_map::OracleMap,
            order_params::{
                parse_optional_params, ModifyOrderParams, OrderParams,
                PlaceAndTakeOrderSuccessCondition, PlaceOrderOptions, PostOnlyParam,
            },
            paused_operations::{PerpOperation, SpotOperation},
            perp_market::PerpMarket,
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            revenue_share::{
                BuilderInfo, RevenueShare, RevenueShareEscrow, RevenueShareEscrowLoader,
                RevenueShareEscrowZeroCopyMut, RevenueShareOrder, REVENUE_SHARE_ESCROW_PDA_SEED,
                REVENUE_SHARE_PDA_SEED,
            },
            scale_order_params::ScaleOrderParams,
            signed_msg_user::{
                SignedMsgOrderId, SignedMsgUserOrders, SignedMsgWsDelegates, SIGNED_MSG_PDA_SEED,
                SIGNED_MSG_WS_PDA_SEED,
            },
            spot_market::{SpotBalanceType, SpotMarket},
            spot_market_map::{
                get_writable_spot_market_set, get_writable_spot_market_set_from_many,
            },
            state::State,
            traits::Size,
            user::{
                transfer_equity_floor, MarketType, Order, OrderStatus, OrderType, ReferrerName,
                ReferrerStatus, SpecialUserStatus, User, UserStats,
            },
            user_conditions::{UserConditionsV0, USER_CONDITIONS_PDA_SEED},
            user_map::{load_user_maps, UserMap, UserStatsMap},
        },
        validate,
        validation::{
            position::validate_perp_position_with_perp_market, user::validate_user_deletion,
            whitelist::validate_whitelist_token,
        },
        ExchangeStatus,
    },
    anchor_lang::{
        prelude::{borsh::BorshDeserialize, *},
        solana_program::system_instruction::transfer,
        Discriminator,
    },
    anchor_spl::{
        associated_token::AssociatedToken,
        token::Token,
        token_2022::Token2022,
        token_interface::{Mint, TokenAccount, TokenInterface},
    },
    solana_program::{
        program::invoke,
        sysvar::{instructions, instructions::ID as IX_ID},
    },
    std::{collections::BTreeSet, convert::TryFrom, iter::Peekable, ops::DerefMut, slice::Iter},
};

mod deposit;
#[cfg(feature = "isolated-position")]
mod isolated_position;
mod lifecycle;
mod orders;
mod place_and_take;
mod revenue_share;
mod settings;
mod signed_msg;
mod swap;
mod transfer_deposit;
mod transfer_pools;
mod transfer_position;

#[cfg(feature = "isolated-position")]
pub use isolated_position::*;
pub use {
    deposit::*, lifecycle::*, orders::*, place_and_take::*, revenue_share::*, settings::*,
    signed_msg::*, swap::*, transfer_deposit::*, transfer_pools::*, transfer_position::*,
};

/// Load the market and oracle maps for an instruction that writes one spot
/// market.
fn load_one_spot_market_maps<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    state: &State,
    market_index: u16,
    slot: u64,
) -> Result<AccountMaps<'a>> {
    Ok(load_maps(
        account_info_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set(market_index),
        slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?)
}

/// Load the market and oracle maps for an instruction that writes one perp
/// market.
fn load_one_perp_market_maps<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    state: &State,
    market_index: u16,
    slot: u64,
) -> Result<AccountMaps<'a>> {
    Ok(load_maps(
        account_info_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?)
}

/// Load the market and oracle maps for an instruction that touches no market
/// of its own. The order paths pass the markets they need as read-only
/// accounts.
fn load_no_market_maps<'a>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'a>>>,
    state: &State,
    slot: u64,
) -> Result<AccountMaps<'a>> {
    Ok(load_maps(
        account_info_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?)
}

/// The two same-authority subaccounts a transfer moves a spot balance between,
/// and the keys their records carry.
struct TransferParties<'a> {
    from_user: &'a mut User,
    to_user: &'a mut User,
    from_user_key: Pubkey,
    to_user_key: Pubkey,
    /// The delegate that signed, stamped on both records. `None` when the
    /// account owner signed.
    signer: Option<Pubkey>,
}

/// The part of a `DepositRecord` that changes from one spot balance move to
/// the next. The user and the market supply the rest.
struct SpotBalanceMove {
    ts: i64,
    direction: DepositDirection,
    amount: u64,
    oracle_price: i64,
    explanation: DepositExplanation,
    transfer_user: Option<Pubkey>,
    signer: Option<Pubkey>,
    total_deposits_after: u64,
    total_withdraws_after: u64,
}

/// Take the next record id of the market and emit the record of one spot
/// balance move.
fn emit_spot_balance_move(
    user: &User,
    user_key: Pubkey,
    spot_market: &mut SpotMarket,
    moved: SpotBalanceMove,
) -> Result<()> {
    let user_token_amount_after = user.get_total_token_amount(spot_market)?;
    let deposit_record_id = get_then_update_id!(spot_market, next_deposit_record_id);

    emit!(DepositRecord {
        ts: moved.ts,
        deposit_record_id,
        user_authority: user.authority,
        user: user_key,
        direction: moved.direction,
        amount: moved.amount,
        oracle_price: moved.oracle_price,
        market_index: spot_market.market_index,
        market_deposit_balance: spot_market.deposit_balance,
        market_withdraw_balance: spot_market.borrow_balance,
        market_cumulative_deposit_interest: spot_market.cumulative_deposit_interest,
        market_cumulative_borrow_interest: spot_market.cumulative_borrow_interest,
        total_deposits_after: moved.total_deposits_after,
        total_withdraws_after: moved.total_withdraws_after,
        explanation: moved.explanation,
        transfer_user: moved.transfer_user,
        signer: moved.signer,
        user_token_amount_after,
    });

    Ok(())
}

/// Leave cross-margin liquidation when the account no longer meets the
/// liquidation threshold.
fn exit_liquidation_if_healthy(
    user: &mut User,
    maps: &mut AccountMaps,
    liquidation_margin_buffer_ratio: u32,
) -> Result<()> {
    if !user.is_cross_margin_being_liquidated() {
        return Ok(());
    }

    if !is_cross_margin_being_liquidated(user, maps, liquidation_margin_buffer_ratio)? {
        user.exit_cross_margin_liquidation();
    }

    Ok(())
}
