//! The taker-risk layer of a perp fill.
//!
//! This layer governs the taker. Before the fill it measures the two facts
//! every limit is scoped by, withholds a fill whose equity floor it cannot
//! verify, and decides whether a builder fee may be charged. After the fill it
//! holds both seats to the post-fill checks. Both seats are the taker and
//! every maker.
//!
//! [`super::liquidity`] draws the liquidity in between.

use {
    super::{super::*, context::*, liquidity::fill_from_liquidity_sources},
    crate::{
        controller,
        error::{ErrorCode, VelocityResult},
        math::{
            constants::MARGIN_PRECISION,
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, MarginRequirementType,
            },
            orders::select_margin_type_for_perp_maker,
            router::RouterLeg,
        },
        state::{
            fill_mode::FillMode,
            margin_calculation::{MarginCalculation, MarginContext, MarginTypeConfig},
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::{msg, Pubkey},
};

/// The taker facts one fill measures its risk limits against, and the
/// scopes the checks run under. A fill changes both facts, so they are
/// measured first: closing makes the order look non-decreasing, and
/// opening makes an absent position look cross-margined.
pub struct TakerRiskLimits {
    pub market_index: u16,
    /// Whether the order reduces the position the taker held before the fill.
    /// A reducing order is held to maintenance margin, not fill margin, and is
    /// exempt from the buffered floor.
    pub order_decreasing: bool,
    /// Whether the taker's position in this market is isolated. It decides the
    /// margin scope every taker check runs under.
    pub is_isolated: bool,
    /// Whether the oracle is too old to price margin.
    pub oracle_stale_for_margin: bool,
    pub mode: FillMode,
    /// See [`RouterLeg::taker_exposure_closed_by_caller`]. It suppresses
    /// the taker's own checks only. Every maker keeps all of theirs.
    pub taker_exposure_closed_by_caller: bool,
    /// The market's open interest before the fill.
    pub perp_market_oi_before: u128,
}

/// Fill the taker's order inside the taker's own risk limits.
///
/// [`fill_from_liquidity_sources`] draws the liquidity in between the checks
/// this function runs before and after the fill.
pub fn fill_within_taker_risk_limits(
    taker: &mut TakerSide,
    rules: &PricingRules,
    conditions: &FillConditions,
    parties: &mut FillParties,
    liquidity: &mut OfferedLiquidity,
    filler: &mut FillerSide,
) -> VelocityResult<FillAmounts> {
    validate_taker_exposure_exemption(taker, liquidity.router)?;

    let limits = TakerRiskLimits::measure(taker, conditions, liquidity.router, parties)?;

    if withhold_unverifiable_floor(taker, &limits, conditions, parties, filler)? == Admission::Skip
    {
        return Ok(FillAmounts::default());
    }

    let rules = rules.allow_builder_fee(builder_fee_allowed(taker, &limits, parties, filler)?);

    let (base_asset_amount, quote_asset_amount, maker_fills) =
        fill_from_liquidity_sources(taker, &rules, conditions, parties, liquidity, filler)?;
    let filled = FillAmounts {
        base: base_asset_amount,
        quote: quote_asset_amount,
    };

    limits.check_after_fill(
        &mut TakerRefs {
            user: taker.user,
            stats: taker.stats,
        },
        parties,
        filled,
        &maker_fills,
        conditions.now,
    )?;

    Ok(filled)
}

/// Refuse a caller that claims the taker-exposure exemption for an account
/// that cannot hold it.
///
/// The exemption follows from the account's identity. The protocol `User` is
/// the only taker whose exposure a caller closes inside the instruction, and
/// `protocol_authority` is `State::signer`, verified by the entrypoint.
fn validate_taker_exposure_exemption(taker: &TakerSide, router: &RouterLeg) -> VelocityResult {
    if !router.standing.taker_exposure_closed_by_caller {
        return Ok(());
    }

    validate!(
        taker
            .user
            .is_protocol_user(&router.standing.protocol_authority),
        ErrorCode::TakerExposureNotProtocolOwned,
        "only the protocol user may settle a fill whose taker checks the caller closes"
    )?;

    Ok(())
}

/// Withhold the whole fill when the taker's equity floor cannot be verified.
///
/// A risk-increasing taker whose floor cannot be verified would execute its
/// fulfillment legs and then revert at the buffered-floor gate.
/// `validate_clears_buffered_floor` fails closed on any invalid oracle in the
/// taker's portfolio, whether or not it is this market's, and the legs have
/// executed by then. The router's maker budget prunes a floored maker with the
/// same defect, but the taker has no counterpart, so its visible order made
/// every fill attempt revert for the length of the outage. Withhold the
/// fill instead and leave the order resting until its oracles recover.
///
/// Runs after the caller's expired and reduce-only cleanup. Reducing orders
/// are exempt at the gate, and a liquidation fill skips the gate too.
fn withhold_unverifiable_floor(
    taker: &TakerSide,
    limits: &TakerRiskLimits,
    conditions: &FillConditions,
    parties: &mut FillParties,
    filler: &mut FillerSide,
) -> VelocityResult<Admission> {
    if taker.user.equity_floor == 0 || limits.mode.is_liquidation() || limits.order_decreasing {
        return Ok(Admission::Proceed);
    }

    let floor_unverifiable = match calculate_net_equity_for_floor(taker.user, parties.maps)? {
        Some(net_equity) => !net_equity.all_oracles_valid,
        None => false,
    };

    if !floor_unverifiable {
        return Ok(Admission::Proceed);
    }

    msg!(
        "taker {} equity floor unverifiable (invalid oracle in portfolio), withholding fill",
        taker.key
    );

    if let Some(filler) = filler.user.as_deref_mut() {
        filler.update_last_active_slot(conditions.slot);
    }

    Ok(Admission::Skip)
}

/// Whether this fill may charge the taker's builder fee.
///
/// A builder fee debits the taker `user_fee + builder_fee`, approved by
/// the taker itself, so it clears the same gate a withdrawal clears:
/// initial margin. A decreasing fill is held only to maintenance margin,
/// so without this gate a taker under initial margin could route
/// `MAX_BUILDER_FEE_TENTH_BPS` per slice to itself across successive
/// reducing fills, each of which loosens the requirement for the next.
///
/// A failed gate waives the fee and keeps the fill, using the same
/// strict oracle rules and pre-fill margin as the withdraw gate, so a
/// stale oracle elsewhere waives the fee rather than reverting it.
fn builder_fee_allowed(
    taker: &TakerSide,
    limits: &TakerRiskLimits,
    parties: &mut FillParties,
    filler: &FillerSide,
) -> VelocityResult<bool> {
    if limits.mode.is_liquidation()
        || !taker.order.is_has_builder()
        || filler.rev_share_escrow.is_none()
    {
        return Ok(false);
    }

    let context = MarginContext::standard_with_config(
        limits.margin_config(MarginRequirementType::Initial, limits.is_isolated),
    )
    .strict(true)
    .ignore_invalid_deposit_oracles(true);
    let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        taker.user,
        parties.maps,
        context,
    )?;

    Ok(calculation.meets_margin_requirement() && calculation.all_liability_oracles_valid)
}

