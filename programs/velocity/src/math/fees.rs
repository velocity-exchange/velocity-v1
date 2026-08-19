use {
    crate::{
        controller::position::PositionDirection,
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::{
                FEE_ADJUSTMENT_MAX, FEE_DENOMINATOR, FEE_PERCENTAGE_DENOMINATOR,
                FIVE_MILLION_QUOTE, PERP_FEE_TIER_MAX_INDEX, TEN_BPS, TEN_MILLION_QUOTE,
            },
            helpers::get_proportion_u128,
            safe_math::SafeMath,
        },
        msg,
        state::{
            state::{FeeStructure, FeeTier, OrderFillerRewardStructure},
            user::{MarketType, UserStats},
        },
    },
    num_integer::Roots,
    std::cmp::{max, min},
};

#[cfg(test)]
mod tests;

/// Split a trade-fee *remainder* (taker fee after maker rebate, referral,
/// referee discount, and filler reward are taken off the top) three ways using
/// the global `FeeStructure` numerators:
///   - `amm_fee` (amm_fee_numerator %): the AMM's fee provision — spendable
///     liquidity, booked into its ledger at fill, and the
///     backstop-of-last-resort clawback tranche, tracked in
///     `PerpMarket.fee_ledger.amm_protocol_fees_received`
///   - `if_fee` (if_fee_numerator %): the insurance fund's cut
///   - `protocol_fee` (the residual): the protocol's withdrawable cut
/// Floor division on the explicit cuts means rounding dust accrues to the
/// protocol residual; `amm + if <= FEE_PERCENTAGE_DENOMINATOR` is validated at
/// fee-structure update so the residual can never underflow.
pub fn split_fee_remainder(
    remainder: u64,
    fee_structure: &FeeStructure,
) -> VelocityResult<(u64, u64, u64)> {
    let denom = FEE_PERCENTAGE_DENOMINATOR as u64;
    let amm_fee = remainder
        .safe_mul(fee_structure.amm_fee_numerator as u64)?
        .safe_div(denom)?;
    let if_fee = remainder
        .safe_mul(fee_structure.if_fee_numerator as u64)?
        .safe_div(denom)?;
    let protocol_fee = remainder.safe_sub(amm_fee)?.safe_sub(if_fee)?;
    Ok((amm_fee, if_fee, protocol_fee))
}

pub struct FillFees {
    pub user_fee: u64,
    pub maker_rebate: u64,
    /// What the AMM books for this fill: its `amm_fee` provision plus any
    /// `quote_asset_amount_surplus` (spread capture). The protocol / IF
    /// carveouts are NOT included — the AMM's ledger only ever contains the
    /// AMM's own money.
    pub fee_to_market: i64,
    pub filler_reward: u64,
    pub referrer_reward: u64,
    pub referee_discount: u64,
    pub builder_fee: Option<u64>,
    /// Protocol's (residual) cut of the trade-fee remainder -> `protocol_fee_pool`.
    pub protocol_fee: u64,
    /// Insurance fund's cut of the trade-fee remainder -> `revenue_pool`.
    pub if_fee: u64,
    /// AMM's fee provision: its cut of the trade-fee remainder, plus (when
    /// the vamm-maker-rebate feature is enabled) the maker rebate the AMM
    /// earns for making the fill. Booked into the AMM's ledger at fill,
    /// tokenized into `amm.fee_pool` by the sweep, and clawable in bankruptcy
    /// (tracked via `amm_protocol_fees_received` / `pending_amm_provision`).
    pub amm_fee: u64,
}

