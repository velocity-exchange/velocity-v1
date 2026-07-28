import { expect } from 'chai';
import sinon from 'sinon';
import {
	AddressLookupTableAccount,
	Connection,
	PublicKey,
} from '@solana/web3.js';
import { encode } from '@msgpack/msgpack';
import { BN } from '../../src/isomorphic/anchor';
import { TitanClient } from '../../src/titan/titanClient';
import { SwapQuote, buildSwapQuote } from '../../src/swap/types';

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

describe('TitanClient.getQuote', () => {
	let client: TitanClient;

	/** Titan's msgpack reply for a route that consumes `inAmount`. */
	const quoteResponse = (
		inAmount: number | bigint,
		outAmount: number | bigint,
		envelope: Record<string, unknown> = {}
	) => ({
		ok: true,
		status: 200,
		arrayBuffer: async () =>
			encode(
				{
					id: 'quote-1',
					inputMint: INPUT_MINT.toBytes(),
					outputMint: OUTPUT_MINT.toBytes(),
					swapMode: 'ExactOut',
					amount: outAmount,
					quotes: {
						best: {
							inAmount,
							outAmount,
							slippageBps: 50,
							steps: [],
							addressLookupTables: [],
							instructions: [
								{ p: USER.toBytes(), a: [], d: new Uint8Array([1]) },
							],
						},
					},
					...envelope,
				},
				{ useBigInt64: true }
			).slice().buffer,
	});

	beforeEach(() => {
		client = new TitanClient({
			connection: sinon.createStubInstance(Connection) as unknown as Connection,
			authToken: '',
		});
	});

	afterEach(() => {
		sinon.restore();
	});

	it('reports the route input, not the requested amount, under ExactOut', async () => {
		// The request is the desired output here. Reporting it as `inAmount` made
		// callers size `beginSwap` off the output amount.
		sinon
			.stub(global, 'fetch')
			.resolves(quoteResponse(1234567, 2000000) as never);

		const quote = await client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(2000000),
			userPublicKey: USER,
			swapMode: 'ExactOut',
		});

		expect(quote.inAmount).to.equal('1234567');
		expect(quote.outAmount).to.equal('2000000');
	});

	it('binds the route to the quoting wallet', async () => {
		sinon
			.stub(global, 'fetch')
			.resolves(quoteResponse(1000000, 2000000) as never);

		const quote = await client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(1000000),
			userPublicKey: USER,
		});

		expect(quote.providerRoute).to.have.property('quotedFor', USER.toString());
	});

	it('decodes a u64 amount that does not fit a double', async () => {
		// Decoded without `useBigInt64` these round to the nearest double, and the
		// rounding is silent: 9007199254740993 comes back as ...992. `beginSwap` is
		// funded from `inAmount`, so the release would be short of what the route
		// consumes.
		const inAmount = BigInt('9007199254740993');
		sinon
			.stub(global, 'fetch')
			.resolves(
				quoteResponse(inAmount, BigInt('18446744073709551615')) as never
			);

		const quote = await client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN('18446744073709551615'),
			userPublicKey: USER,
			swapMode: 'ExactOut',
		});

		expect(quote.inAmount).to.equal('9007199254740993');
		expect(quote.outAmount).to.equal('18446744073709551615');
	});

	it('rejects a route for a pair other than the one requested', async () => {
		// A route for another pair pays out into a token account `endSwap` isn't
		// watching, and only fails on-chain after the funds have moved.
		const otherMint = new PublicKey(
			'mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So'
		);
		sinon.stub(global, 'fetch').resolves(
			quoteResponse(1000000, 2000000, {
				outputMint: otherMint.toBytes(),
			}) as never
		);

		const err = await captureError(
			client.getQuote({
				inputMint: INPUT_MINT,
				outputMint: OUTPUT_MINT,
				amount: new BN(1000000),
				userPublicKey: USER,
			})
		);

		expect(err.message).to.contain(otherMint.toString());
		expect(err.message).to.contain(OUTPUT_MINT.toString());
	});

	it('refuses to quote without a wallet', async () => {
		const err = await captureError(
			client.getQuote({
				inputMint: INPUT_MINT,
				outputMint: OUTPUT_MINT,
				amount: new BN(1000000),
			})
		);

		expect(err.message).to.contain('userPublicKey');
	});
});

describe('TitanClient.getRouteInstructions', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: TitanClient;

	const QUOTE_FIELDS = {
		inputMint: INPUT_MINT.toString(),
		outputMint: OUTPUT_MINT.toString(),
		inAmount: '1000000',
		outAmount: '13131908',
		swapMode: 'ExactIn' as const,
		slippageBps: 50,
		routePlan: [],
	};

	/** A quote as `getQuote` would return it, carrying its own route. */
	const quoteWithRoute = (
		instructions: unknown[],
		route: Record<string, unknown> = {},
		quotedFor?: string
	): SwapQuote =>
		buildSwapQuote(QUOTE_FIELDS, {
			provider: 'titan',
			route: { instructions, addressLookupTables: [], ...route },
			quotedFor,
		});

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

	it('does not fetch a blockhash to build a route', async () => {
		// The route's instructions are used as-is; no TransactionMessage is
		// constructed, so there is nothing that needs a blockhash.
		const quote = quoteWithRoute([
			{
				p: new PublicKey(
					'T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT'
				).toBytes(),
				a: [],
				d: new Uint8Array([1]),
			},
		]);

		await client.getRouteInstructions({ quote, userPublicKey: USER });

		expect(connection.getLatestBlockhash.called).to.be.false;
	});

	it('reuses a cached lookup table on a second build', async () => {
		const altKey = new PublicKey(
			'HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc'
		);
		connection.getAddressLookupTable.resolves({
			context: { slot: 1 },
			value: { key: altKey } as AddressLookupTableAccount,
		});

		const quote = quoteWithRoute(
			[
				{
					p: new PublicKey(
						'T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT'
					).toBytes(),
					a: [],
					d: new Uint8Array([1]),
				},
			],
			{ addressLookupTables: [altKey.toBytes()] }
		);

		await client.getRouteInstructions({ quote, userPublicKey: USER });
		await client.getRouteInstructions({ quote, userPublicKey: USER });

		expect(connection.getAddressLookupTable.calledOnce).to.be.true;
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

	it('rejects a route quoted for a different wallet', async () => {
		// Titan resolves the user's token accounts at quote time, so executing
		// someone else's route moves funds through accounts the signer doesn't
		// own. Nothing about the route itself makes that visible.
		const other = new PublicKey('4kSjWQnPCFCkzKnFuNCMhutFsPzWqMbEnJyxgAJLwLjE');
		const quote = quoteWithRoute([], {}, other.toString());

		const err = await captureError(
			client.getRouteInstructions({ quote, userPublicKey: USER })
		);

		expect(err.message).to.contain(other.toString());
		expect(err.message).to.contain(USER.toString());
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