impl TakerRiskLimits {
    /// Measure the taker facts one fill is held to, before it moves anything.
    pub fn measure(
        taker: &TakerSide,
        conditions: &FillConditions,
        router: &RouterLeg,
        parties: &mut FillParties,
    ) -> VelocityResult<Self> {
        let market_index = taker.order.market_index;
        Ok(Self {
            market_index,
            order_decreasing: determine_if_user_order_is_position_decreasing(
                taker.user,
                market_index,
                taker.order,
            )?,

            // A fresh ephemeral taker has no position yet. It opens a
            // cross-margin one, so a missing position is not isolated.
            is_isolated: taker
                .user
                .get_perp_position(market_index)
                .map(|position| position.is_isolated())
                .unwrap_or(false),
            oracle_stale_for_margin: conditions.oracle_stale_for_margin,
            mode: conditions.mode,
            taker_exposure_closed_by_caller: router.standing.taker_exposure_closed_by_caller,
            perp_market_oi_before: parties
                .maps
                .perp_market_map
                .get_ref(&market_index)?
                .get_open_interest(),
        })
    }

    /// The margin scope one seat of this fill is measured under.
    ///
    /// An isolated position is measured inside its own market scope and every
    /// other position keeps maintenance margin, so one seat's fill cannot draw
    /// on collateral another position holds.
    pub fn margin_config(
        &self,
        requirement: MarginRequirementType,
        is_isolated: bool,
    ) -> MarginTypeConfig {
        if is_isolated {
            MarginTypeConfig::IsolatedPositionOverride {
                market_index: self.market_index,
                margin_requirement_type: requirement,
                default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
                cross_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        } else {
            MarginTypeConfig::CrossMarginOverride {
                margin_requirement_type: requirement,
                default_margin_requirement_type: MarginRequirementType::Maintenance,
            }
        }
    }

    /// Hold a settled fill to every post-fill rule: fill-amount coherence, the
    /// taker's margin and floor, each maker's margin and floor, and the
    /// stale-oracle open-interest rule.
    ///
    /// Every fill path runs these. A path that settles a match itself must
    /// call this directly, or the fill lands with no collateral check behind
    /// it.
    pub fn check_after_fill(
        &self,
        taker: &mut TakerRefs,
        parties: &mut FillParties,
        filled: FillAmounts,
        maker_fills: &MakerFills,
        now: i64,
    ) -> VelocityResult {
        check_fill_amounts_coherent(filled, maker_fills)?;
        self.check_taker(taker, parties, now)?;
        for (maker_key, (base, is_isolated)) in maker_fills {
            self.check_maker(
                maker_key,
                MakerFill {
                    base: *base,
                    is_isolated: *is_isolated,
                },
                taker,
                parties,
                now,
            )?;
        }

        // On a liquidation fill the taker seat is the liquidatee, who did not
        // place the fill, so it does not enroll. The maker seat is unaffected.
        if filled.base != 0 && !self.mode.is_liquidation() {
            taker
                .stats
                .try_auto_enroll_accelerated_referral_and_emit(now);
        }

        self.check_open_interest(parties)
    }

    /// Hold the taker to its own margin, borrow and equity-floor rules.
    ///
    /// A liquidation fill skips them, and so does a fill whose caller closes
    /// the taker's whole exposure inside the instruction.
    fn check_taker(
        &self,
        taker: &mut TakerRefs,
        parties: &mut FillParties,
        now: i64,
    ) -> VelocityResult {
        if self.mode.is_liquidation() || self.taker_exposure_closed_by_caller {
            return Ok(());
        }

        self.check_taker_margin(taker.user, parties, now)?;
        self.check_taker_equity_floor(taker, parties)
    }

    /// Hold the taker to fill margin, or to maintenance margin when the order
    /// reduces the position, and to the borrow rules the margin walk cannot
    /// see.
    fn check_taker_margin(
        &self,
        user: &User,
        parties: &mut FillParties,
        now: i64,
    ) -> VelocityResult {
        let requirement = if self.order_decreasing {
            MarginRequirementType::Maintenance
        } else {
            MarginRequirementType::Fill
        };

        // A stale-oracle deposit contributes zero collateral, not its stale
        // value: crediting it would let unpriceable collateral buy an in-band
        // losing trade the counterparty then settles for a real profit.
        // `meets_withdraw_margin_requirement` and its siblings drop it the same way.
        let mut context =
            MarginContext::standard_with_config(self.margin_config(requirement, self.is_isolated))
                .ignore_invalid_deposit_oracles(true);
        if self.oracle_stale_for_margin && !self.order_decreasing {
            context = context.margin_ratio_override(MARGIN_PRECISION);
        }

        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            parties.maps,
            context,
        )?;

        reject_margin_breach(&calculation, self.market_index, None)?;
        validate_borrow_rules(user, &calculation, parties, now)
    }

