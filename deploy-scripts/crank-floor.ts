/**
 * The payment floor of a market's cross fallback poll. The poll can stage a cross or a band cancel,
 * so the floor is the lowest of the cross, taker-origin cross and removal payments. An attach
 * written before the band cancel holds the cross floor, and relay then refuses every band cancel.
 */

/** Account byte offset of the fallback condition's `min_payment`: the discriminator, then the
 * 16-byte relay block header. The fallback is condition 0, and `min_payment` is its first field.
 * The Rust test `the_scripts_read_the_fallback_floor_at_its_offset` pins this number. */
export const CROSS_FALLBACK_MIN_PAYMENT_OFFSET = 24;

export type CrankPayments = {
	removal: number;
	cross: number;
	takerOriginCross: number;
};

/** Mirrors `cross_fallback_floor` in `crank_conditions_setup.rs`. */
export function crossFallbackFloor(payments: CrankPayments): bigint {
	return BigInt(
		Math.min(payments.cross, payments.takerOriginCross, payments.removal)
	);
}

/** The floor the stored fallback condition advertises. */
export function armedCrossFallbackFloor(conditionsData: Buffer): bigint {
	return conditionsData.readBigUInt64LE(CROSS_FALLBACK_MIN_PAYMENT_OFFSET);
}

/** Whether a market's crank conditions advertise a fallback floor other than the one an attach
 * writes from its stored payments. */
export function crossFallbackFloorIsStale(
	conditionsData: Buffer,
	payments: CrankPayments
): boolean {
	return (
		armedCrossFallbackFloor(conditionsData) !== crossFallbackFloor(payments)
	);
}
