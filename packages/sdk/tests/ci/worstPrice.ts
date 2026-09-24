/**
 * Parity tests for the worst-price mirror, with the inputs and outputs that
 * `math/worst_price/tests.rs` asserts against `derive_worst_price`.
 */

import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { deriveWorstPrice } from '../../src/math/worstPrice';
import { ContractTier, PositionDirection } from '../../src/types';

const ORACLE = new BN(100_000_000);

function unnamedBounds(oracle: BN, tier: ContractTier): [number, number] {
	const long = deriveWorstPrice(
		oracle,
		tier,
		PositionDirection.LONG,
		new BN(0)
	);
	const short = deriveWorstPrice(
		oracle,
		tier,
		PositionDirection.SHORT,
		new BN(0)
	);

	return [long.toNumber(), short.toNumber()];
}

describe('deriveWorstPrice', () => {
	it('a named price is the cap however far from the oracle', () => {
		const long = deriveWorstPrice(
			ORACLE,
			ContractTier.A,
			PositionDirection.LONG,
			new BN(105_000_000)
		);
		const short = deriveWorstPrice(
			ORACLE,
			ContractTier.A,
			PositionDirection.SHORT,
			new BN(90_000_000)
		);

		assert(long.eq(new BN(105_000_000)));
		assert(short.eq(new BN(90_000_000)));
	});

	it('an unnamed price takes the tier bound from the oracle', () => {
		const expected: [ContractTier, [number, number]][] = [
			[ContractTier.A, [102_000_000, 98_000_000]],
			[ContractTier.B, [105_000_000, 95_000_000]],
			[ContractTier.C, [105_000_000, 95_000_000]],
			[ContractTier.SPECULATIVE, [110_000_000, 90_000_000]],
			[ContractTier.HIGHLY_SPECULATIVE, [120_000_000, 80_000_000]],
			[ContractTier.ISOLATED, [120_000_000, 80_000_000]],
		];

		for (const [tier, bounds] of expected) {
			assert.deepEqual(unnamedBounds(ORACLE, tier), bounds);
		}
	});

	it('an unnamed price rounds the slippage down', () => {
		assert.deepEqual(
			unnamedBounds(new BN(123_456_789), ContractTier.A),
			[125_925_924, 120_987_654]
		);
		assert.deepEqual(
			unnamedBounds(new BN(33_333), ContractTier.SPECULATIVE),
			[36_666, 30_000]
		);
	});
});
