import { assert } from 'chai';
import {
	BN,
	DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS,
	DLOBSubscriber,
	MAJORS_TOP_OF_BOOK_QUOTE_AMOUNTS,
	MarketType,
	QUOTE_PRECISION,
} from '../../src';
import * as orderBookLevels from '../../src/dlob/orderBookLevels';
import { mockPerpMarkets, mockStateAccount } from './helpers';

/**
 * Which top-of-book breakpoints a market receives must follow `isMajorPerpMarket`,
 * not a hardcoded index comparison. The two arrays currently hold equal values, so
 * these assert reference identity to pin the routing rather than the notionals.
 */
describe('vAMM top-of-book level selection', () => {
	const original = orderBookLevels.getVammL2Generator;
	let seenTopOfBookQuoteAmounts: BN[] | undefined;

	beforeEach(() => {
		seenTopOfBookQuoteAmounts = undefined;
		Object.defineProperty(orderBookLevels, 'getVammL2Generator', {
			configurable: true,
			writable: true,
			value: (args: { topOfBookQuoteAmounts?: BN[] }) => {
				seenTopOfBookQuoteAmounts = args.topOfBookQuoteAmounts;
				// Matches L2OrderBookGenerator; nothing consumes it here, but a
				// wrong-shaped double would hide a change to that contract.
				return { getL2Bids: () => [], getL2Asks: () => [] };
			},
		});
	});

	afterEach(() => {
		Object.defineProperty(orderBookLevels, 'getVammL2Generator', {
			configurable: true,
			writable: true,
			value: original,
		});
	});

	const selectFor = (marketIndex: number): BN[] | undefined => {
		const velocityClient = {
			getStateAccount: () => mockStateAccount,
			getPerpMarketAccountOrThrow: () => ({
				...mockPerpMarkets[0],
				marketIndex,
			}),
			getMMOracleDataForPerpMarket: () => ({
				price: new BN(0),
				slot: new BN(0),
				confidence: new BN(0),
				hasSufficientNumberOfDataPoints: true,
			}),
		};
		const subscriber = new DLOBSubscriber({
			velocityClient: velocityClient as never,
			dlobSource: { getDLOB: async () => undefined as never },
			slotSource: { getSlot: () => 0 },
			updateFrequency: 1_000,
		} as never);
		(subscriber as unknown as { dlob: unknown }).dlob = {
			getL2: () => ({ bids: [], asks: [], slot: 0 }),
		};

		subscriber.getL2({
			marketIndex,
			marketType: MarketType.PERP,
			depth: 1,
			includeVamm: true,
		});
		return seenTopOfBookQuoteAmounts;
	};

	it('gives major markets the majors breakpoints', () => {
		[0, 1, 2].forEach((marketIndex) => {
			assert.strictEqual(
				selectFor(marketIndex),
				MAJORS_TOP_OF_BOOK_QUOTE_AMOUNTS,
				`market ${marketIndex} should use the majors breakpoints`
			);
		});
	});

	it('gives HYPE (index 3) the default breakpoints', () => {
		assert.strictEqual(
			selectFor(3),
			DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS,
			'HYPE is not a major and must fall through to the default breakpoints'
		);
	});

	it('gives later listings the default breakpoints', () => {
		[4, 5, 10].forEach((marketIndex) => {
			assert.strictEqual(
				selectFor(marketIndex),
				DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS,
				`market ${marketIndex} should use the default breakpoints`
			);
		});
	});

	it('holds the tuned $250/$750/$2000/$5000 ladder on both arrays', () => {
		const expected = [250, 750, 2000, 5000];
		[
			DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS,
			MAJORS_TOP_OF_BOOK_QUOTE_AMOUNTS,
		].forEach((amounts) => {
			assert.deepStrictEqual(
				amounts.map((amount) => amount.div(QUOTE_PRECISION).toNumber()),
				expected
			);
		});
	});

	it('keeps the breakpoints strictly ascending', () => {
		[
			DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS,
			MAJORS_TOP_OF_BOOK_QUOTE_AMOUNTS,
		].forEach((amounts) => {
			amounts.forEach((amount, i) => {
				if (i > 0) assert.isTrue(amount.gt(amounts[i - 1]));
			});
		});
	});
});
