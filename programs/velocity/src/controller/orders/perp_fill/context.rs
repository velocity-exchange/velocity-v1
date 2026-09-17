//! The types the three layers of a perp fill share.
//!
//! The account bundles each layer is handed, the conditions one fill runs
//! under, and what the market oracle says the fill may do.

use {
    super::super::MakerOrderInfo,
    crate::{
        error::VelocityResult,
        instructions::optional_accounts::AccountMaps,
        math::{casting::Cast, orders::is_oracle_too_divergent_with_twap_5min, router::RouterLeg},
        state::{
            fill_mode::FillMode,
            oracle_map::OracleMap,
            state::State,
            user::{User, UserStats},
            user_map::{UserMap, UserStatsMap},
        },
    },
    anchor_lang::prelude::Pubkey,
    std::collections::BTreeMap,
};

/// Whether a fill may go on.
///
/// A refused fill is not an error. The keeper gets an empty fill back and the
/// transaction stands, so the work it did on the way here is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Proceed,
    Skip,
}

/// The conditions one fill runs under: when it runs, how it was asked to fill,
/// and what the market oracle lets it do.
///
/// The order layer reads this off the market once. The two layers below it
/// read these values instead of the market.
#[derive(Clone, Copy)]
pub struct FillConditions {
    pub mode: FillMode,
    pub now: i64,
    pub slot: u64,
    /// The safe mm oracle price every band is measured against.
    pub oracle_price: i64,
    /// The 5-minute oracle TWAP, read before this fill's own refresh advanced
    /// it.
    pub oracle_twap_5min: i64,
    /// The price an oracle-relative limit resolves against. `None` leaves such
    /// a limit unresolved.
    pub valid_oracle_price: Option<i64>,
    /// Whether the vAMM may fill this order at all.
    pub amm_is_available: bool,
    /// Whether the oracle is too old to price margin.
    pub oracle_stale_for_margin: bool,
    /// Whether the safe oracle admits a match fill.
    pub safe_match_fills_allowed: bool,
    /// Whether the raw exchange oracle admits a match fill.
    pub exchange_match_fills_allowed: bool,
}

impl FillConditions {
    /// The conditions a test sets by hand when it drives a layer below the
    /// order layer.
    ///
    /// Only the order layer reads the oracle price, the TWAP and the two
    /// match-fill gates, so a test that never reaches it leaves them neutral.
    #[cfg(test)]
    pub fn for_layer_test(
        mode: FillMode,
        now: i64,
        slot: u64,
        valid_oracle_price: Option<i64>,
        amm_is_available: bool,
        oracle_stale_for_margin: bool,
    ) -> Self {
        Self {
            mode,
            now,
            slot,
            oracle_price: valid_oracle_price.unwrap_or(0),
            oracle_twap_5min: 0,
            valid_oracle_price,
            amm_is_available,
            oracle_stale_for_margin,
            safe_match_fills_allowed: true,
            exchange_match_fills_allowed: true,
        }
    }

    /// Whether the oracle has run too far from its 5-minute TWAP for any fill.
    pub(super) fn oracle_too_divergent_with_twap(&self, state: &State) -> VelocityResult<bool> {
        is_oracle_too_divergent_with_twap_5min(
            self.oracle_price,
            self.oracle_twap_5min,
            state
                .oracle_guard_rails
                .max_oracle_twap_5min_percent_divergence()
                .cast()?,
        )
    }
}

/// The accounts one fill values and settles against.
pub struct FillParties<'a, 'm, 's, 'info> {
    pub maps: &'a mut AccountMaps<'info>,
    pub makers_and_referrer: &'a UserMap<'m>,
    pub makers_and_referrer_stats: &'a UserStatsMap<'s>,
}

/// The liquidity one fill may draw on: the DLOB maker orders discovery found,
/// and the external quoter books with the leg that executes on them.
pub struct OfferedLiquidity<'a, 'r, 'b, 'info> {
    pub dlob_makers: &'a [MakerOrderInfo],
    pub router: &'a mut RouterLeg<'r, 'b, 'info>,
}

/// The loaded counterparties the liquidity pass settles against, and the
/// oracle prices it values them at.
pub struct FillCounterparties<'a, 'o, 'm, 's> {
    pub oracle_map: &'a mut OracleMap<'o>,
    pub users: &'a UserMap<'m>,
    pub stats: &'a UserStatsMap<'s>,
}

/// The taker account the post-fill checks read, and the stats they arm.
pub struct TakerRefs<'a> {
    pub user: &'a User,
    pub stats: &'a mut UserStats,
}

/// What one fill moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FillAmounts {
    pub base: u64,
    pub quote: u64,
}

/// What one maker filled: signed base, and whether the position it landed in
/// is isolated.
#[derive(Clone, Copy, Debug)]
pub struct MakerFill {
    pub base: i64,
    pub is_isolated: bool,
}

/// Signed base each maker filled, and whether that maker is isolated.
pub type MakerFills = BTreeMap<Pubkey, (i64, bool)>;
