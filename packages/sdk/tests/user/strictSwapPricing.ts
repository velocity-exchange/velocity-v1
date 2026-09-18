import {
	BN,
	User,
	PublicKey,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	SPOT_MARKET_BALANCE_PRECISION,
	SPOT_MARKET_WEIGHT_PRECISION,
	SpotBalanceType,
	ZERO,
} from '../../src';
import { mockPerpMarkets, mockSpotMarkets } from '../fixtures/mockAccounts';
import { assert } from '../../src/assert/assert';
import {
	mockUserAccount as baseMockUserAccount,
	makeMockUser,
} from './helpers';
import * as _ from 'lodash';

const USDT_MARKET_INDEX = 0;
const SOL_MARKET_INDEX = 1;
const USDT_ORACLE_PRICE = 1;
const SOL_ORACLE_PRICE = 100;
const SOL_PRECISION = new BN(1_000_000_000);

function priceBN(price: number): BN {
	return new BN(price * PRICE_PRECISION.toNumber());
}

function assertClose(actual: BN, expected: BN, tolerance: BN, label: string) {
	assert(
		actual.sub(expected).abs().lte(tolerance),
		`${label}: ${actual.toString()} not within ${tolerance.toString()} of ${expected.toString()}`
	);
}

/**
 * USDT (market 0, $1) and SOL (market 1, $100) with signed balances in whole
 * tokens. `solTwap5Min` is the SOL market's *stored* 5min TWAP; every mock
 * market leaves `lastOraclePriceTwapTs` at 0, so a live-projected TWAP would
 * collapse back onto the oracle price here.
 */
async function makeSwapUser({
	solTwap5Min,
	usdtTokens = 0,
	solTokens = 0,
}: {
	solTwap5Min: number;
	usdtTokens?: number;
	solTokens?: number;
}): Promise<User> {
	const myMockPerpMarkets = _.cloneDeep(mockPerpMarkets);
	const myMockSpotMarkets = _.cloneDeep(mockSpotMarkets);
	const myMockUserAccount = _.cloneDeep(baseMockUserAccount);

	// distinct oracle per market: all mock markets otherwise share the
	// default pubkey, which would collapse their prices onto one entry
	const usdtMarket = myMockSpotMarkets[USDT_MARKET_INDEX];
	usdtMarket.oracle = new PublicKey(10);
	usdtMarket.initialAssetWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
	usdtMarket.initialLiabilityWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
	usdtMarket.historicalOracleData.lastOraclePriceTwap5Min =
		priceBN(USDT_ORACLE_PRICE);

	const solMarket = myMockSpotMarkets[SOL_MARKET_INDEX];
	solMarket.oracle = new PublicKey(11);
	solMarket.initialAssetWeight = 8000;
	solMarket.initialLiabilityWeight = 12000;
	solMarket.historicalOracleData.lastOraclePriceTwap5Min = priceBN(solTwap5Min);

	for (const [index, tokens] of [
		[USDT_MARKET_INDEX, usdtTokens],
		[SOL_MARKET_INDEX, solTokens],
	] as const) {
		const position = myMockUserAccount.spotPositions[index];
		position.marketIndex = index;
		position.balanceType =
			tokens < 0 ? SpotBalanceType.BORROW : SpotBalanceType.DEPOSIT;
		position.scaledBalance = new BN(Math.abs(tokens)).mul(
			SPOT_MARKET_BALANCE_PRECISION
		);
	}

	const spotOraclePrices = myMockSpotMarkets.map(() => 1);
	spotOraclePrices[USDT_MARKET_INDEX] = USDT_ORACLE_PRICE;
	spotOraclePrices[SOL_MARKET_INDEX] = SOL_ORACLE_PRICE;

	return makeMockUser(
		myMockPerpMarkets,
		myMockSpotMarkets,
		myMockUserAccount,
		myMockPerpMarkets.map(() => 1),
		spotOraclePrices
	);
}