#[allow(clippy::too_many_arguments)]
pub fn calculate_fee_for_fulfillment_with_amm(
    user_stats: &UserStats,
    quote_asset_amount: u64,
    fee_structure: &FeeStructure,
    order_slot: u64,
    clock_slot: u64,
    reward_filler: bool,
    reward_referrer: bool,
    quote_asset_amount_surplus: i64,
    is_post_only: bool,
    fee_adjustment: i16,
    builder_fee_bps: Option<u16>,
    vamm_maker_rebate: bool,
    taker_fee_addon_tenth_bps: u16,
    now: i64,
    promo_fee_tier: u8,
    filler_reward_paid: u64,
) -> VelocityResult<FillFees> {
    let fee_tier = determine_user_fee_tier(
        user_stats,
        fee_structure,
        &MarketType::Perp,
        now,
        promo_fee_tier,
    )?;

    // if there was a quote_asset_amount_surplus, the order was a maker order and fee_to_market comes from surplus
    if is_post_only {
        let maker_rebate = calculate_maker_rebate(quote_asset_amount, &fee_tier, fee_adjustment)?;

        let fee = quote_asset_amount_surplus
            .cast::<u64>()?
            .safe_sub(maker_rebate)
            .inspect_err(|_e| {
                msg!(
                    "quote_asset_amount_surplus {} quote_asset_amount {} maker_rebate {}",
                    quote_asset_amount_surplus,
                    quote_asset_amount,
                    maker_rebate
                );
            })?;

        let filler_reward = if !reward_filler {
            0_u64
        } else {
            calculate_filler_reward(
                fee,
                order_slot,
                clock_slot,
                0,
                &fee_structure.filler_reward_structure,
                filler_reward_paid,
            )?
        };
        // (spread-derived) house fee net of the filler reward, split three
        // ways like a taker-fee remainder. The AMM books ONLY its own cut;
        // the protocol / IF carveouts accrue as pending counters and are
        // materialized out of the pnl pool by `sweep_market_fees`.
        let remainder = fee.safe_sub(filler_reward)?;
        let (amm_fee, if_fee, protocol_fee) = split_fee_remainder(remainder, fee_structure)?;
        let fee_to_market = amm_fee.cast::<i64>()?;
        let user_fee = 0_u64;

        Ok(FillFees {
            user_fee,
            maker_rebate,
            fee_to_market,
            filler_reward,
            referrer_reward: 0,
            referee_discount: 0,
            builder_fee: None,
            protocol_fee,
            if_fee,
            amm_fee,
        })
    } else {
        let fee = calculate_taker_fee(
            quote_asset_amount,
            &fee_tier,
            fee_adjustment,
            taker_fee_addon_tenth_bps,
        )?;

        let (fee, referee_discount, referrer_reward) = if reward_referrer {
            calculate_referee_fee_and_referrer_reward(fee, &fee_tier)?
        } else {
            (fee, 0, 0)
        };

        let filler_reward = if !reward_filler {
            0_u64
        } else {
            calculate_filler_reward(
                fee,
                order_slot,
                clock_slot,
                0,
                &fee_structure.filler_reward_structure,
                filler_reward_paid,
            )?
        };

        // taker-fee remainder after filler + referral are taken off the top
        // (referee discount already reduced `fee`), split three ways. The AMM
        // books ONLY its own cut + its spread surplus; the protocol / IF
        // carveouts accrue as pending counters and are materialized out of
        // the pnl pool by `sweep_market_fees` — they never transit the AMM.
        let mut remainder = fee.safe_sub(filler_reward)?.safe_sub(referrer_reward)?;

        // when enabled, the AMM earns the maker rebate for making this fill.
        // Carved off the remainder before the three-way split (like the
        // user-maker rebate) and folded into `amm_fee`, so it rides the
        // existing AMM ledger / pending-provision plumbing. Clamped to the
        // remainder: fee-structure numerators are admin-mutable, so the
        // rebate is not guaranteed to fit.
        let amm_rebate = if vamm_maker_rebate {
            calculate_vamm_maker_rebate(quote_asset_amount, fee_structure, fee_adjustment)?
                .min(remainder)
        } else {
            0
        };
        remainder = remainder.safe_sub(amm_rebate)?;

        let (amm_fee, if_fee, protocol_fee) = split_fee_remainder(remainder, fee_structure)?;
        let amm_fee = amm_fee.safe_add(amm_rebate)?;

        let fee_to_market = amm_fee
            .cast::<i64>()?
            .safe_add(quote_asset_amount_surplus)?;

        let builder_fee = if let Some(builder_fee_bps) = builder_fee_bps {
            Some(
                quote_asset_amount
                    .safe_mul(builder_fee_bps.cast()?)?
                    .safe_div(100_000)?,
            )
        } else {
            None
        };

        // must be non-negative
        Ok(FillFees {
            user_fee: fee,
            maker_rebate: 0,
            fee_to_market,
            filler_reward,
            referrer_reward,
            referee_discount,
            builder_fee,
            protocol_fee,
            if_fee,
            amm_fee,
        })
    }
}

