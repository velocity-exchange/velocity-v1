//! Liquidation planning: which position to liquidate, by which route, from which subaccount, and
//! for how much
//!
//! A liquidatable user's positions are valued on the cached market state, and the largest
//! liability the program lets the liquidator take (by tier) is paired with the best asset. The
//! pair decides the route: settle pnl, a perp liquidation, pnl for deposit, borrow for pnl, or a
//! spot swap. A perp liquidation prefers a fill against resting makers and falls back to a
//! collateral takeover, reading the same oracle validities `liquidate_perp` checks onchain. No
//! function here sends a tx; `execute.rs` does.

use {
    crate::{
        common::{
            collateral::CollateralBook,
            keeper::unix_now_ms,
            metrics::{MarginStatus, UserMarginStatus},
            oracle::{project_perp_oracle, ExchangeState, PythPriceUpdate},
            tx::TakeoverFallback,
        },
        liquidator::{execute::LiquidationEngine, TARGET},
    },
    dashmap::DashMap,
    std::{
        collections::HashSet,
        sync::{Arc, RwLock},
    },
    velocity_rs::{
        dlob::{L3Order, DLOB},
        math::{
            constants::{
                BASE_PRECISION, MARGIN_PRECISION_U128, PRICE_PRECISION, QUOTE_PRECISION,
                SPOT_WEIGHT_PRECISION_U128,
            },
            tiers::{perp_tier_is_as_safe_as, AssetTierExt, ContractTierExt},
        },
        program::math::oracle::{is_oracle_valid_for_action, OracleValidity, VelocityAction},
        types::{
            accounts::User, MarginRequirementType, MarketType, PerpPosition, SpotBalanceType,
            SpotPosition,
        },
        MarketState, Pubkey, VelocityClient,
    },
};

/// Takeover routings a fallback marker may drive before it is dropped.
pub(super) const TAKEOVER_FALLBACK_MAX_ATTEMPTS: u32 = 3;
/// Lifetime of a fallback marker. A marker this old describes a book and an oracle state that no
/// longer exist.
pub(super) const TAKEOVER_FALLBACK_EXPIRY_MS: u64 = 30_000;
/// A liquidator subaccount at this many open perp or spot positions has no room for another.
const MAX_OPEN_POSITIONS: usize = 8;

#[derive(Debug, PartialEq)]
pub(super) enum LiquidationType {
    SettlePnl,
    PerpWithFill,
    PerpTakeover,
    PerpPnlForDeposit,
    BorrowForPerpPnl,
    SpotForSpot,
    Skip,
}

#[derive(Debug, Clone)]
pub(super) struct LiquidatablePosition {
    pub market_type: MarketType,
    pub market_index: u16,
    pub is_asset: bool,
    pub collateral_required: i128,
    pub base_amount: i64,
    pub quote_amount: i64,
}

/// What to do with one liquidatable user.
pub(super) enum LiquidationPlan {
    /// The user's only exposure is positive perp pnl in these markets: settle it.
    SettlePnl {
        markets: Vec<u16>,
    },
    /// Liquidate perp positions, isolated ones first, until one tx sends.
    Perp,
    /// Swap the user's spot borrows against their largest deposit.
    Spot,
    PerpPnlForDeposit {
        liability: LiquidatablePosition,
        asset: LiquidatablePosition,
    },
    BorrowForPerpPnl {
        liability: LiquidatablePosition,
        asset: LiquidatablePosition,
    },
    Skip(&'static str),
}

/// How to liquidate one perp position.
pub(super) enum PerpRoute {
    /// `liquidate_perp_with_fill` against resting makers, sent from `subaccount`.
    WithFill {
        subaccount: Pubkey,
        makers: Vec<User>,
    },
    /// `liquidate_perp`, taking the position over into `subaccount` against its collateral.
    Takeover {
        subaccount: Pubkey,
        base_asset_amount: u64,
        collateral_required: u128,
    },
    Skip(&'static str),
}

pub(super) struct PerpDecision {
    pub route: PerpRoute,
    /// The pyth-lazer update to post, when the program would use it.
    pub pyth_update: Option<PythPriceUpdate>,
    /// A fill on this position failed onchain, so a pending fallback marker forces a takeover.
    pub force_takeover: bool,
    pub fallback_key: (Pubkey, u16),
}

/// A pnl-for-deposit or borrow-for-pnl liquidation, sized to the subaccount's free collateral.
pub(super) struct PnlLiquidation {
    pub subaccount: Pubkey,
    pub liability: LiquidatablePosition,
    pub asset: LiquidatablePosition,
    /// The liability to transfer, in the liability's own units: quote for a perp pnl
    /// liability, spot tokens for a borrow.
    pub amount: u128,
    /// The collateral the transfer costs the liquidator, reserved while the tx is in flight.
    pub collateral_required: u128,
    pub pyth_update: Option<PythPriceUpdate>,
}

/// The oracle validities that decide the perp liquidation routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PerpOracleRoutePolicy {
    pub safe_match_allowed: bool,
    pub exchange_match_allowed: bool,
    pub liquidation_allowed: bool,
    pub uses_pyth_update: bool,
}

impl LiquidationEngine {
    /// Choose the route for a liquidatable user from their largest eligible liability and best
    /// asset.
    pub(super) fn plan(&self, user: &User) -> LiquidationPlan {
        let perp_positions = liquidatable_perp_positions(&self.market_state, &user.perp_positions);
        let spot_positions = liquidatable_spot_positions(&self.market_state, &user.spot_positions);
        let (safest_perp_tier, safest_spot_tier) = safest_tiers(user, self.keeper.velocity);

        let Some((liability, asset)) = pick_best_asset_liability_combo(
            &self.market_state,
            &perp_positions,
            &spot_positions,
            safest_perp_tier,
            safest_spot_tier,
        ) else {
            // a perp fill does not depend on the best asset and liability, so try it anyway;
            // with no perp position it skips on its own
            return LiquidationPlan::Perp;
        };

        let pnl_only = has_settleable_pnl_only(&perp_positions, &spot_positions);
        match liquidation_type(&liability, asset.as_ref(), pnl_only) {
            LiquidationType::SettlePnl => LiquidationPlan::SettlePnl {
                markets: perp_positions
                    .iter()
                    .filter(|position| {
                        position.base_amount == 0 && position.quote_amount != 0 && position.is_asset
                    })
                    .map(|position| position.market_index)
                    .collect(),
            },
            LiquidationType::PerpTakeover | LiquidationType::PerpWithFill => LiquidationPlan::Perp,
            LiquidationType::SpotForSpot => LiquidationPlan::Spot,
            LiquidationType::PerpPnlForDeposit => match asset {
                Some(asset) => LiquidationPlan::PerpPnlForDeposit { liability, asset },
                None => LiquidationPlan::Skip("no_asset_for_perp_pnl_for_deposit"),
            },
            LiquidationType::BorrowForPerpPnl => match asset {
                Some(asset) => LiquidationPlan::BorrowForPerpPnl { liability, asset },
                None => LiquidationPlan::Skip("no_asset_for_borrow_for_perp_pnl"),
            },
            LiquidationType::Skip => LiquidationPlan::Skip("no_viable_strategy"),
        }
    }

