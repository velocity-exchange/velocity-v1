//! The vocabulary every liquidation path shares: who it moves value between,
//! what it is asked to move, and the terms the exchange sets for it.
//!
//! A liquidation names the same three things whatever form it takes. The
//! parties are the failing account and the liquidator that takes its risk on.
//! The request names the markets and the most the liquidator offers to take.
//! The terms are the clock and the exchange-wide limits that bound the size.
//! Each path builds its own request, because the markets and the amounts
//! differ, but they all speak the same parties and the same terms.

use super::*;

/// The two accounts a liquidation moves value between.
pub struct LiquidationParties<'a> {
    /// The failing account.
    pub user: &'a mut User,
    pub user_key: &'a Pubkey,
    /// The account that takes the risk on.
    pub liquidator: &'a mut User,
    pub liquidator_key: &'a Pubkey,
}

/// The parties of a perp liquidation, with the stats accounts that record the
/// volume each side trades.
pub struct PerpLiquidationParties<'a> {
    pub user: &'a mut User,
    pub user_key: &'a Pubkey,
    pub user_stats: &'a mut UserStats,
    pub liquidator: &'a mut User,
    pub liquidator_key: &'a Pubkey,
    pub liquidator_stats: &'a mut UserStats,
}

/// The accounts a book-filled perp liquidation reads.
///
/// This path places an order and fills it, so it hands the loaders on to the
/// fill instead of holding the accounts open across the call.
pub struct PerpFillLiquidationAccounts<'a, 'info> {
    pub user: &'a AccountLoader<'info, User>,
    pub user_key: &'a Pubkey,
    pub user_stats: &'a AccountLoader<'info, UserStats>,
    pub liquidator: &'a AccountLoader<'info, User>,
    pub liquidator_key: &'a Pubkey,
    pub liquidator_stats: &'a AccountLoader<'info, UserStats>,
    pub makers_and_referrer: &'a UserMap<'info>,
    pub makers_and_referrer_stats: &'a UserStatsMap<'info>,
}

/// When a liquidation runs, and the exchange-wide limits that bound its size.
#[derive(Clone, Copy, Debug)]
pub struct LiquidationTerms {
    pub now: i64,
    pub slot: u64,
    /// The extra margin an account must clear to leave liquidation.
    pub margin_buffer_ratio: u32,
    /// The share of the shortage the first call may close.
    pub initial_pct_to_liquidate: u128,
    /// The time over which the allowed share ramps to the whole shortage.
    pub duration: Millis,
}

impl LiquidationTerms {
    /// The terms the state account sets.
    pub fn from_state(state: &State, now: i64, slot: u64) -> Self {
        Self {
            now,
            slot,
            margin_buffer_ratio: state.liquidation_margin_buffer_ratio,
            initial_pct_to_liquidate: state.initial_pct_to_liquidate as u128,
            duration: state.liquidation_duration_ms(),
        }
    }

    /// The margin context a liquidation measures the account with.
    pub fn margin_context(&self) -> MarginContext {
        MarginContext::liquidation(self.margin_buffer_ratio)
    }

    /// The margin context, with one market's own requirement tracked.
    pub fn margin_context_tracking(
        &self,
        market: MarketIdentifier,
    ) -> VelocityResult<MarginContext> {
        self.margin_context()
            .track_market_margin_requirement(market)
    }
}

/// A perp position reduction: the market, and the most the liquidator takes.
#[derive(Clone, Copy, Debug)]
pub struct LiquidatePerpRequest {
    pub market_index: u16,
    pub liquidator_max_base_asset_amount: u64,
    /// The worst transfer price the liquidator accepts.
    pub limit_price: Option<u64>,
}

/// A borrow repayment paid for with the account's deposit.
#[derive(Clone, Copy, Debug)]
pub struct LiquidateSpotRequest {
    pub asset_market_index: u16,
    pub liability_market_index: u16,
    pub liquidator_max_liability_transfer: u128,
    pub limit_price: Option<u64>,
}

/// The opening half of a swap-backed spot liquidation.
#[derive(Clone, Copy, Debug)]
pub struct LiquidateSpotSwapBeginRequest {
    pub asset_market_index: u16,
    pub liability_market_index: u16,
    /// The deposit amount the swap takes out.
    pub swap_amount_in: u64,
}

/// The closing half of a swap-backed spot liquidation, with what the swap
/// actually moved.
#[derive(Clone, Copy, Debug)]
pub struct LiquidateSpotSwapEndRequest {
    pub asset_market_index: u16,
    pub liability_market_index: u16,
    pub asset_transfer: u128,
    pub liability_transfer: u128,
}

/// A borrow the liquidator takes over in exchange for the account's positive
/// perp pnl.
#[derive(Clone, Copy, Debug)]
pub struct LiquidateBorrowForPerpPnlRequest {
    pub perp_market_index: u16,
    pub liability_market_index: u16,
    pub liquidator_max_liability_transfer: u128,
    pub limit_price: Option<u64>,
}

/// Negative perp pnl the liquidator takes over in exchange for the account's
/// deposit.
#[derive(Clone, Copy, Debug)]
pub struct LiquidatePerpPnlForDepositRequest {
    pub perp_market_index: u16,
    pub asset_market_index: u16,
    pub liquidator_max_pnl_transfer: u128,
    pub limit_price: Option<u64>,
}
