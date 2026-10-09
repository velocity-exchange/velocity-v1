import {
	BN,
	BASE_PRECISION,
	MMOraclePriceData,
	OraclePriceData,
	PRICE_PRECISION,
	ZERO,
	calculateAllEstimatedFundingRate,
	calculateLongShortFundingRate,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';
import { assert } from '../../src/assert/assert';
import * as _ from 'lodash';

// baseAssetAmountShort is stored negative, so the long/short split must compare magnitudes. The
// larger side receives the capped rate and the smaller side pays the uncapped one.
describe('long/short funding rate estimate', () => {
	const now = new BN(1688878353);
	const oraclePrice = 1.9535;
	const price = (x: number) =>
		new BN(Math.round(x * PRICE_PRECISION.toNumber()));

	function estimate(premium: number, longBase: number, shortBase: number) {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		market.marketStats.fundingPeriod = new BN(3600);
		market.lastFundingRateTs = new BN(1688860817);
		const markPrice = oraclePrice * (1 + premium);
		const mmOraclePriceData = {
			price: price(oraclePrice),
			slot: new BN(0),
			confidence: new BN(1),
			hasSufficientNumberOfDataPoints: true,
			isMMOracleActive: true,
		} as MMOraclePriceData;
		const oraclePriceData = mmOraclePriceData as unknown as OraclePriceData;
		market.marketStats.historicalOracleData.lastOraclePrice =
			price(oraclePrice);
		market.marketStats.lastMarkPriceTwap = price(markPrice);
		market.marketStats.lastBidPriceTwap = price(markPrice * 0.999);
		market.marketStats.lastAskPriceTwap = price(markPrice * 1.001);
		market.marketStats.lastMarkPriceTwapTs = new BN(1688877729);
		market.marketStats.historicalOracleData.lastOraclePriceTwap =
			price(oraclePrice);
		market.marketStats.historicalOracleData.lastOraclePriceTwapTs = new BN(
			1688878333
		);
		market.baseAssetAmountLong = new BN(longBase).mul(BASE_PRECISION);
		market.baseAssetAmountShort = new BN(-shortBase).mul(BASE_PRECISION);
		market.amm.totalFeeMinusDistributions = ZERO;

		const [, , , capped, uncapped] = calculateAllEstimatedFundingRate(
			market,
			mmOraclePriceData,
			oraclePriceData,
			price(markPrice),
			now
		);
		const [longRate, shortRate] = calculateLongShortFundingRate(
			market,
			mmOraclePriceData,
			oraclePriceData,
			price(markPrice),
			now
		);
		return { capped, uncapped, longRate, shortRate };
	}

	it('caps the short side when shorts are larger', () => {
		const { capped, uncapped, longRate, shortRate } = estimate(0.01, 1, 10);
		assert(!capped.eq(uncapped));
		assert(longRate.eq(uncapped));
		assert(shortRate.eq(capped));
	});

	it('caps the long side when longs are larger', () => {
		const { capped, uncapped, longRate, shortRate } = estimate(-0.01, 10, 1);
		assert(!capped.eq(uncapped));
		assert(longRate.eq(capped));
		assert(shortRate.eq(uncapped));
	});

	it('pays the uncapped rate on both sides when they are equal', () => {
		const { uncapped, longRate, shortRate } = estimate(0.01, 5, 5);
		assert(longRate.eq(uncapped));
		assert(shortRate.eq(uncapped));
	});
});
