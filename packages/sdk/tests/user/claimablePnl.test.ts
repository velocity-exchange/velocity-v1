import {
	BN,
	BASE_PRECISION,
	OraclePriceData,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	SpotBalanceType,
	ZERO,
	calculateClaimablePnl,
	calculatePositionPNL,
	getTokenAmount,
} from '../../src';
import {
	mockPerpMarkets,
	mockPerpPosition,
	mockSpotMarkets,
} from '../dlob/helpers';
import { assert } from '../../src/assert/assert';
import * as _ from 'lodash';

// Mirrors the excess settle_pnl allows in programs/velocity/src/controller/pnl.rs: pnl pool tokens
// minus net user pnl floored at zero. The AMM fee pool is never a settlement buffer.
describe('calculateClaimablePnl', () => {
	it('caps unrealized pnl at the pnl pool excess, ignoring the fee pool', () => {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		const spotMarket = _.cloneDeep(mockSpotMarkets[0]);
		const oraclePriceData = {
			price: new BN(100).mul(PRICE_PRECISION),
		} as OraclePriceData;

		// Net user pnl is -50: users owe the pool, so nothing is reserved for them.
		market.amm.baseAssetAmountWithAmm = ZERO;
		market.quoteAssetAmount = new BN(-50).mul(QUOTE_PRECISION);
		market.netUnsettledFundingPnl = ZERO;
		market.pnlPool.scaledBalance = new BN(20).mul(BASE_PRECISION);
		market.amm.feePool.scaledBalance = new BN(1000).mul(BASE_PRECISION);
		market.amm.cumulativeFundingRateLong = ZERO;
		market.amm.cumulativeFundingRateShort = ZERO;

		// 1 base long entered at 5, worth 100 now, nothing realized yet.
		const position = _.cloneDeep(mockPerpPosition);
		position.baseAssetAmount = BASE_PRECISION;
		position.quoteAssetAmount = new BN(-5).mul(QUOTE_PRECISION);
		position.quoteEntryAmount = new BN(-5).mul(QUOTE_PRECISION);
		position.lastCumulativeFundingRate = ZERO;

		const unrealized = calculatePositionPNL(
			market,
			position,
			true,
			oraclePriceData
		);
		const pnlPool = getTokenAmount(
			market.pnlPool.scaledBalance,
			spotMarket,
			SpotBalanceType.DEPOSIT
		);
		assert(unrealized.gt(pnlPool));

		const claimable = calculateClaimablePnl(
			market,
			spotMarket,
			position,
			oraclePriceData
		);
		assert(claimable.eq(pnlPool));
	});
});
