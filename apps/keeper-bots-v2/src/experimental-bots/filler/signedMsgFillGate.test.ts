import { expect } from 'chai';
import { BN, SlotDurationState } from '@velocity-exchange/sdk';
import {
	signedMsgFillInFlightTtlMs,
	signedMsgOrderSlotReached,
} from './fillerMultithreaded';

// A State with no slot-duration transitions synchronized: every slot is the
// 400ms baseline, so a slot count converts to ms by a factor of 400.
const BASELINE_STATE: SlotDurationState = {};

// Production-scale slot numbers. Anything derived from them must go through BN
// comparisons: `BN.gtn`/`BN.lten` assert their argument fits in 26 bits.
const CURRENT_SLOT = 443_184_694;

describe('signedMsgOrderSlotReached', () => {
	it('defers an order stamped ahead of the current slot', () => {
		// The UI's signing buffer: the incident order was stamped +2 at the moment
		// the filler evaluated it.
		expect(signedMsgOrderSlotReached(new BN(CURRENT_SLOT + 2), CURRENT_SLOT)).to
			.be.false;
		expect(signedMsgOrderSlotReached(new BN(CURRENT_SLOT + 7), CURRENT_SLOT)).to
			.be.false;
	});

	it('allows an order whose message slot has arrived', () => {
		expect(signedMsgOrderSlotReached(new BN(CURRENT_SLOT), CURRENT_SLOT)).to.be
			.true;
		expect(signedMsgOrderSlotReached(new BN(CURRENT_SLOT - 5), CURRENT_SLOT)).to
			.be.true;
	});
});

describe('signedMsgFillInFlightTtlMs', () => {
	it('reserves for the wall clock left in the order auction', () => {
		// 20 stored units = 8s of auction, none of it elapsed.
		expect(
			signedMsgFillInFlightTtlMs(
				BASELINE_STATE,
				new BN(CURRENT_SLOT),
				20,
				CURRENT_SLOT
			)
		).to.equal(8_000);
	});

	it('counts the auction from the message slot, not from now', () => {
		// Stamped 7 slots ahead: the window runs to (message slot + 20), so 27
		// actual slots of validity remain.
		expect(
			signedMsgFillInFlightTtlMs(
				BASELINE_STATE,
				new BN(CURRENT_SLOT + 7),
				20,
				CURRENT_SLOT
			)
		).to.equal(10_800);
	});

	it('floors the reservation so it outlasts confirmation latency', () => {
		// Two slots of validity left (800ms), still long enough that a second
		// place+fill must not be launched for a tx that may be about to land.
		expect(
			signedMsgFillInFlightTtlMs(
				BASELINE_STATE,
				new BN(CURRENT_SLOT - 18),
				20,
				CURRENT_SLOT
			)
		).to.equal(3_000);
	});

	it('never reserves past the drop-detection window', () => {
		// Longest encodable auction (u8 stored units, ~102s).
		expect(
			signedMsgFillInFlightTtlMs(
				BASELINE_STATE,
				new BN(CURRENT_SLOT),
				255,
				CURRENT_SLOT
			)
		).to.equal(15_000);
	});
});
