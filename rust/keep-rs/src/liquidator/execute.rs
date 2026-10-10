//! Liquidation execution: the engine that runs a plan, and the txs each route sends
//!
//! `LiquidationEngine::liquidate` asks `plan.rs` for the route, then builds and sends the tx for
//! it. A perp liquidation walks the user's liquidatable positions, isolated first, planning and
//! sending one at a time until a tx goes out. A route that takes collateral (a takeover, pnl for
//! deposit, borrow for pnl) reserves it in the `CollateralBook` before its tx is sent, so
//! concurrent liquidations cannot commit the same collateral twice, and the tx worker settles or
//! releases the reservation with the tx's outcome.

use {
    crate::{
        common::{
            collateral::CollateralBook,
            keeper::Keeper,
            metrics::MarginStatus,
            oracle::PythPriceUpdate,
            tx::{
                scale_cu_limit_for_accounts, with_spot_interest_cranks, TakeoverFallback, TxIntent,
            },
        },
        liquidator::{
            plan::{
                isolated_liquidatable_positions, largest_cross_position, LiquidatablePosition,
                LiquidationPlan, PerpRoute, PnlLiquidation,
            },
            worker::LiquidationRequest,
            LiquidationOutcome, BLOCKED_SPOT_MARKETS, TARGET,
        },
    },
    dashmap::DashMap,
    std::{
        borrow::Cow,
        sync::{Arc, RwLock},
    },
    velocity_rs::{
        dlob::DLOB,
        jupiter::JupiterSwapApi,
        titan::{self, TitanSwapApi},
        types::{accounts::User, PerpPosition, SpotBalanceType},
        MarketState, Pubkey, TransactionBuilder, Wallet,
    },
};

/// Spot liquidations swap through Jupiter or Titan, whose routes need far more compute than a
/// perp liquidation.
const SPOT_LIQUIDATION_CU_LIMIT: u32 = 400_000;
/// Plans and sends liquidations. One engine serves every task the worker spawns.
pub(super) struct LiquidationEngine {
    pub keeper: Keeper,
    pub dlob: &'static DLOB,
    pub market_state: Arc<RwLock<MarketState>>,
    pub subaccounts: Vec<Pubkey>,
    pub use_spot_liquidation: bool,
    pub collateral: CollateralBook,
    /// Markers left by liquidate-with-fill txs that failed to fill, keyed by (liquidatee,
    /// market). See `plan::peek_takeover_fallback`.
    pub takeover_fallbacks: Arc<DashMap<(Pubkey, u16), TakeoverFallback>>,
}

/// Which pnl liquidation instruction to send.
#[derive(Clone, Copy)]
enum PnlKind {
    PerpPnlForDeposit,
    BorrowForPerpPnl,
}

impl LiquidationEngine {
    /// Liquidate one user along the route `plan` picks.
    pub async fn liquidate(
        &self,
        request: &LiquidationRequest,
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        match self.plan(&request.user) {
            LiquidationPlan::SettlePnl { markets } => {
                self.send_settle_pnl(request.pubkey, &markets, priority_fee, cu_limit)
                    .await
            }
            LiquidationPlan::Perp => {
                self.liquidate_perp_positions(request, priority_fee, cu_limit)
                    .await
            }
            LiquidationPlan::Spot if self.use_spot_liquidation => {
                self.send_spot_liquidations(request, priority_fee).await
            }
            LiquidationPlan::Spot => LiquidationOutcome::Skipped("spot_liquidation_disabled"),
            LiquidationPlan::PerpPnlForDeposit { liability, asset } => {
                // the perp market in this route is the liability side
                let pyth_update = request
                    .pyth_price_updates
                    .get(&liability.market_index)
                    .cloned();
                match self.plan_pnl_liquidation(liability, asset, true, pyth_update) {
                    Ok(plan) => {
                        self.send_pnl_liquidation(
                            PnlKind::PerpPnlForDeposit,
                            request,
                            plan,
                            priority_fee,
                            cu_limit,
                        )
                        .await
                    }
                    Err(reason) => LiquidationOutcome::Skipped(reason),
                }
            }
            LiquidationPlan::BorrowForPerpPnl { liability, asset } => {
                // the perp market in this route is the asset side
                let pyth_update = request.pyth_price_updates.get(&asset.market_index).cloned();
                match self.plan_pnl_liquidation(liability, asset, false, pyth_update) {
                    Ok(plan) => {
                        self.send_pnl_liquidation(
                            PnlKind::BorrowForPerpPnl,
                            request,
                            plan,
                            priority_fee,
                            cu_limit,
                        )
                        .await
                    }
                    Err(reason) => LiquidationOutcome::Skipped(reason),
                }
            }
            LiquidationPlan::Skip(reason) => LiquidationOutcome::Skipped(reason),
        }
    }

