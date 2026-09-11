import { expect } from 'chai';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';
import {
	AccountToLoad,
	BulkAccountLoader,
} from '../../src/accounts/bulkAccountLoader';

const KEY_A = new PublicKey(Keypair.generate().publicKey);
const KEY_B = new PublicKey(Keypair.generate().publicKey);

// Data each account should come back with, so a mix-up is visible in the buffer.
const DATA_BY_KEY = new Map([
	[KEY_A.toBase58(), 'account-a-data'],
	[KEY_B.toBase58(), 'account-b-data'],
]);

/**
 * Answers the `getMultipleAccounts` batch correctly but hands the responses
 * back reversed, which JSON-RPC explicitly permits. `getMultipleAccounts`
 * results carry no pubkeys, so only the request ids can tell them apart.
 */
function reorderingConnection(): Connection {
	return {
		_rpcClient: {
			request: (
				batch: Array<{ id: number; params: any[] }>,
				callback: (error: any, responses: any) => void
			) => {
				const responses = batch.map((request) => ({
					id: request.id,
					result: {
						context: { slot: 1 },
						value: (request.params?.[0] ?? []).map((pubkey: string) => ({
							data: [
								Buffer.from(DATA_BY_KEY.get(pubkey) ?? '').toString('base64'),
								'base64',
							],
						})),
					},
				}));

				callback(null, responses.reverse());
			},
		},
	} as unknown as Connection;
}

function chunkFor(
	publicKey: PublicKey,
	received: Map<string, string>
): AccountToLoad[] {
	return [
		{
			publicKey,
			callbacks: new Map([
				[
					publicKey.toBase58(),
					(buffer: Buffer) =>
						received.set(publicKey.toBase58(), buffer.toString()),
				],
			]),
		},
	];
}

describe('BulkAccountLoader batch response order', () => {
	it('delivers each account its own data when the RPC reorders the batch', async () => {
		const received = new Map<string, string>();
		const loader = new BulkAccountLoader(
			reorderingConnection(),
			'processed',
			0
		);

		// Two chunks in one JSON-RPC batch, which is what `load()` builds for more
		// than GET_MULTIPLE_ACCOUNTS_CHUNK_SIZE accounts.
		await loader.loadChunk([
			chunkFor(KEY_A, received),
			chunkFor(KEY_B, received),
		]);

		expect(received.get(KEY_A.toBase58())).to.equal('account-a-data');
		expect(received.get(KEY_B.toBase58())).to.equal('account-b-data');
	});
});
