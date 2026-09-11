import { expect } from 'chai';
import { Connection, PublicKey } from '@solana/web3.js';
import { EventSubscriber } from '../../src/events/eventSubscriber';
import { Program } from '../../src/isomorphic/anchor';
import { stubRpcClient } from '../util/stubRpcClient';

const PAGE_SIZE = 3;

// Oldest-first; `fetchLogs` sorts by slot so the slots here fix the order.
const HISTORY = Array.from({ length: 9 }, (_, index) => ({
	signature: `sig${index + 1}`,
	slot: 101 + index,
	err: null,
	memo: null,
	blockTime: 1_700_000_000 + index,
}));

/**
 * Minimal stand-in for `Connection`: pages `HISTORY` newest-first, `PAGE_SIZE`
 * at a time, fails `getTransaction` for `failOnce` the first time only, and
 * fails it every time for anything in `failAlways`.
 */
function stubConnection(
	failOnce: string,
	failAlways: string[] = []
): Connection {
	let failed = false;

	return {
		getSignaturesForAddress: async (
			_address: PublicKey,
			{ before }: { before?: string }
		) => {
			const end = before
				? HISTORY.findIndex((entry) => entry.signature === before)
				: HISTORY.length;
			return HISTORY.slice(Math.max(0, end - PAGE_SIZE), end)
				.slice()
				.reverse();
		},
		...stubRpcClient(([signature]: any[]) => {
			if (
				failAlways.includes(signature) ||
				(signature === failOnce && !failed)
			) {
				failed = failed || signature === failOnce;
				return {
					error: {
						code: -32015,
						message: 'Transaction version (1) is not supported',
					},
				};
			}
			return {
				result: {
					slot: HISTORY.find((entry) => entry.signature === signature)?.slot,
					transaction: { signatures: [signature] },
					meta: { logMessages: [] },
				},
			};
		}),
	} as unknown as Connection;
}

function subscriberFor(connection: Connection, maxTx: number): EventSubscriber {
	return new EventSubscriber(
		connection,
		{ programId: PublicKey.default } as unknown as Program,
		{
			address: PublicKey.default,
			maxTx,
			logProviderConfig: { type: 'polling', frequency: 60_000 },
		}
	);
}

describe('fetchPreviousTx backfill budget', () => {
	it('does not spend maxTx on the page re-read after a failed getTransaction', async () => {
		// sig8 fails once, so `earliestTx` stays at sig9 and the next page
		// re-delivers sig7. Counting that duplicate would stop the backfill at
		// sig6 instead of reaching maxTx unique transactions.
		const subscriber = subscriberFor(stubConnection('sig8'), 5);

		await subscriber.fetchPreviousTx(true);

		// sig8 came back on the retry, and the budget still reached sig5.
		// Counting the duplicate stopped at sig6.
		expect(subscriber.getEventsByTx('sig8'), 'sig8').to.not.equal(undefined);
		expect(subscriber.getEventsByTx('sig5'), 'sig5').to.not.equal(undefined);
	});

	it('delivers the page it fetched when no signature is safe to page back from', async () => {
		// sig9 is the newest signature in the first page and keeps failing, so
		// `fetchLogs` has no `earliestTx` to hand back and the backfill must stop.
		// sig7 and sig8 were fetched, and dropping them is the log loss this guards.
		const subscriber = subscriberFor(stubConnection('none', ['sig9']), 5);

		await subscriber.fetchPreviousTx(true);

		expect(subscriber.getEventsByTx('sig8'), 'sig8').to.not.equal(undefined);
		expect(subscriber.getEventsByTx('sig7'), 'sig7').to.not.equal(undefined);
	});

	it('stops once maxTx transactions are fetched when nothing fails', async () => {
		const subscriber = subscriberFor(stubConnection('none'), 5);

		await subscriber.fetchPreviousTx(true);

		expect(subscriber.getEventsByTx('sig4')).to.not.equal(undefined);
		expect(subscriber.getEventsByTx('sig3')).to.equal(undefined);
	});
});
