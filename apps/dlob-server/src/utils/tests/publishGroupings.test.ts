import { describe, it, expect, jest } from '@jest/globals';
import { BN, MarketType } from '@velocity-exchange/sdk';
import { GROUPING_OPTIONS, publishGroupings } from '../utils';

const l2 = {
	bids: [{ price: '100', size: '1', sources: {} }],
	asks: [{ price: '101', size: '2', sources: {} }],
	slot: 42,
};

const marketArgs = {
	marketIndex: 0,
	marketType: MarketType.PERP,
	marketName: 'SOL-PERP',
	depth: 100,
	includeVamm: true,
	tickSize: new BN(1),
};

const run = (indicative: boolean) => {
	const redis = {
		publish: jest.fn(async (_key: string, _value: unknown) => 1),
		set: jest.fn(async (_key: string, _value: unknown) => undefined),
	};
	publishGroupings(
		l2,
		marketArgs,
		redis as any,
		'dlob:',
		'perp',
		(indicative ? {} : undefined) as any
	);
	return redis;
};

describe('publishGroupings', () => {
	it.each([false, true])(
		'stores a snapshot for every grouped channel it publishes (indicative=%s)',
		(indicative) => {
			const redis = run(indicative);
			expect(redis.publish).toHaveBeenCalledTimes(GROUPING_OPTIONS.length);
			expect(redis.set).toHaveBeenCalledTimes(GROUPING_OPTIONS.length);

			// The ws manager reads `last_update_<channel>`; the key prefix is applied by the client.
			const published = redis.publish.mock.calls.map(([channel, value]) => [
				`last_update_${channel.replace('dlob:', '')}`,
				value,
			]);
			expect(redis.set.mock.calls).toEqual(published);
		}
	);

	it('uses the channel names the ws manager subscribes to', () => {
		const keys = run(true).set.mock.calls.map(([key]) => key);
		expect(keys).toContain('last_update_orderbook_perp_0_grouped_1_indicative');
		expect(keys).toContain(
			'last_update_orderbook_perp_0_grouped_1000_indicative'
		);
	});
});
