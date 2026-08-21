import { AuctionParamArgs } from './types';

export const MEASURED_ENDPOINTS = [
	'/priorityFees',
	'/batchPriorityFees',
	'/topMakers',
	'/l2',
	'/batchL2',
	'/batchL2Cache',
	'/l3',
];

// Auction durations below are wall-clock ms; createMarketBasedAuctionParams
// expresses them in actual slots at the current State.slotDurationMs so the
// pacing holds as Solana slot time drops.
export const DEFAULT_MARKET_AUCTION_DURATION_MS = 8_000;
// Version 3+ defaults: weight toward fast fills over price improvement.
// Start just inside the touch so the auction becomes marketable within the
// first slots, and walk to the end price quickly (~2s) instead of ~8s.
// The 5bps improvement doubles as a stale-quote buffer: sign/transmit delay
// can shift the book's oracle-relative premium after quoting, and starting
// slightly inside the stale touch avoids landing already-through the live one.
export const FAST_FILL_AUCTION_DURATION_MS = 2_000;
export const FAST_FILL_AUCTION_START_PRICE_OFFSET = -0.05;
export const DEFAULT_LIMIT_AUCTION_DURATION_MS = 24_000; // currently unused
const DEFAULT_AUCTION_END_PRICE_OFFSET = 0.1;
const DEFAULT_AUCTION_END_PRICE_FROM = 'worst';

// `auctionDuration` is intentionally absent: createMarketBasedAuctionParams
// derives it from the ms constants above and the live slot duration.
export const DEFAULT_AUCTION_PARAMS: Partial<AuctionParamArgs> = {
	isOracleOrder: true,
	auctionStartPriceOffset: 'marketBased',
	auctionEndPriceOffset: DEFAULT_AUCTION_END_PRICE_OFFSET,
	auctionStartPriceOffsetFrom: 'marketBased',
	auctionEndPriceOffsetFrom: DEFAULT_AUCTION_END_PRICE_FROM,
};

export const MAJOR_MARKETS = [0, 1, 2, 3]; // SOL, BTC, ETH, HYPE
// Intentionally empty: these were hardcoded numeric indices that went stale after
// on-chain market renumbering (e.g. it listed HYPE as 59, but HYPE is now index 3),
// silently mis-bucketing whatever market currently occupies each index. Everything
// outside MAJOR_MARKETS now takes the non-major default. Re-add by index only if the
// mapping is re-verified against on-chain market indices.
export const MID_MAJOR_MARKETS: number[] = [];
