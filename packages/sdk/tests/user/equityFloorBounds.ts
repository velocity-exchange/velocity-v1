import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import * as _ from 'lodash';
import { QUOTE_PRECISION, User, ZERO } from '../../src';
import { mockPerpMarkets, mockSpotMarkets } from '../dlob/helpers';
import { makeMockUser, mockUserAccount } from './helpers';

const quote = (n: number) => new BN(n).mul(QUOTE_PRECISION);

/**
 * The floor consumers must read the bound that fails closed for the decision they
 * make. Being below the floor RESTRICTS the account almost everywhere, so those
 * consumers read `lower` and a bad price cannot price the account up through the
 * floor. `force_cancel_orders` is the exception: there it AUTHORIZES a keeper
 * against the account, so it reads `upper` and additionally requires every oracle
 * to be valid.
 *
 * Getting either direction backwards silently reopens the hole the onchain change
 * closed, and no existing test would catch it.
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

describe('equity floor reads the bound that fails closed', () => {
	it('no floor set means no predicate fires and no headroom is reported', async () => {
		const user = await userWithFloor(ZERO, ZERO);
		assert.isFalse(user.isBelowEquityFloor());
		assert.isFalse(user.isBelowEquityFloor(new BN(0)));
		assert.isNull(user.getEquityAboveFloor());
		assert.isNull(user.getEquityAboveBufferedFloor());
		assert.isFalse(user.isForceCancelAuthorizedByEquityFloor(new BN(0)));
	});

	it('agrees with the point value when the bounds have not been widened', async () => {
		const user = await userWithFloor(quote(1), ZERO);
		// Valid oracles collapse lower == upper == getNetUsdValue(), so passing a
		// slot must not change any answer. This is the no-behaviour-change guarantee
		// that lets the wiring land without moving existing expectations.
		const bounds = user.getNetUsdValueBounds(new BN(0));
		assert.isTrue(bounds.lower.eq(bounds.upper));
		assert.isTrue(bounds.lower.eq(user.getNetUsdValue()));
		assert.isTrue(bounds.allOraclesValid);
		assert.equal(user.isBelowEquityFloor(), user.isBelowEquityFloor(new BN(0)));
	});

	it('reports headroom from the same side the gates compare', async () => {
		const user = await userWithFloor(quote(1), quote(1));
		const withSlot = user.getEquityAboveBufferedFloor(new BN(0));
		const withoutSlot = user.getEquityAboveBufferedFloor();
		assert.isNotNull(withSlot);
		assert.isNotNull(withoutSlot);
		// Equal while oracles are valid; the slot form is the one that stays correct
		// when they are not.
		assert.isTrue(withSlot!.eq(withoutSlot!));
		// Headroom is floored at zero rather than reported negative.
		assert.isTrue(withSlot!.gte(ZERO));
	});

	it('withholds force-cancel authorization when an oracle cannot be trusted', async () => {
		const user = await userWithFloor(quote(1), ZERO);
		const slot = new BN(0);

		// Authorization requires BOTH a proven breach and trustworthy prices. Stub
		// the bounds to isolate the decision rule from the pricing walk.
		const authorized = (upper: BN, allOraclesValid: boolean) => {
			user.getNetUsdValueBounds = () => ({
				lower: upper,
				upper,
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
