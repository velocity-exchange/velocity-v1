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

// The auction durations below are wall-clock milliseconds.
// createMarketBasedAuctionParams expresses them in actual slots at the current
// State.slotDurationMs, so the pacing holds as Solana slot time drops.
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

// `auctionDuration` is absent on purpose. createMarketBasedAuctionParams
// derives it from the millisecond constants above and the live slot duration.
export const DEFAULT_AUCTION_PARAMS: Partial<AuctionParamArgs> = {
	isOracleOrder: true,
	auctionStartPriceOffset: 'marketBased',
	auctionEndPriceOffset: DEFAULT_AUCTION_END_PRICE_OFFSET,
	auctionStartPriceOffsetFrom: 'marketBased',
	auctionEndPriceOffsetFrom: DEFAULT_AUCTION_END_PRICE_FROM,
};

// Perp indexes that take the mid-major slippage tier, which is a base of 0.25%
// and a multiplier of 1.25, instead of the non-major defaults. Index 3 is
// HYPE-PERP on mainnet. Devnet has no market at index 3.
//
// These are raw indexes, so they do not survive on-chain market renumbering. A
// stale index buckets whatever market later occupies it. The MID_MAJOR_MARKETS
// test pins each entry to its expected symbol in MainnetPerpMarkets. Update both
// together.
export const MID_MAJOR_MARKETS: number[] = [3];

/** The symbol each MID_MAJOR_MARKETS index resolves to on mainnet. */
export const MID_MAJOR_MARKET_SYMBOLS: Record<number, string> = {
	3: 'HYPE-PERP',
};
