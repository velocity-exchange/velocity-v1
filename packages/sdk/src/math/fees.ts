import { BN } from '@coral-xyz/anchor';

import {
	VIP_FEE_TIER_ONE_VOLUME_QUOTE,
	VIP_FEE_TIER_THREE_VOLUME_QUOTE,
	VIP_FEE_TIER_TWO_VOLUME_QUOTE,
} from '../constants/numericConstants';
import {
	FeeTier,
	isVariant,
	MarketType,
	PerpMarketAccount,
	SpotMarketAccount,
	StateAccount,
	UserStatsAccount,
} from '../types';
import { getUser30dRollingVolumeEstimate } from './trade';

/**
 * Trailing-30d volume breakpoints that separate the perp fee tiers, in
 * QUOTE_PRECISION and index order. Mirrors `VOLUME_THRESHOLDS` in the
 * program's `determine_perp_fee_tier`; the values are hardcoded on-chain, so
 * unlike the fee rates they cannot be read from the state account.
 */
export const PERP_FEE_TIER_VOLUME_THRESHOLDS = [
	VIP_FEE_TIER_ONE_VOLUME_QUOTE,
	VIP_FEE_TIER_TWO_VOLUME_QUOTE,
	VIP_FEE_TIER_THREE_VOLUME_QUOTE,
];

/**
 * Highest perp fee-tier index the program can select. Tiers `0..=this` are
 * live; the remaining `feeTiers` slots are spares the program never picks.
 */
export const PERP_FEE_TIER_MAX_INDEX = PERP_FEE_TIER_VOLUME_THRESHOLDS.length;

/**
 * Selects the perp fee-tier index for an account, mirroring the program's
 * `determine_perp_fee_tier`.
 *
 * The tier comes from the account's trailing 30-day volume projected to `now`
 * (`getUser30dRollingVolumeEstimate`, which applies virtually the same decay
 * the on-chain rolling sum applies lazily), taking the lowest-index tier whose
 * breakpoint the volume is still under. Tiers 0/1/2/3 are named Regular /
 * VIP 1 / VIP 2 / VIP 3; the names are presentation only, selection is
 * index-based.
 *
 * `state.promoFeeTier` then floors the result for everyone while it is set, so
 * an account already above the promo keeps its own tier and nobody is
 * downgraded. 0 is a no-op floor (promo disabled), which is also what accounts
 * written before the field existed read out of former padding.
 *
 * @param userStatsAccount The account's stats, holding the volume the tier is
 *   derived from. Omit it for the generic schedule (no account in hand): the
 *   volume tier is then the entry tier, and an active promo still applies.
 * @param state Global state, for the promo floor.
 * @param now Optional unix timestamp (seconds) to evaluate the rolling volume
 *   window as of; defaults to the current time.
 * @returns The index into `state.perpFeeStructure.feeTiers`.
 */
export function getPerpFeeTierIndex(
	userStatsAccount: UserStatsAccount | undefined,
	state: StateAccount,
	now?: BN
): number {
	let feeTierIndex = 0;

	if (userStatsAccount) {
		const total30dVolume = getUser30dRollingVolumeEstimate(
			userStatsAccount,
			now
		);

		feeTierIndex = PERP_FEE_TIER_MAX_INDEX;
		for (let i = 0; i < PERP_FEE_TIER_VOLUME_THRESHOLDS.length; i++) {
			if (total30dVolume.lt(PERP_FEE_TIER_VOLUME_THRESHOLDS[i])) {
				feeTierIndex = i;
				break;
			}
		}
	}

	// A promo floor is clamped to the live tiers, matching the program's own
	// clamp, so a misconfigured floor can never select a spare slot. A state
	// account missing the field entirely reads as the disabled floor rather
	// than propagating NaN into the tier lookup.
	return Math.max(
		feeTierIndex,
		Math.min(state.promoFeeTier ?? 0, PERP_FEE_TIER_MAX_INDEX)
	);
}

/**
 * Applies every per-market and per-account modifier the program applies on top
 * of a fee tier's own rates, in the program's order (`calculate_taker_fee` and
 * `calculate_referee_fee_and_referrer_reward`, `math/fees.rs`):
 *
 * 1. the market's `takerFeeAddonTenthBps` surcharge, taker leg only (perp only)
 * 2. the market's `feeAdjustment` percentage, scaling both legs
 * 3. the referee discount, taker leg only
 * 4. the builder fee, taker leg only
 *
 * `VelocityClient.getMarketFees` is this function with the tier and the
 * account-derived inputs resolved for you. Call this directly to price a tier
 * the account is not on, e.g. to show what a fee promotion is saving someone
 * against their own volume tier; both figures then come out of the same
 * pipeline and differ only by the tier.
 *
 * @param feeTier The tier to price.
 * @param marketType `MarketType.PERP` or `MarketType.SPOT`.
 * @param marketAccount The market whose surcharge and `feeAdjustment` apply.
 *   Omit for the market-independent schedule.
 * @param opts.isReferee Whether the taker is a referee, which discounts the
 *   taker fee by the tier's referee fraction.
 * @param opts.builderFeeTenthBps A builder fee to add to the taker leg, in
 *   tenth-bps. Omit when no builder fee is charged; the caller decides that,
 *   since the program waives it for a taker below initial margin.
 * @returns Taker fee and maker rebate as fractions of notional (0.0001 = 1bp).
 */
export function getMarketFeesForFeeTier(
	feeTier: FeeTier,
	marketType: MarketType,
	marketAccount?: PerpMarketAccount | SpotMarketAccount,
	opts?: {
		isReferee?: boolean;
		builderFeeTenthBps?: number;
	}
): { takerFee: number; makerFee: number } {
	let takerFee = feeTier.feeNumerator / feeTier.feeDenominator;
	let makerFee = feeTier.makerRebateNumerator / feeTier.makerRebateDenominator;

	if (marketAccount) {
		// The surcharge is unsigned tenth-bps and lands on the tier fee BEFORE
		// feeAdjustment scales the sum. Taker only; the maker rebate sees
		// feeAdjustment alone.
		if (isVariant(marketType, 'perp')) {
			takerFee +=
				(marketAccount as PerpMarketAccount).takerFeeAddonTenthBps / 100_000;
		}
		takerFee += (takerFee * marketAccount.feeAdjustment) / 100;
		makerFee += (makerFee * marketAccount.feeAdjustment) / 100;
	}

	// After feeAdjustment and taker-only, matching the program's ordering.
	if (opts?.isReferee && feeTier.refereeFeeDenominator > 0) {
		takerFee -=
			(takerFee * feeTier.refereeFeeNumerator) / feeTier.refereeFeeDenominator;
	}

	if (opts?.builderFeeTenthBps) {
		takerFee += opts.builderFeeTenthBps / 100_000;
	}

	return { takerFee, makerFee };
}
