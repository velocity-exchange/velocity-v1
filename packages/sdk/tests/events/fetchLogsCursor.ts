import { expect } from 'chai';
import { Connection, PublicKey } from '@solana/web3.js';
import { fetchLogs } from '../../src/events/fetchLogs';
import { stubRpcClient } from '../util/stubRpcClient';

// Oldest-first; `fetchLogs` sorts by slot so the slots here fix the order.
const SIGNATURES = ['sigA', 'sigB', 'sigC', 'sigD'];

/**
 * Minimal stand-in for `Connection`: returns the four signatures above and
 * answers the `getTransaction` batch, erroring on `erroredSignatures`.
 */
function stubConnection(erroredSignatures: string[]): Connection {
	return {
		getSignaturesForAddress: async () =>
			SIGNATURES.map((signature, index) => ({
				signature,
				slot: 100 + index,
				err: null,
				memo: null,
				blockTime: 1_700_000_000 + index,
			})),
		...stubRpcClient(([signature]: any[]) =>
			erroredSignatures.includes(signature)
				? {
						error: {
							code: -32015,
							message: 'Transaction version (1) is not supported',
						},
				  }
				: {
						result: {
							slot: 100 + SIGNATURES.indexOf(signature),
							transaction: { signatures: [signature] },
							meta: { logMessages: [] },
						},
				  }
		),
	} as unknown as Connection;
}

async function fetch(erroredSignatures: string[]) {
	return await fetchLogs(
		stubConnection(erroredSignatures),
		PublicKey.default,
		'confirmed'
	);
}

describe('fetchLogs cursor', () => {
	it('spans the whole range when every transaction is fetched', async () => {
		const response = await fetch([]);

		expect(response?.transactionLogs.map((log) => log.txSig)).to.deep.equal(
			SIGNATURES
		);
		expect(response?.earliestTx).to.equal('sigA');
		expect(response?.mostRecentTx).to.equal('sigD');
	});

	it('does not advance mostRecentTx past an errored signature', async () => {
		const response = await fetch(['sigC']);

		// `mostRecentTx` is fed back as `untilTx`, so anything at or newer than
		// sigC would drop sigC for good.
		expect(response?.mostRecentTx).to.equal('sigB');
		expect(response?.mostRecentSlot).to.equal(101);
		// `earliestTx` is fed back as `beforeTx`, so it must stay newer than sigC.
		expect(response?.earliestTx).to.equal('sigD');
		// The transactions that did come back are still delivered.
		expect(response?.transactionLogs.map((log) => log.txSig)).to.deep.equal([
			'sigA',
			'sigB',
			'sigD',
		]);
	});

	it('stops at the outermost error when several fail', async () => {
		const response = await fetch(['sigB', 'sigC']);

		expect(response?.mostRecentTx).to.equal('sigA');
		expect(response?.earliestTx).to.equal('sigD');
	});

	it('leaves a cursor undefined but still returns the logs it fetched', async () => {
		// sigA is the oldest, so nothing is safe to resume forwards from, but the
		// rest of the page was fetched and must not be thrown away.
		const oldestFailed = await fetch(['sigA']);
		expect(oldestFailed?.mostRecentTx, 'mostRecentTx').to.equal(undefined);
		expect(oldestFailed?.mostRecentSlot, 'mostRecentSlot').to.equal(undefined);
		// `earliestTx` is fed back as `beforeTx`, so it stays just newer than sigA.
		expect(oldestFailed?.earliestTx, 'earliestTx').to.equal('sigB');
		expect(oldestFailed?.transactionLogs.map((log) => log.txSig)).to.deep.equal(
			['sigB', 'sigC', 'sigD']
		);

		// sigD is the newest, so there is nothing to page backwards from.
		const newestFailed = await fetch(['sigD']);
		expect(newestFailed?.earliestTx, 'earliestTx').to.equal(undefined);
		expect(newestFailed?.earliestSlot, 'earliestSlot').to.equal(undefined);
		expect(newestFailed?.mostRecentTx, 'mostRecentTx').to.equal('sigC');

		// Every signature failing leaves both cursors undefined and no logs, but
		// it is still a response rather than the "nothing in range" undefined.
		const allFailed = await fetch(SIGNATURES);
		expect(allFailed?.transactionLogs).to.deep.equal([]);
		expect(allFailed?.earliestTx).to.equal(undefined);
		expect(allFailed?.mostRecentTx).to.equal(undefined);
	});

	it('does not attribute a failure to the wrong signature when the RPC reorders the batch', async () => {
		// JSON-RPC lets a server return batch responses in any order, so position
		// says nothing about which signature a bare error object belongs to.
		const connection = {
			getSignaturesForAddress: async () =>
				SIGNATURES.map((signature, index) => ({
					signature,
					slot: 100 + index,
					err: null,
					memo: null,
					blockTime: 1_700_000_000 + index,
				})),
			...stubRpcClient(
				([signature]: any[]) =>
					signature === 'sigB'
						? { error: { code: -32015, message: 'not supported' } }
						: {
								result: {
									slot: 100 + SIGNATURES.indexOf(signature),
									transaction: { signatures: [signature] },
									meta: { logMessages: [] },
								},
						  },
				(responses) => responses.reverse()
			),
		} as unknown as Connection;

		const response = await fetchLogs(connection, PublicKey.default, 'confirmed');

		// sigB is the one that failed. Reading the error by position blames sigC
		// instead and hands back sigB as the forward cursor, dropping sigB for good.
		expect(response?.mostRecentTx).to.equal('sigA');
		expect(response?.earliestTx).to.equal('sigC');
		// The reordering must not disturb the logs themselves; each result carries
		// its own signature, so they come back oldest-first regardless.
		expect(response?.transactionLogs.map((log) => log.txSig)).to.deep.equal([
			'sigA',
			'sigC',
			'sigD',
		]);
	});

	it('returns undefined when there is nothing in range', async () => {
		const connection = {
			getSignaturesForAddress: async () => [],
			...stubRpcClient(() => ({ result: null })),
		} as unknown as Connection;

		const response = await fetchLogs(connection, PublicKey.default, 'confirmed');

		expect(response).to.equal(undefined);
	});

	it('still advances past a signature the RPC has no transaction for', async () => {
		// A null result is the RPC answering definitively, so retrying is pointless
		// and the cursor may move past it.
		const connection = {
			getSignaturesForAddress: async () =>
				SIGNATURES.map((signature, index) => ({
					signature,
					slot: 100 + index,
					err: null,
					memo: null,
					blockTime: 1_700_000_000 + index,
				})),
			...stubRpcClient(([signature]: any[]) =>
				signature === 'sigC'
					? { result: null }
					: {
							result: {
								slot: 100 + SIGNATURES.indexOf(signature),
								transaction: { signatures: [signature] },
								meta: { logMessages: [] },
							},
					  }
			),
		} as unknown as Connection;

		const response = await fetchLogs(connection, PublicKey.default, 'confirmed');

		expect(response?.earliestTx).to.equal('sigA');
		expect(response?.mostRecentTx).to.equal('sigD');
	});
});
