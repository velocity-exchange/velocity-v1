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
 * Trailing-30d volume breakpoints separating the perp fee tiers, in QUOTE_PRECISION and index
 * order. Mirror `VOLUME_THRESHOLDS` in `determine_perp_fee_tier`. The program hardcodes these values, unlike the fee rates, so they cannot be read from state.
 */
export const PERP_FEE_TIER_VOLUME_THRESHOLDS = [
	VIP_FEE_TIER_ONE_VOLUME_QUOTE,
	VIP_FEE_TIER_TWO_VOLUME_QUOTE,
	VIP_FEE_TIER_THREE_VOLUME_QUOTE,
];

/**
 * Highest perp fee-tier index the program can select. Tiers 0 through this index are live.
 * The remaining `feeTiers` slots are spares the program never picks.
 */
export const PERP_FEE_TIER_MAX_INDEX = PERP_FEE_TIER_VOLUME_THRESHOLDS.length;

/**
 * The perp fee-tier index for an account, mirroring the program's `determine_perp_fee_tier`.
 * The tier comes from the account's trailing 30-day volume projected to `now`.
 * `getUser30dRollingVolumeEstimate` applies nearly the same decay the on-chain rolling sum
 * applies lazily. The tier is the lowest index whose breakpoint the volume is still under.
 * `state.promoFeeTier` then floors the result while it is set, without downgrading an account
 * already above the promo.
 * @param userStatsAccount Omit for the generic schedule: the volume tier is then the entry
 *   tier, and an active promo still applies.
 * @param now Unix seconds the rolling volume window is evaluated at. Defaults to now.
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

	// The promo floor is clamped to the live tiers, as the program clamps it,
	// so a misconfigured floor can never select a spare slot. A state account
	// that lacks the field reads as the disabled floor rather than putting NaN
	// into the tier lookup.
	return Math.max(
		feeTierIndex,
		Math.min(state.promoFeeTier ?? 0, PERP_FEE_TIER_MAX_INDEX)
	);
}

/**
 * Applies every per-market and per-account modifier on top of a fee tier's own rates. The order
 * matches `calculate_taker_fee` and `calculate_referee_fee_and_referrer_reward` in `math/fees.rs`.
 * 1. `takerFeeAddonTenthBps` surcharge, taker leg of a perp market only
 * 2. `feeAdjustment` percentage, both legs
 * 3. referee discount, taker leg only
 * 4. builder fee, taker leg only
 * `VelocityClient.getMarketFees` calls this with the account's own tier resolved. Call it
 * directly to price a tier the account is not on, such as a promotion's savings against it.
 * @param marketAccount The market whose surcharge and `feeAdjustment` apply, omitted for the market-independent schedule.
 * @param opts.builderFeeTenthBps Tenth-bps added to the taker leg. The caller decides whether to
 *   pass it, since the program waives the fee for a taker below initial margin.
 * @returns Taker fee and maker rebate as fractions of notional. 0.0001 is one basis point.
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
		// The surcharge is unsigned tenth-bps. It lands on the tier fee before
		// feeAdjustment scales the sum, and it applies to the taker leg only.
		// The maker rebate sees feeAdjustment alone.
		if (isVariant(marketType, 'perp')) {
			takerFee +=
				(marketAccount as PerpMarketAccount).takerFeeAddonTenthBps / 100_000;
		}
		takerFee += (takerFee * marketAccount.feeAdjustment) / 100;
		makerFee += (makerFee * marketAccount.feeAdjustment) / 100;
	}

	// The program applies the referee discount after feeAdjustment, and to the
	// taker leg only.
	if (opts?.isReferee && feeTier.refereeFeeDenominator > 0) {
		takerFee -=
			(takerFee * feeTier.refereeFeeNumerator) / feeTier.refereeFeeDenominator;
	}

	if (opts?.builderFeeTenthBps) {
		takerFee += opts.builderFeeTenthBps / 100_000;
	}

	return { takerFee, makerFee };
}