    /// Route one perp position: a fill against resting makers when the oracle allows a match
    /// and makers exist, else a takeover when the oracle allows a liquidation and a subaccount
    /// has the collateral.
    pub(super) fn plan_perp_position(
        &self,
        liquidatee: Pubkey,
        user: &User,
        position: &PerpPosition,
        kind: &'static str,
        slot: u64,
        pyth_update: Option<PythPriceUpdate>,
    ) -> PerpDecision {
        let market_index = position.market_index;
        let fallback_key = (liquidatee, market_index);
        let skip = |reason| PerpDecision {
            route: PerpRoute::Skip(reason),
            pyth_update: None,
            force_takeover: false,
            fallback_key,
        };

        let Some(&match_subaccount) = self.subaccounts.first() else {
            return skip("no_subaccount");
        };
        let Some(policy) = perp_oracle_route_policy(
            self.keeper.velocity,
            market_index,
            slot,
            pyth_update.as_ref(),
        ) else {
            return skip("invalid_oracle");
        };
        let Some(collateral_required) = perp_collateral_requirement(
            &self.market_state,
            market_index,
            position.base_asset_amount,
        ) else {
            return skip("collateral_calc_failed");
        };

        let free_collateral = self.collateral.max_available(&self.subaccounts);
        let makers = if match_participation_allowed(user, policy) {
            find_top_makers(
                self.keeper.velocity,
                self.dlob,
                &self.market_state,
                market_index,
                position.base_asset_amount,
                policy.exchange_match_allowed,
            )
        } else {
            None
        };
        let force_takeover = peek_takeover_fallback(
            &self.takeover_fallbacks,
            fallback_key,
            policy.liquidation_allowed,
            free_collateral,
            collateral_required,
            unix_now_ms(),
        );
        let method = perp_liquidation_method(
            free_collateral,
            collateral_required,
            makers.is_some(),
            policy.safe_match_allowed,
            policy.liquidation_allowed,
            force_takeover,
        );

        self.keeper
            .metrics
            .liquidation_attempts
            .with_label_values(&["perp"])
            .inc();
        log::info!(
            target: TARGET,
            "attempting liquidation: kind={kind} liquidatee={liquidatee:?} market={market_index} method={method:?} force_takeover={force_takeover} slot={slot}",
        );

        let route = match method {
            LiquidationType::PerpWithFill => match makers {
                Some(makers) => PerpRoute::WithFill {
                    subaccount: match_subaccount,
                    makers,
                },
                None => PerpRoute::Skip("no_makers"),
            },
            LiquidationType::PerpTakeover => match find_best_subaccount_for_liquidation(
                self.keeper.velocity,
                &self.subaccounts,
                true,
                false,
                &self.collateral,
                collateral_required,
            ) {
                Some(subaccount) => PerpRoute::Takeover {
                    subaccount,
                    base_asset_amount: position.base_asset_amount.unsigned_abs(),
                    collateral_required,
                },
                None => PerpRoute::Skip("no_subaccount_for_takeover"),
            },
            _ if !policy.liquidation_allowed && makers.is_none() => {
                PerpRoute::Skip("oracle_not_eligible")
            }
            _ => PerpRoute::Skip("no_eligible_route"),
        };

        PerpDecision {
            route,
            pyth_update: pyth_update.filter(|_| policy.uses_pyth_update),
            force_takeover,
            fallback_key,
        }
    }

