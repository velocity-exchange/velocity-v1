import { expect } from 'chai';
import { Connection, PublicKey } from '@solana/web3.js';
import { EventSubscriber } from '../../src/events/eventSubscriber';
import { Program } from '../../src/isomorphic/anchor';

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
 * at a time, and fails `getTransaction` for `failOnce` the first time only.
 */
function stubConnection(failOnce: string): Connection {
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
		_rpcBatchRequest: async (requests: { args: any[] }[]) =>
			requests.map(({ args }) => {
				const signature = args[0] as string;
				if (signature === failOnce && !failed) {
					failed = true;
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

	it('stops once maxTx transactions are fetched when nothing fails', async () => {
		const subscriber = subscriberFor(stubConnection('none'), 5);

		await subscriber.fetchPreviousTx(true);

		expect(subscriber.getEventsByTx('sig4')).to.not.equal(undefined);
		expect(subscriber.getEventsByTx('sig3')).to.equal(undefined);
	});
});