/// Taker fee = `(tier fee + market add-on) * (1 +/- fee_adjustment%)`.
/// The add-on (`PerpMarket.taker_fee_addon_tenth_bps`, tenth-bps, unsigned)
/// is an additive per-market surcharge (an absolute markup the multiplicative
/// `fee_adjustment` cannot express across tiers), and `fee_adjustment` then
/// scales the whole configured fee. Surcharge only: the fee never drops below
/// the tier fee, so it always funds the maker rebate the tier validation
/// guarantees. The maker rebate sees `fee_adjustment` only, never the add-on.
fn calculate_taker_fee(
    quote_asset_amount: u64,
    fee_tier: &FeeTier,
    fee_adjustment: i16,
    taker_fee_addon_tenth_bps: u16,
) -> VelocityResult<u64> {
    let tier_fee = quote_asset_amount
        .cast::<u128>()?
        .safe_mul(fee_tier.fee_numerator.cast::<u128>()?)?
        .safe_div_ceil(fee_tier.fee_denominator.cast::<u128>()?)?;

    // tenth-bps against FEE_DENOMINATOR (100_000 = 100%), same unit the tier
    // numerators use at the default denominator
    let addon_fee = quote_asset_amount
        .cast::<u128>()?
        .safe_mul(taker_fee_addon_tenth_bps.cast::<u128>()?)?
        .safe_div(FEE_DENOMINATOR.cast::<u128>()?)?;

    let mut taker_fee = tier_fee.safe_add(addon_fee)?.cast::<u64>()?;

    if fee_adjustment < 0 {
        taker_fee = taker_fee.saturating_sub(
            taker_fee
                .safe_mul(fee_adjustment.unsigned_abs().cast()?)?
                .safe_div(FEE_ADJUSTMENT_MAX)?,
        );
    } else if fee_adjustment > 0 {
        taker_fee = taker_fee.saturating_add(
            taker_fee
                .safe_mul(fee_adjustment.cast()?)?
                .safe_div_ceil(FEE_ADJUSTMENT_MAX)?,
        );
    }

    Ok(taker_fee)
}

fn calculate_maker_rebate(
    quote_asset_amount: u64,
    fee_tier: &FeeTier,
    fee_adjustment: i16,
) -> VelocityResult<u64> {
    let mut maker_fee = quote_asset_amount
        .cast::<u128>()?
        .safe_mul(fee_tier.maker_rebate_numerator as u128)?
        .safe_div(fee_tier.maker_rebate_denominator as u128)?
        .cast::<u64>()?;

    if fee_adjustment < 0 {
        maker_fee = maker_fee.saturating_sub(
            maker_fee
                .safe_mul(fee_adjustment.unsigned_abs().cast()?)?
                .safe_div_ceil(FEE_ADJUSTMENT_MAX)?,
        );
    } else if fee_adjustment > 0 {
        maker_fee = maker_fee.saturating_add(
            maker_fee
                .safe_mul(fee_adjustment.cast()?)?
                .safe_div(FEE_ADJUSTMENT_MAX)?,
        );
    }

    Ok(maker_fee)
}

/// Rebate the vAMM earns when it makes a fill, computed from the base fee
/// tier (`fee_tiers[0]`). A rebate is a property of the maker, and the vAMM
/// has no fee tier of its own; using the taker's tier would make the vAMM's
/// earnings vary with who the taker is. Tier 0 keeps the rebate deterministic
/// and tracking whatever base maker rebate the admin configures.
fn calculate_vamm_maker_rebate(
    quote_asset_amount: u64,
    fee_structure: &FeeStructure,
    fee_adjustment: i16,
) -> VelocityResult<u64> {
    calculate_maker_rebate(
        quote_asset_amount,
        &fee_structure.fee_tiers[0],
        fee_adjustment,
    )
}

fn calculate_referee_fee_and_referrer_reward(
    fee: u64,
    fee_tier: &FeeTier,
) -> VelocityResult<(u64, u64, u64)> {
    let referee_discount = get_proportion_u128(
        fee as u128,
        fee_tier.referee_fee_numerator as u128,
        fee_tier.referee_fee_denominator as u128,
    )?
    .cast::<u64>()?;

    let referrer_reward = get_proportion_u128(
        fee as u128,
        fee_tier.referrer_reward_numerator as u128,
        fee_tier.referrer_reward_denominator as u128,
    )?
    .cast::<u64>()?;

    let referee_fee = fee.safe_sub(referee_discount)?;

    Ok((referee_fee, referee_discount, referrer_reward))
}

