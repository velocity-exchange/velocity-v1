import { assert } from 'chai';
import { isMajorPerpMarket, MAJOR_PERP_MARKET_INDEXES } from '../../src';

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

	it('agrees with the exported index list', () => {
		MAJOR_PERP_MARKET_INDEXES.forEach((marketIndex) => {
			assert.isTrue(isMajorPerpMarket(marketIndex));
		});
	});
});