    /// Hold a risk-increasing taker to its equity breaker and its buffered
    /// floor, and arm the breaker on a reducing fill.
    fn check_taker_equity_floor(
        &self,
        taker: &mut TakerRefs,
        parties: &mut FillParties,
    ) -> VelocityResult {
        if self.order_decreasing {
            // A reducing fill is exempt from the buffered-floor gate and may
            // leave the subaccount below its raw floor. Arm the breaker here
            // instead of waiting for the permissionless trip.
            return controller::equity_floor::try_lazy_equity_breaker_trip(
                taker.user,
                taker.stats,
                parties.maps,
            );
        }

        validate!(
            !taker.stats.is_equity_breaker_tripped(),
            ErrorCode::EquityBelowFloor,
            "taker equity breaker is tripped"
        )?;

        // A risk-increasing fill must prove the taker clears its buffered
        // floor. An invalid oracle then cannot price the taker up through the
        // floor and buy the fill.
        if let Some(net_equity) = calculate_net_equity_for_floor(taker.user, parties.maps)? {
            net_equity.validate_clears_buffered_floor(taker.user)?;
        }

        Ok(())
    }

    /// Hold one maker to the same margin, borrow and equity-floor rules as the
    /// taker, under its own margin scope and its own risk direction.
    fn check_maker(
        &self,
        maker_key: &Pubkey,
        fill: MakerFill,
        taker: &mut TakerRefs,
        parties: &mut FillParties,
        now: i64,
    ) -> VelocityResult {
        let maker = parties.makers_and_referrer.get_ref_mut(maker_key)?;
        let (requirement, risk_increasing) =
            select_margin_type_for_perp_maker(&maker, fill.base, self.market_index)?;
        let context = self.maker_margin_context(requirement, fill.is_isolated, risk_increasing)?;
        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &maker,
            parties.maps,
            context,
        )?;

