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
 * QUOTE_PRECISION and index order. They mirror `VOLUME_THRESHOLDS` in the
 * program's `determine_perp_fee_tier`. The program hardcodes the values, so
 * unlike the fee rates they cannot be read from the state account.
 */
export const PERP_FEE_TIER_VOLUME_THRESHOLDS = [
	VIP_FEE_TIER_ONE_VOLUME_QUOTE,
	VIP_FEE_TIER_TWO_VOLUME_QUOTE,
	VIP_FEE_TIER_THREE_VOLUME_QUOTE,
];

/**
 * Highest perp fee-tier index the program can select. The tiers from 0 to this
 * index are live. The remaining `feeTiers` slots are spares the program never
 * picks.
 */
export const PERP_FEE_TIER_MAX_INDEX = PERP_FEE_TIER_VOLUME_THRESHOLDS.length;

/**
 * The perp fee-tier index for an account. This mirrors the program's
 * `determine_perp_fee_tier`.
 *
 * The tier comes from the account's trailing 30-day volume projected to `now`.
 * `getUser30dRollingVolumeEstimate` applies nearly the same decay that the
 * on-chain rolling sum applies lazily. The tier is the lowest index whose
 * breakpoint the volume is still under. Tiers 0, 1, 2 and 3 are named Regular,
 * VIP 1, VIP 2 and VIP 3. The names are presentation only, and the program
 * selects by index.
 *
 * `state.promoFeeTier` then floors the result for every account while it is
 * set. An account already above the promo keeps its own tier, so no account is
 * downgraded. A floor of 0 changes nothing, which is also what an account
 * written before the field existed reads out of former padding.
 *
 * @param userStatsAccount The account's stats, which hold the volume the tier
 *   derives from. Omit it for the generic schedule when no account is in hand.
 *   The volume tier is then the entry tier, and an active promo still applies.
 * @param state Global state, for the promo floor.
 * @param now Unix timestamp in seconds that the rolling volume window is
 *   evaluated at. It defaults to the current time.
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
 * Applies every per-market and per-account modifier the program applies on top
 * of a fee tier's own rates, in the program's order. `calculate_taker_fee` and
 * `calculate_referee_fee_and_referrer_reward` in `math/fees.rs` hold that
 * order:
 *
 * 1. the market's `takerFeeAddonTenthBps` surcharge, on the taker leg of a
 *    perp market only
 * 2. the market's `feeAdjustment` percentage, on both legs
 * 3. the referee discount, on the taker leg only
 * 4. the builder fee, on the taker leg only
 *
 * `VelocityClient.getMarketFees` calls this function with the tier and the
 * account-derived inputs already resolved. Call this function directly to
 * price a tier the account is not on. One such case is showing what a fee
 * promotion saves an account against its own volume tier. Both figures then
 * come from the same pipeline and differ only by the tier.
 *
 * @param feeTier The tier to price.
 * @param marketType `MarketType.PERP` or `MarketType.SPOT`.
 * @param marketAccount The market whose surcharge and `feeAdjustment` apply.
 *   Omit it for the market-independent schedule.
 * @param opts.isReferee Whether the taker is a referee. A referee's taker fee
 *   drops by the tier's referee fraction.
 * @param opts.builderFeeTenthBps A builder fee to add to the taker leg, in
 *   tenth-bps. Omit it when no builder fee is charged. The caller decides
 *   that, because the program waives the fee for a taker below initial margin.
 * @returns The taker fee and the maker rebate as fractions of notional. A
 *   value of 0.0001 is one basis point.
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
