import { BN } from '@coral-xyz/anchor';

import {
	VIP_FEE_TIER_ONE_VOLUME_QUOTE,
	VIP_FEE_TIER_TWO_VOLUME_QUOTE,
} from '../constants/numericConstants';
import { StateAccount, UserStatsAccount } from '../types';
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
 * breakpoint the volume is still under. Tiers 0/1/2
 * are named Regular / VIP 1 / VIP 2; the names are presentation only,
 * selection is index-based.
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
