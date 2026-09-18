import { BN } from '../isomorphic/anchor';

/**
 * Wall-clock durations and the live slot length. This is the TypeScript mirror
 * of the program's `math/time.rs`.
 *
 * Solana's slot time drops from 400ms to 200ms through feature gates
 * (400, 350, 300, 250, 200). Slot counts and wall-clock durations therefore
 * carry separate branded types:
 *
 * - `Millis` is the only duration unit. Every threshold, window, ramp and
 *   grace period is a `Millis`. A comparison against a slot count must convert
 *   through the live slot length.
 * - `SlotDurationMs` is the current slot length from `State.slotDurationMs`.
 *   `slotDurationFromState` reads the 0 sentinel as the 400ms baseline.
 * - Plain numbers and BN stay the type of actual slot counts.
 *
 * Legacy admin-set onchain fields keep their compact encoding in units of
 * `STORED_UNIT_MS`, which is 400ms, the historical slot length. Decode them
 * with `millisFromStoredUnits`. That factor is a storage codec, not a unit to
 * think in. A new compact onchain field declares its encoding with Rust's
 * `StoredSlotDuration<T, SLOT_MS>` and normalizes to `Millis` for math.
 *
 * Rounding matches the program exactly. `millisToSlots` floors, which makes a
 * staleness window marginally tighter. That is the safe direction.
 * `millisToSlotsCeil` is for user-protection windows. `millisFromSlots` is
 * exact. `divPeriods` floors.
 *
 * `docs/SLOT-DURATION.md` holds the design rationale and worked examples.
 */

/**
 * Storage quantum for pre-gate duration fields, the historical 400ms slot
 * length. Use it only to decode those fields and legacy per-slot rates.
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
 * Every slot length in the current gate rollout, longest first.
 * Synchronization never lets the active duration move backward.
 */
export const SLOT_DURATION_SCHEDULE_MS: readonly SlotDurationMs[] = [
	400, 350, 300, 250, 200,
] as SlotDurationMs[];

/**
 * The shortest scheduled slot length, 200ms at full rollout. A user-protection
 * window assumes it when the slot feed is dead. That under-promises wall clock.
 */
export const SLOT_DURATION_FLOOR = SLOT_DURATION_SCHEDULE_MS[
	SLOT_DURATION_SCHEDULE_MS.length - 1
] as SlotDurationMs;

/**
 * The historical 400ms calibration period of the legacy per-slot rates. It
 * mirrors `Millis::UNIT`.
 */
export const MILLIS_UNIT = new BN(STORED_UNIT_MS) as Millis;

/**
 * A raw `0` is a pre-upgrade account and selects the 400ms baseline. The result
 * precedes any staged switch, unlike {@link activeSlotDurationFromState}.
 */
export function slotDurationFromState(raw?: number): SlotDurationMs {
	const value = raw ?? 0;
	return (value === 0 ? STORED_UNIT_MS : value) as SlotDurationMs;
}

/**
 * The four post baseline slot lengths in activation order, mirroring the
 * program constant. `State.slotDurationTransitionSlots` matches by index.
 */
export const SLOT_DURATION_TRANSITION_MS: readonly SlotDurationMs[] = [
	350, 300, 250, 200,
] as SlotDurationMs[];

/**
 * The `State` fields that resolve the live slot duration. A structural shape
 * avoids the `StateAccount` import cycle through `constants/numericConstants`.
 */
export type SlotDurationState = {
	slotDurationMs?: number;
	pendingSlotDurationMs?: number;
	slotDurationEffectiveSlot?: BN;
	/**
	 * First slot of each post baseline regime. Zero means that transition is
	 * not synchronized yet. A set entry outranks the legacy staging fields.
	 */
	slotDurationTransitionSlots?: BN[];
};

/**
 * A holder of a subscribed `State` account, such as `VelocityClient`. The type
 * stays duck-typed so `math/time` imports only `BN`.
 */
export type SlotDurationSource = {
	getStateAccount(): SlotDurationState;
};

/**
 * The slot length a client converts with now. `isLive` is false whenever the
 * fallback applied, so a caller never presents an assumption as a measurement.
 */
export type SlotClock = {
	slotDurationMs: SlotDurationMs;
	isLive: boolean;
};

/**
 * The live slot duration at `currentSlot`, mirroring
 * `State::active_slot_duration_ms`. It returns the staged
 * `pendingSlotDurationMs` once `currentSlot` reaches
 * `slotDurationEffectiveSlot`, and the base `slotDurationMs` otherwise. Use it
 * wherever a prediction must match the on-chain value across a gate flip.
 * `slotDurationFromState` alone keeps returning the pre-switch value.
 */
