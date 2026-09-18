import { describe, expect, it, beforeEach } from '@jest/globals';
import { sampleBookFreshness } from '../bookFreshness';
import { resetHealthState, slotDiffWindows } from '../healthCheck';

/**
 * The server detects frozen books even though it holds no order state.
 */
describe('book freshness', () => {
	const markets = [{ marketIndex: 0, marketName: 'SOL-PERP' }];
	const selectFirst = (responses: any[]) => responses[0];

	// The watcher records a window per market while it is behind and clears it
	// as soon as it catches up, so the window map is what a case reads.
	const isFlagged = (marketName: string) => slotDiffWindows.has(marketName);

	const redisHolding = (document: unknown) => async () => document;

	beforeEach(() => {
		resetHealthState();
	});

	it('flags a book that has stopped advancing', async () => {
		await sampleBookFreshness(
			markets,
			10_000,
			redisHolding(JSON.stringify({ slot: 1_000 })),
			selectFirst
		);

		expect(isFlagged('SOL-PERP')).toBe(true);
	});

	it('leaves a book that keeps up alone', async () => {
		await sampleBookFreshness(
			markets,
			10_000,
			redisHolding(JSON.stringify({ slot: 9_990 })),
			selectFirst
		);

		expect(isFlagged('SOL-PERP')).toBe(false);
	});

	it('clears the window once a stalled book catches up', async () => {
		await sampleBookFreshness(
			markets,
			10_000,
			redisHolding(JSON.stringify({ slot: 1_000 })),
			selectFirst
		);

		expect(isFlagged('SOL-PERP')).toBe(true);

		await sampleBookFreshness(
			markets,
			10_100,
			redisHolding(JSON.stringify({ slot: 10_090 })),
			selectFirst
		);

		expect(isFlagged('SOL-PERP')).toBe(false);
	});

	/**
	 * A deployment that serves a subset of the markets has no document for the
	 * rest. Calling that a fault would restart every such pod.
	 */
	it('does not flag a market this deployment does not publish', async () => {
		await sampleBookFreshness(
			markets,
			10_000,
			redisHolding(undefined),
			selectFirst
		);

		expect(isFlagged('SOL-PERP')).toBe(false);
	});

	it('samples nothing before the first slot arrives', async () => {
		await sampleBookFreshness(
			markets,
			0,
			redisHolding(JSON.stringify({ slot: 1 })),
			selectFirst
		);

		expect(isFlagged('SOL-PERP')).toBe(false);
	});
});
