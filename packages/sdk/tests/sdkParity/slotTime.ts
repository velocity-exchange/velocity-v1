import { assert } from 'chai';
import { BN } from '../../src';
import {
	STORED_UNIT_MS,
	SLOT_DURATION_BASELINE,
	SLOT_DURATION_SCHEDULE_MS,
	SLOT_DURATION_FLOOR,
	MILLIS_UNIT,
	slotDurationFromState,
	activeSlotDurationFromState,
	elapsedMillis,
	elapsedMillisFromSlotDelta,
	slotAtOrAfterDuration,
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

	it('transition archive is authoritative over the legacy staging fields', () => {
		// mirrors SlotClock::slot_duration_at: stale legacy fields lose
		const state: SlotDurationState = {
			slotDurationMs: 200,
			pendingSlotDurationMs: 250,
			slotDurationEffectiveSlot: new BN(5),
			slotDurationTransitionSlots: [
				new BN(1_000),
				new BN(2_000),
				new BN(0),
				new BN(0),
			],
		};
		assert.equal(activeSlotDurationFromState(state, new BN(999)), 400);
		assert.equal(activeSlotDurationFromState(state, new BN(1_000)), 350);
		assert.equal(activeSlotDurationFromState(state, new BN(1_999)), 350);
		assert.equal(activeSlotDurationFromState(state, new BN(2_000)), 300);
		assert.equal(activeSlotDurationFromState(state, new BN(1_000_000)), 300);
	});

	it('elapsedMillis integrates each slot-duration regime (SlotClock::elapsed parity)', () => {
		const state: SlotDurationState = {
			slotDurationTransitionSlots: [
				new BN(1_000),
				new BN(2_000),
				new BN(3_000),
				new BN(4_000),
			],
		};
		// fully inside one regime
		assert.equal(
			elapsedMillis(state, new BN(0), new BN(10)).toNumber(),
			10 * 400
		);
		assert.equal(
			elapsedMillis(state, new BN(4_000), new BN(4_010)).toNumber(),
			10 * 200
		);
		// spanning one transition: 10 slots at 400ms + 10 at 350ms
		assert.equal(
			elapsedMillis(state, new BN(990), new BN(1_010)).toNumber(),
			10 * 400 + 10 * 350
		);
		// spanning every transition
		assert.equal(
			elapsedMillis(state, new BN(0), new BN(5_000)).toNumber(),
			1_000 * (400 + 350 + 300 + 250 + 200)
		);
		// degenerate intervals are zero
		assert.equal(elapsedMillis(state, new BN(10), new BN(10)).toNumber(), 0);
		assert.equal(elapsedMillis(state, new BN(20), new BN(10)).toNumber(), 0);
		// the delta form anchors at the end slot and saturates at slot zero
		assert.equal(
			elapsedMillisFromSlotDelta(state, new BN(20), new BN(1_010)).toNumber(),
			10 * 400 + 10 * 350
		);
		assert.equal(
			elapsedMillisFromSlotDelta(state, new BN(100), new BN(50)).toNumber(),
			50 * 400
		);
	});

	it('projects duration across a known future transition', () => {
		const state: SlotDurationState = {
			slotDurationTransitionSlots: [
				new BN(1_000),
				new BN(2_000),
				new BN(0),
				new BN(0),
			],
		};
		assert.equal(
			slotAtOrAfterDuration(state, new BN(995), millisFromSecs(4)).toNumber(),
			1_006
		);
	});

	it('elapsedMillis without an archive prices the delta at the end-slot duration', () => {
		const staged: SlotDurationState = {
			slotDurationMs: 400,
			pendingSlotDurationMs: 350,
			slotDurationEffectiveSlot: new BN(1_000),
		};
		assert.equal(
			elapsedMillis(staged, new BN(0), new BN(10)).toNumber(),
			10 * 400
		);
		assert.equal(
			elapsedMillis(staged, new BN(1_000), new BN(1_010)).toNumber(),
			10 * 350
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
	// The mirrored program schedule, not a local copy of it.
	const GATES = SLOT_DURATION_SCHEDULE_MS;

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
		const clock = currentSlotClock(source(), 5_000);
		assert.equal(clock.slotDurationMs, SLOT_DURATION_BASELINE);
		assert.isFalse(clock.isLive);
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

	it('never reports a malformed duration as a measurement', () => {
		// A NaN duration would propagate silently through msToSlotsNum and make
		// every threshold comparison false, so it must not come back isLive.
		const malformed: Array<Partial<SlotDurationState>> = [
			{ slotDurationMs: NaN },
			{ pendingSlotDurationMs: NaN },
			{ slotDurationMs: -200 },
			{ pendingSlotDurationMs: -200 },
			{ slotDurationMs: 250.5 },
			{ slotDurationEffectiveSlot: undefined },
			{ slotDurationEffectiveSlot: null as unknown as BN },
			{ slotDurationEffectiveSlot: 1_000 as unknown as BN },
			{ slotDurationEffectiveSlot: new BN(-1) },
		];
		for (const override of malformed) {
			const clock = currentSlotClock(
				source({ ...stateAt(250), ...override }),
				5_000
			);
			assert.equal(clock.slotDurationMs, SLOT_DURATION_BASELINE);
			assert.isFalse(clock.isLive, JSON.stringify(override));
		}
	});

	it('does not throw on a staged flip with a garbage effective slot', () => {
		// pending != 0 reaches the BN comparison, where a non-BN used to throw
		// straight out of the resolver and into the caller's loop.
		const state = {
			slotDurationMs: 250,
			pendingSlotDurationMs: 200,
			slotDurationEffectiveSlot: null as unknown as BN,
		};
		assert.equal(
			currentSlotDuration(source(state), 5_000),
			SLOT_DURATION_BASELINE
		);
		// The underlying primitive reads it as "nothing staged", not an error.
		assert.equal(activeSlotDurationFromState(state, new BN(5_000)), 250);
	});

	it('accepts an off-schedule duration, as the program does', () => {
		// The program's reader takes any u16; only the setter enforces the
		// schedule. Rejecting 201 or 275 here would return 400 where the chain
		// returns the stored value, the mirror divergence this SDK must not
		// have. Off-schedule values are not reachable through the setter, but
		// the resolver must not be stricter than the reader it mirrors.
		for (const ms of [201, 275, 399, 65_535]) {
			const clock = currentSlotClock(source(stateAt(ms)), 5_000);
			assert.equal(clock.slotDurationMs, ms);
			assert.isTrue(clock.isLive);
		}
	});

	it('applies a staged flip at a realistic mainnet effective slot', () => {
		// The effective slot is a chain slot read from the IBRL feature gate,
		// not a duration: validating it against the duration schedule would
		// reject every real staging and make the flip unreachable.
		const state: SlotDurationState = {
			slotDurationMs: 250,
			pendingSlotDurationMs: 200,
			slotDurationEffectiveSlot: new BN(372_000_000),
		};
		assert.equal(currentSlotDuration(source(state), 371_999_999), 250);
		assert.equal(currentSlotDuration(source(state), 372_000_000), 200);
	});

	it('treats a negative, fractional or non-finite slot as a dead feed', () => {
		// 1_000.5 truncates in `new BN`, and anything past 2^53 asserts, so
		// neither may reach it.
		for (const slot of [-5, NaN, Infinity, 1_000.5, 2 ** 53]) {
			const clock = currentSlotClock(source(stateAt(200)), slot);
			assert.equal(clock.slotDurationMs, SLOT_DURATION_BASELINE);
			assert.isFalse(clock.isLive, String(slot));
		}
	});

	it('the floor is the shortest scheduled slot', () => {
		// What a user-protection window substitutes when isLive is false: the
		// baseline would promise up to 2x the wall clock actually available.
		assert.equal(SLOT_DURATION_FLOOR, Math.min(...GATES));
		assert.equal(SLOT_DURATION_FLOOR, 200);
		assert.equal(GATES[0], SLOT_DURATION_BASELINE);
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

// Pins the auction mirrors against the program's wall clock auction math:
// `Order.auctionDuration` is 400ms units and progress integrates elapsed
// slots through the slot clock (`auction_wall_clock_across_gates` in
// `math/auction.rs` tests).
describe('auction wall-clock across gates (program parity)', () => {
	// deferred import to avoid a cycle at module load
	// eslint-disable-next-line @typescript-eslint/no-var-requires
	const {
		getAuctionPrice,
		isAuctionComplete,
	} = require('../../src/math/auction');
	// eslint-disable-next-line @typescript-eslint/no-var-requires
	const { OrderType, PositionDirection } = require('../../src/types');
	const PRICE = new BN(1_000_000);

	const order = () => ({
		orderType: OrderType.MARKET,
		direction: PositionDirection.LONG,
		auctionDuration: 10, // 4s in 400ms units
		slot: new BN(1_000),
		auctionStartPrice: new BN(100).mul(PRICE),
		auctionEndPrice: new BN(110).mul(PRICE),
		price: new BN(0),
		oraclePriceOffset: new BN(0),
		bitFlags: 0,
	});

	const clock200: SlotDurationState = {
		slotDurationTransitionSlots: [new BN(1), new BN(1), new BN(1), new BN(1)],
	};

	it('interpolation holds its wall-clock shape at 200ms', () => {
		const o = order();
		// 10 slots at 200ms = 2s = halfway through the 4s ramp
		assert.equal(
			getAuctionPrice(o, 1_010, new BN(0), undefined, clock200).toString(),
			new BN(105).mul(PRICE).toString()
		);
		// 20 slots = 4s = the end
		assert.equal(
			getAuctionPrice(o, 1_020, new BN(0), undefined, clock200).toString(),
			new BN(110).mul(PRICE).toString()
		);
		// baseline identity: 5 slots at 400ms = 2s = halfway
		assert.equal(
			getAuctionPrice(o, 1_005, new BN(0), undefined, {}).toString(),
			new BN(105).mul(PRICE).toString()
		);
	});

	it('completion holds its wall-clock length at 200ms', () => {
		const o = order();
		assert.isFalse(isAuctionComplete(o, 1_020, clock200));
		assert.isTrue(isAuctionComplete(o, 1_021, clock200));
		// baseline: 10 slots at 400ms is exactly the 4s length
		assert.isFalse(isAuctionComplete(o, 1_010, {}));
		assert.isTrue(isAuctionComplete(o, 1_011, {}));
	});
});
