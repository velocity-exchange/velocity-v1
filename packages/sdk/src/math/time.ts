import { BN } from '@coral-xyz/anchor';

/**
 * Wall-clock durations and the live slot length — the TypeScript mirror of the
 * program's `math/time.rs`.
 *
 * Solana's slot time is dropping from 400ms to 200ms through feature gates
 * (400 -> 350 -> 300 -> 250 -> 200), so slot counts and wall-clock durations
 * are separated by (branded) type:
 *
 * - `Millis` is the only duration unit: every threshold, window, ramp, and
 *   grace period. It cannot be compared against a slot count without
 *   converting through the live slot length.
 * - `SlotDurationMs` is the current slot length from `State.slotDurationMs`
 *   (`slotDurationFromState` resolves the 0-unset sentinel to the 400ms
 *   baseline).
 * - Plain numbers/BN stay the type of actual slot counts.
 *
 * Legacy admin-set onchain fields keep their compact encoding in units of
 * `STORED_UNIT_MS` = 400ms (the historical slot length); decode them with
 * `millisFromStoredUnits`. That factor is a storage codec detail, not a unit
 * to think in; new stored durations should store milliseconds natively.
 *
 * Rounding matches the program exactly: `millisToSlots` floors (staleness
 * windows marginally tighter, the safe direction), `millisToSlotsCeil` is for
 * user-protection windows, `millisFromSlots` is exact, `divPeriods` floors.
 *
 * Full design rationale and worked examples: `docs/SLOT-DURATION.md` in the
 * repo root.
 */

/**
 * Storage encoding quantum for pre-gate duration fields: the historical 400ms
 * slot length. Confined to decoding those fields (and the calibration periods
 * of a few legacy per-slot rates).
 */
export const STORED_UNIT_MS = 400;

/** A wall-clock duration in milliseconds (branded BN). */
export type Millis = BN & { readonly __millis: unique symbol };

/** The live slot length in milliseconds (branded number). */
export type SlotDurationMs = number & {
	readonly __slotDuration: unique symbol;
};

/** The pre-gate 400ms baseline slot length. */
export const SLOT_DURATION_BASELINE = STORED_UNIT_MS as SlotDurationMs;

/**
 * The historical 400ms calibration period of the legacy per-slot rates
 * (mirrors `Millis::UNIT`).
 */
export const MILLIS_UNIT = new BN(STORED_UNIT_MS) as Millis;

/**
 * Resolve the raw `State.slotDurationMs` field: `0` is what pre-upgrade
 * accounts read out of former padding and means "unset" (the 400ms baseline).
 */
export function slotDurationFromState(raw: number): SlotDurationMs {
	return (raw === 0 ? STORED_UNIT_MS : raw) as SlotDurationMs;
}

export function millis(ms: number): Millis {
	return new BN(ms) as Millis;
}

export function millisFromSecs(secs: number): Millis {
	return new BN(secs).muln(1_000) as Millis;
}

/**
 * Decode a legacy stored value denominated in `STORED_UNIT_MS` units.
 * Storage codec only; never use for new values.
 */
export function millisFromStoredUnits(units: BN | number): Millis {
	return new BN(units).muln(STORED_UNIT_MS) as Millis;
}

/**
 * The exact wall-clock time a measured slot delta represents at the current
 * slot duration. Mirrors `Millis::from_slots`.
 */
export function millisFromSlots(slots: BN, d: SlotDurationMs): Millis {
	return slots.muln(d) as Millis;
}

/**
 * A duration expressed in actual slots, rounding down (mirrors
 * `Millis::to_slots`). Default for staleness windows.
 */
export function millisToSlots(m: Millis, d: SlotDurationMs): BN {
	return m.divn(Math.max(1, d));
}

/**
 * A duration expressed in actual slots, rounding up (mirrors
 * `Millis::to_slots_ceil`). For user-protection windows.
 */
export function millisToSlotsCeil(m: Millis, d: SlotDurationMs): BN {
	const dd = Math.max(1, d);
	return m.addn(dd - 1).divn(dd);
}

/**
 * How many whole `period`s fit in a duration, rounding down (mirrors
 * `Millis::div_periods`). For legacy per-period rates.
 */
export function divPeriods(m: Millis, period: Millis): BN {
	return m.div(BN.max(new BN(1), period));
}

/** `millisToSlots` for plain numbers (off-chain pacing/threshold code). */
export function msToSlotsNum(ms: number, d: SlotDurationMs): number {
	return Math.floor(ms / Math.max(1, d));
}

/** `millisFromSlots` for plain numbers (off-chain pacing/threshold code). */
export function slotsToMsNum(slots: number, d: SlotDurationMs): number {
	return slots * d;
}
