import { expect } from 'chai';
import {
	BN,
	OrderType,
	SlotDurationState,
	signedMsgOrderPlaceable,
	signedMsgOrderSlotReached,
} from '@velocity-exchange/sdk';
import {
	MAX_SIGNED_MSG_ATTEMPT_REFUNDS,
	refundedLastAttemptSlot,
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

describe('signedMsgOrderPlaceable', () => {
	it('holds an auction order until its message slot, like the slot gate', () => {
		const auctionOrder = {
			slot: new BN(CURRENT_SLOT + 7),
			orderType: OrderType.MARKET,
			auctionDuration: 20,
		};
		expect(signedMsgOrderPlaceable(BASELINE_STATE, auctionOrder, CURRENT_SLOT))
			.to.be.false;
		expect(
			signedMsgOrderPlaceable(BASELINE_STATE, auctionOrder, CURRENT_SLOT + 7)
		).to.be.true;
	});

	it('places a resting limit ahead of its message slot', () => {
		// The UI stamps a no-auction limit its whole signing budget (~14s) ahead: the
		// program treats that slot as the placement deadline and places before it.
		const restingLimit = {
			slot: new BN(CURRENT_SLOT + 35),
			orderType: OrderType.LIMIT,
			auctionDuration: null,
		};
		expect(signedMsgOrderPlaceable(BASELINE_STATE, restingLimit, CURRENT_SLOT))
			.to.be.true;
		// but not one stamped past the program's ~200s window (500 baseline slots)
		expect(
			signedMsgOrderPlaceable(
				BASELINE_STATE,
				{ ...restingLimit, slot: new BN(CURRENT_SLOT + 501) },
				CURRENT_SLOT
			)
		).to.be.false;
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

describe('refundedLastAttemptSlot', () => {
	it('reopens the pacing gate one slot after the failed attempt', () => {
		// Gate: currentSlot - lastAttemptSlot < pacingSlots. 2000ms at 400ms
		// baseline = 5 pacing slots; a rewind of 4 makes the very next slot pass.
		const rewound = refundedLastAttemptSlot(CURRENT_SLOT, 5);
		expect(rewound).to.equal(CURRENT_SLOT - 4);
		expect(CURRENT_SLOT - rewound < 5).to.be.true;
		expect(CURRENT_SLOT + 1 - rewound < 5).to.be.false;
	});

	it('never rewinds past the attempt slot at 1-slot pacing', () => {
		expect(refundedLastAttemptSlot(CURRENT_SLOT, 1)).to.equal(CURRENT_SLOT);
		expect(refundedLastAttemptSlot(CURRENT_SLOT, 0)).to.equal(CURRENT_SLOT);
	});

	it('caps refunds so a persistent slot-ahead failure stays bounded', () => {
		// Not a behavior test (the cap lives in refundFillAttempt); pin the
		// constant so a change to it is a deliberate diff.
		expect(MAX_SIGNED_MSG_ATTEMPT_REFUNDS).to.equal(5);
	});
});