export function activeSlotDurationFromState(
	state: SlotDurationState,
	currentSlot: BN
): SlotDurationMs {
	// Once any transition archive entry exists, the archive is authoritative.
	// This mirrors `SlotClock::slot_duration_at`.
	const transitions = validTransitionSlots(state);
	if (transitions) {
		let duration = SLOT_DURATION_BASELINE;
		for (let i = 0; i < SLOT_DURATION_TRANSITION_MS.length; i++) {
			if (!transitions[i].isZero() && currentSlot.gte(transitions[i])) {
				duration = SLOT_DURATION_TRANSITION_MS[i];
			}
		}
		return duration;
	}

	// Tolerate a hand-built or older State object that omits the staging
	// fields. An absent pending field means nothing is staged. The check uses
	// `isBN` rather than `!== undefined`, so a null or garbage effective slot
	// reads as nothing staged instead of throwing out of `gte`.
	const pending = state.pendingSlotDurationMs ?? 0;
	const effective = state.slotDurationEffectiveSlot;
	if (pending !== 0 && BN.isBN(effective) && currentSlot.gte(effective)) {
		return slotDurationFromState(pending);
	}
	return slotDurationFromState(state.slotDurationMs);
}

const transitionSlotsCache = new WeakMap<
	SlotDurationState,
	{ source: BN[] | undefined; value: BN[] | undefined }
>();

/**
 * The transition archive when it is present, well formed and non empty.
 * Returns `undefined` for a hand built or pre upgrade State object, and when
 * no transition is synchronized yet. The legacy staging fields then apply.
 */
function validTransitionSlots(state: SlotDurationState): BN[] | undefined {
	const transitions = state.slotDurationTransitionSlots;
	const cached = transitionSlotsCache.get(state);
	if (cached && cached.source === transitions) {
		return cached.value;
	}

	if (
		!Array.isArray(transitions) ||
		transitions.length !== SLOT_DURATION_TRANSITION_MS.length ||
		!transitions.every((slot) => BN.isBN(slot) && !slot.isNeg())
	) {
		transitionSlotsCache.set(state, { source: transitions, value: undefined });
		return undefined;
	}
	const value = transitions.some((slot) => !slot.isZero())
		? transitions
		: undefined;
	transitionSlotsCache.set(state, { source: transitions, value });
	return value;
}

/**
 * Exact elapsed wall clock time from the start of `startSlot` to the start of
 * `endSlot`. It integrates every crossed slot duration regime separately and
 * mirrors the program's `SlotClock::elapsed`. Without a synchronized transition
 * archive, it prices the whole delta at the end slot duration. That is the
 * legacy staging behavior.
 */
export function elapsedMillis(
	state: SlotDurationState,
	startSlot: BN,
	endSlot: BN
): Millis {
	if (endSlot.lte(startSlot)) {
		return millis(0);
	}

	const transitions = validTransitionSlots(state);
	if (!transitions) {
		return millisFromSlots(
			endSlot.sub(startSlot),
			activeSlotDurationFromState(state, endSlot)
		);
	}

	let cursor = startSlot;
	let elapsed = new BN(0);
	let duration = activeSlotDurationFromState(state, startSlot);
	for (let i = 0; i < transitions.length; i++) {
		const transitionSlot = transitions[i];
		if (
			transitionSlot.isZero() ||
			transitionSlot.lte(cursor) ||
			transitionSlot.gt(endSlot)
		) {
			continue;
		}
		elapsed = elapsed.add(transitionSlot.sub(cursor).muln(duration));
		cursor = transitionSlot;
		duration = activeSlotDurationFromState(state, cursor);
	}
	elapsed = elapsed.add(endSlot.sub(cursor).muln(duration));
	return elapsed as Millis;
}

/**
 * Elapsed wall clock time represented by `slotDelta`, ending at `endSlot`. It
 * mirrors `SlotClock::elapsed_slot_delta`.
 */
export function elapsedMillisFromSlotDelta(
	state: SlotDurationState,
	slotDelta: BN,
	endSlot: BN
): Millis {
	return elapsedMillis(
		state,
		BN.max(endSlot.sub(slotDelta), new BN(0)),
		endSlot
	);
}

/**
 * First slot whose start is at least `duration` after `startSlot`. It
 * integrates known future transition boundaries. It mirrors
 * `SlotClock::slot_at_or_after_duration`.
 */
export function slotAtOrAfterDuration(
	state: SlotDurationState,
	startSlot: BN,
	duration: Millis
): BN {
	if (duration.isZero()) {
		return startSlot;
	}

	const transitions = validTransitionSlots(state);
	let cursor = startSlot;
	let remaining = duration;
	let currentDuration = activeSlotDurationFromState(state, cursor);

	for (const transitionSlot of transitions ?? []) {
		if (transitionSlot.isZero() || transitionSlot.lte(cursor)) {
			continue;
		}

		const regimeMillis = transitionSlot.sub(cursor).muln(currentDuration);
		if (remaining.lte(regimeMillis)) {
			return cursor.add(millisToSlotsCeil(remaining, currentDuration));
		}

		remaining = remaining.sub(regimeMillis) as Millis;
		cursor = transitionSlot;
		currentDuration = activeSlotDurationFromState(state, cursor);
	}

	return cursor.add(millisToSlotsCeil(remaining, currentDuration));
}

