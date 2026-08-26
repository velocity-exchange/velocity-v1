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
	areQuotedLevelsValid,
	quotedPrefix,
	isExecutedNotionalInQuote,
	isChangeNotionalInQuote,
	RouterQuoterBook,
	VAMM_PRIORITY,
	CLOB_PRIORITY,
	CUSTOM_PRIORITY,
} from '../../src/math/router';
import { PositionDirection } from '../../src/types';
import { BASE_PRECISION } from '../../src/constants/numericConstants';

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
	// `withheld_depth_takes_no_size`
	it('gives withheld depth no part of the size', () => {
		const shaped: RouterQuoterBook[] = [
			{
				priority: CLOB_PRIORITY,
				levels: [level(100, B)],
				withheld: level(101, B.muln(2)),
			},
			{ priority: VAMM_PRIORITY, levels: [level(102, B.muln(5))] },
		];
		const out = splitAcrossQuoters(
			PositionDirection.LONG,
			B.muln(5),
			shaped,
			ONE
		);
		assert.isTrue(out[0].base.eq(B), 'the book fills what it quoted');
		assert.isTrue(out[1].base.eq(B.muln(4)), 'the vAMM fills the rest');
		assert.isTrue(
			out[0].base.add(out[1].base).eq(B.muln(5)),
			'the taker is filled in full despite the withheld report'
		);
	});

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

/**
 * Parity for the quote↔execute binding, transcribed from the same Rust unit
 * tests (`math/router.rs`): the level contract velocity enforces on ingestion,
 * and the bounds a quoter's execute is held to against the levels it quoted.
 */
describe('quoter response bounds (mirror of math/router.rs)', () => {
	const PRICE = BASE_PRECISION;
	/** Two rungs a base unit deep, at 100 and 102 in PRICE_PRECISION. */
	const ladder = () => [
		{ price: PRICE.muln(100), size: B },
		{ price: PRICE.muln(102), size: B },
	];

	it('rejects a zero price or size on ingestion', () => {
		assert(!areQuotedLevelsValid(PositionDirection.LONG, [level(0, B)]));
		assert(
			!areQuotedLevelsValid(PositionDirection.LONG, [level(100, new BN(0))])
		);
		assert(areQuotedLevelsValid(PositionDirection.LONG, [level(100, B)]));
	});

	it('requires levels to run best price first, non-strictly', () => {
		assert(
			!areQuotedLevelsValid(PositionDirection.LONG, [
				level(100, B),
				level(99, B),
			])
		);
		assert(
			!areQuotedLevelsValid(PositionDirection.SHORT, [
				level(100, B),
				level(101, B),
			])
		);
		// Equal consecutive prices are legal: distinct ladder offsets can round
		// to the same tick.
		assert(
			areQuotedLevelsValid(PositionDirection.LONG, [
				level(100, B),
				level(100, B),
				level(101, B),
			])
		);
	});

	it('prices a partial fill at the prefix it reached', () => {
		const levels = ladder();
		const full = quotedPrefix(levels, ONE, B.muln(2));
		assert(full !== undefined);
		assert(isExecutedNotionalInQuote(full, B.muln(2), PRICE.muln(202)));
		assert(
			!isExecutedNotionalInQuote(full, B.muln(2), PRICE.muln(202).addn(2))
		);

		// Half filled: only the 100 level was reached, so the whole
		// allocation's 101 average is not available.
		const half = quotedPrefix(levels, ONE, B);
		assert(half !== undefined);
		assert(half.bestPrice.eq(PRICE.muln(100)));
		assert(half.worstPrice.eq(PRICE.muln(100)));
		assert(isExecutedNotionalInQuote(half, B, PRICE.muln(100)));
		assert(!isExecutedNotionalInQuote(half, B, PRICE.muln(101)));
	});

	it('rejects charging worse than quoted in both directions', () => {
		const long = quotedPrefix(ladder(), ONE, B.muln(2));
		assert(long !== undefined);
		assert(!isExecutedNotionalInQuote(long, B.muln(2), PRICE.muln(203)));
		assert(!isExecutedNotionalInQuote(long, B.muln(2), PRICE.muln(199)));
		assert(isExecutedNotionalInQuote(long, B.muln(2), PRICE.muln(201)));

		const bids = [
			{ price: PRICE.muln(100), size: B },
			{ price: PRICE.muln(98), size: B },
		];
		const short = quotedPrefix(bids, ONE, B.muln(2));
		assert(short !== undefined);
		assert(!isExecutedNotionalInQuote(short, B.muln(2), PRICE.muln(197)));
		assert(!isExecutedNotionalInQuote(short, B.muln(2), PRICE.muln(201)));
		assert(isExecutedNotionalInQuote(short, B.muln(2), PRICE.muln(199)));
	});

	it('has no prefix for a fill past the quoted depth', () => {
		assert(quotedPrefix(ladder(), ONE, B.muln(2)) !== undefined);
		assert(quotedPrefix(ladder(), ONE, B.muln(2).addn(1)) === undefined);
	});

	it('holds a single change to the quoted band', () => {
		const prefix = quotedPrefix(ladder(), ONE, B.muln(2));
		assert(prefix !== undefined);
		assert(isChangeNotionalInQuote(prefix, B, PRICE.muln(100), ONE));
		assert(isChangeNotionalInQuote(prefix, B, PRICE.muln(102), ONE));
		assert(!isChangeNotionalInQuote(prefix, B, PRICE.muln(99), ONE));
		assert(!isChangeNotionalInQuote(prefix, B, PRICE.muln(103), ONE));
		// Slack is one quote unit per merged order, not a licence to reprice.
		assert(
			isChangeNotionalInQuote(prefix, B, PRICE.muln(100).subn(4), new BN(4))
		);
		assert(
			!isChangeNotionalInQuote(prefix, B, PRICE.muln(100).subn(6), new BN(4))
		);
	});

	it('quantizes prefix levels the way the split does', () => {
		const levels = [
			{ price: PRICE.muln(100), size: new BN(3) },
			{ price: PRICE.muln(200), size: new BN(4) },
		];
		const prefix = quotedPrefix(levels, new BN(2), new BN(6));
		assert(prefix !== undefined);
		assert(
			prefix.scaledQuote.eq(
				PRICE.muln(100).muln(2).add(PRICE.muln(200).muln(4))
			)
		);
		assert(quotedPrefix(levels, new BN(2), new BN(7)) === undefined);
	});
});