        reject_margin_breach(&calculation, self.market_index, Some(maker_key))?;

        // Borrow rules are skipped during liquidation, like the taker side.
        // This function runs for liquidation fills too, so an unqualified
        // reject would let one maker's stale oracle or un-cranked borrow
        // market block the liquidation of another account.
        if !self.mode.is_liquidation() {
            validate_borrow_rules(&maker, &calculation, parties, now)?;
        }

        self.check_maker_equity_floor(&maker, maker_key, risk_increasing, taker, parties)?;

        if maker.authority != taker.user.authority {
            let mut maker_stats = parties
                .makers_and_referrer_stats
                .get_ref_mut(&maker.authority)?;
            maker_stats.try_auto_enroll_accelerated_referral_and_emit(now);
        }

        Ok(())
    }

    /// The margin scope one maker is measured under.
    ///
    /// A stale oracle requires one of the two seats to be reducing, and prices
    /// a risk-increasing maker at full margin. A stale spot deposit gets the
    /// same treatment as on the taker seat. The two-account transfer this
    /// closes needs both seats, so collateral the program cannot price is
    /// worth as much on either one.
    fn maker_margin_context(
        &self,
        requirement: MarginRequirementType,
        maker_is_isolated: bool,
        maker_risk_increasing: bool,
    ) -> VelocityResult<MarginContext> {
        let mut context =
            MarginContext::standard_with_config(self.margin_config(requirement, maker_is_isolated))
                .ignore_invalid_deposit_oracles(true);
        if self.oracle_stale_for_margin {
            validate!(
                self.order_decreasing || !maker_risk_increasing,
                ErrorCode::InvalidOracle,
                "taker or maker must be reducing position if oracle stale for margin"
            )?;

            if maker_risk_increasing {
                context = context.margin_ratio_override(MARGIN_PRECISION);
            }
        }

        Ok(context)
    }

    /// Hold a risk-increasing maker to its equity breaker and its buffered
    /// floor, and arm the breaker on a reducing fill.
    ///
    /// The invalid-oracle arm of the floor gate is normally unreachable:
    /// oracle validity cannot change across the fill, and the router's maker
    /// budget sizes a maker to zero while any of its oracles is invalid. What
    /// reverts here is a genuine value breach, or a fill that flipped a
    /// reducing order into new risk.
    fn check_maker_equity_floor(
        &self,
        maker: &User,
        maker_key: &Pubkey,
        maker_risk_increasing: bool,
        taker: &mut TakerRefs,
        parties: &mut FillParties,
    ) -> VelocityResult {
        if !maker_risk_increasing {
            if maker.equity_floor == 0 {
                return Ok(());
            }

            // A reducing maker fill is exempt from the buffered-floor gate and
            // may leave the subaccount below its raw floor. Arm the breaker
            // here instead of waiting for the permissionless trip.
            if maker.authority == taker.user.authority {
                return controller::equity_floor::try_lazy_equity_breaker_trip(
                    maker,
                    taker.stats,
                    parties.maps,
                );
            }

            let mut maker_stats = parties
                .makers_and_referrer_stats
                .get_ref_mut(&maker.authority)?;
            return controller::equity_floor::try_lazy_equity_breaker_trip(
                maker,
                &mut maker_stats,
                parties.maps,
            );
        }

        let breaker_tripped = if maker.authority == taker.user.authority {
            taker.stats.is_equity_breaker_tripped()
        } else {
            parties
                .makers_and_referrer_stats
                .get_ref(&maker.authority)?
                .is_equity_breaker_tripped()
        };

        validate!(
            !breaker_tripped,
            ErrorCode::EquityBelowFloor,
            "maker ({}) equity breaker is tripped",
            maker_key
        )?;

        if let Some(net_equity) = calculate_net_equity_for_floor(maker, parties.maps)? {
            net_equity.validate_clears_buffered_floor(maker)?;
        }

        Ok(())
    }

    /// A stale oracle may not price new exposure into the market.
    fn check_open_interest(&self, parties: &mut FillParties) -> VelocityResult {
        if !self.oracle_stale_for_margin {
            return Ok(());
        }

        let perp_market_oi_after = parties
            .maps
            .perp_market_map
            .get_ref(&self.market_index)?
            .get_open_interest();
        validate!(
            perp_market_oi_after <= self.perp_market_oi_before,
            ErrorCode::InvalidOracle,
            "oracle stale for margin but open interest increased"
        )?;

        Ok(())
    }
}

