import { assert } from 'chai';
import { BN, OrderType } from '../../src';
import { SlotDurationState } from '../../src/math/time';
import {
	SIGNED_MSG_RESTING_LIMIT_MAX_LEAD_MS,
	isRestingSignedMsgLimitOrder,
	signedMsgOrderPlaceable,
	signedMsgOrderSlotReached,
} from '../../src/math/orders';

// Pins the signed-msg placement predicate against the slot gates in the program's
// `place_signed_msg_taker_order` (instructions/keeper.rs): a market or oracle order waits
// for its message slot. A resting limit may be placed ahead of it within the 30s lead bound.

// No transitions synchronized: every slot is the 400ms baseline.
const BASELINE_STATE: SlotDurationState = {};
// Production-scale slot numbers, so comparisons must go through BN.
const CURRENT_SLOT = 443_673_929;
const MAX_LEAD_SLOTS = SIGNED_MSG_RESTING_LIMIT_MAX_LEAD_MS / 400;

describe('signed-msg placement gate (program parity)', () => {
	it('classifies only a limit as a resting limit', () => {
		assert.isTrue(isRestingSignedMsgLimitOrder(OrderType.LIMIT));
		assert.isFalse(isRestingSignedMsgLimitOrder(OrderType.MARKET));
		assert.isFalse(isRestingSignedMsgLimitOrder(OrderType.ORACLE));
	});

	it('a market order waits for its message slot', () => {
		const stampedAhead = {
			slot: new BN(CURRENT_SLOT + 7),
			orderType: OrderType.MARKET,
		};
		assert.isFalse(
			signedMsgOrderPlaceable(BASELINE_STATE, stampedAhead, CURRENT_SLOT)
		);
		assert.isTrue(
			signedMsgOrderPlaceable(BASELINE_STATE, stampedAhead, CURRENT_SLOT + 7)
		);
		assert.isFalse(
			signedMsgOrderPlaceable(
				BASELINE_STATE,
				{ ...stampedAhead, orderType: OrderType.ORACLE },
				CURRENT_SLOT
			)
		);

		// Same answer as the slot-only predicate it wraps.
		assert.isFalse(signedMsgOrderSlotReached(stampedAhead.slot, CURRENT_SLOT));
	});

	it('a resting limit is placeable ahead of its message slot, within the window', () => {
		// The UI stamps a resting limit its whole signing budget (~14s) ahead.
		const restingLimit = {
			slot: new BN(CURRENT_SLOT + 35),
			orderType: OrderType.LIMIT,
		};
		assert.isTrue(
			signedMsgOrderPlaceable(BASELINE_STATE, restingLimit, CURRENT_SLOT)
		);
		// Exactly the window is still accepted; one slot more is refused.
		assert.isTrue(
			signedMsgOrderPlaceable(
				BASELINE_STATE,
				{ ...restingLimit, slot: new BN(CURRENT_SLOT + MAX_LEAD_SLOTS) },
				CURRENT_SLOT
			)
		);
		assert.isFalse(
			signedMsgOrderPlaceable(
				BASELINE_STATE,
				{ ...restingLimit, slot: new BN(CURRENT_SLOT + MAX_LEAD_SLOTS + 1) },
				CURRENT_SLOT
			)
		);
		// Behind the chain it is placeable like any other order.
		assert.isTrue(
			signedMsgOrderPlaceable(
				BASELINE_STATE,
				{ ...restingLimit, slot: new BN(CURRENT_SLOT - 3) },
				CURRENT_SLOT
			)
		);
	});

	it('converts the lead through the live slot duration', () => {
		// At 200ms slots the same 30s bound is 150 slots wide.
		const state: SlotDurationState = {
			slotDurationTransitionSlots: [new BN(1), new BN(2), new BN(3), new BN(4)],
		};
		const restingLimit = {
			slot: new BN(CURRENT_SLOT + 150),
			orderType: OrderType.LIMIT,
		};
		assert.isTrue(signedMsgOrderPlaceable(state, restingLimit, CURRENT_SLOT));
		assert.isFalse(
			signedMsgOrderPlaceable(
				state,
				{ ...restingLimit, slot: new BN(CURRENT_SLOT + 151) },
				CURRENT_SLOT
			)
		);
	});
});