    /// Size a pnl-for-deposit (`perp_liability`) or borrow-for-pnl liquidation to the free
    /// collateral of the subaccount that takes it.
    pub(super) fn plan_pnl_liquidation(
        &self,
        liability: LiquidatablePosition,
        asset: LiquidatablePosition,
        perp_liability: bool,
        pyth_update: Option<PythPriceUpdate>,
    ) -> Result<PnlLiquidation, &'static str> {
        let params = collateral_params(&self.market_state, &liability, &asset);
        // pnl for deposit takes on a spot position, borrow for pnl a perp one
        let (liability_size, needs_perp_room, needs_spot_room) = if perp_liability {
            (liability.collateral_required.unsigned_abs(), false, true)
        } else {
            (liability.base_amount.unsigned_abs() as u128, true, false)
        };
        let net_collateral_required =
            net_collateral_requirement(liability_size, &liability, &asset, &params);

        let subaccount = find_best_subaccount_for_liquidation(
            self.keeper.velocity,
            &self.subaccounts,
            needs_perp_room,
            needs_spot_room,
            &self.collateral,
            net_collateral_required.max(0) as u128,
        )
        .ok_or("no_subaccount_with_collateral")?;
        let available = self
            .collateral
            .available(&subaccount)
            .ok_or("no_free_collateral")? as i128;

        // sized in the liability's own units, which the instruction's max transfer takes
        let amount = if net_collateral_required <= available {
            liability_size
        } else {
            max_liquidation_amount(liability_size, available, available / 10, |size| {
                net_collateral_requirement(size, &liability, &asset, &params)
            })
        };
        if amount == 0 {
            return Err("zero_liq_amount");
        }
        let collateral_required =
            net_collateral_requirement(amount, &liability, &asset, &params).max(0) as u128;

        Ok(PnlLiquidation {
            subaccount,
            liability,
            asset,
            amount,
            collateral_required,
            pyth_update,
        })
    }
}

/// The user's liquidatable isolated positions, in market order.
pub(super) fn isolated_liquidatable_positions<'a>(
    user: &'a User,
    status: &'a UserMarginStatus,
) -> impl Iterator<Item = &'a PerpPosition> {
    status
        .isolated
        .iter()
        .filter(|(_, status)| *status == MarginStatus::Liquidatable)
        .filter_map(|(market_index, _)| {
            user.perp_positions.iter().find(|position| {
                position.market_index == *market_index
                    && position.isolated_position_scaled_balance != 0
            })
        })
}

/// The cross position to liquidate: the open position with the largest quote amount.
pub(super) fn largest_cross_position(user: &User) -> Option<&PerpPosition> {
    user.perp_positions
        .iter()
        .filter(|position| position.base_asset_amount != 0)
        .max_by_key(|position| position.quote_asset_amount.unsigned_abs())
}

/// Whether a pending fill-failed fallback should force the takeover route.
///
/// The marker is NOT removed here: a takeover can still die between this decision and the send
/// (no eligible subaccount, account-load failure, `send_tx` returning None), and consuming the
/// one-shot signal on those paths sent the next pass back to the maker route that already failed
/// onchain. The marker is removed when the forced takeover's tx is sent, and expires here after
/// `TAKEOVER_FALLBACK_MAX_ATTEMPTS` routings or `TAKEOVER_FALLBACK_EXPIRY_MS`, so it cannot
/// become permanent.
pub(super) fn peek_takeover_fallback(
    fallbacks: &DashMap<(Pubkey, u16), TakeoverFallback>,
    key: (Pubkey, u16),
    liquidation_allowed: bool,
    collateral_available: u128,
    collateral_required: u128,
    now_ms: u64,
) -> bool {
    let Some(mut entry) = fallbacks.get_mut(&key) else {
        return false;
    };
    if now_ms.saturating_sub(entry.recorded_ms) > TAKEOVER_FALLBACK_EXPIRY_MS
        || entry.attempts >= TAKEOVER_FALLBACK_MAX_ATTEMPTS
    {
        drop(entry);
        fallbacks.remove(&key);
        return false;
    }
    if !liquidation_allowed || collateral_available < collateral_required {
        return false;
    }
    entry.attempts += 1;
    true
}

/// Prefer a valid maker fill, then use collateral takeover.
pub(super) fn perp_liquidation_method(
    collateral_available: u128,
    collateral_required: u128,
    has_makers: bool,
    match_allowed: bool,
    liquidation_allowed: bool,
    force_takeover: bool,
) -> LiquidationType {
    let takeover_affordable = liquidation_allowed && collateral_available >= collateral_required;
    if force_takeover {
        return if takeover_affordable {
            LiquidationType::PerpTakeover
        } else {
            LiquidationType::Skip
        };
    }
    if match_allowed && has_makers {
        LiquidationType::PerpWithFill
    } else if takeover_affordable {
        LiquidationType::PerpTakeover
    } else {
        LiquidationType::Skip
    }
}

pub(super) fn route_policy_from_validities(
    exchange_validity: OracleValidity,
    safe_validity: OracleValidity,
    uses_pyth_update: bool,
) -> Option<PerpOracleRoutePolicy> {
    Some(PerpOracleRoutePolicy {
        safe_match_allowed: is_oracle_valid_for_action(
            safe_validity,
            Some(VelocityAction::FillOrderMatch),
        )
        .ok()?,
        exchange_match_allowed: is_oracle_valid_for_action(
            exchange_validity,
            Some(VelocityAction::FillOrderMatch),
        )
        .ok()?,
        // liquidate_perp validates the selected safe/MM oracle onchain
        // (`update_amm_and_check_validity` under `VelocityAction::Liquidate`), so takeover
        // eligibility reads the same view. Raw exchange validity stays a separate signal for
        // floored DLOB filtering.
        liquidation_allowed: is_oracle_valid_for_action(
            safe_validity,
            Some(VelocityAction::Liquidate),
        )
        .ok()?,
        uses_pyth_update,
    })
}