    /// Each liquidatable isolated position, then the largest cross position, until one tx
    /// sends. Returns the last skip reason when none does.
    async fn liquidate_perp_positions(
        &self,
        request: &LiquidationRequest,
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        let mut last_skip = None;
        for position in isolated_liquidatable_positions(&request.user, &request.status) {
            let outcome = self
                .liquidate_perp_position(request, position, "perp_isolated", priority_fee, cu_limit)
                .await;
            if outcome.is_sent() {
                return outcome;
            }
            last_skip = Some(outcome.reason());
        }

        if request.status.cross != MarginStatus::Liquidatable {
            return LiquidationOutcome::Skipped(last_skip.unwrap_or("cross_not_liquidatable"));
        }
        let Some(position) = largest_cross_position(&request.user) else {
            log::info!(target: TARGET, "no perp positions with base_asset_amount for {:?}, skipping perp liquidation", request.pubkey);
            return LiquidationOutcome::Skipped("no_perp_positions");
        };
        self.liquidate_perp_position(request, position, "perp_cross", priority_fee, cu_limit)
            .await
    }

    async fn liquidate_perp_position(
        &self,
        request: &LiquidationRequest,
        position: &PerpPosition,
        kind: &'static str,
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        let market_index = position.market_index;
        let decision = self.plan_perp_position(
            request.pubkey,
            &request.user,
            position,
            kind,
            request.slot,
            request.pyth_price_updates.get(&market_index).cloned(),
        );

        let outcome = match decision.route {
            PerpRoute::WithFill { subaccount, makers } => {
                self.send_perp_with_fill(
                    request,
                    market_index,
                    subaccount,
                    &makers,
                    decision.pyth_update.as_ref(),
                    priority_fee,
                    cu_limit,
                )
                .await
            }
            PerpRoute::Takeover {
                subaccount,
                base_asset_amount,
                collateral_required,
            } => {
                self.send_perp_takeover(
                    request,
                    market_index,
                    subaccount,
                    base_asset_amount,
                    collateral_required,
                    decision.pyth_update.as_ref(),
                    priority_fee,
                    cu_limit,
                )
                .await
            }
            PerpRoute::Skip(reason) => LiquidationOutcome::Skipped(reason),
        };

        // the fallback marker is spent by a sent takeover, not by the decision to route one
        if decision.force_takeover && outcome.is_sent() {
            self.takeover_fallbacks.remove(&decision.fallback_key);
        }
        outcome
    }

