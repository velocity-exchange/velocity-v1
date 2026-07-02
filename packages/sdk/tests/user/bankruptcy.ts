import { assert } from 'chai';
import _ from 'lodash';
import { PositionFlag } from '../../src/types';
import {
	BASE_PRECISION,
	QUOTE_PRECISION,
} from '../../src/constants/numericConstants';
import { BN } from '../../src';
import { mockPerpMarkets, mockSpotMarkets } from '../dlob/helpers';
import {
	mockUserAccount as baseMockUserAccount,
	makeMockUser,
} from './helpers';
import {
	isUserBankrupt,
	isIsolatedPositionBankrupt,
} from '../../src/math/bankruptcy';

async function makeUserWithAccount(account) {
	const user = await makeMockUser(
		_.cloneDeep(mockPerpMarkets),
		_.cloneDeep(mockSpotMarkets),
		account,
		[1, 1, 1, 1, 1, 1, 1, 1],
		[1, 1, 1, 1, 1, 1, 1, 1]
	);
	return user;
}

describe('isUserBankrupt', () => {
	it('cross-bankrupt user is still bankrupt when an open isolated position is present', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// cross position: no base, negative quote (liability), no open orders -> cross-bankrupt shape
		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-100).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = 0;

		// isolated position: still open (nonzero base) - would have tripped the old
		// unconditional loop into reporting "not bankrupt"
		account.perpPositions[1].marketIndex = 1;
		account.perpPositions[1].baseAssetAmount = new BN(5).mul(BASE_PRECISION);
		account.perpPositions[1].quoteAssetAmount = new BN(0);
		account.perpPositions[1].positionFlag = PositionFlag.IsolatedPosition;

		const user = await makeUserWithAccount(account);

		assert.equal(isUserBankrupt(user), true);
	});

	it('user with no liability is not bankrupt', async () => {
		const account = _.cloneDeep(baseMockUserAccount);
		const user = await makeUserWithAccount(account);
		assert.equal(isUserBankrupt(user), false);
	});
});

describe('isIsolatedPositionBankrupt', () => {
	it('isolated position with no deposit, no base, and a quote liability is bankrupt', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-50).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = PositionFlag.IsolatedPosition;
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(0);

		const user = await makeUserWithAccount(account);

		assert.equal(isIsolatedPositionBankrupt(user, 0), true);
	});

	it('isolated position with a remaining deposit is not bankrupt', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-50).mul(
			QUOTE_PRECISION
		);
		account.perpPositions[0].positionFlag = PositionFlag.IsolatedPosition;
		account.perpPositions[0].isolatedPositionScaledBalance = new BN(1000);

		const user = await makeUserWithAccount(account);

		assert.equal(isIsolatedPositionBankrupt(user, 0), false);
	});
});
