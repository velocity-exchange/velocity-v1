import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import _ from 'lodash';
import {
	calculateAllEstimatedFundingRate,
	MARK_TWAP_RESEED_FUNDING_PERIODS,
	ONE_HOUR,
	PRICE_PRECISION,
} from '../../src';
import { MMOraclePriceData } from '../../src/oracles/types';
import { mockPerpMarkets } from '../dlob/helpers';

/**
 * Mirror of `MarketStats::update_mark_twap`: a mark TWAP left unwritten for several
 * funding periods holds no usable history, so the program discards it and re-seeds
 * from the oracle TWAP. The estimate must not predict a premium the next on-chain
 * update will not charge.
 *
 * The gap is longest after a funding pause, because both funding cranks reject while
 * the pause is set.
 */
describe('mark twap re-seed', () => {
	const NOW = new BN(1_688_878_353);
	const ORACLE = new BN(2 * PRICE_PRECISION.toNumber());
	// A 10% mark premium, so a stale projection and a re-seeded one differ plainly.
	const STALE_MARK = new BN(2.2 * PRICE_PRECISION.toNumber());

	function marketWithMarkTwapAge(secondsSinceLastWrite: BN) {
		const market = _.cloneDeep(mockPerpMarkets)[0];

		market.marketStats.fundingPeriod = ONE_HOUR;
		market.lastFundingRateTs = NOW.sub(ONE_HOUR);
		market.marketStats.lastMarkPriceTwap = STALE_MARK;
		market.marketStats.lastMarkPriceTwap5Min = STALE_MARK;
		market.marketStats.lastBidPriceTwap = STALE_MARK;
		market.marketStats.lastAskPriceTwap = STALE_MARK;
		market.marketStats.lastMarkPriceTwapTs = NOW.sub(secondsSinceLastWrite);
		market.marketStats.historicalOracleData.lastOraclePrice = ORACLE;
		market.marketStats.historicalOracleData.lastOraclePriceTwap = ORACLE;
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min = ORACLE;
		// Both stamps move together, as they do when the funding crank is the only
		// writer. `shrinkStaleTwaps` reads the difference between them, so leaving one
		// behind would drag the mark TWAP onto the oracle for an unrelated reason and
		// hide what these cases measure.
		market.marketStats.historicalOracleData.lastOraclePriceTwapTs =
			market.marketStats.lastMarkPriceTwapTs;

		return market;
	}

	const mmOraclePriceData: MMOraclePriceData = {
		price: ORACLE,
		slot: new BN(0),
		confidence: new BN(1),
		hasSufficientNumberOfDataPoints: true,
		isMMOracleActive: true,
	};

	/// The mark price is held at the premium, not at the oracle, so every case here
	/// separates a re-seeded estimate from a blended one. A mark price equal to the
	/// oracle would make the blend land on the oracle by itself.
	function estimatedMarkTwap(secondsSinceLastWrite: BN): BN {
		const [markTwap] = calculateAllEstimatedFundingRate(
			marketWithMarkTwapAge(secondsSinceLastWrite),
			mmOraclePriceData,
			{ price: ORACLE },
			STALE_MARK,
			NOW
		);
		return markTwap;
	}

	it('discards the stored mark twap past the threshold', () => {
		// Four hours against a one-hour funding period, stated outright rather than
		// derived from the threshold, so this case still fails if the re-seed goes away.
		assert(estimatedMarkTwap(ONE_HOUR.muln(4)).eq(ORACLE));
	});

	it('re-seeds one second past the threshold and not at it', () => {
		const threshold = ONE_HOUR.mul(MARK_TWAP_RESEED_FUNDING_PERIODS);

		assert(estimatedMarkTwap(threshold).gt(ORACLE));
		assert(estimatedMarkTwap(threshold.addn(1)).eq(ORACLE));
	});

	it('keeps the stored mark twap at the normal funding cadence', () => {
		// `on_the_hour_update` can stretch one legitimate interval to 5/3 of a period,
		// and a market cranked that late still holds usable history.
		const markTwap = estimatedMarkTwap(ONE_HOUR.muln(5).divn(3));

		assert(markTwap.gt(ORACLE));
	});

	it('floors the threshold at one hour when the funding period is zero', () => {
		const market = marketWithMarkTwapAge(ONE_HOUR.addn(1));
		market.marketStats.fundingPeriod = new BN(0);

		const [markTwap] = calculateAllEstimatedFundingRate(
			market,
			mmOraclePriceData,
			{ price: ORACLE },
			STALE_MARK,
			NOW
		);

		assert(markTwap.eq(ORACLE));
	});
});
