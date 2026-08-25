import { BN } from '../isomorphic/anchor';

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
 * to think in; new compact onchain fields should declare their encoding with
 * Rust's `StoredSlotDuration<T, SLOT_MS>` and normalize to `Millis` for math.
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
 * Every slot length the gate rollout schedules, longest first — the mirror of
 * the program's `SLOT_DURATION_SCHEDULE_MS`. The setter is monotonic-decreasing
 * and rejects gate skips, so the live duration is always one of these.
 */
export const SLOT_DURATION_SCHEDULE_MS: readonly SlotDurationMs[] = [
	400, 350, 300, 250, 200,
] as SlotDurationMs[];

/**
 * The shortest scheduled slot length (200ms, fully rolled out). The safe
 * assumption for a **user-protection window** (swift signing budgets, blockhash
 * and auction countdowns) when the slot feed is dead: it under-promises the
 * wall clock the user has instead of doubling it. The opposite direction from
 * {@link SLOT_DURATION_BASELINE} — see `docs/SLOT-DURATION.md`.
 */
export const SLOT_DURATION_FLOOR = SLOT_DURATION_SCHEDULE_MS[
	SLOT_DURATION_SCHEDULE_MS.length - 1
] as SlotDurationMs;

/**
 * The historical 400ms calibration period of the legacy per-slot rates
 * (mirrors `Millis::UNIT`).
 */
export const MILLIS_UNIT = new BN(STORED_UNIT_MS) as Millis;

/**
 * Resolve the raw `State.slotDurationMs` (base) field: `0` is what pre-upgrade
 * accounts read out of former padding and means "unset" (the 400ms baseline).
 * This is the value *before* any staged switch — most callers want
 * {@link activeSlotDurationFromState}, which also applies a staged flip.
 */
export function slotDurationFromState(raw?: number): SlotDurationMs {
	const value = raw ?? 0;
	return (value === 0 ? STORED_UNIT_MS : value) as SlotDurationMs;
}

/**
 * The three `State` staging fields the live slot duration is resolved from.
 * Declared structurally rather than as `Pick<StateAccount, ...>`: importing
 * `StateAccount` closes the cycle `types.ts -> constants/numericConstants.ts ->
 * math/time.ts`, and `numericConstants` calls into this module at load time.
 * `StateAccount` satisfies this shape.
 */
export type SlotDurationState = {
	slotDurationMs?: number;
	pendingSlotDurationMs?: number;
	slotDurationEffectiveSlot?: BN;
};

/**
 * Anything holding a subscribed `State` account, e.g. `VelocityClient`. Kept
 * duck-typed so `math/time` keeps importing only `BN`.
 */
export type SlotDurationSource = {
	getStateAccount(): SlotDurationState;
};

/**
 * The slot length a client should convert with right now, plus whether it came
 * from live chain data. `isLive` is false whenever the fallback was used, so a
 * caller can degrade its UI or logging instead of presenting an assumption as
 * a measurement.
 */
export type SlotClock = {
	slotDurationMs: SlotDurationMs;
	isLive: boolean;
};

/**
 * The live slot duration at `currentSlot`, mirroring
 * `State::active_slot_duration_ms`: the staged `pendingSlotDurationMs` once
 * `currentSlot` reaches `slotDurationEffectiveSlot`, otherwise the base
 * `slotDurationMs`. Use this wherever a prediction must match the on-chain value
 * across a gate flip; `slotDurationFromState` alone would keep returning the
 * pre-switch value.
 */
export function activeSlotDurationFromState(
	state: SlotDurationState,
	currentSlot: BN
): SlotDurationMs {
	// Tolerate hand-built / older State objects that omit the staging fields:
	// an absent pending field means nothing is staged, not `undefined !== 0`.
	// `isBN` rather than `!== undefined` so a null/garbage effective slot reads
	// as "nothing staged" instead of throwing out of `gte`.
	const pending = state.pendingSlotDurationMs ?? 0;
	const effective = state.slotDurationEffectiveSlot;
	if (pending !== 0 && BN.isBN(effective) && currentSlot.gte(effective)) {
		return slotDurationFromState(pending);
	}
	return slotDurationFromState(state.slotDurationMs);
}

