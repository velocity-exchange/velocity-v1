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
                find_bids_and_asks_from_users, Level,
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
            user::{MarketType, OrderStatus, OrderTriggerCondition, OrderType, User, UserStats},
            user_conditions::{UserConditionsV0, USER_CONDITIONS_PDA_SEED},
            user_map::{load_user_map, load_user_maps, UserMap, UserStatsMap},
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
mod fill;
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
mod trigger;
mod user_maintenance;

pub use {
    amm::*, bankruptcy::*, fill::*, force_delete::*, funding::*, insurance_fund::*, liquidation::*,
    liquidation_swap::*, perp_fees::*, prelaunch_oracle::*, revenue_share::*, settle_pnl::*,
    signed_msg::*, spot_interest::*, trigger::*, user_maintenance::*,
};

/// The leftover accounts of a router fill, split into the sections it reads.
///
/// Both keeper fill paths lay the sections out in the same order: the market
/// and oracle accounts, the maker and referrer set, the taker's revenue-share
/// escrow, and then the quoter tail.
struct FillSections<'info> {
    maps: AccountMaps<'info>,
    makers_and_referrer: UserMap<'info>,
    makers_and_referrer_stats: UserStatsMap<'info>,
    escrow: Option<RevenueShareEscrowZeroCopyMut<'info>>,
    referrer_is_accelerated: bool,
    /// The quoter section: the market's `QuoterSlabV0` plus the union of the
    /// consulted quoters' registered CPI accounts.
    ///
    /// A subslice rather than a collected list. What the sections above
    /// consumed is the difference in the iterator's remaining length, and
    /// borrowing from there costs nothing where cloning every account did.
    tail: &'info [AccountInfo<'info>],
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
        let (makers_and_referrer, makers_and_referrer_stats) = load_user_maps(iter, true)?;
        let escrow = if state.builder_codes_enabled() {
            get_revenue_share_escrow_account(iter, &load!(taker)?.authority)?
        } else {
            None
        };
        let referrer_is_accelerated = get_referrer_accelerated_status(iter, escrow.as_ref())?;
        Ok(Self {
            maps,
            makers_and_referrer,
            makers_and_referrer_stats,
            escrow,
            referrer_is_accelerated,
            tail: &remaining_accounts[remaining_accounts.len() - iter.len()..],
        })
    }

    /// The loaded-user set a quoter must not fill outside of.
    fn wire_users(&self) -> Result<Vec<crate::state::prop_amm::ClobUserRefV0>> {
        Ok(crate::state::prop_amm::quoter_wire_users(
            self.makers_and_referrer.user_ref_index()?.into_keys().map(
                |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                    authority,
                    sub_account_id,
                },
            ),
        )?)
    }

    /// Price the room of every counterparty the quote may stand on.
    ///
    /// Sizing runs before the quote, so a quoter never publishes depth this
    /// fill would refuse to settle against.
    fn quote_route<'a>(
        &mut self,
        inputs: crate::instructions::QuoteInputs<'a>,
        claim: Option<crate::instructions::RouteClaim<'_>>,
        taker_key: &Pubkey,
        clock: &Clock,
        scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> Result<crate::instructions::QuotedFill<'a, 'info>> {
        crate::instructions::quote_route(
            self.tail,
            inputs,
            claim,
            &mut crate::instructions::CapInputs {
                taker_key,
                makers_and_referrer: &self.makers_and_referrer,
                makers_and_referrer_stats: &self.makers_and_referrer_stats,
                maps: &mut self.maps,
                slot: clock.slot,
                now: clock.unix_timestamp,
            },
            scratch,
        )
    }

    /// Hand an assembled route to the perp fill, and report the base it moved.
    fn run_fill(
        &mut self,
        accounts: &FillAccounts<'_, 'info>,
        request: controller::orders::FillRequest<'_>,
        router_inputs: &mut RouterFillInputs<'_, '_, 'info>,
        clock: &Clock,
    ) -> Result<u64> {
        let (base_asset_amount_filled, _) = controller::orders::fill_perp_order(
            request,
            &*accounts.state.load()?,
            clock,
            controller::orders::PerpFillAccounts {
                user: accounts.user,
                user_stats: accounts.user_stats,
                filler: accounts.filler,
                filler_stats: accounts.filler_stats,
                rev_share_escrow: &mut self.escrow.as_mut(),
            },
            &mut controller::orders::FillParties {
                maps: &mut self.maps,
                makers_and_referrer: &self.makers_and_referrer,
                makers_and_referrer_stats: &self.makers_and_referrer_stats,
            },
            router_inputs,
        )?;
        Ok(base_asset_amount_filled)
    }
}

