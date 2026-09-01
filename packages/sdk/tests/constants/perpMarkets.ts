import { assert } from 'chai';
import {
	DevnetPerpMarkets,
	isMajorPerpMarket,
	MAJOR_PERP_MARKET_INDEXES,
	MainnetPerpMarkets,
} from '../../src';

describe('isMajorPerpMarket', () => {
	it('treats SOL, BTC and ETH as majors', () => {
		assert.isTrue(isMajorPerpMarket(0));
		assert.isTrue(isMajorPerpMarket(1));
		assert.isTrue(isMajorPerpMarket(2));
	});

	it('does not treat HYPE (index 3) as a major', () => {
		assert.isFalse(isMajorPerpMarket(3));
	});

	it('does not treat later listings as majors', () => {
		[4, 5, 10, 59].forEach((marketIndex) => {
			assert.isFalse(isMajorPerpMarket(marketIndex));
		});
	});

	it('rejects non-integer and negative indexes rather than coercing them', () => {
		assert.isFalse(isMajorPerpMarket(-1));
		assert.isFalse(isMajorPerpMarket(1.5));
		assert.isFalse(isMajorPerpMarket(NaN));
	});

	// Tiering is positional, so a renumbering would silently retier production with
	// every other assertion here still green. Pin the indexes to actual symbols.
	it('maps the major indexes onto SOL, BTC and ETH in both registries', () => {
		assert.deepStrictEqual(
			MAJOR_PERP_MARKET_INDEXES.map(
				(marketIndex) => MainnetPerpMarkets[marketIndex]?.baseAssetSymbol
			),
			['SOL', 'BTC', 'ETH']
		);
		assert.deepStrictEqual(
			MAJOR_PERP_MARKET_INDEXES.map(
				(marketIndex) => DevnetPerpMarkets[marketIndex]?.baseAssetSymbol
			),
			['SOL', 'BTC', 'ETH']
		);
	});

	it('classifies every registry index it is asked about', () => {
		MainnetPerpMarkets.forEach((market) => {
			assert.equal(
				isMajorPerpMarket(market.marketIndex),
				MAJOR_PERP_MARKET_INDEXES.includes(market.marketIndex),
				`${market.symbol} tiering should match the index list`
			);
		});
	});
});
