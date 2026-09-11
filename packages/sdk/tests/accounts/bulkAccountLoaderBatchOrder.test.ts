import { expect } from 'chai';
import {
	Connection as SolanaConnection,
	Keypair,
	PublicKey,
} from '@solana/web3.js';
import {
	AccountToLoad,
	BulkAccountLoader,
} from '../../src/accounts/bulkAccountLoader';
import { Connection } from '../../src/bankrun/bankrunConnection';
import { stubRpcClient } from '../util/stubRpcClient';

const KEY_A = new PublicKey(Keypair.generate().publicKey);
const KEY_B = new PublicKey(Keypair.generate().publicKey);
const KEY_C = new PublicKey(Keypair.generate().publicKey);

// Data each account should come back with, so a mix-up is visible in the buffer.
const DATA_BY_KEY = new Map([
	[KEY_A.toBase58(), 'account-a-data'],
	[KEY_B.toBase58(), 'account-b-data'],
	[KEY_C.toBase58(), 'account-c-data'],
]);

/** `getMultipleAccounts` result for the pubkeys in one request, drawn from `DATA_BY_KEY`. */
function resultFor(pubkeys: string[]) {
	return {
		result: {
			context: { slot: 1 },
			value: pubkeys.map((pubkey) => ({
				data: [
					Buffer.from(DATA_BY_KEY.get(pubkey) ?? '').toString('base64'),
					'base64',
				],
			})),
		},
	};
}

function chunkFor(
	received: Map<string, string>,
	...publicKeys: PublicKey[]
): AccountToLoad[] {
	return publicKeys.map((publicKey) => ({
		publicKey,
		callbacks: new Map([
			[
				publicKey.toBase58(),
				(buffer: Buffer) =>
					received.set(publicKey.toBase58(), buffer.toString()),
			],
		]),
	}));
}

/** An `AccountToLoad` nobody is subscribed to, so it is never requested. */
function unsubscribed(publicKey: PublicKey): AccountToLoad {
	return { publicKey, callbacks: new Map() };
}

async function loadWith(
	connection: Connection,
	chunks: AccountToLoad[][]
): Promise<void> {
	await new BulkAccountLoader(connection, 'processed', 0).loadChunk(chunks);
}

describe('BulkAccountLoader batch response correlation', () => {
	it('delivers each account its own data when the RPC reorders the batch', async () => {
		const received = new Map<string, string>();
		// Two chunks in one JSON-RPC batch, which is what `load()` builds for more
		// than GET_MULTIPLE_ACCOUNTS_CHUNK_SIZE accounts.
		await loadWith(
			stubRpcClient(
				([pubkeys]: any[]) => resultFor(pubkeys),
				(responses) => responses.reverse()
			),
			[chunkFor(received, KEY_A), chunkFor(received, KEY_B)]
		);

		expect(received.get(KEY_A.toBase58())).to.equal('account-a-data');
		expect(received.get(KEY_B.toBase58())).to.equal('account-b-data');
	});

	it('correlates ids the server echoes back as strings', async () => {
		// JSON-RPC allows any id type, so a proxy may normalise 0 to "0". Matching
		// those by identity would miss every response and stall the loader silently.
		const received = new Map<string, string>();
		await loadWith(
			stubRpcClient(
				([pubkeys]: any[]) => resultFor(pubkeys),
				(responses) =>
					responses.map((response) => ({
						...response,
						id: String(response.id),
					}))
			),
			[chunkFor(received, KEY_A), chunkFor(received, KEY_B)]
		);

		expect(received.get(KEY_A.toBase58())).to.equal('account-a-data');
		expect(received.get(KEY_B.toBase58())).to.equal('account-b-data');
	});

	it('raises an error rather than silently skipping when a request goes unanswered', async () => {
		// A dropped or unmatchable response is a correlation failure, not a "no
		// result" one. Swallowing it would leave subscribers on stale data forever;
		// `load()` turns this rejection into its registered error callbacks.
		const received = new Map<string, string>();
		const failed = await loadWith(
			stubRpcClient(
				([pubkeys]: any[]) => resultFor(pubkeys),
				(responses) => responses.slice(1)
			),
			[chunkFor(received, KEY_A), chunkFor(received, KEY_B)]
		).catch((error: Error) => error);

		expect(received.size).to.equal(0);
		expect(failed).to.be.instanceOf(Error);
		expect((failed as Error).message).to.match(/no response matching id/);
	});

	it('raises an error rather than silently keeping the last response when ids collide', async () => {
		// Two responses sharing an id would otherwise sit in the same Map slot, so
		// every expected id is still present and the missing-id check never fires,
		// leaving a caller reading the wrong request's data.
		const received = new Map<string, string>();
		const failed = await loadWith(
			stubRpcClient(
				([pubkeys]: any[]) => resultFor(pubkeys),
				(responses) => responses.map((response) => ({ ...response, id: 0 }))
			),
			[chunkFor(received, KEY_A), chunkFor(received, KEY_B)]
		).catch((error: Error) => error);

		expect(received.size).to.equal(0);
		expect(failed).to.be.instanceOf(Error);
		expect((failed as Error).message).to.match(/duplicate response id/);
	});

	it('reads results back against the accounts it actually requested', async () => {
		// Accounts with no callbacks are filtered out of the request, so indexing
		// the response against the original chunk shifts every later account onto
		// the wrong account's data.
		const received = new Map<string, string>();
		await loadWith(
			stubRpcClient(([pubkeys]: any[]) => resultFor(pubkeys)),
			[
				[
					...chunkFor(received, KEY_A),
					unsubscribed(KEY_B),
					...chunkFor(received, KEY_C),
				],
			]
		);

		expect(received.get(KEY_A.toBase58())).to.equal('account-a-data');
		expect(received.get(KEY_C.toBase58())).to.equal('account-c-data');
	});

	it('reaches the real jayson batch path through a live Connection', async () => {
		// Every other test here stubs `_rpcClient`, so none of them prove the batch
		// is shaped the way jayson's batch path expects. In particular the response
		// callback must take exactly two parameters: declaring a third flips jayson
		// into splitting errors from results, and the code would read the error
		// array as the response array.
		const received = new Map<string, string>();
		const connection = new SolanaConnection('http://127.0.0.1:1', {
			fetch: (async (_url: any, options: any) => {
				const batch = JSON.parse(options.body);
				const responses = batch
					.map((request: any) => ({
						id: request.id,
						...resultFor(request.params[0]),
					}))
					.reverse();
				return {
					ok: true,
					status: 200,
					text: async () => JSON.stringify(responses),
				};
			}) as any,
		});

		await loadWith(connection, [
			chunkFor(received, KEY_A),
			chunkFor(received, KEY_B),
		]);

		expect(received.get(KEY_A.toBase58())).to.equal('account-a-data');
		expect(received.get(KEY_B.toBase58())).to.equal('account-b-data');
	});
});
