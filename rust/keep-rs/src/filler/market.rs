//! One perp market as the program will see it when a fill lands
//!
//! A fill tx sent in slot `s` lands about one slot later. `MarketView` keeps the market as
//! loaded, which the program's AMM gates read before any quote projection, plus two oracle
//! views for the two kinds of tx the filler sends: auction fills post the pyth-lazer price, while
//! AMM-taker, uncross and swift fills post nothing. Each `OracleView` starts from one exchange
//! observation, which gives both the safe price and validity and the AMM quote projection, so the
//! two cannot pick different safe prices. `AmmGate` and `amm_fill_size` then ask the program's
//! own functions whether, and how much, the AMM fills an order.

use {
    crate::{
        common::oracle::{
            classify_perp_oracle, observe_exchange_oracle, ExchangeObservation, ExchangeState,
            ProjectedPerpOracle, PythPriceUpdate,
        },
        filler::SlotTick,
    },
    velocity_rs::{
        math::amm_quote::project_perp_market_for_quoting,
        program::{
            math::{
                oracle::{is_oracle_valid_for_action, VelocityAction},
                orders::calculate_base_asset_amount_for_amm_to_fulfill,
            },
            state::fill_mode::FillMode,
        },
        types::{accounts::PerpMarket, FeeTier, Order, SdkError, SdkResult},
        VelocityClient,
    },
};

/// What one kind of tx reads at the landing slot.
#[derive(Clone, Copy)]
pub(super) struct OracleView {
    /// The exchange observation, the safe price the program selects from it, and validities.
    pub oracle: ProjectedPerpOracle,
    /// The market with its oracle-derived stats refreshed and its AMM projected onto the same
    /// observation, as `fill_perp_order` and `AmmQuoter::setup` prepare it before quoting. Use
    /// it for crossing and sizing, never for the AMM gates: the projection moves fee and revenue
    /// counters that the gates read before it.
    pub quote_market: PerpMarket,
    /// The trigger price the trigger instruction computes from this view's exchange oracle.
    /// Triggers read the exchange oracle, not the safe price.
    pub trigger_price: u64,
}

impl OracleView {
    fn new(tick: &SlotTick, market: &PerpMarket, observation: ExchangeObservation) -> Option<Self> {
        let exchange = &tick.exchange;
        let oracle = classify_perp_oracle(exchange, market, observation, tick.landing_slot)?;
        let quote_market = project_perp_market_for_quoting(
            *market,
            observation.data,
            exchange.validity_guard_rails(),
            tick.landing_slot,
            exchange.slot_clock,
            tick.unix_now,
        )
        .unwrap_or(*market);
        let exchange_price = observation.data.price;
        let trigger_price = market
            .get_trigger_price(exchange_price, tick.unix_now, exchange.median_trigger_price)
            .unwrap_or(exchange_price.max(0) as u64);
        Some(Self {
            oracle,
            quote_market,
            trigger_price,
        })
    }

    /// The safe oracle price, which the program prices oracle-relative orders from.
    pub fn price(&self) -> u64 {
        self.oracle.safe.price.max(0) as u64
    }
}

pub(super) struct MarketView {
    pub market_index: u16,
    /// The market as loaded. The program's AMM gates read this, before quote projection.
    pub market: PerpMarket,
    /// What a tx that posts `pyth_update` reads. The chain view when there is no newer update.
    pub posted: OracleView,
    /// What a tx that posts nothing reads.
    pub chain: OracleView,
    /// The fresh pyth-lazer update an auction fill can post.
    pub pyth_update: Option<PythPriceUpdate>,
    /// The posted view is the chain view: no update, or one the program would not apply.
    pub posted_is_chain: bool,
    /// The chain view's safe oracle is older than the AMM staleness window at landing.
    pub oracle_stale_for_amm: bool,
}

