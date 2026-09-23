import * as _ from 'lodash';
import * as fs from 'fs';
import * as path from 'path';
import { assert } from 'chai';
import {
	BN,
	MMOraclePriceData,
	LAZER_CONF_FLOOR_PCT,
	ZERO,
	applyOracleGuard,
	calculateAskPrice,
	calculateBidAskPrice,
	calculateBidPrice,
	calculatePrice,
	calculateReferencePriceOffset,
	calculateSpreadBN,
	calculateReservePrice,
	calculateSpread,
	calculateSpreadReserves,
	calculateVolSpreadBN,
	squareRootBN,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';

// `update_spreads` (vlp/amm/math/spread.rs) computes the dynamic spread whenever
// `curve_update_intensity > 0`. A `base_spread` of 0 does NOT disable it — it only
// lowers the floor that the vol spread is maxed against — and `amm_spread_adjustment`
// is applied to both that branch and the frozen-curve one.
//
// The snapshot below is devnet SOL-PERP (base_spread 0, curve_update_intensity 100)
// while market orders were failing to fill: the AMM was short 108.6 base, which
// skewed long_spread to 111985 (~11.2%) on chain and put the vAMM ask ~$82.1 against
// a ~$73.96 oracle. The SDK reported a zero-width spread for the same state.
//
// The short side is the vol floor. This snapshot's confidence (20bp) is the Lazer
// floor, which the vol spread discounts to 1/20, so the short spread is about 1bp
// (103, from the program's own `calculate_spread` with these inputs). The long
// side is driven by inventory skew.
const ON_CHAIN_LONG_SPREAD = 111985;
const SHORT_SPREAD = 103;

// Anchored to the market's own lastMarkPriceTwapTs / lastOraclePriceTwapTs so the
// `now`-derived inputs (liveOracleStd, oracle conf pct) match the on-chain crank.
const NOW = new BN(1785310851);

function devnetSolPerp() {
	const market = _.cloneDeep(mockPerpMarkets[0]);

	const amm = market.amm;
	amm.baseSpread = 0;
	amm.maxSpread = 142500;
	amm.curveUpdateIntensity = 100;
	amm.ammSpreadAdjustment = 0;
	amm.ammInventorySpreadAdjustment = 0;
	amm.fundingBiasSensitivity = 0;
	amm.referencePriceOffset = 0;
	amm.pegMultiplier = new BN('58739930');
	amm.baseAssetReserve = new BN('891395200000');
	amm.quoteAssetReserve = new BN('1121836868764');
	amm.sqrtK = new BN('1000000000000');
	amm.terminalQuoteAssetReserve = new BN('1000000000000');
	amm.minBaseAssetReserve = new BN('707113562438');
	amm.maxBaseAssetReserve = new BN('1414200000000');
	amm.baseAssetAmountWithAmm = new BN('108604800000');
	amm.totalFeeMinusDistributions = new BN('2693603146');
	amm.netRevenueSinceLastFunding = new BN('20502253');

	const stats = market.marketStats;
	stats.markStd = new BN('121081');
	stats.oracleStd = new BN('129839');
	stats.lastOracleConfPct = new BN('2000');
	stats.longIntensityVolume = new BN('81715111');
	stats.shortIntensityVolume = new BN('232583160');
	stats.volume24H = new BN('1056240165');
	stats.last24HAvgFundingRate = new BN('926173');
	stats.lastFundingOracleTwap = new BN('73675623');
	stats.lastMarkPriceTwap = new BN('81111343');
	stats.lastMarkPriceTwap5Min = new BN('81234218');
	stats.lastMarkPriceTwapTs = new BN('1785310851');
	stats.minOrderSize = new BN('100000');
	stats.lastReferencePriceOffset = 0;

	const hist = stats.historicalOracleData;
	hist.lastOraclePrice = new BN('73959685');
	hist.lastOracleConf = new BN('0');
	hist.lastOracleDelay = new BN('0');
	hist.lastOraclePriceTwap = new BN('73814259');
	hist.lastOraclePriceTwap5Min = new BN('73956962');
	hist.lastOraclePriceTwapTs = new BN('1785310851');

	return market;
}

const oracle = {
	price: new BN('73959860'),
	slot: new BN('479689665'),
	confidence: new BN('148094'),
	hasSufficientNumberOfDataPoints: true,
	isMMOracleActive: true,
} as MMOraclePriceData;

// The SDK derives liveOracleStd / conf pct from `now` rather than reading the values
// the on-chain crank used, so allow a small band around the on-chain spread.
function assertCloseTo(actual: number, expected: number, tolerancePct: number) {
	const drift = Math.abs(actual - expected) / expected;
	assert(
		drift <= tolerancePct / 100,
		`expected ~${expected} (±${tolerancePct}%), got ${actual} (${(
			drift * 100
		).toFixed(2)}% off)`
	);
}

describe('AMM spread parity with update_spreads', () => {
	it('applies the dynamic spread when baseSpread is 0 and curveUpdateIntensity > 0', () => {
		const market = devnetSolPerp();

		const [longSpread, shortSpread] = calculateSpread(
			market.amm,
			market.marketStats,
			oracle,
			NOW
		);

		assertCloseTo(longSpread, ON_CHAIN_LONG_SPREAD, 2);
		assertCloseTo(shortSpread, SHORT_SPREAD, 5);
	});

	it('quotes an ask above the reserve price when baseSpread is 0', () => {
		const market = devnetSolPerp();

		const [bidReserves, askReserves] = calculateSpreadReserves(
			market.amm,
			market.marketStats,
			oracle,
			NOW
		);
		assert(
			!askReserves.quoteAssetReserve.eq(market.amm.quoteAssetReserve),
			'ask reserves were left unadjusted, so the vAMM ask carries no spread'
		);
		assert(
			!bidReserves.quoteAssetReserve.eq(askReserves.quoteAssetReserve),
			'bid and ask reserves are identical, so the book shows a zero-width spread'
		);

		const reservePrice = calculateReservePrice(market, oracle);
		const ask = calculateAskPrice(market, oracle);
		const bid = calculateBidPrice(market, oracle);
		assert(
			ask.gt(reservePrice),
			`ask ${ask.toString()} should exceed reserve price ${reservePrice.toString()}`
		);
		assert(
			bid.lt(reservePrice),
			`bid ${bid.toString()} should be below reserve price ${reservePrice.toString()}`
		);
	});

	it('falls back to half of baseSpread, truncated, when curveUpdateIntensity is 0', () => {
		const market = devnetSolPerp();
		market.amm.curveUpdateIntensity = 0;
		market.amm.baseSpread = 175;

		const [longSpread, shortSpread] = calculateSpread(
			market.amm,
			market.marketStats,
			oracle,
			NOW
		);

		// `base_spread.safe_div(2)` is integer division: 175 / 2 == 87, not 87.5.
		assert(longSpread === 87, `expected 87, got ${longSpread}`);
		assert(shortSpread === 87, `expected 87, got ${shortSpread}`);
	});

	it('applies ammSpreadAdjustment on the curveUpdateIntensity == 0 branch', () => {
		const market = devnetSolPerp();
		market.amm.curveUpdateIntensity = 0;
		market.amm.baseSpread = 200;
		market.amm.ammSpreadAdjustment = 50;

		const [longSpread, shortSpread] = calculateSpread(
			market.amm,
			market.marketStats,
			oracle,
			NOW
		);

		// 100 + ceil(100 * 50 / 100) == 150
		assert(longSpread === 150, `expected 150, got ${longSpread}`);
		assert(shortSpread === 150, `expected 150, got ${shortSpread}`);
	});

	it('does not apply the oracle guard on the curveUpdateIntensity == 0 branch', () => {
		const market = devnetSolPerp();
		market.amm.curveUpdateIntensity = 0;
		market.amm.baseSpread = 175;

		// The snapshot oracle sits ~4.7bp above the reserve price. A market with
		// curveUpdateIntensity 0 never repegs and quotes off its curve alone, so
		// the spread stays at half the base spread on both sides.
		const [longSpread, shortSpread] = calculateSpread(
			market.amm,
			market.marketStats,
			oracle,
			NOW
		);
		assert(longSpread === 87, `expected 87, got ${longSpread}`);
		assert(shortSpread === 87, `expected 87, got ${shortSpread}`);
	});

	it('requires oracle data whenever curveUpdateIntensity is nonzero', () => {
		const market = devnetSolPerp();

		// A baseSpread of 0 used to short-circuit to [0, 0] without an oracle,
		// handing callers a zero-width spread instead of asking for the input the
		// dynamic branch needs.
		assert.throws(
			() => calculateSpread(market.amm, market.marketStats),
			/oraclePriceData is required/
		);
		assert.throws(
			() => calculateSpreadReserves(market.amm, market.marketStats),
			/oraclePriceData is required/
		);
	});

	it('does not require oracle data when curveUpdateIntensity is 0', () => {
		const market = devnetSolPerp();
		market.amm.curveUpdateIntensity = 0;
		market.amm.baseSpread = 175;

		const [longSpread, shortSpread] = calculateSpread(
			market.amm,
			market.marketStats
		);

		assert(longSpread === 87, `expected 87, got ${longSpread}`);
		assert(shortSpread === 87, `expected 87, got ${shortSpread}`);
	});
});

// Isolates the confidence component of `calculateVolSpreadBN`: with zero std the vol
// base is the confidence component itself and the intensity factor floors at 0.01, so
// the term competing with it is component/100. `max()` therefore returns the confidence
// component. Expected values are the ones asserted by the program's own
// `confidence_component_discounts_the_lazer_floor` test.
function confComponent(confidencePct: number): number {
	const [longVolSpread, shortVolSpread] = calculateVolSpreadBN(
		new BN(confidencePct),
		new BN(1_000_000),
		ZERO,
		ZERO,
		ZERO,
		ZERO,
		new BN(1)
	);
	assert(
		longVolSpread.eq(shortVolSpread),
		'the two sides should collapse to the same confidence component'
	);
	return longVolSpread.toNumber();
}

describe('vol spread confidence component parity with calculate_spread_conf_component', () => {
	const floor = LAZER_CONF_FLOOR_PCT.toNumber();

	it('matches the program around the Lazer confidence floor', () => {
		assert(floor === 2000, `expected 2000, got ${floor}`);
		assert(confComponent(0) === 0);
		assert(confComponent(1000) === 50);
		assert(confComponent(floor) === 100);
		assert(confComponent(floor + 1) === 101);
		assert(confComponent(2500) === 625);
		assert(confComponent(4000) === 2200);
		assert(confComponent(10000) === 8500);
		assert(confComponent(40000) === 40000);
		assert(confComponent(50000) === 50000);
	});

	it('is continuous and monotone, never exceeding the confidence itself', () => {
		let previous = 0;
		for (let confidence = 0; confidence <= 60_000; confidence++) {
			const component = confComponent(confidence);
			assert(
				component >= previous,
				`component fell from ${previous} to ${component} at conf ${confidence}`
			);
			assert(
				component - previous <= 2,
				`component jumped from ${previous} to ${component} at conf ${confidence}`
			);
			assert(
				component <= confidence,
				`component ${component} exceeds conf ${confidence}`
			);
			previous = component;
		}
	});
});

// Mirrors the program's `oracle_guard_never_quotes_through_the_oracle`: for random
// curves, spreads and offsets, the quotes the curve produces after the guard sit on
// the right side of the oracle, and the guard only ever widens.
describe('oracle guard keeps quotes on the right side of the oracle', () => {
	const P = new BN(1_000_000);
	// mulberry32: a small deterministic generator for the property test
	let seed = 7;
	const next = () => {
		seed = (seed + 0x6d2b79f5) | 0;
		let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return (t ^ (t >>> 14)) >>> 0;
	};
	const range = (lo: number, hi: number) => lo + (next() % (hi - lo + 1));

	// The signed composite spread s moves the quote reserve by quote * s / 2P and the
	// base reserve follows from k, as in `compute_spread_reserves_for_direction`.
	const quotedPrice = (
		base: BN,
		quote: BN,
		sqrtK: BN,
		peg: BN,
		s: number
	): BN => {
		const newQuote = quote.add(quote.mul(new BN(s)).div(P.muln(2)));
		const newBase = sqrtK.mul(sqrtK).div(newQuote);
		return calculatePrice(newBase, newQuote, peg);
	};

	it('holds for random markets', () => {
		let widened = 0;
		for (let i = 0; i < 2000; i++) {
			const base = new BN(range(1_000_000_000, 1_000_000_000_000));
			const quote = base.muln(range(500, 2000)).divn(1000);
			const peg = new BN(range(1_000, 100_000_000_000));
			const sqrtK = squareRootBN(base.mul(quote));
			const reservePrice = calculatePrice(base, quote, peg);
			if (reservePrice.ltn(1_000)) continue;
			const oracle = reservePrice.muln(range(800_000, 1_200_000)).div(P);
			const long = range(0, 30_000);
			const short = range(0, 30_000);
			const offset = range(0, 10_000) - 5_000;

			const [gLong, gShort] = applyOracleGuard(
				long,
				short,
				offset,
				reservePrice,
				oracle
			);
			assert(gLong >= long && gShort >= short, 'the guard only widens');
			assert(gLong + gShort <= 1_000_000, 'the pair stays legal');

			const bid = quotedPrice(base, quote, sqrtK, peg, offset - gShort);
			const ask = quotedPrice(base, quote, sqrtK, peg, gLong + offset);
			assert(bid.lte(oracle), `bid ${bid} above oracle ${oracle}`);
			assert(ask.gte(oracle), `ask ${ask} below oracle ${oracle}`);
			if (gLong > long || gShort > short) widened++;
		}
		assert(widened > 200 && widened < 1900, `widened ${widened}`);
	});

	it('holds against the admin adjustments end to end', () => {
		// The mainnet case: the curve 30bp above the oracle with both admin
		// adjustments at -25. The bid must not end up above the oracle.
		const market = _.cloneDeep(mockPerpMarkets[0]);
		const amm = market.amm;
		amm.baseAssetReserve = new BN(100).mul(new BN(1_000_000_000));
		amm.quoteAssetReserve = new BN(100).mul(new BN(1_000_000_000));
		amm.sqrtK = new BN(100).mul(new BN(1_000_000_000));
		amm.terminalQuoteAssetReserve = amm.quoteAssetReserve;
		amm.pegMultiplier = new BN(1_000_000);
		amm.baseAssetAmountWithAmm = ZERO;
		amm.baseSpread = 500;
		amm.maxSpread = 20_000;
		amm.curveUpdateIntensity = 100;
		amm.ammSpreadAdjustment = -25;
		amm.ammInventorySpreadAdjustment = -25;
		amm.totalFeeMinusDistributions = new BN(100_000_000);
		amm.netRevenueSinceLastFunding = ZERO;
		market.marketStats.lastOracleConfPct = new BN(2000);

		const oracle = {
			price: new BN(997_000),
			slot: new BN(0),
			confidence: new BN(1),
			hasSufficientNumberOfDataPoints: true,
			isMMOracleActive: true,
		} as MMOraclePriceData;
		const [bid] = calculateBidAskPrice(amm, market.marketStats, oracle, false);
		assert(bid.lte(oracle.price), `bid ${bid} above oracle ${oracle.price}`);
	});
});

// Shared with the program's `parity_fixtures` tests: both implementations assert
// against the same expected outputs, which come from the program.
describe('spread math parity with the program fixtures', () => {
	const rows = (file: string): string[][] =>
		fs
			.readFileSync(path.join(__dirname, 'fixtures', file), 'utf8')
			.trim()
			.split('\n')
			.slice(1)
			.map((line) => line.split(','));
	const bn = (x: string) => new BN(x);

	it('calculateSpreadBN matches calculate_spread', () => {
		for (const [i, c] of rows('calculate_spread.csv').entries()) {
			const out = calculateSpreadBN(
				Number(c[0]),
				bn(c[1]),
				bn(c[2]),
				Number(c[3]),
				bn(c[4]),
				bn(c[5]),
				bn(c[6]),
				bn(c[7]),
				bn(c[8]),
				bn(c[9]),
				bn(c[10]),
				bn(c[11]),
				bn(c[12]),
				bn(c[13]),
				bn(c[14]),
				bn(c[15]),
				bn(c[16]),
				bn(c[17]),
				bn(c[18]),
				Number(c[19]),
				bn(c[20]),
				bn(c[21]),
				Number(c[22])
			);
			assert.deepEqual(out, [Number(c[23]), Number(c[24])], `row ${i + 1}`);
		}
	});

	it('applyOracleGuard matches apply_oracle_guard', () => {
		for (const [i, c] of rows('apply_oracle_guard.csv').entries()) {
			const out = applyOracleGuard(
				Number(c[0]),
				Number(c[1]),
				Number(c[2]),
				bn(c[3]),
				bn(c[4])
			);
			assert.deepEqual(out, [Number(c[5]), Number(c[6])], `row ${i + 1}`);
		}
	});

	it('calculateReferencePriceOffset matches calculate_reference_price_offset', () => {
		for (const [i, c] of rows('reference_price_offset.csv').entries()) {
			const out = calculateReferencePriceOffset(
				bn(c[0]),
				bn(c[1]),
				bn(c[2]),
				bn(c[3]),
				bn(c[4]),
				bn(c[5]),
				bn(c[6]),
				Number(c[7])
			);
			assert.equal(out.toNumber(), Number(c[8]), `row ${i + 1}`);
		}
	});
});
