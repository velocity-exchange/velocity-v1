import { expect } from 'chai';
import { Connection, PublicKey } from '@solana/web3.js';
import { fetchLogs } from '../../src/events/fetchLogs';

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
		_rpcBatchRequest: async (requests: { args: any[] }[]) =>
			requests.map(({ args }) => {
				const signature = args[0] as string;
				if (erroredSignatures.includes(signature)) {
					return {
						error: {
							code: -32015,
							message: 'Transaction version (1) is not supported',
						},
					};
				}
				return {
					result: {
						slot: 100 + SIGNATURES.indexOf(signature),
						transaction: { signatures: [signature] },
						meta: { logMessages: [] },
					},
				};
			}),
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

	it('returns undefined when no signature is safe to resume from', async () => {
		expect(await fetch(['sigA'])).to.equal(undefined);
		expect(await fetch(SIGNATURES)).to.equal(undefined);
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
			_rpcBatchRequest: async (requests: { args: any[] }[]) =>
				requests.map(({ args }) =>
					args[0] === 'sigC'
						? { result: null }
						: {
								result: {
									slot: 100 + SIGNATURES.indexOf(args[0] as string),
									transaction: { signatures: [args[0]] },
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