describe('strict swap pricing uses the stored 5min oracle twap', () => {
	it('getMaxSwapAmount values the bought asset at min(oracle, stored twap)', async () => {
		// 1000 USDT of collateral swapped into SOL at 0.8 initial asset weight:
		// free collateral hits zero at 1000 / (1 - 0.8 * twap/oracle) USDT in
		const control = await makeSwapUser({
			solTwap5Min: SOL_ORACLE_PRICE,
			usdtTokens: 1000,
		});
		const { inAmount: controlIn } = control.getMaxSwapAmount({
			inMarketIndex: USDT_MARKET_INDEX,
			outMarketIndex: SOL_MARKET_INDEX,
		});
		assertClose(
			controlIn,
			new BN(5000).mul(QUOTE_PRECISION),
			new BN(2).mul(QUOTE_PRECISION),
			'max swap in with twap == oracle'
		);

		// stored twap $90: each SOL bought is worth 10% less as collateral
		const discounted = await makeSwapUser({
			solTwap5Min: 90,
			usdtTokens: 1000,
		});
		const { inAmount: discountedIn } = discounted.getMaxSwapAmount({
			inMarketIndex: USDT_MARKET_INDEX,
			outMarketIndex: SOL_MARKET_INDEX,
		});
		assertClose(
			discountedIn,
			new BN(3571_428_571),
			new BN(2).mul(QUOTE_PRECISION),
			'max swap in with a discounted stored twap'
		);
	});

	it('getMaxSwapAmount values the resulting borrow at max(oracle, stored twap)', async () => {
		// 10 SOL sold into USDT: past 10 SOL the swap opens a SOL borrow at 1.2
		// initial liability weight, so free collateral hits zero at
		// (10 * 0.8 * oracle + 10 * 1.2 * twap) / (1.2 * twap - oracle) SOL in
		const control = await makeSwapUser({
			solTwap5Min: SOL_ORACLE_PRICE,
			solTokens: 10,
		});
		const { inAmount: controlIn } = control.getMaxSwapAmount({
			inMarketIndex: SOL_MARKET_INDEX,
			outMarketIndex: USDT_MARKET_INDEX,
		});
		assertClose(
			controlIn,
			new BN(60).mul(SOL_PRECISION),
			new BN(50_000_000),
			'max swap in with twap == oracle'
		);

		// stored twap $110: the SOL borrow is marked 10% higher
		const marked = await makeSwapUser({ solTwap5Min: 110, solTokens: 10 });
		const { inAmount: markedIn } = marked.getMaxSwapAmount({
			inMarketIndex: SOL_MARKET_INDEX,
			outMarketIndex: USDT_MARKET_INDEX,
		});
		assertClose(
			markedIn,
			new BN(41_250_000_000),
			new BN(50_000_000),
			'max swap in with a marked-up stored twap'
		);
	});

	it('accountLeverageAfterSwap keeps its legs on the live oracle basis', async () => {
		// 10 SOL deposit against a 500 USDT borrow, selling 2 SOL for 200 USDT
		const swap = {
			inMarketIndex: SOL_MARKET_INDEX,
			outMarketIndex: USDT_MARKET_INDEX,
			inAmount: new BN(2).mul(SOL_PRECISION),
			outAmount: new BN(200).mul(QUOTE_PRECISION),
		};

		const control = await makeSwapUser({
			solTwap5Min: SOL_ORACLE_PRICE,
			solTokens: 10,
			usdtTokens: -500,
		});
		// $300 borrow / ($800 spot assets - $300 borrow) = 0.6x
		assert(
			control.accountLeverageAfterSwap(swap).eq(new BN(6000)),
			`leverage with twap == oracle: ${control
				.accountLeverageAfterSwap(swap)
				.toString()}`
		);

		// the leverage readout is a delta on getLeverageComponents' live-oracle
		// baseline, so the stored twap must not move it — pricing the delta
		// strictly here would value the remaining 8 SOL at $820, a price that is
		// neither the $100 oracle nor the $90 twap
		const discounted = await makeSwapUser({
			solTwap5Min: 90,
			solTokens: 10,
			usdtTokens: -500,
		});
		assert(
			discounted.accountLeverageAfterSwap(swap).eq(new BN(6000)),
			`leverage with a discounted stored twap: ${discounted
				.accountLeverageAfterSwap(swap)
				.toString()}`
		);

		// a zero-size swap must agree with the account's current leverage
		assert(
			discounted
				.accountLeverageAfterSwap({ ...swap, inAmount: ZERO, outAmount: ZERO })
				.eq(discounted.getLeverage()),
			`zero-size swap diverges from getLeverage: ${discounted
				.accountLeverageAfterSwap({ ...swap, inAmount: ZERO, outAmount: ZERO })
				.toString()} vs ${discounted.getLeverage().toString()}`
		);
	});

	it('getMaxSwapAmount keeps its leverage readout on the live oracle basis', async () => {
		// buying the twap-discounted asset, so the discount binds on the sizing:
		// selling it instead would liquidate the whole SOL deposit and leave a SOL
		// borrow priced at max(100, 90) = the oracle, moving the max by ~0.0002%
		const swap = {
			inMarketIndex: USDT_MARKET_INDEX,
			outMarketIndex: SOL_MARKET_INDEX,
		};

		const control = await makeSwapUser({
			solTwap5Min: SOL_ORACLE_PRICE,
			usdtTokens: 1000,
		});
		const discounted = await makeSwapUser({
			solTwap5Min: 90,
			usdtTokens: 1000,
		});

		const controlMax = control.getMaxSwapAmount(swap);
		const discountedMax = discounted.getMaxSwapAmount(swap);

		// the stored twap bounds how much can be swapped: 1000 / (1 - 0.8 * 0.9)
		// instead of 1000 / (1 - 0.8), a 29% smaller max
		assertClose(
			discountedMax.inAmount,
			new BN(3571_428_571),
			new BN(2).mul(QUOTE_PRECISION),
			'discounted max swap size'
		);
		assertClose(
			controlMax.inAmount,
			new BN(5000).mul(QUOTE_PRECISION),
			new BN(2).mul(QUOTE_PRECISION),
			'control max swap size'
		);

		// each max is a different end state, so the two leverages do NOT converge.
		// what pins the basis is the value of each: the discounted account ends up
		// holding 35.71 SOL against a 2571 USDT borrow, which is 2.57x marked at the
		// $100 oracle and would be 4.00x marked at the $90 twap
		assertClose(
			discountedMax.leverage,
			new BN(25714),
			new BN(20),
			'discounted max swap leverage'
		);
		assertClose(
			controlMax.leverage,
			new BN(40000),
			new BN(20),
			'control max swap leverage'
		);

		// and both helpers agree on that basis for the same amounts
		assertClose(
			discounted.accountLeverageAfterSwap({ ...swap, ...discountedMax }),
			discountedMax.leverage,
			new BN(2),
			'getMaxSwapAmount leverage vs accountLeverageAfterSwap'
		);
		assertClose(
			control.accountLeverageAfterSwap({ ...swap, ...controlMax }),
			controlMax.leverage,
			new BN(2),
			'getMaxSwapAmount leverage vs accountLeverageAfterSwap (control)'
		);
	});
});
