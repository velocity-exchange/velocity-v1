export const MEASURED_ENDPOINTS = [
	'/priorityFees',
	'/batchPriorityFees',
	'/topMakers',
	'/l2',
	'/batchL2',
	'/batchL2Cache',
	'/l3',
];

// Perp indexes on the mid-major slippage tier (0.25% base, 1.25x multiplier).
// Index 3 is HYPE-PERP on mainnet; devnet has no market there. Raw indexes do
// not survive on-chain renumbering, so the MID_MAJOR_MARKETS test pins each
// entry to its symbol in MainnetPerpMarkets. Update both together.
export const MID_MAJOR_MARKETS: number[] = [3];

/** The symbol each MID_MAJOR_MARKETS index resolves to on mainnet. */
export const MID_MAJOR_MARKET_SYMBOLS: Record<number, string> = {
	3: 'HYPE-PERP',
};
