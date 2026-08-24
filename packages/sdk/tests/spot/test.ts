import {
	BN,
	ZERO,
	calculateSpotMarketBorrowCapacity,
	SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION,
	calculateSizePremiumLiabilityWeight,
	calculateBorrowRate,
	calculateDepositRate,
	calculateMaxDepositTokenAmount,
	checkDepositLimits,
	getTokenAmount,
	SpotBalanceType,
	BPS_PRECISION,
	QUOTE_PRECISION,
	calculateWithdrawLimit,
	getTokenValue,
	getStrictTokenValue,
	StrictOraclePrice,
	SpotMarketAccount,
	MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN,
	MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN,
	ONE_YEAR,
	maxSpotInterestStalenessForMargin,
} from '../../src';
import { mockSpotMarkets } from '../dlob/helpers';
import * as _ from 'lodash';

import { assert } from '../../src/assert/assert';

describe('Spot Tests', () => {
	it('size premium via imf factor', () => {
		const maintLiabWgt = new BN(1.1 * 1e4);

		const ans0 = calculateSizePremiumLiabilityWeight(
			new BN(200000 * 1e9),
			ZERO,
			maintLiabWgt,
			new BN(1e4)
		);
		assert(ans0.eq(maintLiabWgt));

		const ans = calculateSizePremiumLiabilityWeight(
			new BN(200000 * 1e9),
			new BN(0.00055 * 1e6),
			maintLiabWgt,
			new BN(1e4)
		);
		assert(ans.eq(new BN('11259')));
		assert(ans.gt(maintLiabWgt));

		const ans2 = calculateSizePremiumLiabilityWeight(
			new BN(10000 * 1e9),
			new BN(0.003 * 1e6),
			maintLiabWgt,
			new BN(1e4)
		);
		assert(ans2.eq(new BN('11800')));
		assert(ans.gt(maintLiabWgt));

		const ans3 = calculateSizePremiumLiabilityWeight(
			new BN(100000 * 1e9),
			new BN(0.003 * 1e6),
			maintLiabWgt,
			new BN(1e4)
		);
		assert(ans3.eq(new BN('18286')));
		assert(ans3.gt(maintLiabWgt));
	});

	it('base borrow capacity', () => {
		const mockSpot = _.cloneDeep(mockSpotMarkets[0]);
		mockSpot.maxBorrowRate = 1000000;
		mockSpot.optimalBorrowRate = 100000;
		mockSpot.optimalUtilization = 700000;

		mockSpot.decimals = 9;
		mockSpot.cumulativeDepositInterest =
			SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION;
		mockSpot.cumulativeBorrowInterest =
			SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION;

		const tokenAmount = 100000;
		// no borrows
		mockSpot.depositBalance = new BN(tokenAmount * 1e9);
		mockSpot.borrowBalance = ZERO;

		// todo, should incorp all other spot market constraints?
		const { remainingCapacity: aboveMaxAmount } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(2000000));
		assert(aboveMaxAmount.gt(mockSpot.depositBalance));

		const { remainingCapacity: maxAmount } = calculateSpotMarketBorrowCapacity(
			mockSpot,
			new BN(1000000)
		);
		assert(maxAmount.eq(mockSpot.depositBalance));

		const { remainingCapacity: optAmount } = calculateSpotMarketBorrowCapacity(
			mockSpot,
			new BN(100000)
		);
		const ans = new BN((mockSpot.depositBalance.toNumber() * 7) / 10);
		// console.log('optAmount:', optAmount.toNumber(), ans.toNumber());
		assert(optAmount.eq(ans));

		const { remainingCapacity: betweenOptMaxAmount } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(810000));
		// console.log('betweenOptMaxAmount:', betweenOptMaxAmount.toNumber());
		assert(betweenOptMaxAmount.lt(mockSpot.depositBalance));
		assert(betweenOptMaxAmount.gt(ans));
		assert(betweenOptMaxAmount.eq(new BN(93666600000000)));

		const { remainingCapacity: belowOptAmount } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(50000));
		// console.log('belowOptAmount:', belowOptAmount.toNumber());
		assert(belowOptAmount.eq(ans.div(new BN(2))));

		const { remainingCapacity: belowOptAmount2 } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(24900));
		// console.log('belowOptAmount2:', belowOptAmount2.toNumber());
		assert(belowOptAmount2.lt(ans.div(new BN(4))));
		assert(belowOptAmount2.eq(new BN('17430000000000')));

		const { remainingCapacity: belowOptAmount3 } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(1));
		// console.log('belowOptAmount3:', belowOptAmount3.toNumber());
		assert(belowOptAmount3.eq(new BN('700000000'))); //0.7
	});

	it('complex borrow capacity', () => {
		const mockSpot = _.cloneDeep(mockSpotMarkets[0]);
		mockSpot.maxBorrowRate = 1000000;
		mockSpot.optimalBorrowRate = 70000;
		mockSpot.optimalUtilization = 700000;

		mockSpot.decimals = 9;
		mockSpot.cumulativeDepositInterest = new BN(
			1.0154217042 * SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION.toNumber()
		);
		mockSpot.cumulativeBorrowInterest = new BN(
			1.0417153549 * SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION.toNumber()
		);

		mockSpot.depositBalance = new BN(88522.734106451 * 1e9);
		mockSpot.borrowBalance = new BN(7089.91675884 * 1e9);

		// todo, should incorp all other spot market constraints?
		const { remainingCapacity: aboveMaxAmount } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(2000000));
		assert(aboveMaxAmount.eq(new BN('111498270939007')));

		const { remainingCapacity: maxAmount } = calculateSpotMarketBorrowCapacity(
			mockSpot,
			new BN(1000000)
		);
		assert(maxAmount.eq(new BN('82502230374168')));
		// console.log('aboveMaxAmount:', aboveMaxAmount.toNumber(), 'maxAmount:', maxAmount.toNumber());
		const { remainingCapacity: optAmount } = calculateSpotMarketBorrowCapacity(
			mockSpot,
			new BN(70000)
		);
		// console.log('optAmount:', optAmount.toNumber());
		assert(optAmount.eq(new BN('55535858716123'))); // ~ 55535

		const { remainingCapacity: betweenOptMaxAmount } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(810000));
		// console.log('betweenOptMaxAmount:', betweenOptMaxAmount.toNumber());
		assert(betweenOptMaxAmount.lt(maxAmount));
		assert(betweenOptMaxAmount.eq(new BN(76992910756523)));
		assert(betweenOptMaxAmount.gt(optAmount));

		const { remainingCapacity: belowOptAmount } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(50000));
		// console.log('belowOptAmount:', belowOptAmount.toNumber());
		assert(belowOptAmount.eq(new BN('37558277610760')));

		const { remainingCapacity: belowOptAmount2 } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(24900));
		// console.log('belowOptAmount2:', belowOptAmount2.toNumber());
		assert(belowOptAmount2.eq(new BN('14996413323529')));

		const { remainingCapacity: belowOptAmount3 } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(4900));
		// console.log('belowOptAmount2:', belowOptAmount3.toNumber());
		assert(belowOptAmount3.eq(new BN('0')));

		const { remainingCapacity: belowOptAmount4 } =
			calculateSpotMarketBorrowCapacity(mockSpot, new BN(1));
		// console.log('belowOptAmount3:', belowOptAmount4.toNumber());
		assert(belowOptAmount4.eq(new BN('0')));
	});

	it('borrow rates', () => {
		const mockSpot = _.cloneDeep(mockSpotMarkets[0]);
		mockSpot.maxBorrowRate = 1000000;
		mockSpot.optimalBorrowRate = 70000;
		mockSpot.optimalUtilization = 700000;

		mockSpot.decimals = 9;
		mockSpot.cumulativeDepositInterest = new BN(
			1.0154217042 * SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION.toNumber()
		);
		mockSpot.cumulativeBorrowInterest = new BN(
			1.0417153549 * SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION.toNumber()
		);

		mockSpot.depositBalance = new BN(88522.734106451 * 1e9);
		mockSpot.borrowBalance = new BN(17089.91675884 * 1e9);

		const noDeltad = calculateDepositRate(mockSpot);
		// console.log(noDeltad.toNumber());
		assert(noDeltad.eqn(3922));
		const noDelta = calculateBorrowRate(mockSpot);
		// console.log(noDelta.toNumber());
		assert(noDelta.eqn(19805));

		// manually update deposits
		mockSpot.depositBalance = new BN((88522.734106451 + 9848.12512736) * 1e9);
		const noDeltad2 = calculateDepositRate(mockSpot);
		console.log(noDeltad2.toNumber());
		assert(noDeltad2.eqn(3176));
		const noDelta2 = calculateBorrowRate(mockSpot);
		console.log(noDelta2.toNumber());
		assert(noDelta2.eqn(17822));

		mockSpot.depositBalance = new BN(88522.734106451 * 1e9);
		const addDep1d = calculateDepositRate(mockSpot, new BN(10000 * 1e9));
		// console.log(addDep1d.toNumber());
		assert(addDep1d.eqn(3176)); // went down
		const addDep1 = calculateBorrowRate(mockSpot, new BN(10000 * 1e9));
		// console.log(addDep1.toNumber());
		assert(addDep1.eqn(17822)); // went down

		const addBord1 = calculateDepositRate(mockSpot, new BN(-1000 * 1e9));
		// console.log(addBord1.toNumber());
		assert(addBord1.eqn(4375)); // went up
		const addBor1 = calculateBorrowRate(mockSpot, new BN(-1000 * 1e9));
		// console.log(addBor1.toNumber());
		assert(addBor1.eqn(20918)); // went up
	});

	it('calculateMaxDepositTokenAmount', () => {
		const twap = new BN(100).mul(QUOTE_PRECISION);

		// disabled (pct == 0) => null
		assert(calculateMaxDepositTokenAmount(twap, ZERO, 0) === null);

		// 20%/day => cap at 120% of twap
		const pct = BPS_PRECISION.divn(5).toNumber(); // 2000 bps = 20%
		const cap = calculateMaxDepositTokenAmount(twap, ZERO, pct);
		assert(cap!.eq(twap.add(twap.divn(5))));

		// high guard threshold lifts the cap below it
		const guard = new BN(1000).mul(QUOTE_PRECISION);
		const capGuard = calculateMaxDepositTokenAmount(
			new BN(10).mul(QUOTE_PRECISION),
			guard,
			pct
		);
		assert(capGuard!.eq(guard));
	});

	it('checkDepositLimits', () => {
		const mockSpot = _.cloneDeep(mockSpotMarkets[0]);
		mockSpot.decimals = 6;
		mockSpot.cumulativeDepositInterest =
			SPOT_MARKET_CUMULATIVE_INTEREST_PRECISION;
		mockSpot.depositBalance = new BN(100).mul(QUOTE_PRECISION);
		const currentDeposits = getTokenAmount(
			mockSpot.depositBalance,
			mockSpot,
			SpotBalanceType.DEPOSIT
		);
		const pct = BPS_PRECISION.divn(10).toNumber(); // 1000 bps = 10%/day

		// disabled => always allowed even with twap far below current
		mockSpot.maxDepositBpsPerDay = 0;
		mockSpot.depositTokenTwap = currentDeposits.divn(2);
		assert(checkDepositLimits(mockSpot) === true);

		// current == twap, 10% headroom => allowed
		mockSpot.maxDepositBpsPerDay = pct;
		mockSpot.depositTokenTwap = currentDeposits;
		assert(checkDepositLimits(mockSpot) === true);

		// current is 2x the twap (way over 110% cap) => rejected
		mockSpot.depositTokenTwap = currentDeposits.divn(2);
		assert(checkDepositLimits(mockSpot) === false);

		// but a high guard threshold lifts the cap above current => allowed
		mockSpot.depositGuardThreshold = currentDeposits.muln(2);
		assert(checkDepositLimits(mockSpot) === true);
	});

	function buildWithdrawLimitMarket(poolId: number) {
		const mockSpot = _.cloneDeep(mockSpotMarkets[0]);
		mockSpot.decimals = 9;
		mockSpot.cumulativeDepositInterest = new BN(10).pow(new BN(10));
		mockSpot.cumulativeBorrowInterest = new BN(10).pow(new BN(10));
		mockSpot.depositBalance = new BN(100000);
		mockSpot.borrowBalance = new BN(10000);
		mockSpot.depositTokenTwap = new BN(70000);
		mockSpot.borrowTokenTwap = new BN(10000);
		mockSpot.lastTwapTs = new BN(0);
		mockSpot.optimalUtilization = 900000;
		mockSpot.utilizationTwap = new BN(0);
		mockSpot.withdrawGuardThreshold = new BN(0);
		mockSpot.maxTokenBorrowsFraction = 0;
		mockSpot.poolId = poolId;
		return mockSpot;
	}

	it('withdraw limit (main pool) uses lesserDepositAmount with /3, /5, /14', () => {
		// depositTokenTwapLive works out to 85000 (< the 100000 raw deposit
		// amount), so this pins both the divisors and that the twap-min'd
		// amount -- not the raw deposit amount -- feeds the first max() term
		const mockSpot = buildWithdrawLimitMarket(0);
		const now = new BN(43200); // half of the 24h twap window since lastTwapTs

		const result = calculateWithdrawLimit(mockSpot, now);
		assert(result.maxBorrowAmount.eq(new BN(28333)));
		assert(result.borrowLimit.eq(new BN(18333)));
	});

	it('reserves the insurance fund receivable from the withdraw and borrow limits', () => {
		// Free liquidity is deposits 100000 - borrows 10000 = 90000. A receivable
		// of 85000 leaves 5000 that may leave the vault, which is below every
		// other limit the market carries.
		const mockSpot = buildWithdrawLimitMarket(0);
		mockSpot.insuranceFundRevenueReceivable.scaledBalance = new BN(85000);
		const now = new BN(43200);

		const result = calculateWithdrawLimit(mockSpot, now);
		assert(result.withdrawLimit.eq(new BN(5000)));
		assert(result.exceptionWithdrawLimit.eq(new BN(5000)));
		assert(result.borrowLimit.eq(new BN(5000)));
	});

	it('leaves the limits alone when the market holds no receivable', () => {
		const mockSpot = buildWithdrawLimitMarket(0);
		mockSpot.insuranceFundRevenueReceivable.scaledBalance = new BN(0);
		const now = new BN(43200);

		const result = calculateWithdrawLimit(mockSpot, now);
		assert(result.borrowLimit.eq(new BN(18333)));
	});

	it('withdraw limit (isolated pool) uses lesserDepositAmount with /2, /3, /20', () => {
		const mockSpot = buildWithdrawLimitMarket(1);
		const now = new BN(43200);

		const result = calculateWithdrawLimit(mockSpot, now);
		assert(result.maxBorrowAmount.eq(new BN(42500)));
		assert(result.borrowLimit.eq(new BN(32500)));
	});

	// exceptionWithdrawLimit mirrors `exception_floor` in the program's
	// `check_withdraw_limits`. Accounts that pass the per-account eligibility
	// predicate share one `withdrawGuardThreshold` of room below the breaker
	// floor. The numbers below are the shipped USDT config: a 500_000 USDT market
	// with a 9_500 USDT guard threshold and the default 2500 bps breaker, which
	// floors deposits at 375_000 USDT.
	const USDT_GUARD_THRESHOLD = new BN(9_500).mul(QUOTE_PRECISION);
	const USDT_DEPOSIT_TWAP = new BN(500_000).mul(QUOTE_PRECISION);
	const USDT_BREAKER_FLOOR = new BN(375_000).mul(QUOTE_PRECISION);

	function buildExceptionBudgetMarket(depositTokens: BN) {
		const mockSpot = _.cloneDeep(mockSpotMarkets[0]);
		mockSpot.decimals = 6;
		mockSpot.cumulativeDepositInterest = new BN(10).pow(new BN(10));
		mockSpot.cumulativeBorrowInterest = new BN(10).pow(new BN(10));
		// 6 decimals with cumulative interest at precision: 1000 balance units
		// per token unit.
		mockSpot.depositBalance = depositTokens.muln(1000);
		mockSpot.borrowBalance = ZERO;
		mockSpot.depositTokenTwap = USDT_DEPOSIT_TWAP;
		mockSpot.borrowTokenTwap = ZERO;
		mockSpot.utilizationTwap = ZERO;
		mockSpot.optimalUtilization = 0;
		mockSpot.withdrawGuardThreshold = USDT_GUARD_THRESHOLD;
		mockSpot.withdrawCircuitBreakerBps = 2_500;
		mockSpot.maxTokenBorrowsFraction = 0;
		mockSpot.poolId = 0;
		// lastTwapTs === now makes the projected live TWAP equal the stored TWAP,
		// so the breaker floor is exact.
		mockSpot.lastTwapTs = new BN(86400);
		return mockSpot;
	}

	it('exception budget is exactly one withdrawGuardThreshold at the breaker floor', () => {
		const mockSpot = buildExceptionBudgetMarket(USDT_BREAKER_FLOOR);
		const result = calculateWithdrawLimit(mockSpot, new BN(86400));

		// The market is on the floor, so there is no ordinary room left.
		assert(result.minDepositAmount.eq(USDT_BREAKER_FLOOR));
		assert(result.withdrawLimit.eq(ZERO));
		// The exception releases one guard threshold and no more.
		assert(
			result.exceptionWithdrawLimit.eq(USDT_GUARD_THRESHOLD),
			`expected ${USDT_GUARD_THRESHOLD.toString()}, got ${result.exceptionWithdrawLimit.toString()}`
		);
	});

	it('exception budget shrinks by what the cohort already withdrew', () => {
		// The cohort already took 9_400 USDT of the 9_500 USDT budget.
		const spent = new BN(9_400).mul(QUOTE_PRECISION);
		const mockSpot = buildExceptionBudgetMarket(USDT_BREAKER_FLOOR.sub(spent));
		const result = calculateWithdrawLimit(mockSpot, new BN(86400));

		assert(result.withdrawLimit.eq(ZERO));
		assert(
			result.exceptionWithdrawLimit.eq(new BN(100).mul(QUOTE_PRECISION)),
			`expected 100 USDT, got ${result.exceptionWithdrawLimit.toString()}`
		);
	});

	it('exception budget is zero once the cohort spent one guard threshold', () => {
		const mockSpot = buildExceptionBudgetMarket(
			USDT_BREAKER_FLOOR.sub(USDT_GUARD_THRESHOLD)
		);
		const result = calculateWithdrawLimit(mockSpot, new BN(86400));

		assert(result.withdrawLimit.eq(ZERO));
		assert(result.exceptionWithdrawLimit.eq(ZERO));
	});

	it('exception budget adds no room when there is no withdrawGuardThreshold', () => {
		// A zero guard threshold removes the carve-out. The exception limit then
		// equals the ordinary withdraw limit, so an eligible account gets nothing
		// extra.
		const mockSpot = buildExceptionBudgetMarket(USDT_BREAKER_FLOOR);
		mockSpot.withdrawGuardThreshold = ZERO;
		const result = calculateWithdrawLimit(mockSpot, new BN(86400));

		assert(result.exceptionWithdrawLimit.eq(result.withdrawLimit));
		assert(result.exceptionWithdrawLimit.eq(ZERO));
	});

	it('getTokenValue floors (rounds toward -infinity) for a negative product', () => {
		// -3 * 5 = -15; -15/10 truncates to -1 but floors to -2
		const value = getTokenValue(new BN(-3), 1, { price: new BN(5) });
		assert(value.eq(new BN(-2)));
	});

	it('getStrictTokenValue floors (rounds toward -infinity) for a negative product', () => {
		const strictPrice = new StrictOraclePrice(new BN(5), new BN(5));
		const value = getStrictTokenValue(new BN(-3), 1, strictPrice);
		assert(value.eq(new BN(-2)));
	});

	it('maxSpotInterestStalenessForMargin shrinks as the rate ceiling rises', () => {
		const withCeiling = (maxBorrowRate: number, minBorrowRate = 0) =>
			({
				maxBorrowRate,
				minBorrowRate,
			}) as SpotMarketAccount;

		// A low rate earns more than an hour, and the cap keeps it at an hour.
		assert(
			maxSpotInterestStalenessForMargin(withCeiling(200_000)).eq(
				MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN
			)
		);

		// 100% APR: one basis point of the debt takes 3,153s to accrue. 1,000% APR
		// takes a tenth of that, where a fixed hour would have hidden ten times the
		// allowed share.
		assert(
			maxSpotInterestStalenessForMargin(withCeiling(1_000_000)).eq(
				new BN(3_153)
			)
		);
		assert(
			maxSpotInterestStalenessForMargin(withCeiling(10_000_000)).eq(new BN(315))
		);

		// `calculateInterestRate` floors its result at `minBorrowRate`, so the ceiling
		// is the larger of the two fields. `minBorrowRate` counts in half percent, so
		// 40 is 20% APR — here above a `maxBorrowRate` of 1%.
		assert(
			maxSpotInterestStalenessForMargin(withCeiling(10_000, 40)).eq(
				maxSpotInterestStalenessForMargin(withCeiling(200_000))
			)
		);

		// A market that charges nothing cannot understate anything.
		assert(
			maxSpotInterestStalenessForMargin(withCeiling(0)).eq(
				MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN
			)
		);

		// The window is exactly the span in which the ceiling accrues the allowed
		// share, so the hidden share at the window can never exceed it.
		for (const maxBorrowRate of [1_000_000, 10_000_000, 123_456_789]) {
			const window = maxSpotInterestStalenessForMargin(
				withCeiling(maxBorrowRate)
			);
			const hiddenShare = new BN(maxBorrowRate).mul(window).div(ONE_YEAR);
			assert(hiddenShare.lte(MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN));
		}
	});
});