    /// The liquidator subaccount's and the liquidatee's cached accounts.
    fn accounts(
        &self,
        subaccount: &Pubkey,
        liquidatee: &Pubkey,
    ) -> Result<(User, User), &'static str> {
        let Some(keeper_account) = self.keeper.cached_user(subaccount) else {
            log::debug!(target: TARGET, "keeper acc lookup failed={subaccount:?}");
            return Err("keeper_account_lookup_failed");
        };
        let Some(liquidatee_account) = self.keeper.cached_user(liquidatee) else {
            log::debug!(target: TARGET, "liquidatee acc lookup failed={liquidatee:?}");
            return Err("liquidatee_account_lookup_failed");
        };
        Ok((keeper_account, liquidatee_account))
    }

    /// A tx from `subaccount` with the priority fee, the CU limit and, when given, the
    /// pyth-lazer post the liquidation reads.
    fn liquidation_tx<'a>(
        &self,
        subaccount: Pubkey,
        keeper_account: &'a User,
        pyth_update: Option<&PythPriceUpdate>,
        priority_fee: u64,
        cu_limit: u32,
    ) -> TransactionBuilder<'a> {
        let tx_builder = self.keeper.tx_builder(
            subaccount,
            Cow::Borrowed(keeper_account),
            priority_fee,
            cu_limit,
        );
        match pyth_update {
            Some(update) => {
                tx_builder.post_pyth_lazer_oracle_update(&[update.feed_id], &update.message)
            }
            None => tx_builder,
        }
    }

    /// `liquidate_perp_with_fill`: the liquidation order fills against resting makers. No
    /// collateral is reserved, since the liquidator takes no position.
    #[allow(clippy::too_many_arguments)]
    async fn send_perp_with_fill(
        &self,
        request: &LiquidationRequest,
        market_index: u16,
        subaccount: Pubkey,
        makers: &[User],
        pyth_update: Option<&PythPriceUpdate>,
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        let liquidatee = request.pubkey;
        if makers.is_empty() {
            log::debug!(target: TARGET, "skip empty maker cross. market={market_index} user={liquidatee}");
            return LiquidationOutcome::Skipped("no_makers");
        }
        let (keeper_account, liquidatee_account) = match self.accounts(&subaccount, &liquidatee) {
            Ok(accounts) => accounts,
            Err(reason) => return LiquidationOutcome::Skipped(reason),
        };

        let tx_builder = self
            .liquidation_tx(
                subaccount,
                &keeper_account,
                pyth_update,
                priority_fee,
                cu_limit,
            )
            .liquidate_perp_with_fill(market_index, &liquidatee_account, makers);
        let (tx_builder, cu_limit) = scale_cu_limit_for_accounts(tx_builder, cu_limit, 20, 20);

        let intent = TxIntent::LiquidateWithFill {
            market_index,
            liquidatee,
            slot: request.slot,
        };
        match self
            .keeper
            .tx
            .send_tx(tx_builder.build(), intent, cu_limit as u64)
            .await
        {
            Some(signature) => {
                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind=perp_with_fill liquidatee={liquidatee:?} market={market_index} makers={} sig={signature} slot={}",
                    makers.len(),
                    request.slot,
                );
                LiquidationOutcome::TxSent
            }
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    /// `liquidate_perp`: `subaccount` takes the position over against its collateral.
    #[allow(clippy::too_many_arguments)]
    async fn send_perp_takeover(
        &self,
        request: &LiquidationRequest,
        market_index: u16,
        subaccount: Pubkey,
        base_asset_amount: u64,
        collateral_required: u128,
        pyth_update: Option<&PythPriceUpdate>,
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        let liquidatee = request.pubkey;
        let (keeper_account, liquidatee_account) = match self.accounts(&subaccount, &liquidatee) {
            Ok(accounts) => accounts,
            Err(reason) => return LiquidationOutcome::Skipped(reason),
        };
        let Some(reservation) = self.collateral.try_reserve(subaccount, collateral_required) else {
            return LiquidationOutcome::Skipped("collateral_reserved_elsewhere");
        };

        let tx_builder = self.liquidation_tx(
            subaccount,
            &keeper_account,
            pyth_update,
            priority_fee,
            cu_limit,
        );
        let tx_builder =
            with_spot_interest_cranks(tx_builder, self.keeper.velocity, &keeper_account, &[])
                .liquidate_perp(market_index, &liquidatee_account, base_asset_amount, None);
        let (tx_builder, cu_limit) = scale_cu_limit_for_accounts(tx_builder, cu_limit, 20, 20);

        let intent = TxIntent::LiquidatePerp {
            market_index,
            liquidatee,
            slot: request.slot,
        };
        match self
            .keeper
            .tx
            .send_reserved_tx(tx_builder.build(), intent, cu_limit as u64, reservation)
            .await
        {
            Some(signature) => {
                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind=perp_takeover liquidatee={liquidatee:?} market={market_index} base_asset_amount={base_asset_amount} collateral_reserved={collateral_required} subaccount={subaccount:?} sig={signature} slot={}",
                    request.slot,
                );
                LiquidationOutcome::TxSent
            }
            // the reservation guard released the collateral
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    /// `liquidate_perp_pnl_for_deposit` or `liquidate_borrow_for_perp_pnl`, reserving the
    /// liquidated amount.
    async fn send_pnl_liquidation(
        &self,
        kind: PnlKind,
        request: &LiquidationRequest,
        plan: PnlLiquidation,
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        let liquidatee = request.pubkey;
        let PnlLiquidation {
            subaccount,
            liability,
            asset,
            amount,
            collateral_required,
            pyth_update,
        } = plan;
        let (keeper_account, liquidatee_account) = match self.accounts(&subaccount, &liquidatee) {
            Ok(accounts) => accounts,
            Err(reason) => {
                log::warn!(target: TARGET, "pnl liquidation skipped: {reason}");
                return LiquidationOutcome::Skipped(reason);
            }
        };
        let Some(reservation) = self.collateral.try_reserve(subaccount, collateral_required) else {
            return LiquidationOutcome::Skipped("collateral_reserved_elsewhere");
        };

        let tx_builder = self.liquidation_tx(
            subaccount,
            &keeper_account,
            pyth_update.as_ref(),
            priority_fee,
            cu_limit,
        );
        let tx_builder =
            with_spot_interest_cranks(tx_builder, self.keeper.velocity, &keeper_account, &[]);
        let (perp_market_index, spot_market_index) = pnl_markets(kind, &liability, &asset);
        let (tx_builder, intent, label) = match kind {
            PnlKind::PerpPnlForDeposit => (
                tx_builder.liquidate_perp_pnl_for_deposit(
                    &liquidatee_account,
                    perp_market_index,
                    spot_market_index,
                    amount,
                    None,
                ),
                TxIntent::LiquidatePerpPnlForDeposit {
                    perp_market_index,
                    spot_market_index,
                    liquidatee,
                    slot: request.slot,
                },
                "perp_pnl_for_deposit",
            ),
            PnlKind::BorrowForPerpPnl => (
                tx_builder.liquidate_borrow_for_perp_pnl(
                    &liquidatee_account,
                    perp_market_index,
                    spot_market_index,
                    amount,
                    None,
                ),
                TxIntent::LiquidateBorrowForPerpPnl {
                    perp_market_index,
                    spot_market_index,
                    liquidatee,
                    slot: request.slot,
                },
                "borrow_for_perp_pnl",
            ),
        };

        match self
            .keeper
            .tx
            .send_reserved_tx(tx_builder.build(), intent, cu_limit as u64, reservation)
            .await
        {
            Some(signature) => {
                log::info!(
                    target: TARGET,
                    "liquidation tx sent: kind={label} liquidatee={liquidatee:?} perp_market={perp_market_index} spot_market={spot_market_index} amount={amount} collateral_reserved={collateral_required} subaccount={subaccount:?} sig={signature} slot={}",
                    request.slot,
                );
                LiquidationOutcome::TxSent
            }
            // the reservation guard released the collateral
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    /// Settle the liquidatee's positive pnl in `markets`.
    async fn send_settle_pnl(
        &self,
        liquidatee: Pubkey,
        markets: &[u16],
        priority_fee: u64,
        cu_limit: u32,
    ) -> LiquidationOutcome {
        let Some(&first_market) = markets.first() else {
            return LiquidationOutcome::Skipped("no_settleable_markets");
        };
        let subaccount = self.subaccounts[0];
        let (keeper_account, liquidatee_account) = match self.accounts(&subaccount, &liquidatee) {
            Ok(accounts) => accounts,
            Err(reason) => {
                log::warn!(target: TARGET, "settle pnl skipped: {reason}");
                return LiquidationOutcome::Skipped(reason);
            }
        };

        let tx_builder = markets.iter().fold(
            self.liquidation_tx(subaccount, &keeper_account, None, priority_fee, cu_limit),
            |tx_builder, &market_index| {
                tx_builder.settle_pnl(market_index, Some(&liquidatee), Some(&liquidatee_account))
            },
        );

        let intent = TxIntent::SettlePnl {
            market_index: first_market,
            subaccount: liquidatee,
        };
        match self
            .keeper
            .tx
            .send_tx(tx_builder.build(), intent, cu_limit as u64)
            .await
        {
            Some(signature) => {
                log::info!(target: TARGET, "liquidation tx sent: kind=settle_pnl liquidatee={liquidatee:?} markets={markets:?} sig={signature}");
                LiquidationOutcome::TxSent
            }
            None => LiquidationOutcome::Skipped("tx_send_failed"),
        }
    }

    /// Swap each of the liquidatee's spot borrows against their largest deposit, through the
    /// better of a Jupiter and a Titan quote.
    async fn send_spot_liquidations(
        &self,
        request: &LiquidationRequest,
        priority_fee: u64,
    ) -> LiquidationOutcome {
        let velocity = self.keeper.velocity;
        let metrics = &self.keeper.metrics;
        let liquidatee = request.pubkey;
        let user = &request.user;
        let authority = velocity.wallet.authority();
        let Some(&subaccount) = self.subaccounts.first() else {
            log::warn!(target: TARGET, "no subaccount configured");
            return LiquidationOutcome::Skipped("no_subaccount");
        };

        let mut any_sent = false;
        for position in user
            .spot_positions
            .iter()
            .filter(|p| matches!(p.balance_type, SpotBalanceType::Borrow) && !p.is_available())
        {
            if BLOCKED_SPOT_MARKETS.contains(&position.market_index) {
                continue;
            }
            let Some(spot_market) = self
                .market_state
                .read()
                .unwrap()
                .load()
                .spot_market(position.market_index)
                .copied()
            else {
                continue;
            };
            let Ok(token_amount) = position.get_token_amount(&spot_market) else {
                continue;
            };
            let token_amount = token_amount as u64;
            // dust
            if token_amount < spot_market.min_order_size * 2 {
                continue;
            }

            metrics
                .liquidation_attempts
                .with_label_values(&["spot"])
                .inc();

            let Some(asset_market_index) = user
                .spot_positions
                .iter()
                .filter(|p| matches!(p.balance_type, SpotBalanceType::Deposit) && !p.is_available())
                .max_by_key(|p| p.scaled_balance)
                .map(|p| p.market_index)
            else {
                log::warn!(target: TARGET, "no asset found for user {liquidatee:?}, skipping spot liquidation");
                continue;
            };
            let liability_market_index = position.market_index;
            log::info!(
                target: TARGET,
                "attempting spot liquidation: user={liquidatee:?}, asset_market={asset_market_index}, liability_market={liability_market_index}, amount={token_amount}",
            );

            let (keeper_account, liquidatee_account) = match self.accounts(&subaccount, &liquidatee)
            {
                Ok(accounts) => accounts,
                Err(reason) => {
                    log::info!(target: TARGET, "spot liquidation account lookup failed: {reason}");
                    continue;
                }
            };
            let asset_spot_market = velocity
                .program_data()
                .spot_market_config_by_index(asset_market_index)
                .expect("asset spot market");
            let liability_spot_market = velocity
                .program_data()
                .spot_market_config_by_index(liability_market_index)
                .expect("liability spot market");
            let in_token_account =
                Wallet::derive_associated_token_address(authority, asset_spot_market);
            let out_token_account =
                Wallet::derive_associated_token_address(authority, liability_spot_market);

            let started = std::time::Instant::now();
            let (jupiter, titan_quote) = tokio::join!(
                velocity.jupiter_swap_query(
                    authority,
                    token_amount,
                    100,
                    asset_market_index,
                    liability_market_index,
                    None,
                    None,
                ),
                velocity.titan_swap_query(
                    authority,
                    token_amount,
                    Some(50),
                    titan::SwapMode::ExactIn,
                    100,
                    asset_market_index,
                    liability_market_index,
                    Some(true),
                    None,
                    None,
                )
            );
            let quote_latency_ms = started.elapsed().as_millis() as i64;
            metrics.swap_quote_latency_ms.set(quote_latency_ms);

            let tx_builder = self.keeper.tx_builder(
                subaccount,
                Cow::Owned(keeper_account),
                priority_fee,
                SPOT_LIQUIDATION_CU_LIMIT,
            );
            let (tx, venue) = match (jupiter, titan_quote) {
                (Err(_), Err(_)) => {
                    metrics.jupiter_quote_failures.inc();
                    metrics.titan_quote_failures.inc();
                    log::warn!(target: TARGET, "both quotes failed after {quote_latency_ms}ms");
                    continue;
                }
                (Ok(jupiter), Ok(titan_quote))
                    if titan_quote.quote.out_amount <= jupiter.quote.out_amount =>
                {
                    (
                        tx_builder.jupiter_swap_liquidate(
                            jupiter,
                            asset_spot_market,
                            liability_spot_market,
                            &in_token_account,
                            &out_token_account,
                            asset_market_index,
                            liability_market_index,
                            &liquidatee_account,
                        ),
                        "jupiter",
                    )
                }
                (Ok(_), Ok(titan_quote)) => (
                    tx_builder.titan_swap_liquidate(
                        titan_quote,
                        asset_spot_market,
                        liability_spot_market,
                        &in_token_account,
                        &out_token_account,
                        asset_market_index,
                        liability_market_index,
                        &liquidatee_account,
                    ),
                    "titan",
                ),
                (Ok(jupiter), Err(err)) => {
                    metrics.titan_quote_failures.inc();
                    log::warn!(target: TARGET, "titan failed in {quote_latency_ms}ms, using jupiter: {err:?}");
                    (
                        tx_builder.jupiter_swap_liquidate(
                            jupiter,
                            asset_spot_market,
                            liability_spot_market,
                            &in_token_account,
                            &out_token_account,
                            asset_market_index,
                            liability_market_index,
                            &liquidatee_account,
                        ),
                        "jupiter",
                    )
                }
                (Err(err), Ok(titan_quote)) => {
                    metrics.jupiter_quote_failures.inc();
                    log::warn!(target: TARGET, "jupiter failed in {quote_latency_ms}ms, using titan: {err:?}");
                    (
                        tx_builder.titan_swap_liquidate(
                            titan_quote,
                            asset_spot_market,
                            liability_spot_market,
                            &in_token_account,
                            &out_token_account,
                            asset_market_index,
                            liability_market_index,
                            &liquidatee_account,
                        ),
                        "titan",
                    )
                }
            };

            let intent = TxIntent::LiquidateSpot {
                asset_market_index,
                liability_market_index,
                liquidatee,
                slot: request.slot,
            };
            match self
                .keeper
                .tx
                .send_tx(tx.build(), intent, SPOT_LIQUIDATION_CU_LIMIT as u64)
                .await
            {
                Some(signature) => {
                    any_sent = true;
                    log::info!(
                        target: TARGET,
                        "liquidation tx sent: kind=spot liquidatee={liquidatee:?} asset_market={asset_market_index} liability_market={liability_market_index} amount={token_amount} venue={venue} sig={signature} slot={}",
                        request.slot,
                    );
                }
                None => {
                    log::warn!(target: TARGET, "spot liquidation tx send failed: liquidatee={liquidatee:?} liability_market={liability_market_index}");
                }
            }
        }

        if any_sent {
            LiquidationOutcome::TxSent
        } else {
            LiquidationOutcome::Skipped("no_spot_liquidation_sent")
        }
    }
}

/// The (perp, spot) market indexes of a pnl liquidation. Pnl for deposit liquidates a perp
/// liability against a spot asset; borrow for pnl liquidates a spot borrow against perp pnl.
fn pnl_markets(
    kind: PnlKind,
    liability: &LiquidatablePosition,
    asset: &LiquidatablePosition,
) -> (u16, u16) {
    match kind {
        PnlKind::PerpPnlForDeposit => (liability.market_index, asset.market_index),
        PnlKind::BorrowForPerpPnl => (asset.market_index, liability.market_index),
    }
}