/// `filler_reward_paid` is what earlier legs of this same fill already paid the
/// filler. The size-based term is linear in the fee, so it sums correctly across
/// legs on its own; the time-based term is size-independent (same `order_slot`,
/// `clock_slot` and `multiplier` for every leg of one order), so it is an
/// allowance for the whole fill and has to be drawn down rather than re-granted
/// per leg. Without that, a taker crossing N sources pays the time-based reward
/// N times.
fn calculate_filler_reward(
    fee: u64,
    order_slot: u64,
    clock_slot: u64,
    multiplier: u64,
    filler_reward_structure: &OrderFillerRewardStructure,
    filler_reward_paid: u64,
) -> VelocityResult<u64> {
    // incentivize keepers to prioritize filling older orders (rather than just largest orders)
    // for sufficiently small-sized order, reward based on fraction of fee paid

    let size_filler_reward = fee
        .safe_mul(filler_reward_structure.reward_numerator as u64)?
        .safe_div(filler_reward_structure.reward_denominator as u64)?;

    let multiplier_precision = TEN_BPS.cast::<u128>()?;

    let min_time_filler_reward = filler_reward_structure
        .time_based_reward_lower_bound
        .safe_mul(
            multiplier
                .cast::<u128>()?
                .max(multiplier_precision)
                .min(multiplier_precision * 100),
        )?
        .safe_div(multiplier_precision)?;

    let slots_since_order = max(1, clock_slot.safe_sub(order_slot)?.cast::<u128>()?);
    let time_filler_reward = slots_since_order
        .safe_mul(100_000_000)? // 1e8
        .nth_root(4)
        .safe_mul(min_time_filler_reward)?
        .safe_div(100)? // 1e2 = sqrt(sqrt(1e8))
        .cast::<u64>()?;

    // lesser of size-based and the time-based allowance left for this fill
    let fee = min(
        size_filler_reward,
        time_filler_reward.saturating_sub(filler_reward_paid),
    );

    Ok(fee)
}

#[allow(clippy::too_many_arguments)]
pub fn calculate_fee_for_fulfillment_with_match(
    taker_stats: &UserStats,
    maker_stats: &Option<&mut UserStats>,
    quote_asset_amount: u64,
    fee_structure: &FeeStructure,
    order_slot: u64,
    clock_slot: u64,
    filler_multiplier: u64,
    reward_referrer: bool,
    market_type: &MarketType,
    fee_adjustment: i16,
    builder_fee_bps: Option<u16>,
    taker_fee_addon_tenth_bps: u16,
    now: i64,
    promo_fee_tier: u8,
    filler_reward_paid: u64,
) -> VelocityResult<FillFees> {
    let taker_fee_tier =
        determine_user_fee_tier(taker_stats, fee_structure, market_type, now, promo_fee_tier)?;
    let maker_fee_tier = if let Some(maker_stats) = maker_stats {
        determine_user_fee_tier(maker_stats, fee_structure, market_type, now, promo_fee_tier)?
    } else {
        determine_user_fee_tier(taker_stats, fee_structure, market_type, now, promo_fee_tier)?
    };

    let taker_fee = calculate_taker_fee(
        quote_asset_amount,
        &taker_fee_tier,
        fee_adjustment,
        taker_fee_addon_tenth_bps,
    )?;

    let (taker_fee, referee_discount, referrer_reward) = if reward_referrer {
        calculate_referee_fee_and_referrer_reward(taker_fee, &taker_fee_tier)?
    } else {
        (taker_fee, 0, 0)
    };

    let maker_rebate = calculate_maker_rebate(quote_asset_amount, &maker_fee_tier, fee_adjustment)?;

    let filler_reward = if filler_multiplier == 0 {
        0_u64
    } else {
        calculate_filler_reward(
            taker_fee,
            order_slot,
            clock_slot,
            filler_multiplier,
            &fee_structure.filler_reward_structure,
            filler_reward_paid,
        )?
    };

    // remainder after maker rebate + referral + filler (referee discount
    // already reduced taker_fee), split three ways like AMM fills. The AMM cut
    // is credited to the AMM's books by the caller (`fee_to_market` carries it)
    // — the AMM quotes this market and earns its provision on all fills;
    // tokens are realized into its fee pool by the `sweep_market_fees`
    // tokenization step.
    let remainder = taker_fee
        .safe_sub(filler_reward)?
        .safe_sub(referrer_reward)?
        .safe_sub(maker_rebate)?;
    let (amm_fee, if_fee, protocol_fee) = split_fee_remainder(remainder, fee_structure)?;
    let fee_to_market = amm_fee.cast::<i64>()?;

    let builder_fee = if let Some(builder_fee_bps) = builder_fee_bps {
        Some(
            quote_asset_amount
                .safe_mul(builder_fee_bps.cast()?)?
                .safe_div(100_000)?,
        )
    } else {
        None
    };

    Ok(FillFees {
        user_fee: taker_fee,
        maker_rebate,
        fee_to_market,
        filler_reward,
        referrer_reward,
        referee_discount,
        builder_fee,
        protocol_fee,
        if_fee,
        amm_fee,
    })
}

