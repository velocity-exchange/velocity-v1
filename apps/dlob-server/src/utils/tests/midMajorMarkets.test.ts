import { describe, it, expect } from '@jest/globals';
import { MainnetPerpMarkets } from '@velocity-exchange/sdk';
import { MID_MAJOR_MARKETS, MID_MAJOR_MARKET_SYMBOLS } from '../constants';

// Guards the renumbering footgun: MID_MAJOR_MARKETS holds raw indices, so a
// market reshuffle would silently move the mid-major slippage tier onto a
// different listing. Fail loudly instead.
describe('MID_MAJOR_MARKETS', () => {
	it('every index still resolves to its expected mainnet market', () => {
		expect(MID_MAJOR_MARKETS.length).toBeGreaterThan(0);

		for (const marketIndex of MID_MAJOR_MARKETS) {
			const expectedSymbol = MID_MAJOR_MARKET_SYMBOLS[marketIndex];
			expect(expectedSymbol).toBeDefined();

			const market = MainnetPerpMarkets.find(
				(m) => m.marketIndex === marketIndex
			);
			expect(market?.symbol).toBe(expectedSymbol);
		}
	});

	it('does not overlap the major tier', () => {
		for (const marketIndex of MID_MAJOR_MARKETS) {
			expect([0, 1, 2]).not.toContain(marketIndex);
		}
	});
});