impl MarketView {
    /// Fails when the market or its oracle is missing from the client cache.
    pub fn load(
        velocity: &VelocityClient,
        tick: &SlotTick,
        market_index: u16,
        pyth_update: Option<PythPriceUpdate>,
    ) -> SdkResult<Self> {
        let market = velocity.try_get_perp_market_account(market_index)?;
        let exchange = &tick.exchange;
        let landing_slot = tick.landing_slot;

        let chain_observation = observe_exchange_oracle(velocity, &market, landing_slot, None)
            .ok_or(SdkError::InvalidOracle)?;
        let chain =
            OracleView::new(tick, &market, chain_observation).ok_or(SdkError::InvalidOracle)?;
        let posted = pyth_update
            .as_ref()
            .and_then(|update| {
                observe_exchange_oracle(velocity, &market, landing_slot, Some(update))
            })
            .filter(|observation| observation.uses_pyth_update)
            .and_then(|observation| OracleView::new(tick, &market, observation));
        let posted_is_chain = posted.is_none();
        let posted = posted.unwrap_or(chain);

        let oracle_stale_for_amm = exchange
            .slot_clock
            .elapsed_slot_delta(chain.oracle.safe.delay.max(0) as u64, landing_slot)
            > exchange.stale_for_amm();
        Ok(Self {
            market_index,
            market,
            posted,
            chain,
            pyth_update: pyth_update.filter(|_| !posted_is_chain),
            posted_is_chain,
            oracle_stale_for_amm,
        })
    }
}

/// Whether the program lets the AMM fill one order, with the inputs behind the verdict for the
/// decision events.
///
/// The verdict is the program's: `open` is `fill_perp_order`'s `amm_is_available`, the global
/// AMM pause and `PerpMarket::amm_can_fill_order`, evaluated on the landing view. The other
/// fields only explain it. A low-risk order fills past the hard gates. Any other order fills only
/// through the immediate JIT leg, which the program closes when `amm_jit_intensity` is 0 (through
/// `amm_wants_to_jit_make`), when the oracle is too old for immediate fills, or when the user or
/// the market cannot skip the auction. The program checks again onchain, so a wrong `true` costs
/// one failed simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AmmGate {
    pub open: bool,
    pub drawdown: bool,
    pub safe_stale_for_amm: bool,
    pub safe_stale_immediate: bool,
    pub safe_oracle_delay: Option<i64>,
    pub order_low_risk: bool,
    pub can_skip_auction: bool,
    pub wants_jit: bool,
}

impl AmmGate {
    pub const CLOSED: Self = Self {
        open: false,
        drawdown: false,
        safe_stale_for_amm: true,
        safe_stale_immediate: true,
        safe_oracle_delay: None,
        order_low_risk: false,
        can_skip_auction: false,
        wants_jit: false,
    };

    /// `oracle` is the projection that matches the tx: posted for auction fills, chain for the
    /// rest. A missing oracle or order closes the gate, since the program cannot be predicted.
    pub fn evaluate(
        market: &PerpMarket,
        exchange: &ExchangeState,
        oracle: Option<&ProjectedPerpOracle>,
        order: Option<&Order>,
        user_can_skip_auction: bool,
        landing_slot: u64,
    ) -> Self {
        let mut gate = Self {
            drawdown: market.has_too_much_drawdown().unwrap_or(false),
            ..Self::CLOSED
        };

        let Some(oracle) = oracle else {
            return gate;
        };
        let valid_for = |action| {
            is_oracle_valid_for_action(oracle.safe_validity, Some(action)).unwrap_or(false)
        };
        gate.safe_oracle_delay = Some(oracle.safe.delay);
        gate.safe_stale_for_amm = !valid_for(VelocityAction::FillOrderAmmLowRisk);
        gate.safe_stale_immediate = !valid_for(VelocityAction::FillOrderAmmImmediate);

        let Some(order) = order else {
            return gate;
        };
        gate.can_skip_auction = user_can_skip_auction;
        gate.wants_jit = market
            .amm
            .amm_wants_to_jit_make(order.direction, market.order_step_size)
            .unwrap_or(false);
        gate.order_low_risk = order
            .is_low_risk_for_amm(
                oracle.safe.delay,
                landing_slot,
                false,
                user_can_skip_auction,
            )
            .unwrap_or(false);

        let amm_paused = exchange.program.amm_paused().unwrap_or(true);
        gate.open = !amm_paused
            && market
                .amm_can_fill_order(
                    order,
                    landing_slot,
                    FillMode::Fill,
                    &exchange.program,
                    oracle.safe_validity,
                    user_can_skip_auction,
                    &oracle.mm,
                )
                .unwrap_or(false);
        gate
    }
}

