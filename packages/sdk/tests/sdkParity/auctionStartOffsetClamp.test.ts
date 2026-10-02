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
// The fixtures match the program's market_with_mark_5min_premium helper (order_params/tests.rs):
// the bid/ask TWAPs sit at oracle and only the 5min mark TWAP carries the premium, so the fast and
// slow offsets differ by more than 50bps of the 5min TWAP and both implementations return the fast
// offset alone. The asserted numbers are the ones the program's own tests assert.
describe('baseline auction start offset clamp parity', () => {
	const ORACLE_TWAP = new BN(100).mul(PRICE_PRECISION);
	// getTriggerAuctionStartPrice applies a -500 bps (tier A/B) or -3500 bps start buffer on top of
	// the offset; recovering the offset means subtracting it back out.
	const START_BUFFER_A_B = ORACLE_TWAP.muln(500).div(PRICE_PRECISION);
	const START_BUFFER_OTHER = ORACLE_TWAP.muln(3500).div(PRICE_PRECISION);

	function makeMarket(contractTier: ContractTier, markPremium: BN) {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		market.contractTier = contractTier;
		market.marketStats.lastBidPriceTwap = ORACLE_TWAP;
		market.marketStats.lastAskPriceTwap = ORACLE_TWAP;
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

	// The 50bps selector. slow is 0 (bid TWAP at oracle) and fast is the premium, so |slow - fast| is
	// the premium and the threshold is floor((oracle + premium) / 200). 502_512 is the largest
	// premium still inside it: blend, which with zero spreads is min(0, premium) = 0. One unit more
	// leaves the band and the fast offset passes through alone.
	it('switches from the blend to the fast offset past 50bps of divergence', () => {
		const inside = makeMarket(ContractTier.A, new BN(502_512));
		assert(
			appliedOffset(inside, PositionDirection.LONG, START_BUFFER_A_B).eq(
				new BN(0)
			)
		);

		const outside = makeMarket(ContractTier.A, new BN(502_513));
		assert(
			appliedOffset(outside, PositionDirection.LONG, START_BUFFER_A_B).eq(
				new BN(502_513)
			)
		);
	});

	// Inside the band the slow offset is blended with fractions of the AMM's cached per-side spreads,
	// each scaled by the slow mark TWAP / (PRICE_PRECISION * 10). Same inputs and numbers as
	// blends_the_slow_offset_with_the_amm_spreads_inside_the_band in order_params/tests.rs.
	it('blends the slow offset with the AMM spreads inside the band', () => {
		// long: slow 200_000, fast 300_000, |diff| 100_000 <= 100_300_000 / 200.
		// fracLong = 1000 * 100_200_000 / 1e7 = 10_020.
		// fracShort = 2000 * 100_200_000 / 1e7 = 20_040.
		// min(200_000 + 10_020, 300_000 - 20_040) = 210_020.
		const long = makeMarket(ContractTier.A, new BN(300_000));
		long.marketStats.lastBidPriceTwap = ORACLE_TWAP.addn(200_000);
		long.amm.longSpread = 1000;
		long.amm.shortSpread = 2000;
		assert(
			appliedOffset(long, PositionDirection.LONG, START_BUFFER_A_B).eq(
				new BN(210_020)
			)
		);

		// short: slow -200_000, fast -300_000, |diff| 100_000 <= 99_700_000 / 200.
		// fracLong = 1000 * 99_800_000 / 1e7 = 9_980.
		// fracShort = 2000 * 99_800_000 / 1e7 = 19_960.
		// max(-200_000 - 19_960, -300_000 + 9_980) = -219_960.
		const short = makeMarket(ContractTier.A, new BN(-300_000));
		short.marketStats.lastAskPriceTwap = ORACLE_TWAP.subn(200_000);
		short.amm.longSpread = 1000;
		short.amm.shortSpread = 2000;
		assert(
			appliedOffset(short, PositionDirection.SHORT, START_BUFFER_A_B).eq(
				new BN(-219_960)
			)
		);
	});
});
