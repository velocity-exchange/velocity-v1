import {
	BN,
	ZERO,
	User,
	MarketType,
	SpotBalanceType,
	ReferrerStatus,
	UserStatsAccount,
	SPOT_MARKET_BALANCE_PRECISION,
	QUOTE_PRECISION,
	getMarketFeesForFeeTier,
	PerpMarketAccount,
	PERP_FEE_TIER_MAX_INDEX,
	VIP_FEE_TIER_TWO_VOLUME_QUOTE,
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
	equityBreakerTripped: 0,
	acceleratedReferralStatus: 0,
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
			promoFeeTier: 0,
		}) as any;

	const userStatsAccount = {
		..._.cloneDeep(mockUserStatsAccount),
		referrerStatus,
	};
	user.velocityClient.getUserStatsOrThrow = () =>
		({
			getAccountOrThrow: () => userStatsAccount,
		}) as any;
	// getMarketFees reads referee status via getUserStats()?.getAccount()
	user.velocityClient.getUserStats = () =>
		({
			getAccount: () => userStatsAccount,
		}) as any;
	// getMarketFees(marketIndex) reads the market's feeAdjustment
	user.velocityClient.getPerpMarketAccountOrThrow = () =>
		({
			marketIndex: 0,
			feeAdjustment: 0,
			takerFeeAddonTenthBps: 0,
		}) as any;

	return user;
}

describe('User fee calculation', () => {
	it('taker fee for a non-market-index quote amount rounds up (ceil)', async () => {
		const user = await makeFeeMockUser(0);

		// 1_000_007 * 1 / 1000 = 1000.007 -> ceil = 1001, floor would be 1000
		const fee = user.calculatePerpTakerFee(new BN(1_000_007));
		assert(
			fee.eq(new BN(1001)),
			`expected ceil-rounded fee of 1001, got ${fee.toString()}`
		);
	});

	it('referee discount is not applied for a non-referred user', async () => {
		const user = await makeFeeMockUser(0);

		const fee = user.calculatePerpTakerFee(new BN(1_000_000));
		// 1_000_000 * 1 / 1000 = 1000, no referee discount
		assert(
			fee.eq(new BN(1000)),
			`expected undiscounted fee, got ${fee.toString()}`
		);
	});

	it('referee discount is applied when the user stats account marks them as referred', async () => {
		const user = await makeFeeMockUser(ReferrerStatus.IsReferred);

		const fee = user.calculatePerpTakerFee(new BN(1_000_000));
		// base fee = 1000, referee discount = 25% of 1000 = 250 -> fee = 750
		assert(
			fee.eq(new BN(750)),
			`expected 25% referee discount applied, got ${fee.toString()}`
		);
	});

	it('an explicit isReferee override applies the discount regardless of user stats', async () => {
		const user = await makeFeeMockUser(0);

		const fee = user.calculatePerpTakerFee(new BN(1_000_000), undefined, true);
		assert(
			fee.eq(new BN(750)),
			`expected 25% referee discount applied via override, got ${fee.toString()}`
		);
	});

	// M11: getMarketFees (the primary fee-prediction entry point) must apply the
	// referee discount, not just calculatePerpTakerFee's volume-tier branch.
	it('getMarketFees applies the referee discount to the taker fee for a referred user', async () => {
		const referred = await makeFeeMockUser(ReferrerStatus.IsReferred);
		const notReferred = await makeFeeMockUser(0);

		const { takerFee: referredTakerFee } =
			referred.velocityClient.getMarketFees(MarketType.PERP, 0, referred);
		const { takerFee: baseTakerFee } = notReferred.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			notReferred
		);

		// base taker fee = 1/1000 = 0.001; referee discount = 25% -> 0.00075
		assert(
			Math.abs(baseTakerFee - 0.001) < 1e-12,
			`expected base taker fee 0.001, got ${baseTakerFee}`
		);
		assert(
			Math.abs(referredTakerFee - 0.00075) < 1e-12,
			`expected discounted taker fee 0.00075, got ${referredTakerFee}`
		);
	});

	// M11: the calculatePerpTakerFee marketIndex path (which delegates to
	// getMarketFees) must now also reflect the referee discount.
	it('calculatePerpTakerFee marketIndex path applies the referee discount', async () => {
		const user = await makeFeeMockUser(ReferrerStatus.IsReferred);

		const fee = user.calculatePerpTakerFee(new BN(1_000_000), 0);
		// 1_000_000 * 0.00075 = 750
		assert(
			fee.eq(new BN(750)),
			`expected discounted market-index fee 750, got ${fee.toString()}`
		);
	});

	// M12: builder fee must be added by getMarketFees when orderParams carry a builder code.
	it('getMarketFees adds the builder fee fraction to the taker fee', async () => {
		const user = await makeFeeMockUser(0);

		const { takerFee } = user.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			user,
			{ builderIdx: 0, builderFeeTenthBps: 10 }
		);
		// base 0.001 + builder 10/100_000 = 0.0001 -> 0.0011
		assert(
			Math.abs(takerFee - 0.0011) < 1e-12,
			`expected taker fee incl. builder 0.0011, got ${takerFee}`
		);
	});

	// M12: builder fee must also be applied by calculatePerpTakerFee on both
	// the volume-tier branch and the marketIndex branch.
	it('calculatePerpTakerFee adds the builder fee on the volume-tier branch', async () => {
		const user = await makeFeeMockUser(0);

		const fee = user.calculatePerpTakerFee(
			new BN(1_000_000),
			undefined,
			false,
			{ builderIdx: 0, builderFeeTenthBps: 10 }
		);
		// base 1000 + builderFee(1_000_000, 10) = 1_000_000*10/100_000 = 100 -> 1100
		assert(
			fee.eq(new BN(1100)),
			`expected fee incl. builder 1100, got ${fee.toString()}`
		);
	});

	it('calculatePerpTakerFee adds the builder fee on the marketIndex branch', async () => {
		const user = await makeFeeMockUser(0);

		const fee = user.calculatePerpTakerFee(new BN(1_000_000), 0, false, {
			builderIdx: 0,
			builderFeeTenthBps: 10,
		});
		// 1_000_000 * (0.001 + 0.0001) = 1100
		assert(
			fee.eq(new BN(1100)),
			`expected market-index fee incl. builder 1100, got ${fee.toString()}`
		);
	});
});