/// The base the program's AMM sizing fills for `order`, already rounded down to the step size,
/// so 0 means the AMM fills nothing. `None` when the sizing errors.
///
/// The program applies no other size floor to an AMM fill: `min_order_size` only bounds order
/// placement. `limit_price` caps the fill the way the program's limit price does; `None` sizes
/// up to the AMM's available liquidity.
pub(super) fn amm_fill_size(
    market: &PerpMarket,
    order: &Order,
    existing_base_asset_amount: i64,
    limit_price: Option<u64>,
) -> Option<u64> {
    calculate_base_asset_amount_for_amm_to_fulfill(
        order,
        market,
        limit_price,
        None,
        existing_base_asset_amount,
        &FeeTier::default(),
    )
    .ok()
    .map(|(base_asset_amount, _limit_price)| base_asset_amount)
}

#[cfg(test)]
mod tests {
    use {
        super::AmmGate,
        crate::common::oracle::{perp_oracle_validity, ExchangeState, ProjectedPerpOracle},
        velocity_rs::{
            program::{
                math::time::legacy_slot_duration_i64,
                state::state::{State as ProgramState, ValidityGuardRails},
            },
            types::{accounts::PerpMarket, OraclePriceData, OracleSource, Order},
        },
    };

    const SLOT: u64 = 449_781_953;
    const PRICE: i64 = 1_515_532_437;
    /// `ExchangeStatus::AmmPaused`
    const AMM_PAUSED: u8 = 0b0000_0100;

    fn exchange(exchange_status: u8) -> ExchangeState {
        let mut program = ProgramState {
            exchange_status,
            ..ProgramState::default()
        };
        program.oracle_guard_rails.validity = ValidityGuardRails {
            slots_before_stale_for_amm: legacy_slot_duration_i64(10),
            slots_before_stale_for_margin: legacy_slot_duration_i64(120),
            confidence_interval_max_size: 20_000,
            too_volatile_ratio: 5,
        };
        ExchangeState {
            slot_clock: program.slot_clock(),
            program,
            median_trigger_price: false,
            liquidation_margin_buffer_ratio: 0,
        }
    }

    /// ZEC-PERP as it was observed: no MM crank, so the safe price is the exchange oracle.
    fn market(amm_jit_intensity: u8) -> PerpMarket {
        let mut market = PerpMarket {
            market_index: 4,
            oracle_source: OracleSource::PythLazer,
            oracle_slot_delay_override: -1,
            ..Default::default()
        };
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap = PRICE;
        market.market_stats.mm_oracle_price = 1_506_838_551;
        market.market_stats.mm_oracle_slot = SLOT - 458_505;
        market.market_stats.mm_oracle_sequence_id = 1_790_065_354_600_000;
        market.amm.amm_jit_intensity = amm_jit_intensity;
        market
    }

    /// The oracle a fill that posts a same-slot pyth-lazer price sees.
    fn posted_oracle(market: &PerpMarket, exchange: &ExchangeState) -> ProjectedPerpOracle {
        let posted = OraclePriceData {
            price: PRICE,
            confidence: (PRICE / 500) as u64,
            delay: 0,
            has_sufficient_number_of_data_points: true,
            sequence_id: Some(1_790_187_409_000_000),
        };
        perp_oracle_validity(
            market,
            posted,
            true,
            SLOT,
            exchange.validity_guard_rails(),
            exchange.slot_clock,
        )
        .expect("classifies")
    }

    fn order(slot: u64) -> Order {
        Order {
            slot,
            market_index: 4,
            ..Order::default()
        }
    }

    fn gate(
        market: &PerpMarket,
        exchange: &ExchangeState,
        order: &Order,
        can_skip_auction: bool,
    ) -> AmmGate {
        let oracle = posted_oracle(market, exchange);
        AmmGate::evaluate(
            market,
            exchange,
            Some(&oracle),
            Some(order),
            can_skip_auction,
            SLOT,
        )
    }