/**
 * Resolve the slot clock an off-chain client converts with. It returns the live
 * duration from the subscribed `State` of `source` at `currentSlot`, or the
 * 400ms {@link SLOT_DURATION_BASELINE} when the state or slot is unavailable.
 *
 * `currentSlot` must be the live chain slot, such as `slotSubscriber.getSlot()`,
 * not the slot `State` was last written at. `State` does not change at the gate
 * boundary, so a cached State slot never triggers the staged switch. A missing
 * or `0` slot counts as a dead feed, because a failed subscription reports `0`.
 *
 * The 400ms fallback tightens a ms-to-slots conversion and widens a slots-to-ms
 * one, by up to 2x once the chain reaches 200ms. A caller in the widening
 * direction should branch on `isLive` and substitute {@link SLOT_DURATION_FLOOR}.
 */
export function currentSlotClock(
	source: SlotDurationSource,
	currentSlot: number | undefined
): SlotClock {
	const dead: SlotClock = {
		slotDurationMs: SLOT_DURATION_BASELINE,
		isLive: false,
	};

	// `0`, NaN, undefined, a negative and a fraction are dead feeds rather than
	// slot numbers. `isSafeInteger` also keeps a garbage magnitude out of
	// `new BN`, which asserts above 2^53 rather than returning anything.
	if (!currentSlot || !Number.isSafeInteger(currentSlot) || currentSlot < 0) {
		return dead;
	}

	let state: SlotDurationState | undefined;
	try {
		state = source?.getStateAccount();
	} catch {
		// The client throws rather than returning undefined when it is not
		// subscribed yet.
		return dead;
	}

	// Validate the fields instead of testing for presence. Hand-built State is a
	// supported input, and a NaN duration or a non-BN effective slot would
	// surface deep in a filler loop. The duration is not checked against
	// SLOT_DURATION_SCHEDULE_MS, because the program's reader takes any u16.
	if (
		!state ||
		!isPlainSlotDuration(state.slotDurationMs) ||
		!isPlainSlotDuration(state.pendingSlotDurationMs) ||
		!BN.isBN(state.slotDurationEffectiveSlot) ||
		state.slotDurationEffectiveSlot.isNeg()
	) {
		return dead;
	}

	return {
		slotDurationMs: activeSlotDurationFromState(state, new BN(currentSlot)),
		isLive: true,
	};
}

/** A decoded `u16` duration field. It is a non-negative integer, `0` for unset. */
function isPlainSlotDuration(raw: number | undefined): raw is number {
	return raw !== undefined && Number.isInteger(raw) && raw >= 0;
}

/**
 * The slot duration half of {@link currentSlotClock}, for a call site that does
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
 * Decode a stored value denominated in `STORED_UNIT_MS` units. This is a
 * storage codec. Never use it for a new value.
 */
export function millisFromStoredUnits(units: BN | number): Millis {
	return new BN(units).muln(STORED_UNIT_MS) as Millis;
}

/**
 * The exact wall-clock time a measured slot delta represents at the current
 * slot duration. It mirrors `Millis::from_slots`.
 */
export function millisFromSlots(slots: BN, d: SlotDurationMs): Millis {
	return slots.muln(d) as Millis;
}

/**
 * A duration expressed in actual slots, rounded down. It mirrors
 * `Millis::to_slots`. This is the default for a staleness window.
 */
export function millisToSlots(m: Millis, d: SlotDurationMs): BN {
	return m.divn(Math.max(1, d));
}

/**
 * A duration expressed in actual slots, rounded up. It mirrors
 * `Millis::to_slots_ceil`. Use it for a user-protection window.
 */
export function millisToSlotsCeil(m: Millis, d: SlotDurationMs): BN {
	const dd = Math.max(1, d);
	return m.addn(dd - 1).divn(dd);
}

/**
 * How many whole `period`s fit in a duration, rounded down. It mirrors
 * `Millis::div_periods`. Use it for a legacy per-period rate.
 */
export function divPeriods(m: Millis, period: Millis): BN {
	return m.div(BN.max(new BN(1), period));
}

/** `millisToSlots` for plain numbers, used by off-chain pacing and thresholds. */
export function msToSlotsNum(ms: number, d: SlotDurationMs): number {
	return Math.floor(ms / Math.max(1, d));
}

/**
 * `millisToSlotsCeil` for plain numbers. Use it for a duration the value must
 * not fall below, such as an auction length or a minimum cooldown.
 */
export function msToSlotsCeilNum(ms: number, d: SlotDurationMs): number {
	return Math.ceil(ms / Math.max(1, d));
}

/** `millisFromSlots` for plain numbers, used by off-chain pacing and thresholds. */
export function slotsToMsNum(slots: number, d: SlotDurationMs): number {
	return slots * d;
}