// The promo fee tier (`State.promoFeeTier`) floors every account's perp tier at
// the configured index while it is set. These tiers differ per index so the
// selected index is observable in the returned rates.
const tieredFeeStructure = {
	...mockFeeStructure,
	feeTiers: [
		{ ...mockFeeTier, feeNumerator: 30, makerRebateNumerator: 1 },
		{ ...mockFeeTier, feeNumerator: 20, makerRebateNumerator: 2 },
		{ ...mockFeeTier, feeNumerator: 10, makerRebateNumerator: 3 },
		// Spare slots the program never selects, left with the zeroed
		// denominators an unwritten slot really carries.
		...Array.from({ length: 3 }, () => ({
			...mockFeeTier,
			feeNumerator: 0,
			feeDenominator: 0,
			makerRebateDenominator: 0,
		})),
	],
};

async function makePromoMockUser(
	promoFeeTier: number,
	takerVolume30D: BN = ZERO
): Promise<User> {
	const user = await makeMockUser(
		_.cloneDeep(mockPerpMarkets),
		_.cloneDeep(mockSpotMarkets),
		_.cloneDeep(baseMockUserAccount),
		[1, 1, 1, 1, 1, 1, 1, 1],
		[1, 1, 1, 1, 1, 1, 1, 1]
	);

	user.velocityClient.getStateAccount = () =>
		({
			perpFeeStructure: tieredFeeStructure,
			spotFeeStructure: tieredFeeStructure,
			promoFeeTier,
		}) as any;

	const userStatsAccount = {
		..._.cloneDeep(mockUserStatsAccount),
		takerVolume30D,
		// The rolling estimate decays from the last update, so stamp it now to
		// keep the full volume inside the window.
		lastTakerVolume30DTs: new BN(Math.floor(Date.now() / 1000)),
	};
	user.velocityClient.getUserStatsOrThrow = () =>
		({
			getAccountOrThrow: () => userStatsAccount,
		}) as any;
	user.velocityClient.getUserStats = () =>
		({
			getAccount: () => userStatsAccount,
		}) as any;
	user.velocityClient.getPerpMarketAccountOrThrow = () =>
		({
			marketIndex: 0,
			feeAdjustment: 0,
			takerFeeAddonTenthBps: 0,
		}) as any;

	return user;
}

