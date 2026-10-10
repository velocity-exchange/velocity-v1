import {
	BN,
	BASE_PRECISION,
	MMOraclePriceData,
	PEG_PRECISION,
	PositionDirection,
	QUOTE_PRECISION,
	calculateAskPrice,
	calculateReservePrice,
	calculateTradeSlippage,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';
import { assert } from '../../src/assert/assert';
import * as _ from 'lodash';

// The program always fills against the cached spread reserves. A market with base spread 0 still
// has dynamic spreads when its curve update intensity is nonzero, so the SDK prices through them.
describe('zero base spread pricing', () => {
	it('prices a trade through the spread reserves when base spread is 0', () => {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		const amm = market.amm;
		amm.baseAssetReserve = new BN(1000).mul(BASE_PRECISION);
		amm.quoteAssetReserve = new BN(1000).mul(BASE_PRECISION);
		amm.sqrtK = new BN(1000).mul(BASE_PRECISION);
		amm.pegMultiplier = new BN(10).mul(PEG_PRECISION);
		amm.minBaseAssetReserve = new BN(500).mul(BASE_PRECISION);
		amm.maxBaseAssetReserve = new BN(2000).mul(BASE_PRECISION);
		amm.terminalQuoteAssetReserve = amm.quoteAssetReserve;
		amm.curveUpdateIntensity = 100;
		amm.maxSpread = 25000;
		amm.baseSpread = 0;
		amm.totalFeeMinusDistributions = new BN(1000).mul(QUOTE_PRECISION);

		const mmOraclePriceData = {
			price: new BN(10_000_000),
			slot: new BN(0),
			confidence: new BN(1000),
			hasSufficientNumberOfDataPoints: true,
			isMMOracleActive: true,
		} as MMOraclePriceData;
		market.marketStats.historicalOracleData.lastOraclePrice =
			mmOraclePriceData.price;
		market.marketStats.historicalOracleData.lastOraclePriceTwap =
			mmOraclePriceData.price;
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min =
			mmOraclePriceData.price;

		const ask = calculateAskPrice(market, mmOraclePriceData);
		assert(ask.gt(calculateReservePrice(market, mmOraclePriceData)));

		const amount = new BN(10).mul(QUOTE_PRECISION);
		const [, , entryWithSpread] = calculateTradeSlippage(
			PositionDirection.LONG,
			amount,
			market,
			'quote',
			mmOraclePriceData,
			true
		);
		const [, , entryOnCurve] = calculateTradeSlippage(
			PositionDirection.LONG,
			amount,
			market,
			'quote',
			mmOraclePriceData,
			false
		);
		assert(entryWithSpread.gt(entryOnCurve));
		assert(entryWithSpread.gte(ask));
	});
});
