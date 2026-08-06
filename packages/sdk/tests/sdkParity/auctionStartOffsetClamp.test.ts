import * as _ from 'lodash';
import {
	BN,
	ContractTier,
	getAuctionEndMinMaxDivisors,
	getPerpBaselineMaxPriceOffset,
	getTriggerAuctionStartPrice,
	isVariant,
	PositionDirection,
	PRICE_PRECISION,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';
import { assert } from '../../src/assert/assert';

// Mirrors the OtterSec #146 clamp in OrderParams::get_perp_baseline_start_price_offset
// (state/order_params.rs): the baseline auction start offset is clamped to
// ±(oracle TWAP / maxDivisor), the tier auction-width band.
//
// Each case pairs an in-band market with an out-of-band market built the same way, so a passing
// assertion pins the clamp and not some other bound. The in-band case must return the raw offset.
//
// The fixtures move the bid/ask TWAP and the 5min mark TWAP together. The program then takes its
// blend path and the SDK its only path, and with zero AMM spreads both return the premium exactly,
// so the fixture is not sensitive to the SDK's missing 50bps fast-only branch.
describe('baseline auction start offset clamp parity', () => {
	const ORACLE_TWAP = new BN(100).mul(PRICE_PRECISION);
	// getTriggerAuctionStartPrice applies a -500 bps (tier A/B) or -3500 bps start buffer on top of
	// the offset; recovering the offset means subtracting it back out.
	const START_BUFFER_A_B = ORACLE_TWAP.muln(500).div(PRICE_PRECISION);
	const START_BUFFER_OTHER = ORACLE_TWAP.muln(3500).div(PRICE_PRECISION);

	function makeMarket(contractTier: ContractTier, markPremium: BN) {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		market.contractTier = contractTier;
		market.marketStats.lastBidPriceTwap = ORACLE_TWAP.add(markPremium);
		market.marketStats.lastAskPriceTwap = ORACLE_TWAP.add(markPremium);
		market.marketStats.lastMarkPriceTwap5Min = ORACLE_TWAP.add(markPremium);
		market.marketStats.volume24H = new BN(1_000_000).mul(PRICE_PRECISION);
		market.marketStats.historicalOracleData.lastOraclePrice = ORACLE_TWAP;
		market.marketStats.historicalOracleData.lastOraclePriceTwap = ORACLE_TWAP;
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min =
			ORACLE_TWAP;
		return market;
	}

	// Offset the function actually applied, with the directional start buffer removed.
	function appliedOffset(
		market: ReturnType<typeof makeMarket>,
		direction: PositionDirection,
		startBuffer: BN
	) {
		const startPrice = getTriggerAuctionStartPrice({
			perpMarket: market,
			direction,
			oraclePrice: ORACLE_TWAP,
		});
		const offsetPlusBuffer = startPrice.sub(ORACLE_TWAP);

		// The buffer is negative, so a long start price sits above the offset and a short one below.
		return isVariant(direction, 'long')
			? offsetPlusBuffer.sub(startBuffer)
			: offsetPlusBuffer.add(startBuffer);
	}

	it('matches the program tier bands', () => {
		const bands: Array<[ContractTier, number, number]> = [
			[ContractTier.A, 1000, 50],
			[ContractTier.B, 1000, 20],
			[ContractTier.C, 500, 20],
			[ContractTier.SPECULATIVE, 100, 10],
			[ContractTier.HIGHLY_SPECULATIVE, 50, 5],
			[ContractTier.ISOLATED, 50, 5],
		];

		for (const [contractTier, minDivisor, maxDivisor] of bands) {
			const market = makeMarket(contractTier, new BN(0));
			const divisors = getAuctionEndMinMaxDivisors(market);
			assert(divisors.minDivisor === minDivisor);
			assert(divisors.maxDivisor === maxDivisor);
			assert(
				getPerpBaselineMaxPriceOffset(market).eq(ORACLE_TWAP.divn(maxDivisor))
			);
		}
	});

	it('clamps a tier A long offset and leaves an in-band one alone', () => {
		const inBand = makeMarket(ContractTier.A, PRICE_PRECISION);
		assert(
			appliedOffset(inBand, PositionDirection.LONG, START_BUFFER_A_B).eq(
				PRICE_PRECISION
			)
		);

		const outOfBand = makeMarket(ContractTier.A, PRICE_PRECISION.muln(10));
		assert(
			appliedOffset(outOfBand, PositionDirection.LONG, START_BUFFER_A_B).eq(
				PRICE_PRECISION.muln(2) // 2% = oracle TWAP / 50
			)
		);
	});

	it('clamps the short side symmetrically', () => {
		const inBand = makeMarket(ContractTier.A, PRICE_PRECISION.neg());
		assert(
			appliedOffset(inBand, PositionDirection.SHORT, START_BUFFER_A_B).eq(
				PRICE_PRECISION.neg()
			)
		);

		const outOfBand = makeMarket(
			ContractTier.A,
			PRICE_PRECISION.muln(10).neg()
		);
		assert(
			appliedOffset(outOfBand, PositionDirection.SHORT, START_BUFFER_A_B).eq(
				PRICE_PRECISION.muln(2).neg()
			)
		);
	});

	it('gives riskier tiers a wider bound', () => {
		const inBand = makeMarket(
			ContractTier.HIGHLY_SPECULATIVE,
			PRICE_PRECISION.muln(10)
		);
		assert(
			appliedOffset(inBand, PositionDirection.LONG, START_BUFFER_OTHER).eq(
				PRICE_PRECISION.muln(10)
			)
		);

		const outOfBand = makeMarket(
			ContractTier.HIGHLY_SPECULATIVE,
			PRICE_PRECISION.muln(30)
		);
		assert(
			appliedOffset(outOfBand, PositionDirection.LONG, START_BUFFER_OTHER).eq(
				PRICE_PRECISION.muln(20) // 20% = oracle TWAP / 5
			)
		);
	});

	// The low-volume fallback divides a mark TWAP, not the oracle TWAP, so it also needs the clamp.
	// volume24H = 0 selects it, and tier A then uses lastBidPriceTwap / 500.
	it('clamps the low volume fallback path', () => {
		const inBand = makeMarket(ContractTier.A, new BN(0));
		inBand.marketStats.volume24H = new BN(0);
		inBand.marketStats.lastBidPriceTwap = new BN(500).mul(PRICE_PRECISION);
		assert(
			appliedOffset(inBand, PositionDirection.LONG, START_BUFFER_A_B).eq(
				PRICE_PRECISION // 500 / 500 = 1% of oracle, in band
			)
		);

		const outOfBand = makeMarket(ContractTier.A, new BN(0));
		outOfBand.marketStats.volume24H = new BN(0);
		outOfBand.marketStats.lastBidPriceTwap = new BN(2000).mul(PRICE_PRECISION);
		assert(
			appliedOffset(outOfBand, PositionDirection.LONG, START_BUFFER_A_B).eq(
				PRICE_PRECISION.muln(2) // raw 4%, cut to 2%
			)
		);
	});
});
