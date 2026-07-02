import * as _ from 'lodash';
import {
	BN,
	OracleGuardRails,
	OracleValidity,
	PRICE_PRECISION,
	getOracleValidity,
	isOracleTooDivergent,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';
import { assert } from '../../src/assert/assert';

// Pins the UseMMOraclePrice gating semantics from
// `programs/velocity/src/state/perp_market.rs::get_mm_oracle_price_data`: the
// fallback to the exchange oracle is driven by `is_oracle_valid_for_action`
// (NonPositive/TooVolatile only), never by the twap-5min divergence band that
// `isOracleTooDivergent` mirrors elsewhere (`validate_fill_price_within_price_bands`).
describe('MM oracle validity gate (UseMMOraclePrice semantics)', () => {
	it('does not fall back to the exchange oracle solely because mm price diverged from a stale 5min twap', () => {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		market.marketStats.historicalOracleData.lastOraclePriceTwap = new BN(
			100
		).mul(PRICE_PRECISION);
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min = new BN(
			100
		).mul(PRICE_PRECISION);

		// Market has moved a long way from the stale 5min twap, but the mm
		// oracle and the current exchange oracle agree closely with each other.
		const exchangeOraclePrice = new BN(160).mul(PRICE_PRECISION);
		const mmOraclePrice = new BN(161).mul(PRICE_PRECISION);
		const mmOracleSlot = new BN(1000);
		const mmOracleConfidence = new BN(1000);

		const oracleGuardRails: OracleGuardRails = {
			priceDivergence: {
				markOraclePercentDivergence: new BN(0),
				oracleTwap5MinPercentDivergence: new BN(0),
			},
			validity: {
				slotsBeforeStaleForAmm: new BN(10),
				slotsBeforeStaleForMargin: new BN(60),
				confidenceIntervalMaxSize: new BN(20000),
				tooVolatileRatio: new BN(5),
			},
		};

		// Old (incorrect) gate: mirrors validate_fill_price_within_price_bands,
		// which is NOT what the program checks before using the mm oracle price.
		const wasTooDivergentUnderOldGate = isOracleTooDivergent(
			market.marketStats,
			{
				price: mmOraclePrice,
				slot: mmOracleSlot,
				confidence: mmOracleConfidence,
				hasSufficientNumberOfDataPoints: true,
			},
			oracleGuardRails
		);
		assert(
			wasTooDivergentUnderOldGate,
			'expected the twap5min-divergence check to trip on the stale twap'
		);

		// Correct gate: is_oracle_valid_for_action(mm_oracle_validity, UseMMOraclePrice)
		// only rejects NonPositive/TooVolatile.
		const mmOracleValidity = getOracleValidity(
			market,
			{
				price: mmOraclePrice,
				slot: mmOracleSlot,
				confidence: mmOracleConfidence,
				hasSufficientNumberOfDataPoints: true,
			},
			oracleGuardRails,
			mmOracleSlot
		);
		const isMMOracleInvalidForUse =
			mmOracleValidity === OracleValidity.NonPositive ||
			mmOracleValidity === OracleValidity.TooVolatile;

		assert(
			!isMMOracleInvalidForUse,
			`expected mm oracle to remain valid for UseMMOraclePrice, got validity=${OracleValidity[mmOracleValidity]}`
		);

		// Sanity: the exchange oracle is not materially different from the mm
		// oracle, so the 1% mm-vs-exchange fallback threshold used alongside this
		// gate would not itself trigger a fallback either.
		const pctDiff = mmOraclePrice
			.sub(exchangeOraclePrice)
			.abs()
			.mul(new BN(1_000_000))
			.div(exchangeOraclePrice);
		assert(pctDiff.lt(new BN(10_000)), 'expected mm/exchange prices within 1%');
	});

	it('does fall back when the mm oracle itself is too volatile vs its own twap', () => {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		market.marketStats.historicalOracleData.lastOraclePriceTwap = new BN(
			100
		).mul(PRICE_PRECISION);
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min = new BN(
			100
		).mul(PRICE_PRECISION);

		const mmOraclePrice = new BN(600).mul(PRICE_PRECISION); // 6x the twap
		const mmOracleSlot = new BN(1000);
		const mmOracleConfidence = new BN(1000);

		const oracleGuardRails: OracleGuardRails = {
			priceDivergence: {
				markOraclePercentDivergence: new BN(0),
				oracleTwap5MinPercentDivergence: new BN(0),
			},
			validity: {
				slotsBeforeStaleForAmm: new BN(10),
				slotsBeforeStaleForMargin: new BN(60),
				confidenceIntervalMaxSize: new BN(20000),
				tooVolatileRatio: new BN(5),
			},
		};

		const mmOracleValidity = getOracleValidity(
			market,
			{
				price: mmOraclePrice,
				slot: mmOracleSlot,
				confidence: mmOracleConfidence,
				hasSufficientNumberOfDataPoints: true,
			},
			oracleGuardRails,
			mmOracleSlot
		);

		assert(mmOracleValidity === OracleValidity.TooVolatile);
	});
});
