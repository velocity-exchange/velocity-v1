/**
 * Parity tests for the router split mirror. Every case here is transcribed
 * from `programs/velocity/src/math/router.rs`'s own unit tests, with the same
 * inputs and the same expected numbers — if the TS diverges from the Rust,
 * these fail.
 */

import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import {
	splitAcrossQuoters,
	RouterQuoterBook,
	VAMM_PRIORITY,
	CLOB_PRIORITY,
	CUSTOM_PRIORITY,
} from '../../src/math/router';
import { PositionDirection } from '../../src/types';

const B = new BN(1_000_000_000); // one base unit
const ONE = new BN(1);

function level(price: number, size: BN) {
	return { price: new BN(price), size };
}

function split(
	direction: PositionDirection,
	size: BN,
	books: [number, ReturnType<typeof level>[]][],
	step: BN = ONE
) {
	const shaped: RouterQuoterBook[] = books.map(([priority, levels]) => ({
		priority,
		levels,
	}));
	return splitAcrossQuoters(direction, size, shaped, step);
}

describe('router split (mirror of math/router.rs)', () => {
	it('fills one book partially then caps at its depth', () => {
		let out = split(PositionDirection.LONG, B.muln(3), [
			[CLOB_PRIORITY, [level(100, B.muln(2)), level(101, B.muln(2))]],
		]);
		assert(out[0].base.eq(B.muln(3)));
		// 2 @ 100 + 1 @ 101, prices per base unit at BASE_PRECISION.
		assert(out[0].quote.eq(new BN(2 * 100 + 101)));

		out = split(PositionDirection.LONG, B.muln(10), [
			[CLOB_PRIORITY, [level(100, B.muln(2))]],
		]);
		assert(out[0].base.eq(B.muln(2)));
	});

	it('fills tiers in priority order at a shared price', () => {
		let out = split(PositionDirection.LONG, B.muln(3), [
			[CUSTOM_PRIORITY, [level(100, B.muln(4))]],
			[CLOB_PRIORITY, [level(100, B.muln(2))]],
		]);
		assert(out[1].base.eq(B.muln(2)));
		assert(out[0].base.eq(B));

		out = split(PositionDirection.LONG, B.muln(2), [
			[CUSTOM_PRIORITY, [level(100, B.muln(4))]],
			[CLOB_PRIORITY, [level(100, B.muln(2))]],
			[VAMM_PRIORITY, [level(100, B)]],
		]);
		assert(out[2].base.eq(B)); // vAMM drained first
		assert(out[1].base.eq(B)); // then CLOB
		assert(out[0].base.eq(new BN(0))); // custom sees nothing
	});

	it('splits pro rata within a tier and hands dust to the first with depth', () => {
		let out = split(PositionDirection.LONG, B.muln(3), [
			[CUSTOM_PRIORITY, [level(100, B.muln(2))]],
			[CUSTOM_PRIORITY, [level(100, B.muln(4))]],
		]);
		assert(out[0].base.eq(B));
		assert(out[1].base.eq(B.muln(2)));

		out = split(PositionDirection.LONG, new BN(5), [
			[CUSTOM_PRIORITY, [level(100, new BN(3))]],
			[CUSTOM_PRIORITY, [level(100, new BN(3))]],
		]);
		assert(out[0].base.add(out[1].base).eq(new BN(5)));
	});

	it('walks prices best-first across books — price beats tier', () => {
		let out = split(PositionDirection.LONG, B.muln(3), [
			[CLOB_PRIORITY, [level(101, B.muln(2))]],
			[CUSTOM_PRIORITY, [level(100, B), level(102, B.muln(5))]],
		]);
		assert(out[1].base.eq(B)); // 1 @ 100 (custom)
		assert(out[0].base.eq(B.muln(2))); // 2 @ 101 (clob)

		out = split(PositionDirection.SHORT, B.muln(2), [
			[CLOB_PRIORITY, [level(99, B)]],
			[CUSTOM_PRIORITY, [level(100, B), level(98, B)]],
		]);
		assert(out[1].base.eq(B)); // 100 first
		assert(out[0].base.eq(B)); // then 99
	});

	it('quantizes every allocation to the step size', () => {
		const step = new BN(1000);
		// Depth of 2500 on a 1000 step: only 2000 is allocatable.
		const out = split(
			PositionDirection.LONG,
			new BN(2500),
			[[CLOB_PRIORITY, [level(100, new BN(2500))]]],
			step
		);
		assert(out[0].base.eq(new BN(2000)));
		assert(out[0].base.mod(step).eq(new BN(0)));
	});

	it('rounds the quoted notional against the taker', () => {
		// price 101 on 1 base unit: exact = 101. Long ceils, short floors —
		// visible where the division has a remainder.
		const longOut = split(PositionDirection.LONG, new BN(1), [
			[CLOB_PRIORITY, [level(101, new BN(1))]],
		]);
		const shortOut = split(PositionDirection.SHORT, new BN(1), [
			[CLOB_PRIORITY, [level(101, new BN(1))]],
		]);
		// 101 * 1 / 1e9 → ceil 1 for a long, floor 0 for a short.
		assert(longOut[0].quote.eq(new BN(1)));
		assert(shortOut[0].quote.eq(new BN(0)));
	});
});
