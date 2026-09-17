import { expect } from 'chai';
import { LRUCache } from 'lru-cache';

/**
 * The confirm loop gives up on a signature by walking `entries()` and testing
 * `txAge > TX_TIMEOUT_THRESHOLD_MS`, then emitting the terminal `expired` wide
 * event. lru-cache omits stale entries from `entries()`, so the cache TTL must
 * outlive the threshold. Otherwise every entry disappears at the moment it
 * becomes eligible, and the transaction is dropped with no report.
 */
describe('pendingTxSigsToconfirm TTL vs the give-up threshold', () => {
	const THRESHOLD_MS = 200;

	const ageOfVisibleEntry = async (
		ttl: number
	): Promise<number | undefined> => {
		const cache = new LRUCache<string, { ts: number }>({
			max: 10,
			ttl,
			ttlResolution: 10,
		});
		const ts = Date.now();
		cache.set('sig', { ts });
		// Poll past the give-up threshold, looking for the entry still being
		// visible once it is old enough to be given up on.
		for (let i = 0; i < 40; i++) {
			await new Promise((r) => setTimeout(r, 20));
			for (const [, record] of cache.entries()) {
				const age = Date.now() - record.ts;
				if (age > THRESHOLD_MS) {
					return age;
				}
			}
		}
		return undefined;
	};

	it('never sees an expirable entry when the TTL equals the threshold', async () => {
		expect(await ageOfVisibleEntry(THRESHOLD_MS)).to.be.undefined;
	});

	it('sees an expirable entry when the TTL outlives the threshold', async () => {
		const age = await ageOfVisibleEntry(THRESHOLD_MS * 2);
		expect(age).to.be.a('number');
		expect(age!).to.be.greaterThan(THRESHOLD_MS);
	});
});
