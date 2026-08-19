import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import * as _ from 'lodash';
import { QUOTE_PRECISION, User, ZERO } from '../../src';
import { mockPerpMarkets, mockSpotMarkets } from '../dlob/helpers';
import { makeMockUser, mockUserAccount } from './helpers';

const quote = (n: number) => new BN(n).mul(QUOTE_PRECISION);

/**
 * The floor consumers fail closed on the oracle-validity verdict, in the
 * direction of the decision they make. Being below the floor RESTRICTS the
 * account almost everywhere, so those consumers treat an invalid oracle the
 * same as a breach and a bad price cannot authorize an action through the
 * floor. `force_cancel_orders` is the exception: there being below the floor
 * AUTHORIZES a keeper against the account, so it requires every oracle to be
 * valid before the floor counts as grounds at all.
 *
 * Getting either direction backwards silently reopens the hole the onchain
 * change closed, and no existing test would catch it.
 */
async function userWithFloor(floor: BN, buffer: BN): Promise<User> {
	const account = _.cloneDeep(mockUserAccount);
	account.equityFloor = floor;
	account.equityFloorBuffer = buffer;
	return makeMockUser(
		_.cloneDeep(mockPerpMarkets),
		_.cloneDeep(mockSpotMarkets),
		account,
		[1, 1, 1, 1, 1, 1, 1, 1],
		[1, 1, 1, 1, 1, 1, 1, 1]
	);
}

describe('equity floor fails closed on oracle validity', () => {
	it('no floor set means no predicate fires and no headroom is reported', async () => {
		const user = await userWithFloor(ZERO, ZERO);
		assert.isFalse(user.isBelowEquityFloor());
		assert.isFalse(user.isBelowEquityFloor(new BN(0)));
		assert.isNull(user.getEquityAboveFloor());
		assert.isNull(user.getEquityAboveBufferedFloor());
		assert.isFalse(user.isForceCancelAuthorizedByEquityFloor(new BN(0)));
	});

	it('agrees with the point value while every oracle is valid', async () => {
		const user = await userWithFloor(quote(1), ZERO);
		// Valid oracles make the floor metric the exact net usd value, so
		// passing a slot must not change any answer.
		const netEquity = user.getFloorNetEquity(new BN(0));
		assert.isTrue(netEquity.value.eq(user.getNetUsdValue()));
		assert.isTrue(netEquity.allOraclesValid);
		assert.equal(user.isBelowEquityFloor(), user.isBelowEquityFloor(new BN(0)));
	});

	it('reports headroom the way the gates measure it', async () => {
		const user = await userWithFloor(quote(1), quote(1));
		const withSlot = user.getEquityAboveBufferedFloor(new BN(0));
		const withoutSlot = user.getEquityAboveBufferedFloor();
		assert.isNotNull(withSlot);
		assert.isNotNull(withoutSlot);
		// Equal while oracles are valid; the slot form is the one that stays
		// correct when they are not.
		assert.isTrue(withSlot!.eq(withoutSlot!));
		// Headroom is floored at zero rather than reported negative.
		assert.isTrue(withSlot!.gte(ZERO));
	});

	it('an invalid oracle blocks the gates and reports zero headroom', async () => {
		const user = await userWithFloor(quote(1), ZERO);
		const slot = new BN(0);

		// Stub the metric to isolate the decision rules from the pricing walk.
		user.getFloorNetEquity = () => ({
			value: quote(100),
			allOraclesValid: false,
		});

		assert.isTrue(
			user.isBelowBufferedEquityFloor(slot),
			'a value that cannot be trusted must gate like a breach'
		);
		assert.isTrue(
			user.getEquityAboveFloor(slot)!.eq(ZERO),
			'untrusted equity reports no headroom above the floor'
		);
		assert.isTrue(
			user.getEquityAboveBufferedFloor(slot)!.eq(ZERO),
			'untrusted equity reports no headroom above the buffered floor'
		);
	});

	it('the trip walk matches the point value while every oracle is valid', async () => {
		const user = await userWithFloor(quote(1), ZERO);
		const slot = new BN(0);

		const tripNetEquity = user.getTripNetEquity(slot);
		assert.isTrue(tripNetEquity.provable);
		assert.isTrue(
			tripNetEquity.equityUpperBound.eq(user.getFloorNetEquity(slot).value),
			'no concession is taken while every oracle is valid'
		);

		// omitting the slot treats every oracle as valid, same as the point form
		const withoutSlot = user.getTripNetEquity();
		assert.isTrue(withoutSlot.provable);
		assert.isTrue(withoutSlot.equityUpperBound.eq(user.getNetUsdValue()));
	});

	it('the trip fires only on a provable breach', async () => {
		const user = await userWithFloor(quote(100), ZERO);
		const slot = new BN(0);

		// Stub the walk to isolate the predicate: a trip needs BOTH
		// provability and an upper bound below the raw floor.
		const breach = (equityUpperBound: BN, provable: boolean) => {
			user.getTripNetEquity = () => ({ equityUpperBound, provable });
			return user.provesEquityFloorBreach(slot);
		};

		assert.isTrue(
			breach(quote(50), true),
			'a provable upper bound below the floor arms the breaker'
		);
		assert.isFalse(
			breach(quote(50), false),
			'a material invalid-oracle position keeps the breach unprovable'
		);
		assert.isFalse(
			breach(quote(150), true),
			'an upper bound at or above the floor must not arm the breaker'
		);
	});

	it('the trip never fires without a floor', async () => {
		const user = await userWithFloor(ZERO, ZERO);
		user.getTripNetEquity = () => ({
			equityUpperBound: quote(-1000),
			provable: true,
		});
		assert.isFalse(user.provesEquityFloorBreach(new BN(0)));
	});

	it('withholds force-cancel authorization when an oracle cannot be trusted', async () => {
		const user = await userWithFloor(quote(1), ZERO);
		const slot = new BN(0);

		// Authorization requires BOTH a proven breach and trustworthy prices.
		const authorized = (value: BN, allOraclesValid: boolean) => {
			user.getFloorNetEquity = () => ({
				value,
				allOraclesValid,
			});
			return user.isForceCancelAuthorizedByEquityFloor(slot);
		};

		assert.isTrue(
			authorized(ZERO, true),
			'a proven breach on valid oracles authorizes the keeper'
		);
		assert.isFalse(
			authorized(ZERO, false),
			'an invalid oracle must not manufacture authorization against the user'
		);
		assert.isFalse(
			authorized(quote(100), true),
			'a clearly solvent account is never force-cancellable on floor grounds'
		);
	});
});
