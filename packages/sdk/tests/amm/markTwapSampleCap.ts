import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { getMaxMarkTwapSampleElapsed, ONE_HOUR, ONE_MINUTE } from '../../src';

/**
 * Mirror of `MarketStats::max_mark_twap_sample_elapsed`: `max(funding_period / 60,
 * ONE_MINUTE)`. The values pin the program's, so a drift on either side fails here.
 */
describe('mark twap sample-weight cap', () => {
	it('is one sixtieth of the funding period', () => {
		assert(getMaxMarkTwapSampleElapsed(ONE_HOUR).eq(new BN(60)));
		assert(getMaxMarkTwapSampleElapsed(ONE_HOUR.muln(10)).eq(new BN(600)));
	});

	it('floors at one minute', () => {
		assert(getMaxMarkTwapSampleElapsed(new BN(0)).eq(ONE_MINUTE));
		assert(getMaxMarkTwapSampleElapsed(new BN(600)).eq(ONE_MINUTE));
	});
});
