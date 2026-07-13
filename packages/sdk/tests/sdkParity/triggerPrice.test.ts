import * as _ from 'lodash';
import {
	BN,
	getTriggerPrice,
	TRIGGER_PRICE_LAST_FILL_MAX_AGE,
} from '../../src';
import { mockPerpMarkets } from '../dlob/helpers';
import { assert } from '../../src/assert/assert';

// Mirrors PerpMarket::get_trigger_price (perp_market.rs), including the
// last-fill staleness guard: the last-fill leg only votes in the median while
// the fill is younger than TRIGGER_PRICE_LAST_FILL_MAX_AGE; otherwise the
// oracle price stands in.
describe('getTriggerPrice parity', () => {
	const oraclePrice = new BN(100_000_000_000);
	const now = new BN(1_752_082_210);

	// No funding history (leg B = oracle), 5min basis = +10_000_000 (leg C),
	// last fill between them so a fresh last-fill leg is the median.
	function makeMarket() {
		const market = _.cloneDeep(mockPerpMarkets[0]);
		market.lastFillPrice = new BN(100_005_000_000);
		market.marketStats.lastMarkPriceTwap5Min = new BN(100_010_000_000);
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min = new BN(
			100_000_000_000
		);
		market.marketStats.lastTradeTs = now.sub(TRIGGER_PRICE_LAST_FILL_MAX_AGE);
		return market;
	}

	it('returns raw oracle price when useMedianPrice is false', () => {
		const market = makeMarket();
		const triggerPrice = getTriggerPrice(market, oraclePrice, now, false);
		assert(triggerPrice.eq(oraclePrice));
	});

	it('uses the last fill leg while exactly at max age', () => {
		const market = makeMarket();
		const triggerPrice = getTriggerPrice(market, oraclePrice, now, true);
		assert(triggerPrice.eq(new BN(100_005_000_000)));
	});

	it('substitutes oracle for the last fill leg one second past max age', () => {
		const market = makeMarket();
		market.marketStats.lastTradeTs = now
			.sub(TRIGGER_PRICE_LAST_FILL_MAX_AGE)
			.subn(1);
		const triggerPrice = getTriggerPrice(market, oraclePrice, now, true);
		assert(triggerPrice.eq(new BN(100_000_000_000)));
	});

	it('substitutes oracle when there has been no fill', () => {
		const market = makeMarket();
		market.lastFillPrice = new BN(0);
		market.marketStats.lastTradeTs = now;
		const triggerPrice = getTriggerPrice(market, oraclePrice, now, true);
		assert(triggerPrice.eq(new BN(100_000_000_000)));
	});
});