    #[test]
    fn low_risk_order_fills_with_jit_off() {
        // amm_jit_intensity 0 closes only the JIT leg: an order older than the oracle still
        // fills against the AMM
        let gate = gate(&market(0), &exchange(0), &order(SLOT - 100), true);
        assert!(gate.order_low_risk);
        assert!(!gate.wants_jit);
        assert!(gate.open, "{gate:?}");
    }

    #[test]
    fn order_from_the_landing_slot_needs_the_jit_leg() {
        // without an auction skip the order is not older than the oracle, and the JIT leg
        // needs the skip too, so the program keeps the AMM out
        let gate = gate(&market(0), &exchange(0), &order(SLOT), false);
        assert!(!gate.order_low_risk);
        assert!(!gate.open);
    }

    #[test]
    fn global_amm_pause_closes_every_order() {
        let gate = gate(&market(0), &exchange(AMM_PAUSED), &order(SLOT - 100), true);
        assert!(gate.order_low_risk);
        assert!(!gate.open);
    }

    #[test]
    fn missing_order_or_oracle_closes_the_gate() {
        let market = market(0);
        let exchange = exchange(0);
        let oracle = posted_oracle(&market, &exchange);

        let no_order = AmmGate::evaluate(&market, &exchange, Some(&oracle), None, true, SLOT);
        assert!(!no_order.open);
        assert_eq!(no_order.safe_oracle_delay, Some(0));

        let no_oracle = AmmGate::evaluate(
            &market,
            &exchange,
            None,
            Some(&order(SLOT - 100)),
            true,
            SLOT,
        );
        assert_eq!(
            no_oracle,
            AmmGate {
                drawdown: no_oracle.drawdown,
                ..AmmGate::CLOSED
            }
        );
    }
}

#[cfg(test)]
mod projection_tests {
    use {
        crate::common::oracle::ExchangeState,
        velocity_rs::{
            math::amm_quote::project_perp_market_for_quoting,
            program::{
                math::time::legacy_slot_duration_i64,
                state::{
                    market_status::MarketStatus,
                    oracle::HistoricalOracleData,
                    perp_market::MarketStats,
                    state::{State as ProgramState, ValidityGuardRails},
                },
                vlp::amm::state::AMM,
            },
            types::{accounts::PerpMarket, OraclePriceData},
        },
    };

