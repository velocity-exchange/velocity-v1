import { assert } from 'chai';
import { BN } from '../../src';
import {
	STORED_UNIT_MS,
	SLOT_DURATION_BASELINE,
	MILLIS_UNIT,
	slotDurationFromState,
	activeSlotDurationFromState,
	millis,
	millisFromSecs,
	millisFromStoredUnits,
	millisFromSlots,
	millisToSlots,
	millisToSlotsCeil,
	divPeriods,
	msToSlotsNum,
	msToSlotsCeilNum,
	slotsToMsNum,
	currentSlotDuration,
	currentSlotClock,
	SlotDurationState,
} from '../../src/math/time';

// Pins the TypeScript time helpers against the Rust `math/time.rs` unit tests
// (same cases, same expected numbers). If these drift, the SDK mispredicts the
// program at reduced slot times.
describe('slot-time helpers (program parity)', () => {
	const gate = (ms: number) => slotDurationFromState(ms);

	it('baseline is identity on stored units', () => {
		for (const v of [0, 1, 10, 120, 1500, 18144000]) {
			const m = millisFromStoredUnits(v);
			assert.equal(millisToSlots(m, SLOT_DURATION_BASELINE).toNumber(), v);
			assert.equal(millisToSlotsCeil(m, SLOT_DURATION_BASELINE).toNumber(), v);
			assert.equal(
				divPeriods(
					millisFromSlots(new BN(v), SLOT_DURATION_BASELINE),
					MILLIS_UNIT
				).toNumber(),
				v
			);
		}
	});

	it('0 slotDurationMs resolves to the 400ms baseline', () => {
		assert.equal(slotDurationFromState(0), STORED_UNIT_MS);
		assert.equal(slotDurationFromState(undefined), STORED_UNIT_MS);
		assert.equal(slotDurationFromState(0), SLOT_DURATION_BASELINE);
		assert.equal(slotDurationFromState(200), 200);
	});

	it('staged duration switches exactly at the effective slot', () => {
		const state = {
			slotDurationMs: 350,
			pendingSlotDurationMs: 300,
			slotDurationEffectiveSlot: new BN(1_000),
		};
		assert.equal(activeSlotDurationFromState(state, new BN(999)), 350);
		assert.equal(activeSlotDurationFromState(state, new BN(1_000)), 300);
		assert.equal(activeSlotDurationFromState(state, new BN(1_001)), 300);
	});

	it('older State objects without staging fields resolve safely', () => {
		assert.equal(activeSlotDurationFromState({}, new BN(1_000)), 400);
		assert.equal(
			activeSlotDurationFromState({ slotDurationMs: 350 }, new BN(1_000)),
			350
		);
	});

	it('terminal 200ms gate doubles slot counts', () => {
		const d = gate(200);
		assert.equal(millisToSlots(millisFromSecs(4), d).toNumber(), 20);
		assert.equal(millisToSlotsCeil(millisFromSecs(60), d).toNumber(), 300);
		assert.equal(
			divPeriods(millisFromSlots(new BN(20), d), MILLIS_UNIT).toNumber(),
			10
		);
	});

	it('intermediate gates round exactly as the program does', () => {
		// 4s: 4000/350 = 11.43, 4000/300 = 13.33, 4000/250 = 16 exact
		assert.equal(millisToSlots(millisFromSecs(4), gate(350)).toNumber(), 11);
		assert.equal(
			millisToSlotsCeil(millisFromSecs(4), gate(350)).toNumber(),
			12
		);
		assert.equal(millisToSlots(millisFromSecs(4), gate(300)).toNumber(), 13);
		assert.equal(millisToSlots(millisFromSecs(4), gate(250)).toNumber(), 16);
		// 3 slots at 200ms = 600ms = 1 whole 400ms period
		assert.equal(
			divPeriods(millisFromSlots(new BN(3), gate(200)), MILLIS_UNIT).toNumber(),
			1
		);
	});

	it('millisToSlotsCeil equals floor on exact multiples', () => {
		// 8s at 200ms = 40 slots exactly; ceil must not add one
		assert.equal(millisToSlots(millisFromSecs(8), gate(200)).toNumber(), 40);
		assert.equal(
			millisToSlotsCeil(millisFromSecs(8), gate(200)).toNumber(),
			40
		);
	});

	it('number-domain helpers match the BN ones', () => {
		assert.equal(msToSlotsNum(4000, gate(200)), 20);
		assert.equal(msToSlotsNum(4000, gate(350)), 11);
		assert.equal(slotsToMsNum(20, gate(200)), 4000);
	});

	it('msToSlotsCeilNum rounds up (auction/pacing durations)', () => {
		// floor would shorten these below the intended wall-clock time
		assert.equal(msToSlotsCeilNum(4000, gate(350)), 12); // ceil(4000/350)=12 vs floor 11
		assert.equal(msToSlotsCeilNum(8000, gate(350)), 23); // ceil(8000/350)=23 vs floor 22
		// exact multiples: ceil == floor
		assert.equal(msToSlotsCeilNum(4000, gate(200)), 20);
		assert.equal(msToSlotsCeilNum(8000, gate(200)), 40);
	});

	it('millis constructors', () => {
		assert.equal(millis(800).toNumber(), 800);
		assert.equal(millisFromSecs(2).toNumber(), 2000);
		assert.equal(millisFromStoredUnits(10).toNumber(), 4000);
	});
});

