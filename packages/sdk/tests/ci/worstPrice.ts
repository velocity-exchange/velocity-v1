/**
 * Parity tests for the worst-price mirror, transcribed from
 * `math/worst_price/tests.rs`.
 */

import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { deriveWorstPrice } from '../../src/math/worstPrice';
import { PositionDirection } from '../../src/types';

const ORACLE = new BN(100_000_000);

describe('deriveWorstPrice', () => {
	it('a named price is the cap however far from the oracle', () => {
		const long = deriveWorstPrice(
			ORACLE,
			PositionDirection.LONG,
			new BN(105_000_000)
		);
		const short = deriveWorstPrice(
			ORACLE,
			PositionDirection.SHORT,
			new BN(90_000_000)
		);

		assert(long.eq(new BN(105_000_000)));
		assert(short.eq(new BN(90_000_000)));
	});

	it('an unnamed price takes the default slippage from the oracle', () => {
		const long = deriveWorstPrice(ORACLE, PositionDirection.LONG, new BN(0));
		const short = deriveWorstPrice(ORACLE, PositionDirection.SHORT, new BN(0));

		assert(long.eq(new BN(100_500_000)));
		assert(short.eq(new BN(99_500_000)));
	});
});