fn perp_oracle_route_policy(
    velocity: &VelocityClient,
    market_index: u16,
    slot: u64,
    pyth_price_update: Option<&PythPriceUpdate>,
) -> Option<PerpOracleRoutePolicy> {
    let market = velocity.try_get_perp_market_account(market_index).ok()?;
    let exchange = ExchangeState::load(velocity)?;
    let projected = project_perp_oracle(velocity, &exchange, &market, slot, pyth_price_update)?;
    route_policy_from_validities(
        projected.exchange_validity,
        projected.safe_validity,
        projected.uses_pyth_update,
    )
}

/// Whether the liquidatee can take part in a DLOB match at all under the oracle policy. Each
/// maker is checked in [`find_top_makers`].
pub(super) fn match_participation_allowed(
    liquidatee: &User,
    policy: PerpOracleRoutePolicy,
) -> bool {
    policy.safe_match_allowed && (liquidatee.equity_floor == 0 || policy.exchange_match_allowed)
}

/// Whether one maker can take the other side of a floored-participant match: a floored maker
/// needs the raw exchange oracle valid for the match policy, an unfloored maker always can.
pub(super) fn maker_matchable(maker: &User, exchange_match_allowed: bool) -> bool {
    maker.equity_floor == 0 || exchange_match_allowed
}

/// The book side whose resting orders can fill the liquidation. The liquidation order is the
/// position's opposite (closing a long places a short taker order), so a long liquidatee fills
/// against resting bids and a short one against resting asks.
pub(super) fn liquidation_makers_are_bids(base_asset_amount: i64) -> bool {
    base_asset_amount >= 0
}

pub(super) fn liquidation_type(
    liability: &LiquidatablePosition,
    asset: Option<&LiquidatablePosition>,
    has_pnl_only: bool,
) -> LiquidationType {
    if has_pnl_only {
        return LiquidationType::SettlePnl;
    }
    match (liability.market_type, asset.map(|asset| asset.market_type)) {
        (MarketType::Perp, None) => LiquidationType::PerpTakeover,
        (MarketType::Perp, Some(MarketType::Spot)) => LiquidationType::PerpPnlForDeposit,
        (MarketType::Spot, Some(MarketType::Perp)) => LiquidationType::BorrowForPerpPnl,
        (MarketType::Spot, Some(MarketType::Spot)) => LiquidationType::SpotForSpot,
        _ => LiquidationType::Skip,
    }
}

/// The initial margin a takeover of `base_asset_amount` needs, at the cached oracle price.
fn perp_collateral_requirement(
    market_state: &RwLock<MarketState>,
    market_index: u16,
    base_asset_amount: i64,
) -> Option<u128> {
    let state = market_state.read().unwrap().load();
    let perp_market = state.perp_market(market_index)?;
    let oracle = state.perp_oracle(market_index)?;

    let margin_ratio = perp_market
        .get_margin_ratio(
            base_asset_amount.unsigned_abs() as u128,
            MarginRequirementType::Initial,
        )
        .ok()?;

    Some(
        (base_asset_amount.unsigned_abs() as u128)
            .saturating_mul(oracle.price as u128)
            .saturating_mul(QUOTE_PRECISION)
            .saturating_mul(margin_ratio as u128)
            .saturating_div(MARGIN_PRECISION_U128)
            .saturating_div(PRICE_PRECISION)
            .saturating_div(BASE_PRECISION),
    )
}

/// Prices, precisions and weights for valuing a liability against an asset. A perp side is
/// valued in USDC.
pub(super) struct CollateralParams {
    liability_oracle_price: i64,
    liability_precision: u128,
    liability_weight: u128,
    liability_weight_precision: u128,
    asset_oracle_price: i64,
    asset_precision: u128,
    asset_weight: u128,
    asset_weight_precision: u128,
}

fn collateral_params(
    market_state: &RwLock<MarketState>,
    liability: &LiquidatablePosition,
    asset: &LiquidatablePosition,
) -> CollateralParams {
    let state = market_state.read().unwrap().load();

    let (liability_oracle_price, liability_precision, liability_weight) =
        if liability.market_type == MarketType::Spot {
            let oracle = state
                .spot_oracle(liability.market_index)
                .expect("liability oracle");
            let market = state
                .spot_market(liability.market_index)
                .expect("liability market");
            (
                oracle.price,
                10_u128.pow(market.decimals),
                market.initial_liability_weight as u128,
            )
        } else {
            let oracle = state.spot_oracle(0).expect("USDC oracle");
            (oracle.price, QUOTE_PRECISION, 1u128)
        };

    let (asset_oracle_price, asset_precision, asset_weight) =
        if asset.market_type == MarketType::Spot {
            let oracle = state.spot_oracle(asset.market_index).expect("asset oracle");
            let market = state.spot_market(asset.market_index).expect("asset market");
            (
                oracle.price,
                10_u128.pow(market.decimals),
                market.initial_asset_weight as u128,
            )
        } else {
            let oracle = state.spot_oracle(0).expect("USDC oracle");
            (oracle.price, QUOTE_PRECISION, SPOT_WEIGHT_PRECISION_U128)
        };

    CollateralParams {
        liability_oracle_price,
        liability_precision,
        liability_weight,
        liability_weight_precision: SPOT_WEIGHT_PRECISION_U128,
        asset_oracle_price,
        asset_precision,
        asset_weight,
        asset_weight_precision: SPOT_WEIGHT_PRECISION_U128,
    }
}

