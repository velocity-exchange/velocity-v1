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
// comparisons, because `BN.gtn` and `BN.lten` assert that their argument fits in
// 26 bits.
const CURRENT_SLOT = 443_184_694;

describe('signedMsgOrderSlotReached', () => {
	it('defers an order stamped ahead of the current slot', () => {
		// The UI's signing buffer. The incident order was stamped two slots ahead
		// at the moment the filler evaluated it.
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
		// The UI stamps a no-auction limit its whole signing budget ahead, which is
		// about 14 seconds. The program treats that slot as the placement deadline
		// and places the order before it.
		const restingLimit = {
			slot: new BN(CURRENT_SLOT + 35),
			orderType: OrderType.LIMIT,
			auctionDuration: null,
		};
		expect(signedMsgOrderPlaceable(BASELINE_STATE, restingLimit, CURRENT_SLOT))
			.to.be.true;
		// The program bounds the lead at 30 seconds, which is 75 baseline slots.
		expect(
			signedMsgOrderPlaceable(
				BASELINE_STATE,
				{ ...restingLimit, slot: new BN(CURRENT_SLOT + 76) },
				CURRENT_SLOT
			)
		).to.be.false;
	});
});

describe('signedMsgFillInFlightTtlMs', () => {
	it('leaves half the remaining validity for a retry', () => {
		// 20 stored units is 8 seconds of auction, and none of it has elapsed. The
		// reservation expires at 4 seconds, so a rebuilt transaction still has 4
		// seconds of auction to land in.
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
		// The order is stamped 7 slots ahead. The window runs to the message slot
		// plus 20, so 27 actual slots, or 10.8 seconds, of validity remain.
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
		// The longest encodable auction is a u8 of stored units, about 102 seconds.
		// Half of that is still far beyond the window a dropped transaction is
		// worth waiting out.
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
		// The gate is currentSlot - lastAttemptSlot < pacingSlots. 2000ms at the
		// 400ms baseline is 5 pacing slots, so a rewind of 4 makes the next slot
		// pass.
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
		// This is not a behavior test, because the cap lives in refundFillAttempt.
		// It pins the constant so that a change to it shows up in a diff.
		expect(MAX_SIGNED_MSG_ATTEMPT_REFUNDS).to.equal(5);
	});
});
