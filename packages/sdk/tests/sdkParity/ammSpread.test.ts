import * as _ from 'lodash';
import { assert } from 'chai';
import {
	BN,
	MMOraclePriceData,
	SPREAD_CONF_FULL_WEIGHT_THRESHOLD,
	ZERO,
	calculateAskPrice,
	calculateBidPrice,
	calculateReservePrice,
	calculateSpread,
	calculateSpreadReserves,
	calculateVolSpreadBN,
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
// The short side of that capture predates program commit 440349868 (24 Aug 2026),
// which ramped the confidence contribution to the vol spread. Under the current
// program the confidence floor dominates the short side: with this snapshot's
// inputs `calculate_long_short_vol_spread` returns (1620, 1620), so the short
// spread can no longer sit at the 440 originally recorded here. The long side is
// driven by inventory skew and is unchanged by the ramp.
const ON_CHAIN_LONG_SPREAD = 111985;
const CONF_FLOORED_SHORT_SPREAD = 1620;

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
		assertCloseTo(shortSpread, CONF_FLOORED_SHORT_SPREAD, 5);
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
// base is the confidence itself and the intensity factor floors at 0.01, so the term
// competing with the confidence component is conf/100, which the ramp (>= conf/20)
// always dominates. `max()` therefore returns the confidence component. Expected
// values are the ones asserted by the program's own
// `confidence_component_ramps_continuously` test.
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
	const threshold = SPREAD_CONF_FULL_WEIGHT_THRESHOLD.toNumber();

	it('matches the program at and around the full-weight threshold', () => {
		assert(threshold === 2500, `expected 2500, got ${threshold}`);
		assert(confComponent(0) === 0);
		assert(confComponent(threshold / 2) === 656);
		assert(confComponent(threshold - 1) === 2498);
		assert(confComponent(threshold) === threshold);
		assert(confComponent(threshold + 1) === threshold + 1);
	});

	it('ramps monotonically and never exceeds the confidence itself', () => {
		let previous = 0;
		for (let confidence = 0; confidence <= threshold + 1; confidence++) {
			const component = confComponent(confidence);
			assert(
				component >= previous,
				`component fell from ${previous} to ${component} at conf ${confidence}`
			);
			assert(
				component <= confidence,
				`component ${component} exceeds conf ${confidence}`
			);
			previous = component;
		}
	});

	it('does not step off the old 1/20 cliff just below the threshold', () => {
		// The pre-ramp SDK divided by a flat 20 anywhere below the threshold,
		// returning 102 here against the program's 1691.
		assert(confComponent(2045) === 1691, `got ${confComponent(2045)}`);
	});
});
