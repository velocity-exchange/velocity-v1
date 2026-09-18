/**
 * Watches how far the published books trail the chain.
 *
 * The server holds no order state. It serves what the rust `book-publisher`
 * writes to Redis, and `/health` measures only that this process still sees the
 * slot advance. Those are different things: a publisher that dies leaves every
 * document frozen while the server stays healthy and answers every request with
 * a 200. Nothing then restarts either process.
 *
 * This samples each market's stored book and compares the slot it was quoted at
 * to the current slot. A market that trails for `KILL_SWITCH_SUSTAIN_MS`
 * without a break latches the restart the health check already honours.
 * `recordSlotDiffHealth` owns that window, so a brief RPC or publisher hiccup
 * costs nothing.
 */
import { MarketType, SlotSource } from '@velocity-exchange/sdk';
import { recordSlotDiffHealth } from './healthCheck';
import { logger } from '../utils/logger';
import { fetchL2FromRedis } from '../utils/utils';

/**
 * How far a book may trail the chain before it counts as behind. A publisher
 * tick is 200ms, so a healthy book is a handful of slots back. This is wide
 * enough that a slow tick or a paused market does not trip it, and far short of
 * the sustain window that has to pass before anything restarts.
 */
const MAX_BOOK_SLOT_LAG = Number(process.env.MAX_BOOK_SLOT_LAG ?? 150);

/** How often to sample. The sustain window is measured in minutes. */
const BOOK_FRESHNESS_INTERVAL_MS = Number(
	process.env.BOOK_FRESHNESS_INTERVAL_MS ?? 5_000
);

export type WatchedMarket = { marketIndex: number; marketName: string };

/**
 * One pass over the watched markets. Exported so a test can drive it without a
 * timer.
 *
 * A market whose document is missing is not reported as behind. An empty Redis
 * is a market this deployment does not publish, and calling that a fault would
 * restart every pod that serves a subset of the markets.
 */
export async function sampleBookFreshness(
	markets: WatchedMarket[],
	currentSlot: number,
	fetchFromRedis: (
		key: string,
		selectionCriteria: (responses: any) => any
	) => Promise<any>,
	selectMostRecentBySlot: (responses: any[]) => any
): Promise<void> {
	if (!currentSlot) {
		return;
	}
	for (const { marketIndex, marketName } of markets) {
		let document: any;
		try {
			document = await fetchL2FromRedis(
				fetchFromRedis,
				selectMostRecentBySlot,
				MarketType.PERP,
				marketIndex
			);
		} catch (err) {
			logger.error(`book freshness: ${marketName} read failed: ${err}`);
			continue;
		}
		if (!document) {
			continue;
		}
		const parsed =
			typeof document === 'string' ? JSON.parse(document) : document;
		const bookSlot = Number(parsed?.slot);
		if (!Number.isFinite(bookSlot) || bookSlot <= 0) {
			continue;
		}
		const lag = currentSlot - bookSlot;
		const isBehind = lag > MAX_BOOK_SLOT_LAG;
		if (isBehind) {
			logger.warn(
				`book freshness: ${marketName} is ${lag} slots behind (max ${MAX_BOOK_SLOT_LAG})`
			);
		}
		recordSlotDiffHealth(marketName, isBehind);
	}
}

/** Sample on a timer. Returns a stop function. */
export function startBookFreshnessWatch(
	markets: WatchedMarket[],
	slotSource: SlotSource,
	fetchFromRedis: (
		key: string,
		selectionCriteria: (responses: any) => any
	) => Promise<any>,
	selectMostRecentBySlot: (responses: any[]) => any
): () => void {
	const timer = setInterval(() => {
		void sampleBookFreshness(
			markets,
			slotSource.getSlot(),
			fetchFromRedis,
			selectMostRecentBySlot
		).catch((err) => logger.error(`book freshness: pass failed: ${err}`));
	}, BOOK_FRESHNESS_INTERVAL_MS);
	logger.info(
		`watching ${markets.length} books for staleness every ${BOOK_FRESHNESS_INTERVAL_MS}ms, max lag ${MAX_BOOK_SLOT_LAG} slots`
	);
	return () => clearInterval(timer);
}
