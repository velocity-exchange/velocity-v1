import { expect } from 'chai';
import {
	BN,
	SlotDurationState,
	signedMsgOrderSlotReached,
} from '@velocity-exchange/sdk';
import {
	shouldRefundSignedMsgFillAttempt,
	signedMsgFillInFlightTtlMs,
	txStatusProvesNoTransactionWasSent,
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
	it('leaves half the remaining validity for a retry', () => {
		// 20 stored units = 8s of auction, none of it elapsed: expire at 4s so a
		// rebuilt tx still has 4s of auction to land in.
		expect(
			signedMsgFillInFlightTtlMs(
				BASELINE_STATE,
				new BN(CURRENT_SLOT),
				20,
				CURRENT_SLOT
			)
		).to.equal(4_000);
	});

	it('counts the auction from the message slot, not from now', () => {
		// Stamped 7 slots ahead: the window runs to (message slot + 20), so 27
		// actual slots (10.8s) of validity remain.
		expect(
			signedMsgFillInFlightTtlMs(
				BASELINE_STATE,
				new BN(CURRENT_SLOT + 7),
				20,
				CURRENT_SLOT
			)
		).to.equal(5_400);
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
		// Longest encodable auction (u8 stored units, ~102s); half of that is still
		// far beyond the window a dropped tx is worth waiting out.
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

describe('signed-msg terminal fill state', () => {
	it('keeps the reservation after an ambiguous send error', () => {
		expect(txStatusProvesNoTransactionWasSent('send_error')).to.be.false;
		expect(txStatusProvesNoTransactionWasSent('build_error')).to.be.true;
		expect(txStatusProvesNoTransactionWasSent('sim_rpc_error')).to.be.true;
	});

	it('refunds only the slot-ahead simulation failure', () => {
		expect(shouldRefundSignedMsgFillAttempt('sim_failed', 6288)).to.be.true;
		expect(shouldRefundSignedMsgFillAttempt('sim_failed', 6001)).to.be.false;
		expect(shouldRefundSignedMsgFillAttempt('send_error', 6288)).to.be.false;
	});
});
