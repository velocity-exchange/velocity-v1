/**
 * Parity tests for `signedMsgEntryOrderRefusal`, transcribed from
 * `instructions/keeper/signed_msg/tests.rs`.
 */

import { assert } from 'chai';
import { signedMsgEntryOrderRefusal } from '../../src/math/orders';
import { OrderType, PostOnlyParams } from '../../src/types';

describe('signedMsgEntryOrderRefusal', () => {
	it('admits a market, limit or oracle entry', () => {
		for (const orderType of [
			OrderType.MARKET,
			OrderType.LIMIT,
			OrderType.ORACLE,
		]) {
			assert.isUndefined(
				signedMsgEntryOrderRefusal({ orderType, postOnly: PostOnlyParams.NONE })
			);
		}
	});

	it('refuses a post-only entry', () => {
		for (const postOnly of [
			PostOnlyParams.MUST_POST_ONLY,
			PostOnlyParams.TRY_POST_ONLY,
			PostOnlyParams.SLIDE,
		]) {
			assert.isString(
				signedMsgEntryOrderRefusal({ orderType: OrderType.LIMIT, postOnly })
			);
		}
	});

	it('refuses a trigger entry', () => {
		for (const orderType of [
			OrderType.TRIGGER_MARKET,
			OrderType.TRIGGER_LIMIT,
		]) {
			assert.isString(
				signedMsgEntryOrderRefusal({ orderType, postOnly: PostOnlyParams.NONE })
			);
		}
	});
});