/// The two sides of the fill must report the same trade, and the makers may
/// not report more base than the fill moved.
fn check_fill_amounts_coherent(filled: FillAmounts, maker_fills: &MakerFills) -> VelocityResult {
    validate!(
        (filled.base > 0) == (filled.quote > 0),
        ErrorCode::ImpossibleFill,
        "invalid fill base = {} quote = {}",
        filled.base,
        filled.quote
    )?;

    let total_maker_fill = maker_fills.values().map(|(base, _)| base).sum::<i64>();
    validate!(
        total_maker_fill.unsigned_abs() <= filled.base,
        ErrorCode::ImpossibleFill,
        "invalid total maker fill {} total fill {}",
        total_maker_fill,
        filled.base
    )?;

    Ok(())
}

/// Refuse a fill the account cannot collateralize, and name the numbers of the
/// scope it failed under.
fn reject_margin_breach(
    calculation: &MarginCalculation,
    market_index: u16,
    maker_key: Option<&Pubkey>,
) -> VelocityResult {
    if calculation.meets_margin_requirement() {
        return Ok(());
    }

    let (margin_requirement, total_collateral) =
        if calculation.has_isolated_margin_calculation(market_index) {
            let isolated = calculation.get_isolated_margin_calculation(market_index)?;
            (isolated.margin_requirement, isolated.total_collateral)
        } else {
            (calculation.margin_requirement, calculation.total_collateral)
        };
    match maker_key {
        Some(maker_key) => msg!(
            "maker ({}) breached fill requirements (margin requirement {}) (total_collateral {})",
            maker_key,
            margin_requirement,
            total_collateral
        ),
        None => msg!(
            "taker breached fill requirements (margin requirement {}) (total_collateral {})",
            margin_requirement,
            total_collateral
        ),
    }

    Err(ErrorCode::InsufficientCollateral)
}

/// A borrow the margin walk could not value must not admit the fill.
///
/// A stale oracle or stale index prices a borrow low, so an insolvent
/// account would pass and become protocol bad debt. Unlike a stale
/// deposit, a borrow has no drop-and-still-fill option, so this reverts
/// for either fill direction, matching `meets_withdraw_margin_requirement`.
/// It reads the spot-only liability flag because an invalid perp oracle is
/// handled by the stale-oracle margin override instead of a hard reject.
fn validate_borrow_rules(
    user: &User,
    calculation: &MarginCalculation,
    parties: &FillParties,
    now: i64,
) -> VelocityResult {
    validate!(
        calculation.all_spot_liability_oracles_valid,
        ErrorCode::InvalidOracle,
        "account filling while a spot borrow oracle is invalid for margin"
    )?;

    crate::math::margin::validate_spot_borrow_interest_fresh_for_margin(
        user,
        &parties.maps.spot_market_map,
        now,
    )
}
