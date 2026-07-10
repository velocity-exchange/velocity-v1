import { assert } from 'chai';
import _ from 'lodash';
import { BN, ContractTier } from '../../src';
import { QUOTE_PRECISION } from '../../src/constants/numericConstants';
import { mockPerpMarkets, mockSpotMarkets } from '../dlob/helpers';
import {
	mockUserAccount as baseMockUserAccount,
	makeMockUser,
} from './helpers';

// mirrors calculate_user_safest_position_tiers in math/margin.rs: a zero-base
// position with positive unsettled pnl is a claim on the pnl pool, not a
// liability, and must not register as the user's safest perp liability
describe('getSafestTiers', () => {
	async function makeUserWithAccount(account) {
		const perpMarkets = _.cloneDeep(mockPerpMarkets);
		perpMarkets[0].contractTier = ContractTier.A;
		perpMarkets[1].contractTier = ContractTier.SPECULATIVE;
		return await makeMockUser(
			perpMarkets,
			_.cloneDeep(mockSpotMarkets),
			account,
			[1, 1, 1, 1, 1, 1, 1, 1],
			[1, 1, 1, 1, 1, 1, 1, 1]
		);
	}

	it('positive-pnl claim in a safer market is not a liability', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		// flat position, positive unsettled pnl in the A-tier market: a claim
		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(10).mul(QUOTE_PRECISION);

		// negative pnl in the speculative market: the actual liability
		account.perpPositions[1].marketIndex = 1;
		account.perpPositions[1].baseAssetAmount = new BN(0);
		account.perpPositions[1].quoteAssetAmount = new BN(-300).mul(
			QUOTE_PRECISION
		);

		const user = await makeUserWithAccount(account);
		const { perpTier } = user.getSafestTiers();

		// speculative (3), NOT the A-tier (0) claim
		assert.equal(perpTier, 3);
	});

	it('negative pnl in a safer market still counts', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(-10).mul(
			QUOTE_PRECISION
		);

		const user = await makeUserWithAccount(account);
		const { perpTier } = user.getSafestTiers();

		assert.equal(perpTier, 0);
	});

	it('positive pnl with open orders still counts', async () => {
		const account = _.cloneDeep(baseMockUserAccount);

		account.perpPositions[0].marketIndex = 0;
		account.perpPositions[0].baseAssetAmount = new BN(0);
		account.perpPositions[0].quoteAssetAmount = new BN(10).mul(QUOTE_PRECISION);
		account.perpPositions[0].openOrders = 1;

		const user = await makeUserWithAccount(account);
		const { perpTier } = user.getSafestTiers();

		assert.equal(perpTier, 0);
	});
});