// The off-chain resolver every TypeScript client converts through. A dead slot
// feed must not read as slot zero, and an unavailable State falls back to the
// hardcoded 400ms baseline.
describe('currentSlotDuration (off-chain resolver)', () => {
	const GATES = [400, 350, 300, 250, 200];

	const source = (state?: SlotDurationState) => ({
		getStateAccount: () => {
			if (!state) {
				throw new Error('state not subscribed');
			}
			return state;
		},
	});

	const stateAt = (ms: number): SlotDurationState => ({
		slotDurationMs: ms,
		pendingSlotDurationMs: 0,
		slotDurationEffectiveSlot: new BN(0),
	});

	it('resolves the live duration at every gate', () => {
		for (const ms of GATES) {
			assert.equal(currentSlotDuration(source(stateAt(ms)), 5_000), ms);
			assert.isTrue(currentSlotClock(source(stateAt(ms)), 5_000).isLive);
		}
	});

	it('an unset (0) slotDurationMs resolves to the 400ms baseline', () => {
		const clock = currentSlotClock(source(stateAt(0)), 5_000);
		assert.equal(clock.slotDurationMs, SLOT_DURATION_BASELINE);
		assert.isTrue(clock.isLive);
	});

	it('a missing or 0 slot is a dead feed, not slot zero', () => {
		// A failed slot subscription reports 0, and slot 0 precedes every
		// effective slot, so it would otherwise resolve to the pre-flip base.
		const state: SlotDurationState = {
			slotDurationMs: 400,
			pendingSlotDurationMs: 200,
			slotDurationEffectiveSlot: new BN(1_000),
		};
		for (const slot of [0, undefined]) {
			const clock = currentSlotClock(source(state), slot);
			assert.equal(clock.slotDurationMs, SLOT_DURATION_BASELINE);
			assert.isFalse(clock.isLive);
		}
	});

	it('falls back to the baseline when state is not subscribed', () => {
		assert.equal(currentSlotDuration(source(), 5_000), SLOT_DURATION_BASELINE);
	});

	it('falls back when State predates the staging fields', () => {
		const clock = currentSlotClock(source({ slotDurationMs: 350 }), 5_000);
		assert.equal(clock.slotDurationMs, SLOT_DURATION_BASELINE);
		assert.isFalse(clock.isLive);
	});

	it('applies a staged flip on the effective slot, at every gate step', () => {
		const steps: Array<[number, number]> = [
			[400, 350],
			[350, 300],
			[300, 250],
			[250, 200],
		];
		for (const [base, pending] of steps) {
			const state: SlotDurationState = {
				slotDurationMs: base,
				pendingSlotDurationMs: pending,
				slotDurationEffectiveSlot: new BN(1_000),
			};
			assert.equal(currentSlotDuration(source(state), 999), base);
			assert.equal(currentSlotDuration(source(state), 1_000), pending);
			assert.equal(currentSlotDuration(source(state), 1_001), pending);
		}
	});

	it('holds a wall-clock rule across every gate', () => {
		// 4s of grace stays ~4s in actual slots at every slot duration
		for (const ms of GATES) {
			const d = currentSlotDuration(source(stateAt(ms)), 5_000);
			const slots = msToSlotsCeilNum(4_000, d);
			assert.isAtLeast(slotsToMsNum(slots, d), 4_000);
			assert.isBelow(slotsToMsNum(slots, d), 4_000 + ms);
		}
	});
});
