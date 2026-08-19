import { BN } from '@coral-xyz/anchor';

/**
 * Slot-count scaling for variable slot duration — the TypeScript mirror of the
 * program's `math/slots.rs`.
 *
 * Solana's slot time is dropping from 400ms to 200ms through a series of
 * feature gates (400 -> 350 -> 300 -> 250 -> 200). Every slot-denominated
 * constant and admin-set field in the program keeps its historical value and
 * is interpreted as a count of 400ms baseline units; `State.slotDurationMs`
 * records what a slot is currently worth, and these helpers convert between
 * baseline units and actual slots. The rounding directions here match the
 * program exactly (floor by default; ceil for user-protection windows) — do
 * not change one side without the other.
 */

/** The slot duration every slot-denominated value was calibrated against, in ms. */
export const BASE_SLOT_DURATION_MS = 400;

/**
 * Resolve the raw `State.slotDurationMs` field: `0` is what pre-upgrade
 * accounts read out of former padding and means "unset" (the 400ms baseline).
 */
export function sanitizeSlotDurationMs(raw: number): number {
	return raw === 0 ? BASE_SLOT_DURATION_MS : raw;
}

/**
 * Inflate a baseline(400ms)-denominated slot count into actual slots at the
 * current slot duration, rounding down. Mirrors `effective_slots`.
 */
export function effectiveSlots(baseSlots: BN, slotDurationMs: number): BN {
	const ms = Math.max(1, sanitizeSlotDurationMs(slotDurationMs));
	return baseSlots.muln(BASE_SLOT_DURATION_MS).divn(ms);
}

/**
 * Inflate a baseline(400ms)-denominated slot count into actual slots at the
 * current slot duration, rounding up. Mirrors `effective_slots_ceil`.
 */
export function effectiveSlotsCeil(baseSlots: BN, slotDurationMs: number): BN {
	const ms = Math.max(1, sanitizeSlotDurationMs(slotDurationMs));
	return baseSlots
		.muln(BASE_SLOT_DURATION_MS)
		.addn(ms - 1)
		.divn(ms);
}

/**
 * Deflate a measured slot delta into baseline(400ms) units, rounding down.
 * Mirrors `base_units_from_slots`.
 */
export function baseUnitsFromSlots(slots: BN, slotDurationMs: number): BN {
	const ms = sanitizeSlotDurationMs(slotDurationMs);
	return slots.muln(ms).divn(BASE_SLOT_DURATION_MS);
}

/** `effectiveSlots` for plain numbers (off-chain pacing/threshold code). */
export function effectiveSlotsNum(
	baseSlots: number,
	slotDurationMs: number
): number {
	const ms = Math.max(1, sanitizeSlotDurationMs(slotDurationMs));
	return Math.floor((baseSlots * BASE_SLOT_DURATION_MS) / ms);
}

/** `baseUnitsFromSlots` for plain numbers (off-chain pacing/threshold code). */
export function baseUnitsFromSlotsNum(
	slots: number,
	slotDurationMs: number
): number {
	const ms = sanitizeSlotDurationMs(slotDurationMs);
	return Math.floor((slots * ms) / BASE_SLOT_DURATION_MS);
}