/// The collateral a liquidation of `liability_size` costs the liquidator: the liability it
/// takes on, less the asset it receives in return, each capped at the position's own value.
pub(super) fn net_collateral_requirement(
    liability_size: u128,
    liability: &LiquidatablePosition,
    asset: &LiquidatablePosition,
    params: &CollateralParams,
) -> i128 {
    let asset_amount_back_in_tokens = liability_size
        .saturating_mul(params.liability_oracle_price as u128)
        .saturating_div(params.asset_oracle_price as u128)
        .saturating_mul(params.asset_precision)
        .saturating_div(params.liability_precision);

    let asset_amount_back_in_collateral = asset_amount_back_in_tokens
        .saturating_mul(params.asset_oracle_price as u128)
        .saturating_mul(QUOTE_PRECISION)
        .saturating_mul(params.asset_weight)
        .saturating_div(params.asset_weight_precision)
        .saturating_div(params.asset_precision)
        .saturating_div(PRICE_PRECISION);

    let liability_collateral_impact = if liability.market_type == MarketType::Spot {
        liability_size
            .saturating_mul(params.liability_oracle_price as u128)
            .saturating_div(PRICE_PRECISION)
            .saturating_mul(QUOTE_PRECISION)
            .saturating_div(params.liability_precision)
            .saturating_mul(params.liability_weight)
            .saturating_div(params.liability_weight_precision)
    } else {
        liability
            .collateral_required
            .unsigned_abs()
            .min(liability_size)
    };

    liability_collateral_impact.saturating_sub(
        asset_amount_back_in_collateral.min(asset.collateral_required.unsigned_abs()),
    ) as i128
}

/// The largest liquidation amount whose collateral impact fits within the available collateral,
/// leaving at most `tolerance` of it unused.
pub(super) fn max_liquidation_amount<F>(
    max_amount: u128,
    available_collateral: i128,
    tolerance: i128,
    impact: F,
) -> u128
where
    F: Fn(u128) -> i128,
{
    let mut low = 0u128;
    let mut high = max_amount;
    let mut best = 0u128;

    while low <= high {
        let mid = (low + high) / 2;
        let difference = available_collateral - impact(mid);

        if difference < 0 {
            if mid == 0 {
                break;
            }
            high = mid - 1;
        } else {
            best = mid;
            if difference > tolerance {
                low = mid + 1;
            } else {
                break;
            }
        }
    }

    best
}

/// The collateral impact of each open perp position, or of its unsettled pnl when it has no
/// base.
fn liquidatable_perp_positions(
    market_state: &RwLock<MarketState>,
    perp_positions: &[PerpPosition],
) -> Vec<LiquidatablePosition> {
    let state = market_state.read().unwrap().load();

    perp_positions
        .iter()
        .filter(|position| position.base_asset_amount != 0 || position.quote_asset_amount != 0)
        .filter_map(|position| {
            let perp_market = state.perp_market(position.market_index)?;
            let oracle = state.perp_oracle(position.market_index)?;

            if position.base_asset_amount == 0 {
                state.spot_market(0)?;
                // signed: positive is claimable by the user (asset), negative is owed (liability)
                let claimable_pnl: i128 = position.get_claimable_pnl(oracle.price, 0).unwrap_or(0);
                return Some(LiquidatablePosition {
                    market_type: MarketType::Perp,
                    market_index: position.market_index,
                    is_asset: claimable_pnl > 0,
                    collateral_required: claimable_pnl.abs(),
                    base_amount: 0,
                    quote_amount: position.quote_asset_amount,
                });
            }

            let margin_ratio = perp_market
                .get_margin_ratio(
                    position.base_asset_amount.unsigned_abs() as u128,
                    MarginRequirementType::Initial,
                )
                .ok()?;
            let collateral = (position.base_asset_amount.unsigned_abs() as u128)
                .saturating_mul(oracle.price as u128)
                .saturating_mul(QUOTE_PRECISION)
                .saturating_mul(margin_ratio as u128)
                .saturating_div(MARGIN_PRECISION_U128)
                .saturating_div(PRICE_PRECISION)
                .saturating_div(BASE_PRECISION);

            Some(LiquidatablePosition {
                market_type: MarketType::Perp,
                market_index: position.market_index,
                is_asset: position.quote_asset_amount > 0,
                collateral_required: collateral as i128,
                base_amount: position.base_asset_amount,
                quote_amount: position.quote_asset_amount,
            })
        })
        .collect()
}