describe('Promo fee tier floor', () => {
	it('lifts a zero-volume user to the promo tier', async () => {
		const user = await makePromoMockUser(2);

		const feeTier = user.getUserFeeTier(MarketType.PERP);
		assert(
			feeTier.feeNumerator === 10,
			`expected the promo tier's fee, got ${feeTier.feeNumerator}`
		);
		assert(user.getUserPerpFeeTierIndex() === 2);
	});

	it('does not downgrade a user whose volume already earns a better tier', async () => {
		const user = await makePromoMockUser(
			1,
			VIP_FEE_TIER_TWO_VOLUME_QUOTE.muln(2)
		);

		assert(
			user.getUserPerpFeeTierIndex() === 2,
			'the volume tier must win over a lower promo floor'
		);
	});

	it('is a no-op when disabled', async () => {
		const user = await makePromoMockUser(0);

		assert(user.getUserPerpFeeTierIndex() === 0);
	});

	it('clamps a promo above the live tiers to the top live tier', async () => {
		const user = await makePromoMockUser(5);

		// Slots past the ladder have zeroed denominators; selecting one would
		// divide to NaN rather than charge a fee.
		assert(user.getUserPerpFeeTierIndex() === PERP_FEE_TIER_MAX_INDEX);
		assert(user.getUserFeeTier(MarketType.PERP).feeDenominator === 1000);
	});

	// The regression: getMarketFees with no user quoted the entry tier during a
	// promo, disagreeing with the same call made with a user.
	it('applies to getMarketFees when no user is passed', async () => {
		const user = await makePromoMockUser(2);

		const { takerFee: genericTakerFee, makerFee: genericMakerFee } =
			user.velocityClient.getMarketFees(MarketType.PERP, 0);
		const { takerFee: userTakerFee } = user.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			user
		);

		assert(
			Math.abs(genericTakerFee - 0.01) < 1e-12,
			`expected the promo tier's taker fee 0.01, got ${genericTakerFee}`
		);
		assert(
			Math.abs(genericMakerFee - 0.003) < 1e-12,
			`expected the promo tier's maker rebate 0.003, got ${genericMakerFee}`
		);
		assert(
			Math.abs(genericTakerFee - userTakerFee) < 1e-12,
			'the generic schedule must agree with a zero-volume user under a promo'
		);
	});

	it('leaves spot fees on the entry tier', async () => {
		const user = await makePromoMockUser(2);

		const { takerFee } = user.velocityClient.getMarketFees(MarketType.SPOT);
		assert(
			Math.abs(takerFee - 0.03) < 1e-12,
			`expected the spot entry-tier fee 0.03, got ${takerFee}`
		);
	});
});

// A tier whose four modifiers are all visible in the result: 10bps taker,
// 1bp maker rebate, 25% referee discount.
const modifierTestFeeTier = {
	...mockFeeTier,
	feeNumerator: 10,
	feeDenominator: 10_000,
	makerRebateNumerator: 1,
	makerRebateDenominator: 10_000,
	refereeFeeNumerator: 25,
	refereeFeeDenominator: 100,
};

const makeMarket = (
	takerFeeAddonTenthBps = 0,
	feeAdjustment = 0
): PerpMarketAccount =>
	({
		marketIndex: 0,
		takerFeeAddonTenthBps,
		feeAdjustment,
	}) as PerpMarketAccount;

const closeTo = (actual: number, expected: number, what: string) =>
	assert(
		Math.abs(actual - expected) < 1e-12,
		`expected ${what} ${expected}, got ${actual}`
	);

