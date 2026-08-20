import { assert } from 'chai';
import { BN } from '../../src';
import {
	STORED_UNIT_MS,
	SLOT_DURATION_BASELINE,
	MILLIS_UNIT,
	slotDurationFromState,
	millis,
	millisFromSecs,
	millisFromStoredUnits,
	millisFromSlots,
	millisToSlots,
	millisToSlotsCeil,
	divPeriods,
	msToSlotsNum,
	slotsToMsNum,
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
		assert.equal(slotDurationFromState(0), SLOT_DURATION_BASELINE);
		assert.equal(slotDurationFromState(200), 200);
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

	it('millis constructors', () => {
		assert.equal(millis(800).toNumber(), 800);
		assert.equal(millisFromSecs(2).toNumber(), 2000);
		assert.equal(millisFromStoredUnits(10).toNumber(), 4000);
	});
});
