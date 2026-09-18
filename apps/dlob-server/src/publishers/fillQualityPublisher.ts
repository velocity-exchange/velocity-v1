/**
 * allow-verbose: module doc. States the external dependency, the silent
 * fallback if this stops running, and the timer/gating rationale — none of
 * which the code below states on its own.
 *
 * Taker fill quality against the oracle, per market, from Athena into Redis.
 *
 * `/auctionParams` at version 2 and above reads these numbers to widen or
 * narrow an auction from what takers actually paid. Nothing else writes them,
 * so the endpoint silently falls back to its static offsets whenever this does
 * not run.
 *
 * The query is a 24-hour scan and the answer moves slowly, so it runs on a
 * five-minute timer rather than per request. It is off unless
 * `ENABLE_FILL_QUALITY_ANALYTICS` is set, because it needs Athena credentials.
 */
import { RedisClient } from '@velocity-exchange/common/clients';
import {
	FillQualityAnalyticsRepository,
	TakerFillVsOracleBpsRedisResult,
} from '../athena/repositories/fillQualityAnalytics';
import { logger } from '../utils/logger';

/** The key `/auctionParams` reads. One per market. */
export const fillQualityKey = (marketIndex: string | number): string =>
	`taker_fill_vs_oracle_bps:market:${marketIndex}`;

export type FillQualityPublisherConfig = {
	enabled: boolean;
	intervalMs: number;
	lookbackMs: number;
	smoothingMinutes: number;
};

export function fillQualityConfigFromEnv(): FillQualityPublisherConfig {
	return {
		enabled:
			process.env.ENABLE_FILL_QUALITY_ANALYTICS?.toLowerCase() === 'true',
		intervalMs:
			parseInt(process.env.FILL_QUALITY_ANALYTICS_INTERVAL) || 300_000,
		lookbackMs:
			parseInt(process.env.FILL_QUALITY_ANALYTICS_LOOKBACK_MS) || 86_400_000,
		smoothingMinutes:
			parseInt(process.env.FILL_QUALITY_ANALYTICS_SMOOTHING_MINUTES) || 60,
	};
}

/**
 * One pass. Written to every client the server reads through, because a read
 * takes whichever answered first.
 */
export async function publishFillQuality(
	redisClients: RedisClient[],
	config: FillQualityPublisherConfig
): Promise<number> {
	const startedAt = Date.now();
	const toMs = startedAt;
	const fromMs = toMs - config.lookbackMs;

	const results =
		await FillQualityAnalyticsRepository().getTakerFillVsOracleBps(
			fromMs,
			toMs,
			// baseDecimals
			9,
			config.smoothingMinutes
		);

	for (const result of results) {
		const value: TakerFillVsOracleBpsRedisResult = {
			marketIndex: result.MarketIndex,
			takerBuyBpsFromOracle: {
				all: result.TakerBuyBpsFromOracle_ALL,
				'1e0': result.TakerBuyBpsFromOracle_1e0,
				'1e3': result.TakerBuyBpsFromOracle_1e3,
				'1e4': result.TakerBuyBpsFromOracle_1e4,
				'1e5': result.TakerBuyBpsFromOracle_1e5,
				'1e6': result.TakerBuyBpsFromOracle_1e6,
			},

			takerSellBpsFromOracle: {
				all: result.TakerSellBpsFromOracle_ALL,
				'1e0': result.TakerSellBpsFromOracle_1e0,
				'1e3': result.TakerSellBpsFromOracle_1e3,
				'1e4': result.TakerSellBpsFromOracle_1e4,
				'1e5': result.TakerSellBpsFromOracle_1e5,
				'1e6': result.TakerSellBpsFromOracle_1e6,
			},

			updatedAtTs: Date.now(),
		};
		const body = JSON.stringify(value);
		await Promise.all(
			redisClients.map((client) =>
				client.setRaw(fillQualityKey(result.MarketIndex), body)
			)
		);
	}

	logger.info(
		`fill quality: stored ${results.length} markets in ${
			Date.now() - startedAt
		}ms`
	);

	return results.length;
}

/**
 * Run it on a timer. Returns a stop function, or `undefined` when the feature
 * is off.
 */
export function startFillQualityPublisher(
	redisClients: RedisClient[],
	config: FillQualityPublisherConfig = fillQualityConfigFromEnv()
): (() => void) | undefined {
	if (!config.enabled) {
		logger.info(
			'fill quality analytics disabled; /auctionParams v2+ uses its static offsets'
		);

		return undefined;
	}

	const pass = async () => {
		try {
			await publishFillQuality(redisClients, config);
		} catch (err) {
			// A failed pass leaves the last answer in place. The reader ages it
			// out on its own.
			logger.error(`fill quality: pass failed: ${err}`);
		}
	};

	void pass();
	const timer = setInterval(pass, config.intervalMs);
	logger.info(
		`fill quality analytics every ${config.intervalMs}ms over ${config.lookbackMs}ms`
	);
	return () => clearInterval(timer);
}
