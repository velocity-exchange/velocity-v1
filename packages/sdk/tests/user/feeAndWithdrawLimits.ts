import {
	BN,
	ZERO,
	User,
	SpotBalanceType,
	ReferrerStatus,
	UserStatsAccount,
	SPOT_MARKET_BALANCE_PRECISION,
} from '../../src';
import { assert } from '../../src/assert/assert';
import { mockPerpMarkets, mockSpotMarkets } from '../dlob/helpers';
import {
	mockUserAccount as baseMockUserAccount,
	makeMockUser,
} from './helpers';
import * as _ from 'lodash';

const mockFeeTier = {
	feeNumerator: 1,
	feeDenominator: 1000,
	makerRebateNumerator: 0,
	makerRebateDenominator: 1000,
	referrerRewardNumerator: 0,
	referrerRewardDenominator: 100,
	refereeFeeNumerator: 25,
	refereeFeeDenominator: 100,
};

const mockFeeStructure = {
	feeTiers: Array.from({ length: 6 }, () => ({ ...mockFeeTier })),
	fillerRewardStructure: {
		rewardNumerator: 0,
		rewardDenominator: 1,
		timeBasedRewardLowerBound: ZERO,
	},
	flatFillerFee: ZERO,
	ammFeeNumerator: 0,
	ifFeeNumerator: 0,
};

const mockUserStatsAccount: UserStatsAccount = {
	numberOfSubAccounts: 1,
	numberOfSubAccountsCreated: 1,
	makerVolume30D: ZERO,
	takerVolume30D: ZERO,
	fillerVolume30D: ZERO,
	lastMakerVolume30DTs: ZERO,
	lastTakerVolume30DTs: ZERO,
	lastFillerVolume30DTs: ZERO,
	fees: {
		totalFeePaid: ZERO,
		totalFeeRebate: ZERO,
		totalTokenDiscount: ZERO,
		totalRefereeDiscount: ZERO,
	},
	referrer: undefined as any,
	referrerStatus: 0,
	disableUpdatePerpBidAskTwap: 0,
	pausedOperations: 0,
	authority: undefined as any,
	ifStakedQuoteAssetAmount: ZERO,
	delegatePermissions: 0,
};

async function makeFeeMockUser(referrerStatus: number): Promise<User> {
	const myMockPerpMarkets = _.cloneDeep(mockPerpMarkets);
	const myMockSpotMarkets = _.cloneDeep(mockSpotMarkets);
	const myMockUserAccount = _.cloneDeep(baseMockUserAccount);

	const user = await makeMockUser(
		myMockPerpMarkets,
		myMockSpotMarkets,
		myMockUserAccount,
		[1, 1, 1, 1, 1, 1, 1, 1],
		[1, 1, 1, 1, 1, 1, 1, 1]
	);

	user.velocityClient.getStateAccount = () =>
		({
			perpFeeStructure: mockFeeStructure,
			spotFeeStructure: mockFeeStructure,
		}) as any;

	const userStatsAccount = {
		..._.cloneDeep(mockUserStatsAccount),
		referrerStatus,
	};
	user.velocityClient.getUserStatsOrThrow = () =>
		({
			getAccountOrThrow: () => userStatsAccount,
		}) as any;

	return user;
}

describe('User fee calculation', () => {
	it('taker fee for a non-market-index quote amount rounds up (ceil)', async () => {
		const user = await makeFeeMockUser(0);

		// 1_000_007 * 1 / 1000 = 1000.007 -> ceil = 1001, floor would be 1000
		const fee = user.calculateFeeForQuoteAmount(new BN(1_000_007));
		assert(
			fee.eq(new BN(1001)),
			`expected ceil-rounded fee of 1001, got ${fee.toString()}`
		);
	});

	it('referee discount is not applied for a non-referred user', async () => {
		const user = await makeFeeMockUser(0);

		const fee = user.calculateFeeForQuoteAmount(new BN(1_000_000));
		// 1_000_000 * 1 / 1000 = 1000, no referee discount
		assert(
			fee.eq(new BN(1000)),
			`expected undiscounted fee, got ${fee.toString()}`
		);
	});

	it('referee discount is applied when the user stats account marks them as referred', async () => {
		const user = await makeFeeMockUser(ReferrerStatus.IsReferred);

		const fee = user.calculateFeeForQuoteAmount(new BN(1_000_000));
		// base fee = 1000, referee discount = 25% of 1000 = 250 -> fee = 750
		assert(
			fee.eq(new BN(750)),
			`expected 25% referee discount applied, got ${fee.toString()}`
		);
	});

	it('an explicit isReferee override applies the discount regardless of user stats', async () => {
		const user = await makeFeeMockUser(0);

		const fee = user.calculateFeeForQuoteAmount(
			new BN(1_000_000),
			undefined,
			true
		);
		assert(
			fee.eq(new BN(750)),
			`expected 25% referee discount applied via override, got ${fee.toString()}`
		);
	});
});

describe('User canBypassWithdrawLimits', () => {
	async function makeWithdrawMockUser(cumulativeDeposits: BN): Promise<User> {
		const myMockPerpMarkets = _.cloneDeep(mockPerpMarkets);
		const myMockSpotMarkets = _.cloneDeep(mockSpotMarkets);
		const myMockUserAccount = _.cloneDeep(baseMockUserAccount);

		// generous withdraw guard threshold so canBypass isn't gated on deposit size
		myMockSpotMarkets[0].withdrawGuardThreshold = new BN(100_000).mul(
			SPOT_MARKET_BALANCE_PRECISION
		);

		myMockUserAccount.totalDeposits = new BN(1000).mul(
			SPOT_MARKET_BALANCE_PRECISION
		);
		myMockUserAccount.totalWithdraws = ZERO;
		myMockUserAccount.spotPositions[0].balanceType = SpotBalanceType.DEPOSIT;
		myMockUserAccount.spotPositions[0].scaledBalance = new BN(100).mul(
			SPOT_MARKET_BALANCE_PRECISION
		);
		myMockUserAccount.spotPositions[0].cumulativeDeposits = cumulativeDeposits;

		return makeMockUser(
			myMockPerpMarkets,
			myMockSpotMarkets,
			myMockUserAccount,
			[1, 1, 1, 1, 1, 1, 1, 1],
			[1, 1, 1, 1, 1, 1, 1, 1]
		);
	}

	it('can bypass when net deposits and cumulative deposits are both non-negative', async () => {
		const user = await makeWithdrawMockUser(new BN(100));
		const { canBypass } = user.canBypassWithdrawLimits(0);
		assert(canBypass, 'expected canBypass to be true');
	});

	it('cannot bypass when cumulative deposits on the position are negative', async () => {
		const user = await makeWithdrawMockUser(new BN(-1));
		const { canBypass } = user.canBypassWithdrawLimits(0);
		assert(
			!canBypass,
			'expected canBypass to be false with negative cumulativeDeposits'
		);
	});
});