describe('getMarketFeesForFeeTier', () => {
	it('returns the tier rates untouched without a market', () => {
		const { takerFee, makerFee } = getMarketFeesForFeeTier(
			modifierTestFeeTier,
			MarketType.PERP
		);

		closeTo(takerFee, 0.001, 'taker fee');
		closeTo(makerFee, 0.0001, 'maker rebate');
	});

	it('adds the market surcharge to the taker leg only', () => {
		const { takerFee, makerFee } = getMarketFeesForFeeTier(
			modifierTestFeeTier,
			MarketType.PERP,
			makeMarket(10)
		);

		// 10bps + 1bp surcharge
		closeTo(takerFee, 0.0011, 'taker fee');
		closeTo(makerFee, 0.0001, 'maker rebate');
	});

	it('leaves the surcharge out of spot fees', () => {
		const { takerFee } = getMarketFeesForFeeTier(
			modifierTestFeeTier,
			MarketType.SPOT,
			makeMarket(10)
		);

		closeTo(takerFee, 0.001, 'taker fee');
	});

	it('scales both legs by feeAdjustment, after the surcharge', () => {
		const { takerFee, makerFee } = getMarketFeesForFeeTier(
			modifierTestFeeTier,
			MarketType.PERP,
			makeMarket(10, 50)
		);

		// (10bps + 1bp) * 1.5, not 10bps * 1.5 + 1bp
		closeTo(takerFee, 0.00165, 'taker fee');
		closeTo(makerFee, 0.00015, 'maker rebate');
	});

	it('discounts a referee after feeAdjustment, taker leg only', () => {
		const { takerFee, makerFee } = getMarketFeesForFeeTier(
			modifierTestFeeTier,
			MarketType.PERP,
			makeMarket(10, 50),
			{ isReferee: true }
		);

		// 0.00165 * 0.75
		closeTo(takerFee, 0.0012375, 'taker fee');
		closeTo(makerFee, 0.00015, 'maker rebate');
	});

	it('adds the builder fee last, unscaled', () => {
		const { takerFee } = getMarketFeesForFeeTier(
			modifierTestFeeTier,
			MarketType.PERP,
			makeMarket(10, 50),
			{ isReferee: true, builderFeeTenthBps: 10 }
		);

		// 0.0012375 + 1bp flat: the builder fee is not scaled or discounted
		closeTo(takerFee, 0.0013375, 'taker fee');
	});
});

describe('getMarketFees fee-tier override', () => {
	// Pricing a tier the account is not on is what lets a UI show the fee a
	// promotion is saving someone against their own volume tier.
	it('prices the override instead of the account tier', async () => {
		const user = await makeFeeMockUser(0);
		const doubleFeeTier = { ...mockFeeTier, feeNumerator: 2 };

		const { takerFee: ownTakerFee } = user.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			user
		);
		const { takerFee: overriddenTakerFee } = user.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			user,
			undefined,
			doubleFeeTier
		);

		closeTo(ownTakerFee, 0.001, 'account tier taker fee');
		closeTo(overriddenTakerFee, 0.002, 'overridden taker fee');
	});

	it('keeps every other modifier the account carries', async () => {
		const referred = await makeFeeMockUser(ReferrerStatus.IsReferred);
		const doubleFeeTier = { ...mockFeeTier, feeNumerator: 2 };

		const { takerFee } = referred.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			referred,
			undefined,
			doubleFeeTier
		);

		// 0.002 less the override tier's own 25% referee discount
		closeTo(takerFee, 0.0015, 'overridden taker fee');
	});

	it('agrees with the unoverridden call when handed the account tier', async () => {
		const user = await makeFeeMockUser(ReferrerStatus.IsReferred);
		const ownFeeTier = user.getUserFeeTier(MarketType.PERP);

		const { takerFee: withoutOverride } = user.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			user
		);
		const { takerFee: withOverride } = user.velocityClient.getMarketFees(
			MarketType.PERP,
			0,
			user,
			undefined,
			ownFeeTier
		);

		closeTo(withOverride, withoutOverride, 'overridden taker fee');
	});
});

// Shipped quote-market (USDT, index 0) config from
// deploy-scripts/params/relaunch-spot-markets.json.
// The guard threshold is capped on chain at $10k of notional by
// MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL, so 9_500 USDT is near the ceiling. The
// per-account eligibility allowance is a tenth of it, 950 USDT.
const GUARD_THRESHOLD = new BN(9_500).mul(QUOTE_PRECISION);
// A 500_000 USDT market. The default 2500 bps breaker floors deposits at
// 375_000 USDT, so the market cannot fall below that without an exception.
const DEPOSIT_TWAP = new BN(500_000).mul(QUOTE_PRECISION);
const BREAKER_FLOOR = new BN(375_000).mul(QUOTE_PRECISION);
// 6 decimals and cumulative interest at precision, so this is the token amount
// to scaled balance ratio.
const BALANCE_PER_TOKEN = SPOT_MARKET_BALANCE_PRECISION.div(QUOTE_PRECISION);

