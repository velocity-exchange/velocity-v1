import { expect } from 'chai';
import sinon from 'sinon';
import {
	AddressLookupTableAccount,
	Connection,
	PublicKey,
} from '@solana/web3.js';
import { TitanClient } from '../../src/titan/titanClient';
import { SwapQuote } from '../../src/swap/types';

const ALT_KEY = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');
const INPUT_MINT = new PublicKey('So11111111111111111111111111111111111111112');
const OUTPUT_MINT = new PublicKey(
	'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'
);
const USER = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');

/** Resolves to the rejection reason, or fails if the promise resolves. */
const captureError = async (promise: Promise<unknown>): Promise<Error> => {
	try {
		await promise;
	} catch (err) {
		return err as Error;
	}

	throw new Error('expected the promise to reject, but it resolved');
};

describe('TitanClient.fetchLookupTable', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: TitanClient;

	const lookupTable = { key: ALT_KEY } as AddressLookupTableAccount;

	// Private — a route's lookup tables all have to resolve or the transaction
	// silently exceeds the size limit, so the retry behaviour is worth pinning.
	const fetchLookupTable = (): Promise<AddressLookupTableAccount> =>
		(
			client as unknown as {
				fetchLookupTable: (k: PublicKey) => Promise<AddressLookupTableAccount>;
			}
		).fetchLookupTable(ALT_KEY);

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);
		client = new TitanClient({
			connection: connection as unknown as Connection,
			authToken: '',
		});
	});

	afterEach(() => {
		sinon.restore();
	});

	it('retries a transient RPC failure and returns the table on success', async () => {
		connection.getAddressLookupTable
			.onFirstCall()
			.rejects(new Error('429 Too Many Requests'))
			.onSecondCall()
			.resolves({ context: { slot: 1 }, value: lookupTable });

		expect(await fetchLookupTable()).to.equal(lookupTable);
		expect(connection.getAddressLookupTable.callCount).to.equal(2);
	});

	it('throws with the final error once retries are exhausted', async () => {
		connection.getAddressLookupTable.rejects(
			new Error('429 Too Many Requests')
		);

		const err = await captureError(fetchLookupTable());

		expect(err.message).to.contain('Failed to fetch address lookup table');
		expect(err.message).to.contain(ALT_KEY.toString());
		expect(err.message).to.contain('429 Too Many Requests');
		// initial attempt + LOOKUP_TABLE_FETCH_RETRIES
		expect(connection.getAddressLookupTable.callCount).to.equal(3);
	});

	it('fails fast when the table does not exist on-chain', async () => {
		connection.getAddressLookupTable.resolves({
			context: { slot: 1 },
			value: null,
		});

		const err = await captureError(fetchLookupTable());

		expect(err.message).to.contain('does not exist');
		// A missing table won't appear on a retry, so don't spend attempts on it.
		expect(connection.getAddressLookupTable.calledOnce).to.be.true;
	});

	it('does not silently drop an unresolvable table', async () => {
		// Regression guard: this used to be caught, warned, and skipped, which
		// built a route without the table and blew the transaction size limit.
		connection.getAddressLookupTable.rejects(new Error('Failed to fetch'));

		const err = await captureError(fetchLookupTable());

		expect(err).to.be.instanceOf(Error);
	});
});

describe('TitanClient.getRouteInstructions', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: TitanClient;

	/** A quote as `getQuote` would return it, carrying its own route. */
	const quoteWithRoute = (instructions: unknown[]): SwapQuote =>
		({
			inputMint: INPUT_MINT.toString(),
			outputMint: OUTPUT_MINT.toString(),
			inAmount: '1000000',
			outAmount: '13131908',
			swapMode: 'ExactIn',
			slippageBps: 50,
			routePlan: [],
			providerRoute: {
				provider: 'titan',
				route: { instructions, addressLookupTables: [] },
			},
		}) as unknown as SwapQuote;

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);
		connection.getLatestBlockhash.resolves({
			blockhash: '11111111111111111111111111111111',
			lastValidBlockHeight: 1,
		});
		client = new TitanClient({
			connection: connection as unknown as Connection,
			authToken: '',
		});
	});

	afterEach(() => {
		sinon.restore();
	});

	it('builds from the route on the quote it was handed', async () => {
		const titanProgram = new PublicKey(
			'T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT'
		);
		const quote = quoteWithRoute([
			{
				p: titanProgram.toBytes(),
				a: [{ p: USER.toBytes(), s: true, w: true }],
				d: new Uint8Array([1, 2, 3]),
			},
		]);

		const { instructions } = await client.getRouteInstructions({
			quote,
			userPublicKey: USER,
		});

		expect(instructions).to.have.lengthOf(1);
		expect(instructions[0].programId.equals(titanProgram)).to.be.true;
	});

	it('rejects a quote produced by a different provider', async () => {
		// The bug this interface exists to prevent: Titan used to ignore the
		// quote entirely and replay whatever route it had cached, so a mismatch
		// like this built a transaction for the wrong swap and only surfaced
		// on-chain as "amount_out must be greater than 0".
		const jupiterQuote = {
			...quoteWithRoute([]),
			providerRoute: { provider: 'jupiter', quote: {} },
		} as unknown as SwapQuote;

		const err = await captureError(
			client.getRouteInstructions({ quote: jupiterQuote, userPublicKey: USER })
		);

		expect(err.message).to.contain('jupiter');
		expect(err.message).to.contain('titan');
	});

	it('rejects a quote with no route payload', async () => {
		const bareQuote = { ...quoteWithRoute([]) } as Record<string, unknown>;
		delete bareQuote.providerRoute;

		const err = await captureError(
			client.getRouteInstructions({
				quote: bareQuote as unknown as SwapQuote,
				userPublicKey: USER,
			})
		);

		expect(err.message).to.contain('missing its provider route');
	});

	it('does not depend on a preceding getQuote call', async () => {
		// Two independent builds from the same quote must both succeed. The old
		// client cleared its cached route after one use, so the second failed.
		const quote = quoteWithRoute([
			{
				p: new PublicKey(
					'T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT'
				).toBytes(),
				a: [],
				d: new Uint8Array([1]),
			},
		]);

		const first = await client.getRouteInstructions({
			quote,
			userPublicKey: USER,
		});
		const second = await client.getRouteInstructions({
			quote,
			userPublicKey: USER,
		});

		expect(first.instructions).to.have.lengthOf(1);
		expect(second.instructions).to.have.lengthOf(1);
	});
});
