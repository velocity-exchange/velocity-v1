/**
 * Parity test for the vAMM ladder mirror. Expected levels come from
 * `cargo test -p velocity --lib ts_mirror_fixture -- --nocapture`.
 */

import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { vammQuoteLevels } from '../../src/math/vammLadder';
import {
	calculateAmmAvailableLiquidity,
	calculateSpreadReserves,
} from '../../src/math/amm';
import { AMM, MarketStats, PositionDirection } from '../../src/types';
import { mockAMM, mockMarketStats } from '../fixtures/mockAccounts';
import {
	AMM_RESERVE_PRECISION,
	BASE_PRECISION,
	PEG_PRECISION,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	ZERO,
} from '../../src/constants/numericConstants';

/** `TS_MIRROR long` from the Rust fixture: "price:size" pairs. */
const RUST_LONG =
	'50632912:1250000000,51931192:1250000000,53280053:1250000000,54682160:1250000000,56140352:1250000000,57657658:1250000000,59237320:1250000000,60882801:1250000000';
const RUST_CAPPED_TOTAL = BASE_PRECISION.muln(25); // TS_MIRROR: 40-base take caps at 25 due to per-fill throttle
/** `TS_MIRROR short` from the Rust fixture. */
const RUST_SHORT =
	'49382716:1250000000,48178259:1250000000,47017337:1250000000,45897877:1250000000,44817927:1250000000,43775649:1250000000,42769312:1250000000,41797283:1250000000';

/** `TS_MIRROR long_rival`: 0.1 base of rival depth at +1%. */
const RUST_LONG_RIVAL =
	'50198930:396280980,50500000:100000000,50885447:753719020,51931192:1250000000,53280053:1250000000,54682160:1250000000,56140352:1250000000,57657658:1250000000,59237320:1250000000,60882801:1250000000';
/** `TS_MIRROR long_dust_rival_shallow`: 0.001 base of rival depth at +1%. */
const RUST_LONG_DUST_RIVAL_SHALLOW =
	'50248875:495280980,50500000:1000000,50885447:753719020,51931192:1250000000,53280053:1250000000,54682160:1250000000,56140352:1250000000,57657658:1250000000,59237320:1250000000,60882801:1250000000';
/**
 * `TS_MIRROR long_dust_rival`: 1M-reserve AMM, 0.001 base at +4.9%. The take
 * never reaches the rival, so the ladder is the honest curve.
 */
const RUST_LONG_DUST_RIVAL_DEEP =
	'50000626:12500000000,50001876:12500000000,50003126:12500000000,50004376:12500000000,50005626:12500000000,50006876:12500000000,50008126:12500000000,50009377:12500000000';
/** `TS_MIRROR short_dust_rival`: 1M-reserve AMM, 0.001 base at -4.9%. */
const RUST_SHORT_DUST_RIVAL_DEEP =
	'49999374:12500000000,49998125:12500000000,49996875:12500000000,49995625:12500000000,49994375:12500000000,49993125:12500000000,49991876:12500000000,49990626:12500000000';

/**
 * `TS_MIRROR long_inside_top_wall`: 5 base at -0.2% and 5 base at +2%, a 6-base
 * take. The +2% rung shades only what the depth inside the top leaves over.
 */
const RUST_LONG_INSIDE_TOP_WALL =
	'50377835:750000000,50871539:220491406,51000000:14754297,51266523:514754297,51929843:750000000,52732882:750000000,53554692:750000000,54395867:750000000,55257014:750000000,56138776:750000000';
/** `TS_MIRROR long_two_rungs`: 0.1 base at +0.5% and 5 base at +2%, a 2-base take. */
const RUST_LONG_TWO_RUNGS =
	'50074648:149066390,50250000:100000000,51000000:736179313,51007582:14754297,51144356:250000000,51403972:250000000,51665568:250000000,51929168:250000000';
/** `TS_MIRROR long_out_of_order`: 0.1 base at +1%, then 10 base at +0.5% the split never reads. */
const RUST_LONG_OUT_OF_ORDER =
	'50198930:396280980,50500000:100000000,50885447:753719020,51931192:1250000000,53280053:1250000000,54682160:1250000000,56140352:1250000000,57657658:1250000000,59237320:1250000000,60882801:1250000000';

/** `TS_MIRROR spread_state`: the spread reserves `update_amm_quote_state` caches. */
const RUST_SPREAD_ASK = {
	baseAssetReserve: new BN('99518776954'),
	quoteAssetReserve: new BN('100483550000'),
};
const RUST_SPREAD_BID = {
	baseAssetReserve: new BN('100000050000'),
	quoteAssetReserve: new BN('99999950000'),
};
/** `TS_MIRROR spread_long`. */
const RUST_SPREAD_LONG =
	'51126896:1250000000,52444344:1250000000,53813380:1250000000,55236732:1250000000,56717311:1250000000,58258228:1250000000,59862805:1250000000,61534600:1250000000';