/// What resolving one taker-origin cross is worth, and what the cranker takes
/// out of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TakerOriginCrossFee {
    /// Gross quote the taker gains by trading at the counterparty's price
    /// instead of the price it was resting at: `|rest − counterparty| × base`,
    /// expressed as the difference of the two notionals.
    pub improvement: u64,
    /// The improvement net of the difference in taker fee between the two
    /// notionals — what the taker is actually better off by, and therefore all
    /// the reward can be drawn from. Zero when the two are a wash.
    pub budget: u64,
    /// Paid to the cranker in quote, out of `budget`.
    pub crank_reward: u64,
}

impl TakerOriginCrossFee {
    /// What the taker keeps: the whole budget when the cross resolves for
    /// free, the rest of it when the cranker is paid. Never negative — that is
    /// the R5 invariant, and it is why this is a subtraction and not a
    /// `saturating_sub`.
    pub fn taker_surplus(&self) -> VelocityResult<u64> {
        self.budget.safe_sub(self.crank_reward)
    }
}

/// R5: size the cranker's reward for resolving a taker-origin cross.
///
/// The reward is an ordinary filler reward — same
/// [`calculate_filler_reward`] sizing every fill pays, off the taker fee this
/// fill charges and the age of the resting order — but it is charged to the
/// taker rather than carved out of the taker fee, because the thing it is
/// buying belongs to the taker: the difference between the counterparty's
/// price and the price the order was resting at. Widening the taker fee on
/// this path instead would couple the crank's cost to the protocol fee
/// schedule, which changes for unrelated reasons.
///
/// The reward is paid **in full or not at all**, never shaved to fit: the
/// cranker's revenue stays predictable, and the taker never funds a keeper
/// subsidy out of a gain that could not cover one. So the cap is a threshold —
/// an improvement that does not strictly exceed the reward resolves the cross
/// for *free* instead of paying a reduced one.
///
/// **A cross whose improvement cannot cover a reward is still resolved.** The
/// alternative — refusing it, as "only fires when the improvement exceeds the
/// fee" reads on its own — leaves the order gated against being taken with
/// nothing able to clear the gate, which is strictly worse for the taker than
/// the fill it asked for; and one unit of dust improvement in front of a
/// remainder would be enough to strand it for its whole life. The cranker is
/// not working for nothing either way: its lamport cost is covered by the
/// market's reservoir, the same as every other crank's. The zero-improvement
/// cross — counterparty price equal to the rest price — is the same case and
/// the same answer: resolve it, pay nothing, the taker gets the fill it wanted
/// at a price it had already accepted.
///
/// `budget` is the improvement net of the *fee* difference, not the gross
/// improvement, because the two notionals carry different taker fees. Selling
/// at a better price means a bigger notional and a bigger fee. A cross whose
/// improvement is smaller than that extra fee would leave the taker worse off
/// than resting, and that — the invariant's one hard edge — is refused with
/// [`ErrorCode::TakerOriginCrossWorseForTaker`]. It takes a taker fee above
/// 100% to reach: the extra fee is `rate × improvement`, so any schedule
/// charging less than the whole trade leaves a non-negative budget.
///
/// `rest_quote` is the notional at the price the taker-origin order was
/// resting at, `counterparty_quote` the notional at the counterparty's price
/// (what the match settles at), and `order_slot` the slot the taker-origin
/// order was placed — its age drives the time-based half of the reward, as an
/// `Order.slot` does on the DLOB.
#[allow(clippy::too_many_arguments)]
pub fn calculate_taker_origin_cross_fee(
    taker_direction: PositionDirection,
    rest_quote: u64,
    counterparty_quote: u64,
    fee_tier: &FeeTier,
    fee_adjustment: i16,
    taker_fee_addon_tenth_bps: u16,
    order_slot: u64,
    clock_slot: u64,
    filler_multiplier: u64,
    filler_reward_structure: &OrderFillerRewardStructure,
) -> VelocityResult<TakerOriginCrossFee> {
    let fee_at_rest = calculate_taker_fee(
        rest_quote,
        fee_tier,
        fee_adjustment,
        taker_fee_addon_tenth_bps,
    )?;
    let fee_at_cross = calculate_taker_fee(
        counterparty_quote,
        fee_tier,
        fee_adjustment,
        taker_fee_addon_tenth_bps,
    )?;
    // Buying: the taker pays the smaller notional. Selling: it receives the
    // bigger one. Either way the taker's cost improves by this much, before
    // fees.
    let improvement = match taker_direction {
        PositionDirection::Long => rest_quote.saturating_sub(counterparty_quote),
        PositionDirection::Short => counterparty_quote.saturating_sub(rest_quote),
    };
    // The invariant, as arithmetic: crossing must not cost the taker more than
    // resting did. `improvement + fee_at_rest` is what it saves,
    // `fee_at_cross` what it now owes.
    let budget = improvement
        .cast::<i128>()?
        .safe_add(fee_at_rest.cast::<i128>()?)?
        .safe_sub(fee_at_cross.cast::<i128>()?)?;
    if budget < 0 {
        msg!(
            "taker-origin cross improves {} but costs {} more in taker fee; leaving it resting",
            improvement,
            fee_at_cross.saturating_sub(fee_at_rest)
        );
        return Err(ErrorCode::TakerOriginCrossWorseForTaker);
    }
    let budget = budget.cast::<u64>()?;

    let uncapped = if filler_multiplier == 0 {
        0
    } else {
        calculate_filler_reward(
            fee_at_cross,
            order_slot,
            clock_slot,
            filler_multiplier,
            filler_reward_structure,
            0,
        )?
    };
    Ok(TakerOriginCrossFee {
        improvement,
        budget,
        // Strictly less, so the taker keeps something whenever the cranker is
        // paid at all: its net then *beats* the resting price rather than
        // merely matching it.
        crank_reward: if uncapped < budget { uncapped } else { 0 },
    })
}

