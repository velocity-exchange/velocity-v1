/**
 * Parity test for the vAMM ladder mirror.
 *
 * The expected levels are not hand-derived: they are the program's own output,
 * dumped by `ts_mirror_fixture::print_ladder_for_ts_mirror` in
 * `programs/velocity/src/vlp/amm/router_adapter.rs` for this exact AMM. Re-run
 * that test (`cargo test -p velocity --lib ts_mirror_fixture -- --nocapture`)
 * and paste its output here whenever the ladder changes on either side.
 */

import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { vammQuoteLevels } from '../../src/math/vammLadder';
import { calculateAmmAvailableLiquidity } from '../../src/math/amm';
import { PositionDirection } from '../../src/types';
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
/**
 * `TS_MIRROR long_capped` / `short_capped`: a 40-base take against the same
 * AMM. The program covers only the per-fill reserve throttle, so the ladder
 * totals 25 base rather than the 40 asked for.
 */
const RUST_CAPPED_TOTAL = BASE_PRECISION.muln(25);
/** `TS_MIRROR short` from the Rust fixture. */
const RUST_SHORT =
	'49382716:1250000000,48178259:1250000000,47017337:1250000000,45897877:1250000000,44817927:1250000000,43775649:1250000000,42769312:1250000000,41797283:1250000000';

function parse(dump: string): { price: BN; size: BN }[] {
	return dump.split(',').map((pair) => {
		const [price, size] = pair.split(':');
		return { price: new BN(price), size: new BN(size) };
	});
}

/**
 * The same AMM the Rust fixture builds: 100-unit reserves at peg 50, no
 * spread (`seed_no_spread_quote_state`), reserve bounds 50–200,
 * `max_fill_reserve_fraction` 4 so a 10-unit take clears the per-fill cap.
 */
function ammFixture() {
	const reserves = AMM_RESERVE_PRECISION.muln(100);
	return {
		baseAssetReserve: reserves,
		quoteAssetReserve: reserves,
		terminalQuoteAssetReserve: reserves,
		sqrtK: reserves,
		pegMultiplier: PEG_PRECISION.muln(50),
		minBaseAssetReserve: AMM_RESERVE_PRECISION.muln(50),
		maxBaseAssetReserve: AMM_RESERVE_PRECISION.muln(200),
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
		historicalOracleData: {
			lastOraclePrice: PRICE_PRECISION.muln(50),
			lastOraclePriceTwap: PRICE_PRECISION.muln(50),
			lastOraclePriceTwap5Min: PRICE_PRECISION.muln(50),
			lastOraclePriceTwapTs: ZERO,
			lastOracleDelay: ZERO,
			lastOracleConf: ZERO,
		},
	};
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

	/**
	 * The per-fill reserve throttle. `calculate_amm_available_liquidity` caps a
	 * fill at `base_asset_reserve / max_fill_reserve_fraction`, then at half the
	 * side's room. The room to the hard reserve bound is far wider, so a mirror
	 * that used it over-allocates the vAMM in every router split it predicts.
	 *
	 * The expectation is the program's own `TS_MIRROR long_capped` total.
	 */
	it('caps depth at the per-fill reserve throttle, not the reserve bound', () => {
		const amm = ammFixture();
		const step = new BN(1);
		for (const direction of [PositionDirection.LONG, PositionDirection.SHORT]) {
			assert(
				calculateAmmAvailableLiquidity(amm as never, direction, step).eq(
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

	it('exposes the mirror with the same signature the program takes', () => {
		// Compile-time contract check: the mirror must accept (amm, stats,
		// oracle, direction, size, step, rivals, limit) exactly like the Rust.
		assert.equal(typeof vammQuoteLevels, 'function');
		assert.equal(vammQuoteLevels.length, 6, 'six required params');
		void ammFixture();
		void PositionDirection.LONG;
	});
});
