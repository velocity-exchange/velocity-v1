/**
 * Parity tests for `signedMsgOrderMaxSlot`, transcribed from
 * `state/signed_msg_user/tests.rs`.
 */

import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { signedMsgOrderMaxSlot } from '../../src/math/orders';
import { slotAtOrAfterDuration, millis } from '../../src/math/time';

describe('signedMsgOrderMaxSlot', () => {
	const state = { slotDurationMs: 400 };

	it('a resting limit is placeable only until its stamp', () => {
		assert(signedMsgOrderMaxSlot(state, new BN(100), true).eqn(100));
	});

	it('a taker order gets the fill window past its stamp', () => {
		const maxSlot = signedMsgOrderMaxSlot(state, new BN(100), false);
		assert(
			maxSlot.eq(slotAtOrAfterDuration(state, new BN(100), millis(30_000)))
		);
		assert(maxSlot.gtn(100));
	});
});
