/**
 * Parity test for the vAMM ladder mirror. Expected levels come from
 * `cargo test -p velocity --lib ts_mirror_fixture -- --nocapture`.
 */

import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { vammQuoteLevels } from '../../src/math/vammLadder';
import { calculateAmmAvailableLiquidity } from '../../src/math/amm';
import { AMM, PositionDirection } from '../../src/types';
import { mockAMM, mockMarketStats } from '../fixtures/mockAccounts';
import {
	AMM_RESERVE_PRECISION,
	BASE_PRECISION,
	PEG_PRECISION,
	PRICE_PRECISION,
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
});