/**
 * Resolve the slot clock an off-chain client should convert with: the live
 * duration from `source`'s subscribed `State` at `currentSlot`, or the 400ms
 * {@link SLOT_DURATION_BASELINE} when state/slot is unavailable.
 *
 * `currentSlot` must be the live chain slot (e.g. `slotSubscriber.getSlot()`),
 * NOT the slot `State` was last written at. `State` does not change at the gate
 * boundary, so a cached State slot would never trigger the staged switch.
 * A missing or `0` slot is treated as a dead feed rather than as slot zero: a
 * failed slot subscription reports `0`, and slot zero precedes every effective
 * slot, so it would resolve to the pre-flip base while looking live.
 *
 * The fallback is the longest scheduled slot (400ms). Which direction that is
 * safe in depends on the conversion, not on the caller: converting **ms into
 * slots** (a staleness threshold, a rate limit, an auction duration) tightens,
 * converting **slots into ms** (a countdown, a cache TTL, a signing budget)
 * widens — up to 2x once the chain reaches 200ms. Callers in the widening
 * direction, and user-protection windows generally, should branch on `isLive`
 * and substitute {@link SLOT_DURATION_FLOOR} rather than consume
 * `slotDurationMs` blindly.
 */
export function currentSlotClock(
	source: SlotDurationSource,
	currentSlot: number | undefined
): SlotClock {
	const dead: SlotClock = {
		slotDurationMs: SLOT_DURATION_BASELINE,
		isLive: false,
	};

	// `0`, NaN, undefined and negatives are all dead feeds, not slot numbers.
	if (!currentSlot || !Number.isFinite(currentSlot) || currentSlot < 0) {
		return dead;
	}

	let state: SlotDurationState | undefined;
	try {
		state = source?.getStateAccount();
	} catch {
		// Not subscribed yet: the client throws rather than returning undefined.
		return dead;
	}

	// Validate, don't just test for presence. A duration only ever reaches this
	// module through Anchor decoding, but the staging fields are optional and
	// hand-built State is a supported input, so a partial object must not be
	// reported as a measurement: a NaN duration propagates silently through
	// every threshold comparison, and a non-BN effective slot throws out of
	// `BN.gte`. Both would surface deep in a filler loop instead of here.
	if (
		!state ||
		!isPlainSlotDuration(state.slotDurationMs) ||
		!isPlainSlotDuration(state.pendingSlotDurationMs) ||
		!BN.isBN(state.slotDurationEffectiveSlot)
	) {
		return dead;
	}

	return {
		slotDurationMs: activeSlotDurationFromState(state, new BN(currentSlot)),
		isLive: true,
	};
}

/** A decoded `u16` duration field: a non-negative integer (`0` = unset). */
function isPlainSlotDuration(raw: number | undefined): raw is number {
	return raw !== undefined && Number.isInteger(raw) && raw >= 0;
}

/**
 * The slot duration half of {@link currentSlotClock}, for call sites that do
 * not branch on liveness. See that function for the `currentSlot` rules.
 */
export function currentSlotDuration(
	source: SlotDurationSource,
	currentSlot: number | undefined
): SlotDurationMs {
	return currentSlotClock(source, currentSlot).slotDurationMs;
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

/**
 * `millisToSlotsCeil` for plain numbers. Use for durations/intervals the value
 * must not fall below (auction lengths, minimum pacing/cooldowns): flooring
 * would shorten them below the intended wall-clock time.
 */
export function msToSlotsCeilNum(ms: number, d: SlotDurationMs): number {
	return Math.ceil(ms / Math.max(1, d));
}

/** `millisFromSlots` for plain numbers (off-chain pacing/threshold code). */
export function slotsToMsNum(slots: number, d: SlotDurationMs): number {
	return slots * d;
}