/// The weighted collateral impact of each spot deposit and borrow.
fn liquidatable_spot_positions(
    market_state: &RwLock<MarketState>,
    spot_positions: &[SpotPosition],
) -> Vec<LiquidatablePosition> {
    let state = market_state.read().unwrap().load();

    spot_positions
        .iter()
        .filter(|position| !position.is_available())
        .filter_map(|position| {
            let spot_market = state.spot_market(position.market_index)?;
            let oracle = state.spot_oracle(position.market_index)?;
            let token_amount = position.get_signed_token_amount(spot_market).ok()?;

            let token_precision = 10_u128.pow(spot_market.decimals);
            let weight = if position.balance_type == SpotBalanceType::Deposit {
                spot_market.initial_asset_weight
            } else {
                spot_market.initial_liability_weight
            };
            let collateral_impact = token_amount
                .unsigned_abs()
                .saturating_mul(oracle.price as u128)
                .saturating_div(PRICE_PRECISION)
                .saturating_mul(QUOTE_PRECISION)
                .saturating_div(token_precision)
                .saturating_mul(weight as u128)
                .saturating_div(SPOT_WEIGHT_PRECISION_U128);

            Some(LiquidatablePosition {
                market_type: MarketType::Spot,
                market_index: position.market_index,
                is_asset: position.balance_type == SpotBalanceType::Deposit,
                collateral_required: collateral_impact as i128,
                base_amount: token_amount as i64,
                quote_amount: 0,
            })
        })
        .collect()
}

/// The user's safest perp contract tier and spot asset tier, which bound the liabilities the
/// program lets a liquidator take. Port of the SDK's `getSafestTiers` in `user.ts`.
fn safest_tiers(user: &User, velocity: &VelocityClient) -> (u8, u8) {
    let mut safest_perp_tier = 4;
    let mut safest_spot_tier = 4;

    for position in user.perp_positions.iter().filter(|p| !p.is_available()) {
        // a zero-base position with positive unsettled pnl is a claim on the market's pnl
        // pool, not a liability (mirrors calculate_user_safest_position_tiers in the program)
        if !position.is_open_position()
            && !position.has_open_order()
            && position.isolated_position_scaled_balance == 0
            && position.quote_asset_amount > 0
        {
            continue;
        }
        if let Some(market) = velocity
            .program_data()
            .perp_market_config_by_index(position.market_index)
        {
            safest_perp_tier = safest_perp_tier.min(market.contract_tier.to_number());
        }
    }

    for position in user
        .spot_positions
        .iter()
        .filter(|p| !p.is_available() && p.balance_type != SpotBalanceType::Deposit)
    {
        if let Some(market) = velocity
            .program_data()
            .spot_market_config_by_index(position.market_index)
        {
            safest_spot_tier = safest_spot_tier.min(market.asset_tier.to_number());
        }
    }

    (safest_perp_tier, safest_spot_tier)
}

/// The largest liability the tiers allow, with the largest asset.
fn pick_best_asset_liability_combo(
    market_state: &RwLock<MarketState>,
    perp_positions: &[LiquidatablePosition],
    spot_positions: &[LiquidatablePosition],
    safest_perp_tier: u8,
    safest_spot_tier: u8,
) -> Option<(LiquidatablePosition, Option<LiquidatablePosition>)> {
    let state = market_state.read().unwrap().load();

    let (mut liabilities, mut assets): (Vec<LiquidatablePosition>, Vec<LiquidatablePosition>) =
        perp_positions
            .iter()
            .chain(spot_positions)
            .cloned()
            .partition(|position| !position.is_asset);
    let by_size_desc = |a: &LiquidatablePosition, b: &LiquidatablePosition| {
        b.collateral_required
            .abs()
            .cmp(&a.collateral_required.abs())
    };
    liabilities.sort_by(by_size_desc);
    assets.sort_by(by_size_desc);

    let largest_liability = liabilities.into_iter().find(|liability| {
        liability.market_type == MarketType::Spot
            || state
                .perp_market(liability.market_index)
                .is_some_and(|market| {
                    perp_tier_is_as_safe_as(
                        market.contract_tier.to_number(),
                        safest_perp_tier,
                        safest_spot_tier,
                    )
                })
    })?;

    Some((largest_liability, assets.first().cloned()))
}

/// The subaccount with the most free collateral that also has room for the new position.
fn find_best_subaccount_for_liquidation(
    velocity: &VelocityClient,
    subaccounts: &[Pubkey],
    needs_perp_room: bool,
    needs_spot_room: bool,
    collateral: &CollateralBook,
    min_collateral_required: u128,
) -> Option<Pubkey> {
    subaccounts
        .iter()
        .filter_map(|&subaccount| {
            let free = collateral.available(&subaccount)?;
            if free < min_collateral_required {
                return None;
            }

            let user = velocity.try_get_account::<User>(&subaccount).ok()?;
            let open_perps = user
                .perp_positions
                .iter()
                .filter(|p| p.base_asset_amount != 0 || p.quote_asset_amount != 0)
                .count();
            let open_spots = user
                .spot_positions
                .iter()
                .filter(|p| !p.is_available())
                .count();
            if (needs_perp_room && open_perps >= MAX_OPEN_POSITIONS)
                || (needs_spot_room && open_spots >= MAX_OPEN_POSITIONS)
            {
                return None;
            }

            Some((subaccount, free))
        })
        // the first subaccount wins a tie
        .min_by_key(|&(_, free)| std::cmp::Reverse(free))
        .map(|(subaccount, _)| subaccount)
}

/// Scan one side of the book until three loaded, unique, eligible makers are collected.
/// Eligibility is applied during the scan, not after a cap: a prefix of duplicate, unloadable or
/// floored-ineligible entries must not hide an eligible maker further down the book.
fn collect_top_makers(
    velocity: &VelocityClient,
    orders: impl Iterator<Item = L3Order>,
    exchange_match_allowed: bool,
) -> Vec<User> {
    let mut seen = HashSet::new();
    let mut makers: Vec<User> = Vec::with_capacity(3);
    for order in orders {
        if !order.is_maker() || !seen.insert(order.user) {
            continue;
        }
        let Ok(maker) = velocity.try_get_account::<User>(&order.user) else {
            continue;
        };
        if !maker_matchable(&maker, exchange_match_allowed) {
            continue;
        }
        makers.push(maker);
        if makers.len() == 3 {
            break;
        }
    }
    makers
}