    /// velocity-rs's `btc_market_fixture` (cfg(test) there): peg $19,400, AMM short 1 BTC.
    pub(super) fn btc_market() -> PerpMarket {
        const AMM_RESERVE_PRECISION: u128 = 1_000_000_000;
        const PRICE_PRECISION_I64: i64 = 1_000_000;
        let amm = AMM {
            base_asset_reserve: 65 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 63_015_384_615,
            terminal_quote_asset_reserve: 64 * AMM_RESERVE_PRECISION,
            sqrt_k: 64 * AMM_RESERVE_PRECISION,
            peg_multiplier: 19_400_000_000,
            concentration_coef: 1_414_200,
            max_base_asset_reserve: 90 * AMM_RESERVE_PRECISION,
            min_base_asset_reserve: 45 * AMM_RESERVE_PRECISION,
            base_asset_amount_with_amm: -(AMM_RESERVE_PRECISION as i128),
            curve_update_intensity: 100,
            base_spread: 250,
            max_spread: 975,
            max_fill_reserve_fraction: 1,
            amm_jit_intensity: 100,
            ..AMM::default()
        };
        PerpMarket {
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: 19_400 * PRICE_PRECISION_I64,
                    last_oracle_price_twap: 19_400 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 19_400 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_ts: 1_662_800_000_i64,
                    ..HistoricalOracleData::default()
                },
                last_mark_price_twap_ts: 1_662_800_000,
                mark_std: 1_000_000,
                last_oracle_valid: true,
                funding_period: 3600,
                ..MarketStats::default()
            },
            amm,
            order_step_size: 1,
            order_tick_size: 1,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Initialized,
            ..PerpMarket::default()
        }
    }

    pub(super) fn exchange() -> ExchangeState {
        let mut program = ProgramState::default();
        program.oracle_guard_rails.validity = ValidityGuardRails {
            slots_before_stale_for_amm: legacy_slot_duration_i64(10),
            slots_before_stale_for_margin: legacy_slot_duration_i64(120),
            confidence_interval_max_size: 20_000,
            too_volatile_ratio: 5,
        };
        ExchangeState {
            slot_clock: program.slot_clock(),
            program,
            median_trigger_price: false,
            liquidation_margin_buffer_ratio: 0,
        }
    }

    pub(super) fn observation(price: i64) -> OraclePriceData {
        OraclePriceData {
            price,
            confidence: (price / 1_000) as u64,
            delay: 0,
            has_sufficient_number_of_data_points: true,
            sequence_id: Some(1),
        }
    }

    /// The trigger instruction reads the exchange oracle, so a view's trigger price follows its
    /// exchange observation even when the safe price is the MM oracle.
    #[test]
    fn trigger_price_follows_the_exchange_oracle_not_the_safe_price() {
        use {
            super::OracleView,
            crate::{common::oracle::ExchangeObservation, filler::SlotTick},
        };

        const LANDING_SLOT: u64 = 1_000;
        let exchange_price = 19_400_000_000_i64;
        let mm_price = exchange_price + exchange_price / 400; // 0.25% above, inside the 1% band
        let mut market = btc_market();
        market.market_stats.mm_oracle_price = mm_price;
        market.market_stats.mm_oracle_slot = LANDING_SLOT - 1;
        market.market_stats.mm_oracle_sequence_id = 2;

        let tick = SlotTick {
            slot: LANDING_SLOT - 1,
            landing_slot: LANDING_SLOT,
            priority_fee: 0,
            unix_now: 1_662_800_000,
            exchange: exchange(),
        };
        let observation = ExchangeObservation {
            data: OraclePriceData {
                delay: 3,
                ..observation(exchange_price)
            },
            uses_pyth_update: false,
        };
        let view = OracleView::new(&tick, &market, observation).unwrap();

        assert_eq!(view.oracle.safe.price, mm_price);
        assert_eq!(view.trigger_price, exchange_price as u64);
    }

    /// A projection that crosses a gate threshold: the repeg onto a lower oracle debits the
    /// AMM's `net_revenue_since_last_funding` past the program's -$25 floor for skipping the
    /// auction. The program gates on the loaded market, so the JIT leg is open there and would
    /// be wrongly closed on the projected one.
    #[test]
    fn gate_reads_the_loaded_market_not_the_quote_projection() {
        use {
            super::AmmGate,
            crate::common::oracle::{classify_perp_oracle, ExchangeObservation},
            velocity_rs::{
                math::constants::QUOTE_PRECISION_I64,
                types::{Order, PositionDirection},
            },
        };

        const LANDING_SLOT: u64 = 1_000;
        let price = 18_400_000_000_i64;
        let exchange = exchange();
        let mut market = btc_market();
        // a fresh MM oracle one slot old, newer than the exchange oracle, is the safe price
        market.market_stats.mm_oracle_price = price;
        market.market_stats.mm_oracle_slot = LANDING_SLOT - 1;
        market.market_stats.mm_oracle_sequence_id = 2;
        let observation = ExchangeObservation {
            data: OraclePriceData {
                delay: 3,
                ..observation(price)
            },
            uses_pyth_update: false,
        };
        let projected = project_perp_market_for_quoting(
            market,
            observation.data,
            exchange.validity_guard_rails(),
            LANDING_SLOT,
            exchange.slot_clock,
            1_662_800_000,
        )
        .unwrap();
        assert!(projected.amm.net_revenue_since_last_funding < -25 * QUOTE_PRECISION_I64);

        // an order from the landing slot is not low risk, so it needs the JIT leg
        let order = Order {
            slot: LANDING_SLOT,
            direction: PositionDirection::Long,
            ..Order::default()
        };
        let gate = |market: &PerpMarket| {
            let oracle =
                classify_perp_oracle(&exchange, market, observation, LANDING_SLOT).unwrap();
            assert!(oracle.mm.is_safe_price_mm_sourced());
            AmmGate::evaluate(
                market,
                &exchange,
                Some(&oracle),
                Some(&order),
                true,
                LANDING_SLOT,
            )
        };

        let loaded = gate(&market);
        assert!(!loaded.order_low_risk);
        assert!(loaded.open, "{loaded:?}");
        assert!(!gate(&projected).open);
    }
}