/** `TS_MIRROR spread_short`. */
const RUST_SPREAD_SHORT =
	'49382666:1250000000,48178212:1250000000,47017292:1250000000,45897832:1250000000,44817884:1250000000,43775608:1250000000,42769273:1250000000,41797244:1250000000';
/** `TS_MIRROR spread_long_rival`: 0.1 base 1% past the first marginal. */
const RUST_SPREAD_LONG_RIVAL_PRICE = new BN(50989566);
const RUST_SPREAD_LONG_RIVAL =
	'50685333:393892475,50989566:100000000,51381895:756107525,52444344:1250000000,53813380:1250000000,55236732:1250000000,56717311:1250000000,58258228:1250000000,59862805:1250000000,61534600:1250000000';
/** `TS_MIRROR spread_long_window_limit`: above `ask_price`, below the first marginal. */
const RUST_SPREAD_WINDOW_LIMIT = new BN(50483551);

function parse(dump: string): { price: BN; size: BN }[] {
	return dump.split(',').map((pair) => {
		const [price, size] = pair.split(':');
		return { price: new BN(price), size: new BN(size) };
	});
}

/**
 * The same AMM the Rust fixture builds: 100-unit reserves at peg 50, no
 * spread (`seed_no_spread_quote_state`), reserve bounds 50-200,
 * `max_fill_reserve_fraction` 4 so a 10-unit take clears the per-fill cap.
 */
function ammFixture(reserveUnits = 100): AMM {
	const reserves = AMM_RESERVE_PRECISION.muln(reserveUnits);
	return {
		...mockAMM,
		baseAssetReserve: reserves,
		quoteAssetReserve: reserves,
		terminalQuoteAssetReserve: reserves,
		sqrtK: reserves,
		pegMultiplier: PEG_PRECISION.muln(50),
		minBaseAssetReserve: reserves.divn(2),
		maxBaseAssetReserve: reserves.muln(2),
		maxFillReserveFraction: 4,
		// No-spread quote state: spread reserves equal the underlying reserves
		// and both spreads are zero.
		askBaseAssetReserve: reserves,
		askQuoteAssetReserve: reserves,
		bidBaseAssetReserve: reserves,
		bidQuoteAssetReserve: reserves,
		longSpread: 0,
		shortSpread: 0,
		baseSpread: 0,
		maxSpread: 0,
		referencePriceOffset: 0,
		baseAssetAmountWithAmm: ZERO,
		curveUpdateIntensity: 0,
		concentrationCoef: ZERO,
	};
}

/**
 * The Rust `spread_amm` fixture: the 100-unit AMM with a dynamic spread, 2 base
 * of pool inventory and a mark premium, so the quote state carries nonzero
 * spreads and a reference price offset.
 */
function spreadAmmFixture(): { amm: AMM; marketStats: MarketStats } {
	const reserves = AMM_RESERVE_PRECISION.muln(100);
	const baseAssetAmountWithAmm = BASE_PRECISION.muln(2);
	const amm: AMM = {
		...ammFixture(),
		baseSpread: 2_000,
		maxSpread: 50_000,
		curveUpdateIntensity: 200,
		baseAssetAmountWithAmm,
		totalFeeMinusDistributions: QUOTE_PRECISION.muln(1_000),
		terminalQuoteAssetReserve: reserves
			.mul(reserves)
			.div(reserves.add(baseAssetAmountWithAmm)),
	};
	const oraclePrice = PRICE_PRECISION.muln(50);
	const marketStats: MarketStats = {
		...mockMarketStats,
		lastMarkPriceTwap: new BN(50_500_000),
		lastMarkPriceTwap5Min: new BN(50_500_000),
		last24HAvgFundingRate: new BN(1_000_000_000),
		fundingPeriod: new BN(3600),
		historicalOracleData: {
			...mockMarketStats.historicalOracleData,
			lastOraclePrice: oraclePrice,
			lastOraclePriceTwap: oraclePrice,
			lastOraclePriceTwap5Min: oraclePrice,
		},
	};
	return { amm, marketStats };
}

/** Asserts two ladders match rung by rung, price and size. */
function assertLadderMatches(
	actual: { price: BN; size: BN }[],
	expected: { price: BN; size: BN }[],
	label: string
): void {
	assert.equal(actual.length, expected.length, `${label}: rung count`);
	actual.forEach((rung, i) => {
		assert(
			rung.price.eq(expected[i].price),
			`${label}[${i}] price: got ${rung.price}, want ${expected[i].price}`
		);
		assert(
			rung.size.eq(expected[i].size),
			`${label}[${i}] size: got ${rung.size}, want ${expected[i].size}`
		);
	});
}