describe('User canBypassWithdrawLimits', () => {
	/**
	 * Builds a 100 USDT depositor in a market whose withdraw circuit breaker is
	 * already at its floor. `marketDepositTokens` is the market's current deposit
	 * token amount, which sets how much of the shared exception budget is left.
	 */
	async function makeWithdrawMockUser(
		cumulativeDeposits: BN,
		marketDepositTokens: BN = BREAKER_FLOOR
	): Promise<User> {
		const myMockPerpMarkets = _.cloneDeep(mockPerpMarkets);
		const myMockSpotMarkets = _.cloneDeep(mockSpotMarkets);
		const myMockUserAccount = _.cloneDeep(baseMockUserAccount);

		// The realistic shipped threshold. The user's 100 USDT deposit is under
		// the 950 USDT per-account allowance, so canBypass still fires.
		myMockSpotMarkets[0].withdrawGuardThreshold = GUARD_THRESHOLD;
		myMockSpotMarkets[0].depositTokenTwap = DEPOSIT_TWAP;
		myMockSpotMarkets[0].depositBalance =
			marketDepositTokens.mul(BALANCE_PER_TOKEN);
		myMockSpotMarkets[0].borrowBalance = ZERO;
		myMockSpotMarkets[0].withdrawCircuitBreakerBps = 2_500;
		// The SDK projects a live TWAP from lastTwapTs to now. A fresh timestamp
		// makes the projected TWAP equal the stored one, so the breaker floor is
		// the stored TWAP's floor. A stale timestamp would collapse the projected
		// TWAP onto the current deposit amount and the breaker would never bind.
		myMockSpotMarkets[0].lastTwapTs = new BN(Math.floor(Date.now() / 1000));

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
		const { canBypass, depositAmount, maxDepositAmount } =
			user.canBypassWithdrawLimits(0);
		assert(canBypass, 'expected canBypass to be true');
		// 100 USDT held against a 950 USDT allowance.
		assert(depositAmount.eq(new BN(100).mul(QUOTE_PRECISION)));
		assert(maxDepositAmount.eq(new BN(950).mul(QUOTE_PRECISION)));
	});

	it('cannot bypass when cumulative deposits on the position are negative', async () => {
		const user = await makeWithdrawMockUser(new BN(-1));
		const { canBypass } = user.canBypassWithdrawLimits(0);
		assert(
			!canBypass,
			'expected canBypass to be false with negative cumulativeDeposits'
		);
	});

	it('bypasses the breaker in full while the shared exception budget is untouched', async () => {
		// The market sits on the breaker floor, so the full 9_500 USDT budget is
		// available. The 100 USDT depositor exits in full.
		const user = await makeWithdrawMockUser(new BN(100));
		const { canBypass, depositAmount } = user.canBypassWithdrawLimits(0);
		assert(canBypass);

		const limit = user.getWithdrawalLimit(0, true);
		assert(
			limit.eq(depositAmount),
			`expected full exit of ${depositAmount.toString()}, got ${limit.toString()}`
		);
	});

	it('caps the bypass at the room left in the shared exception budget', async () => {
		// Other eligible accounts already took 9_450 USDT of the 9_500 USDT
		// budget. Only about 50 USDT is left, so the 100 USDT depositor cannot
		// exit in full even though it is still eligible.
		const spent = new BN(9_450).mul(QUOTE_PRECISION);
		const user = await makeWithdrawMockUser(
			new BN(100),
			BREAKER_FLOOR.sub(spent)
		);
		const { canBypass, depositAmount } = user.canBypassWithdrawLimits(0);
		assert(canBypass, 'the account is still eligible');

		const limit = user.getWithdrawalLimit(0, true);
		assert(
			limit.lt(depositAmount),
			`expected less than the ${depositAmount.toString()} deposit, got ${limit.toString()}`
		);
		// About 50 USDT. The live TWAP projection can drift by a second between
		// building the fixture and reading the limit, which moves the floor by
		// about 1.5 USDT, so allow a small band.
		const expected = new BN(50).mul(QUOTE_PRECISION);
		const tolerance = new BN(3).mul(QUOTE_PRECISION);
		assert(
			limit.sub(expected).abs().lte(tolerance),
			`expected about ${expected.toString()}, got ${limit.toString()}`
		);
	});

	it('grants nothing once the shared exception budget is spent', async () => {
		// The cohort already took 9_600 USDT, more than one guard threshold below
		// the breaker floor. The budget is gone. `canBypass` is still true, which
		// is exactly why it must not be read as a promise of a successful
		// withdrawal: on chain this account would revert with DailyWithdrawLimit.
		const spent = new BN(9_600).mul(QUOTE_PRECISION);
		const user = await makeWithdrawMockUser(
			new BN(100),
			BREAKER_FLOOR.sub(spent)
		);
		const { canBypass } = user.canBypassWithdrawLimits(0);
		assert(canBypass, 'the account is still eligible');

		const limit = user.getWithdrawalLimit(0, true);
		assert(limit.eq(ZERO), `expected 0, got ${limit.toString()}`);
	});
});