/// The top makers on the book side that fills the liquidation. `None` when there are none.
fn find_top_makers(
    velocity: &VelocityClient,
    dlob: &DLOB,
    market_state: &Arc<RwLock<MarketState>>,
    market_index: u16,
    base_asset_amount: i64,
    exchange_match_allowed: bool,
) -> Option<Vec<User>> {
    let l3_book = dlob.get_l3_snapshot_safe(market_index, MarketType::Perp)?;
    let oracle_price = match market_state
        .read()
        .unwrap()
        .get_perp_oracle_price(market_index)
    {
        Some(data) if data.price > 0 => data.price as u64,
        _ => return None,
    };

    // only maker orders, so no vAMM or trigger price
    let makers = if liquidation_makers_are_bids(base_asset_amount) {
        collect_top_makers(
            velocity,
            l3_book.bids(Some(oracle_price), None, None),
            exchange_match_allowed,
        )
    } else {
        collect_top_makers(
            velocity,
            l3_book.asks(Some(oracle_price), None, None),
            exchange_match_allowed,
        )
    };

    if makers.is_empty() {
        log::warn!(target: TARGET, "no eligible makers found. market={market_index}");
        return None;
    }
    Some(makers)
}

/// A user's only remaining exposure is settleable positive perp pnl: no perp base positions, no
/// spot liabilities, and at least one settled-pnl-only perp position.
///
/// A user with any open perp base position must go through a real liquidation path: settled
/// pnl in one market must not shadow a liquidatable position in another.
pub(super) fn has_settleable_pnl_only(
    perp_positions: &[LiquidatablePosition],
    spot_positions: &[LiquidatablePosition],
) -> bool {
    perp_positions.iter().all(|p| p.base_amount == 0)
        && perp_positions
            .iter()
            .any(|p| p.base_amount == 0 && p.quote_amount != 0 && p.is_asset)
        && !spot_positions.iter().any(|s| !s.is_asset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perp_info(market_index: u16, is_asset: bool, base: i64, quote: i64) -> LiquidatablePosition {
        LiquidatablePosition {
            market_type: MarketType::Perp,
            market_index,
            is_asset,
            collateral_required: 1_000,
            base_amount: base,
            quote_amount: quote,
        }
    }

    fn spot_info(market_index: u16, is_asset: bool) -> LiquidatablePosition {
        LiquidatablePosition {
            market_type: MarketType::Spot,
            market_index,
            is_asset,
            collateral_required: 1_000,
            base_amount: 100,
            quote_amount: 0,
        }
    }

    #[test]
    fn perp_method_prefers_fill_falls_back_to_takeover() {
        assert_eq!(
            perp_liquidation_method(0, 100, true, true, true, false),
            LiquidationType::PerpWithFill
        );
        assert_eq!(
            perp_liquidation_method(100, 100, true, false, true, false),
            LiquidationType::PerpTakeover
        );
        assert_eq!(
            perp_liquidation_method(99, 100, true, false, true, false),
            LiquidationType::Skip
        );
        assert_eq!(
            perp_liquidation_method(100, 100, true, true, true, true),
            LiquidationType::PerpTakeover
        );
    }

    #[test]
    fn takeover_fallback_survives_failed_routings() {
        let fallbacks = DashMap::new();
        let key = (Pubkey::new_unique(), 7);
        let now = 1_000_u64;
        let marker = TakeoverFallback {
            recorded_ms: now,
            attempts: 0,
        };

        // gates failing must not consume the one-shot signal
        fallbacks.insert(key, marker);
        assert!(!peek_takeover_fallback(&fallbacks, key, true, 99, 100, now));
        assert!(fallbacks.contains_key(&key));
        assert!(!peek_takeover_fallback(
            &fallbacks, key, false, 100, 100, now
        ));
        assert!(fallbacks.contains_key(&key));

        // a passing peek routes the takeover but keeps the marker: the send can still fail,
        // and the next pass must not fall back to the maker route that already failed onchain
        assert!(peek_takeover_fallback(&fallbacks, key, true, 100, 100, now));
        assert!(fallbacks.contains_key(&key));
        assert_eq!(fallbacks.get(&key).unwrap().attempts, 1);
    }

    #[test]
    fn takeover_fallback_is_bounded() {
        let fallbacks = DashMap::new();
        let key = (Pubkey::new_unique(), 7);
        let now = 1_000_u64;

        // attempts cap
        fallbacks.insert(
            key,
            TakeoverFallback {
                recorded_ms: now,
                attempts: 0,
            },
        );
        for _ in 0..TAKEOVER_FALLBACK_MAX_ATTEMPTS {
            assert!(peek_takeover_fallback(&fallbacks, key, true, 100, 100, now));
        }
        assert!(!peek_takeover_fallback(
            &fallbacks, key, true, 100, 100, now
        ));
        assert!(!fallbacks.contains_key(&key), "capped marker is dropped");

        // wall-clock expiry
        fallbacks.insert(
            key,
            TakeoverFallback {
                recorded_ms: now,
                attempts: 0,
            },
        );
        assert!(!peek_takeover_fallback(
            &fallbacks,
            key,
            true,
            100,
            100,
            now + TAKEOVER_FALLBACK_EXPIRY_MS + 1,
        ));
        assert!(!fallbacks.contains_key(&key), "expired marker is dropped");
    }

    #[test]
    fn oracle_policy_uses_takeover_when_matching_is_not_allowed() {
        let policy = route_policy_from_validities(
            OracleValidity::TooUncertain,
            OracleValidity::TooUncertain,
            false,
        )
        .unwrap();

        assert!(!policy.safe_match_allowed);
        assert!(!policy.exchange_match_allowed);
        assert!(policy.liquidation_allowed);
        assert_eq!(
            perp_liquidation_method(
                100,
                100,
                true,
                policy.exchange_match_allowed,
                policy.liquidation_allowed,
                false,
            ),
            LiquidationType::PerpTakeover
        );
    }

    #[test]
    fn oracle_policy_skips_when_liquidation_is_not_allowed() {
        // liquidation eligibility reads the safe/MM oracle, the view liquidate_perp validates
        // onchain
        for validity in [OracleValidity::NonPositive, OracleValidity::TooVolatile] {
            let policy =
                route_policy_from_validities(OracleValidity::Valid, validity, false).unwrap();

            assert!(!policy.liquidation_allowed);
            assert_eq!(
                perp_liquidation_method(
                    100,
                    100,
                    false,
                    policy.safe_match_allowed,
                    policy.liquidation_allowed,
                    false,
                ),
                LiquidationType::Skip
            );
        }
    }

    #[test]
    fn takeover_eligibility_follows_safe_validity_not_exchange() {
        // a fresh MM price can keep the safe oracle valid while the raw exchange oracle is not;
        // the program accepts the takeover in that state, so the keeper must not skip it
        let policy =
            route_policy_from_validities(OracleValidity::TooVolatile, OracleValidity::Valid, false)
                .unwrap();
        assert!(policy.liquidation_allowed);
        assert!(!policy.exchange_match_allowed);

        // the reverse disagreement is rejected onchain, so it must skip
        let policy =
            route_policy_from_validities(OracleValidity::Valid, OracleValidity::TooVolatile, false)
                .unwrap();
        assert!(!policy.liquidation_allowed);
    }

    #[test]
    fn invalid_exchange_oracle_excludes_floored_match_participants() {
        let policy = route_policy_from_validities(
            OracleValidity::TooUncertain,
            OracleValidity::Valid,
            false,
        )
        .unwrap();
        let mut liquidatee = User::default();
        let floored_maker = User {
            equity_floor: 1,
            ..Default::default()
        };
        let regular_maker = User::default();

        liquidatee.equity_floor = 1;
        assert!(!match_participation_allowed(&liquidatee, policy));

        liquidatee.equity_floor = 0;
        assert!(match_participation_allowed(&liquidatee, policy));
        assert!(!maker_matchable(
            &floored_maker,
            policy.exchange_match_allowed
        ));
        assert!(maker_matchable(
            &regular_maker,
            policy.exchange_match_allowed
        ));
    }

    #[test]
    fn liquidation_makers_come_from_the_opposite_book_side() {
        // the liquidation order is the position's opposite
        // (`get_liquidation_order_params` uses `existing_direction.opposite()`):
        // closing a long places a short taker order, which fills against bids
        assert!(liquidation_makers_are_bids(1_000));
        assert!(!liquidation_makers_are_bids(-1_000));
    }

    #[test]
    fn pnl_only_requires_no_open_perp_positions() {
        // regression: a settled-pnl-only market used to shadow an open perp position, routing
        // the user to SettlePnl and never liquidating them
        let pnl_only = perp_info(0, true, 0, 500);
        let open_position = perp_info(1, false, 1_000_000, -500);

        assert!(has_settleable_pnl_only(
            std::slice::from_ref(&pnl_only),
            &[]
        ));
        assert!(!has_settleable_pnl_only(
            &[pnl_only.clone(), open_position],
            &[]
        ));
        // spot liability also disqualifies
        assert!(!has_settleable_pnl_only(
            std::slice::from_ref(&pnl_only),
            &[spot_info(1, false)]
        ));
        // spot deposits are fine
        assert!(has_settleable_pnl_only(&[pnl_only], &[spot_info(1, true)]));
        // no settleable pnl at all
        assert!(!has_settleable_pnl_only(&[], &[]));
    }

    #[test]
    fn largest_position_selected_by_absolute_quote() {
        // regression: selection used signed quote_asset_amount, so long positions (negative
        // quote = cost basis) always lost to any short
        let mut user = User::default();
        user.perp_positions[0].market_index = 0;
        user.perp_positions[0].base_asset_amount = 1_000;
        user.perp_positions[0].quote_asset_amount = 500; // small short
        user.perp_positions[1].market_index = 1;
        user.perp_positions[1].base_asset_amount = -100_000;
        user.perp_positions[1].quote_asset_amount = -50_000; // large long

        assert_eq!(largest_cross_position(&user).unwrap().market_index, 1);
    }

    #[test]
    fn max_liquidation_amount_respects_available_collateral() {
        // identity impact: liquidating N costs N collateral
        let impact = |size: u128| size as i128;
        let best = max_liquidation_amount(1_000, 100, 10, impact);
        assert!(best <= 100, "must not exceed available collateral");
        assert!(
            best >= 90,
            "should use available collateral up to tolerance"
        );

        // nothing affordable
        assert_eq!(max_liquidation_amount(1_000, -5, 10, impact), 0);
    }
}