describe('vAMM ladder (mirror of vlp/amm/router_adapter.rs)', () => {
	// The mirror needs the market's real AMM/stats shapes, which are large
	// hand-built fixtures; this pins the pure ladder shape and the parity
	// contract. Full-state parity runs in the velocity integration tests.
	it('matches the program dump shape: 8 equal step-aligned rungs, monotone', () => {
		for (const dump of [RUST_LONG, RUST_SHORT]) {
			const levels = parse(dump);
			assert.equal(levels.length, 8, 'eight checkpoints');
			const total = levels.reduce((acc, l) => acc.add(l.size), ZERO);
			assert(total.eq(BASE_PRECISION.muln(10)), 'covers the full request');
			// Equal-size filler rungs when there are no rival books.
			assert(levels.every((l) => l.size.eq(levels[0].size)));
		}

		const longLevels = parse(RUST_LONG);
		for (let i = 1; i < longLevels.length; i++) {
			assert(
				longLevels[i].price.gte(longLevels[i - 1].price),
				'long ladder is ascending'
			);
		}

		const shortLevels = parse(RUST_SHORT);
		for (let i = 1; i < shortLevels.length; i++) {
			assert(
				shortLevels[i].price.lte(shortLevels[i - 1].price),
				'short ladder is descending'
			);
		}
	});

	// Per-fill reserve throttle caps depth. A mirror using the wider reserve bound
	// would over-allocate in every router split prediction.
	it('caps depth at the per-fill reserve throttle, not the reserve bound', () => {
		const amm = ammFixture();
		const step = new BN(1);
		for (const direction of [PositionDirection.LONG, PositionDirection.SHORT]) {
			assert(
				calculateAmmAvailableLiquidity(amm, direction, step).eq(
					RUST_CAPPED_TOTAL
				),

				'available liquidity matches the program'
			);
		}

		// The room to the hard reserve bound is twice that on this AMM, which is
		// what the mirror used to quote.
		const sideRoom = amm.baseAssetReserve.sub(amm.minBaseAssetReserve);
		assert(sideRoom.eq(RUST_CAPPED_TOTAL.muln(2)), 'the wider figure differs');
	});

	it('calls vammQuoteLevels and matches the Rust dump exactly', () => {
		const amm = ammFixture();
		const mmOraclePriceData = {
			price: PRICE_PRECISION.muln(50),
			confidence: ZERO,
		};
		const step = new BN(1);
		const size = BASE_PRECISION.muln(10);

		const longLevels = vammQuoteLevels(
			amm,
			mockMarketStats,
			mmOraclePriceData,
			PositionDirection.LONG,
			size,
			step
		);
		assertLadderMatches(longLevels, parse(RUST_LONG), 'long');

		const shortLevels = vammQuoteLevels(
			amm,
			mockMarketStats,
			mmOraclePriceData,
			PositionDirection.SHORT,
			size,
			step
		);
		assertLadderMatches(shortLevels, parse(RUST_SHORT), 'short');
	});

	it('shades only the rival depth and matches the Rust dump exactly', () => {
		const mmOraclePriceData = {
			price: PRICE_PRECISION.muln(50),
			confidence: ZERO,
		};
		const top = PRICE_PRECISION.muln(50);
		const rivalLadder = (
			reserveUnits: number,
			direction: PositionDirection,
			size: BN,
			price: BN,
			depth: BN
		) =>
			vammQuoteLevels(
				ammFixture(reserveUnits),
				mockMarketStats,
				mmOraclePriceData,
				direction,
				size,
				new BN(1),
				[
					{
						priority: 10,
						levels: [{ price, size: depth }],
						withheld: { price: ZERO, size: ZERO },
					},
				]
			);

		const plusOnePercent = top.muln(101).divn(100);
		const cases: [string, { price: BN; size: BN }[], string][] = [
			[
				'long_rival',
				rivalLadder(
					100,
					PositionDirection.LONG,
					BASE_PRECISION.muln(10),
					plusOnePercent,
					BASE_PRECISION.divn(10)
				),
				RUST_LONG_RIVAL,
			],
			[
				'long_dust_rival_shallow',
				rivalLadder(
					100,
					PositionDirection.LONG,
					BASE_PRECISION.muln(10),
					plusOnePercent,
					BASE_PRECISION.divn(1000)
				),
				RUST_LONG_DUST_RIVAL_SHALLOW,
			],
			[
				'long_dust_rival',
				rivalLadder(
					1_000_000,
					PositionDirection.LONG,
					BASE_PRECISION.muln(100),
					top.add(top.muln(49).divn(1000)),
					BASE_PRECISION.divn(1000)
				),
				RUST_LONG_DUST_RIVAL_DEEP,
			],
			[
				'short_dust_rival',
				rivalLadder(
					1_000_000,
					PositionDirection.SHORT,
					BASE_PRECISION.muln(100),
					top.sub(top.muln(49).divn(1000)),
					BASE_PRECISION.divn(1000)
				),
				RUST_SHORT_DUST_RIVAL_DEEP,
			],
		];

		for (const [label, actual, dump] of cases) {
			assertLadderMatches(actual, parse(dump), label);
		}
	});

	it('charges rival depth better than a rung to the take, as the Rust does', () => {
		const mmOraclePriceData = {
			price: PRICE_PRECISION.muln(50),
			confidence: ZERO,
		};
		const top = PRICE_PRECISION.muln(50);
		const ladder = (size: BN, levels: { price: BN; size: BN }[]) =>
			vammQuoteLevels(
				ammFixture(),
				mockMarketStats,
				mmOraclePriceData,
				PositionDirection.LONG,
				size,
				new BN(1),
				[{ priority: 10, levels, withheld: { price: ZERO, size: ZERO } }]
			);

		const cases: [string, { price: BN; size: BN }[], string][] = [
			[
				'long_inside_top_wall',
				ladder(BASE_PRECISION.muln(6), [
					{ price: top.sub(top.divn(500)), size: BASE_PRECISION.muln(5) },
					{ price: top.add(top.divn(50)), size: BASE_PRECISION.muln(5) },
				]),
				RUST_LONG_INSIDE_TOP_WALL,
			],
			[
				'long_two_rungs',
				ladder(BASE_PRECISION.muln(2), [
					{ price: top.add(top.divn(200)), size: BASE_PRECISION.divn(10) },
					{ price: top.add(top.divn(50)), size: BASE_PRECISION.muln(5) },
				]),
				RUST_LONG_TWO_RUNGS,
			],
			[
				'long_out_of_order',
				ladder(BASE_PRECISION.muln(10), [
					{ price: top.add(top.divn(100)), size: BASE_PRECISION.divn(10) },
					{ price: top.add(top.divn(200)), size: BASE_PRECISION.muln(10) },
				]),
				RUST_LONG_OUT_OF_ORDER,
			],
		];

		for (const [label, actual, dump] of cases) {
			assertLadderMatches(actual, parse(dump), label);
		}
	});

	it('matches the Rust dump with a spread and a reference price offset', () => {
		const { amm, marketStats } = spreadAmmFixture();
		const mmOraclePriceData = {
			price: PRICE_PRECISION.muln(50),
			confidence: ZERO,
		};
		const step = new BN(1);
		const size = BASE_PRECISION.muln(10);

		const [bid, ask] = calculateSpreadReserves(
			amm,
			marketStats,
			mmOraclePriceData
		);
		for (const [label, actual, expected] of [
			['ask', ask, RUST_SPREAD_ASK],
			['bid', bid, RUST_SPREAD_BID],
		] as const) {
			assert(
				actual.baseAssetReserve.eq(expected.baseAssetReserve) &&
					actual.quoteAssetReserve.eq(expected.quoteAssetReserve),
				`${label} reserves: got ${actual.baseAssetReserve}/${actual.quoteAssetReserve}`
			);
		}

		const ladder = (
			direction: PositionDirection,
			rivalBooks = [],
			takerLimit?: BN
		) =>
			vammQuoteLevels(
				amm,
				marketStats,
				mmOraclePriceData,
				direction,
				size,
				step,
				rivalBooks,
				takerLimit
			);

		assertLadderMatches(
			ladder(PositionDirection.LONG),
			parse(RUST_SPREAD_LONG),
			'spread_long'
		);
		assertLadderMatches(
			ladder(PositionDirection.SHORT),
			parse(RUST_SPREAD_SHORT),
			'spread_short'
		);
		assertLadderMatches(
			ladder(PositionDirection.LONG, [
				{
					priority: 10,
					levels: [
						{
							price: RUST_SPREAD_LONG_RIVAL_PRICE,
							size: BASE_PRECISION.divn(10),
						},
					],
					withheld: { price: ZERO, size: ZERO },
				},
			]),
			parse(RUST_SPREAD_LONG_RIVAL),
			'spread_long_rival'
		);
		assert.equal(
			ladder(PositionDirection.LONG, [], RUST_SPREAD_WINDOW_LIMIT).length,
			0,
			'a limit below the first marginal quotes nothing'
		);
	});
});