/// The market, price and clock facts every leg of a router fill reads.
struct RouteContext<'a, 'info> {
    market_index: u16,
    maps: &'a mut AccountMaps<'info>,
    state: &'a State,
    clock: &'a Clock,
}

/// What the router needs to know about the taker order it is about to fill.
struct RoutedOrder {
    direction: Direction,
    /// Base the order still has to fill.
    unfilled: u64,
    taker: crate::state::prop_amm::ClobUserRefV0,
    /// The worst price this fill accepts, or zero for no bound.
    limit_price: u64,
    /// The mark a quoter prices a capped maker's loss against.
    reference_price: i64,
    /// The market's initial margin ratio, which a quoter's oracle band
    /// defaults to.
    margin_ratio_initial: u32,
}

impl RouteContext<'_, '_> {
    /// Read the route facts off one taker order of `user`.
    ///
    /// The order is passed in rather than looked up, because a signed-message
    /// order never enters `user.orders` and the caller holds it.
    fn routed_order(
        &mut self,
        user: &User,
        order: &crate::state::user::Order,
        mode: FillMode,
    ) -> Result<RoutedOrder> {
        let position_base = user
            .get_perp_position(self.market_index)
            .map(|position| position.base_asset_amount)
            .ok();
        let (tick_size, oracle_id, margin_ratio_initial) = {
            let market = self.maps.perp_market_map.get_ref(&self.market_index)?;
            (
                market.order_tick_size,
                market.oracle_id(),
                market.margin_ratio_initial,
            )
        };
        Ok(RoutedOrder {
            direction: match order.direction {
                PositionDirection::Long => Direction::Long,
                PositionDirection::Short => Direction::Short,
            },
            unfilled: order.get_base_asset_amount_unfilled(position_base)?,
            taker: user.clob_user_ref(),
            limit_price: mode.quote_limit_price(
                order,
                self.clock.slot,
                tick_size,
                self.state.slot_clock(),
            ),
            reference_price: self.maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        })
    }
}

impl RoutedOrder {
    /// The inputs the route is quoted from. The caller owns `users`, because
    /// the inputs borrow it.
    fn quote_inputs<'a>(
        &self,
        market_index: u16,
        users: &'a [crate::state::prop_amm::ClobUserRefV0],
        taker_served_window: bool,
    ) -> crate::instructions::QuoteInputs<'a> {
        crate::instructions::QuoteInputs {
            market_index,
            margin_ratio_initial: self.margin_ratio_initial,
            direction: self.direction,
            size: self.unfilled,
            users,
            reference_price: self.reference_price,
            taker: self.taker,
            limit_price: self.limit_price,
            taker_served_window,
            consume_reservation: false,
        }
    }
}

/// Bar a liquidator whose authority equity breaker is tripped.
///
/// A position-acquiring liquidation both takes on the liquidatee's risk and
/// earns a liquidation fee. That is the risk-taking the authority-wide equity
/// breaker freezes, so a tripped authority must not liquidate out of a healthy
/// sibling subaccount. PnL-settlement liquidations stay allowed. They are
/// protocol-protective and acquire no new risk.
///
/// `liquidate_perp_with_fill` is also exempt: its liquidator routes the
/// position to the book and never acquires a balance.
fn require_liquidator_not_frozen(liquidator_stats: &UserStats) -> Result<()> {
    validate!(
        !liquidator_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "liquidator authority equity breaker is tripped"
    )?;
    Ok(())
}