pub fn determine_user_fee_tier(
    user_stats: &UserStats,
    fee_structure: &FeeStructure,
    market_type: &MarketType,
    now: i64,
    promo_fee_tier: u8,
) -> VelocityResult<FeeTier> {
    match market_type {
        MarketType::Perp => determine_perp_fee_tier(user_stats, fee_structure, now, promo_fee_tier),
        MarketType::Spot => Ok(*determine_spot_fee_tier(user_stats, fee_structure)?),
    }
}

/// Select the perp fee tier from the trailing-30d volume, evaluated LIVE.
/// The populated tiers are named Regular / VIP 1 / VIP 2 (indices 0/1/2);
/// the names are presentation only, everything onchain is index-based.
///
/// The volume window:
/// the stored rolling sum decays lazily (only when the account trades; see
/// `UserStats::update_taker_volume_30d`), so the raw value can be stale by
/// the whole idle gap. Projecting the decay to `now` at read time makes
/// demotion track the live 30d window at every fill, while promotion stays
/// instant (each fill's volume lands in the sum immediately, so the next
/// fill after crossing a threshold is already priced at the better tier).
///
/// `promo_fee_tier` (`State.promo_fee_tier`) forces a tier-index floor for
/// everyone while set: the effective tier is the better of the volume tier
/// and the promo tier, so accounts already above the promo are not
/// downgraded. 0 is a no-op floor (= disabled, also what legacy accounts
/// read from former padding), and resetting to 0 drops every account back
/// to its volume tier on their next fill; no per-user state.
fn determine_perp_fee_tier(
    user_stats: &UserStats,
    fee_structure: &FeeStructure,
    now: i64,
    promo_fee_tier: u8,
) -> VelocityResult<FeeTier> {
    let total_30d_volume = user_stats.get_total_30d_volume_at(now)?;

    const VOLUME_THRESHOLDS: [u64; PERP_FEE_TIER_MAX_INDEX] =
        [FIVE_MILLION_QUOTE, TEN_MILLION_QUOTE * 8];

    let mut fee_tier_index = PERP_FEE_TIER_MAX_INDEX;
    for i in 0..PERP_FEE_TIER_MAX_INDEX {
        if total_30d_volume < VOLUME_THRESHOLDS[i] {
            fee_tier_index = i;
            break;
        }
    }

    fee_tier_index = fee_tier_index.max((promo_fee_tier as usize).min(PERP_FEE_TIER_MAX_INDEX));

    Ok(fee_structure.fee_tiers[fee_tier_index])
}

fn determine_spot_fee_tier<'a>(
    _user_stats: &UserStats,
    fee_structure: &'a FeeStructure,
) -> VelocityResult<&'a FeeTier> {
    Ok(&fee_structure.fee_tiers[0])
}
