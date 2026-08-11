import { expect } from 'chai';
import sinon from 'sinon';
import {
	Connection,
	PublicKey,
	TransactionInstruction,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import { encode } from '@msgpack/msgpack';
import { BN } from '../../src/isomorphic/anchor';
import { JupiterClient } from '../../src/jupiter/jupiterClient';
import { TitanClient } from '../../src/titan/titanClient';
import { SwapProvider, SwapQuote } from '../../src/swap/types';

// jupiterClient imports node-fetch; titanClient uses the global. Stub both.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const nodeFetch = require('node-fetch');

const INPUT_MINT = new PublicKey('So11111111111111111111111111111111111111112');
const OUTPUT_MINT = new PublicKey(
	'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'
);
const HOP_MINT = new PublicKey('mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So');
const USER = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');
const OTHER_WALLET = new PublicKey(
	'4kSjWQnPCFCkzKnFuNCMhutFsPzWqMbEnJyxgAJLwLjE'
);

const COMPUTE_BUDGET = new PublicKey(
	'ComputeBudget111111111111111111111111111111'
);
const ATA_PROGRAM = new PublicKey(
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
);
const AMM_PROGRAM = new PublicKey(
	'675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8'
);

const BLOCKHASH = '11111111111111111111111111111111';
const AMOUNT_IN = '153200000';
const AMOUNT_OUT = '1999997166';
const SLIPPAGE_BPS = 175;

/** Resolves to the rejection reason, or fails if the promise resolves. */
const captureError = async (promise: Promise<unknown>): Promise<Error> => {
	try {
		await promise;
	} catch (err) {
		return err as Error;
	}

	throw new Error('expected the promise to reject, but it resolved');
};

/**
 * The route both providers are made to return: velocity-supplied setup that has
 * to be stripped, an intermediate-hop account that has to survive, and the AMM
 * hop itself.
 */
const ROUTE: Array<{
	programId: PublicKey;
	keys: PublicKey[];
	data: number[];
}> = [
	{ programId: COMPUTE_BUDGET, keys: [], data: [2, 64, 66, 15, 0] },
	{ programId: ATA_PROGRAM, keys: [USER, USER, USER, OUTPUT_MINT], data: [1] },
	{ programId: ATA_PROGRAM, keys: [USER, USER, USER, HOP_MINT], data: [1] },
	{ programId: AMM_PROGRAM, keys: [USER, HOP_MINT], data: [9, 1, 2, 3] },
];

/** Compared instead of the instructions themselves: flags normalize differently
 * through Jupiter's compile/decompile round trip, and identity is what matters. */
const summarize = (instructions: TransactionInstruction[]) =>
	instructions.map((instruction) => ({
		programId: instruction.programId.toString(),
		keys: instruction.keys.map((key) => key.pubkey.toString()),
		data: Buffer.from(instruction.data).toString('hex'),
	}));

const jupiterQuoteBody = {
	inputMint: INPUT_MINT.toString(),
	outputMint: OUTPUT_MINT.toString(),
	inAmount: AMOUNT_IN,
	outAmount: AMOUNT_OUT,
	swapMode: 'ExactIn',
	slippageBps: SLIPPAGE_BPS,
	routePlan: [],
};

/** Jupiter's `/swap` reply: the route compiled into a versioned transaction. */
const jupiterSwapTransaction = (): string => {
	const message = new TransactionMessage({
		payerKey: USER,
		recentBlockhash: BLOCKHASH,
		instructions: ROUTE.map(
			({ programId, keys, data }) =>
				new TransactionInstruction({
					programId,
					keys: keys.map((pubkey) => ({
						pubkey,
						isSigner: pubkey.equals(USER),
						isWritable: true,
					})),
					data: Buffer.from(data),
				})
		),
	}).compileToV0Message();

	return Buffer.from(new VersionedTransaction(message).serialize()).toString(
		'base64'
	);
};

/**
 * The same route as a `/swap/v2/build` body: instructions raw rather than
 * compiled into a transaction, split across the fields v2 returns them in.
 */
const jupiterBuildBody = () => {
	const asApiInstruction = ({
		programId,
		keys,
		data,
	}: (typeof ROUTE)[number]) => ({
		programId: programId.toString(),
		accounts: keys.map((pubkey) => ({
			pubkey: pubkey.toString(),
			isSigner: pubkey.equals(USER),
			isWritable: true,
		})),
		data: Buffer.from(data).toString('base64'),
	});

	return {
		...jupiterQuoteBody,
		otherAmountThreshold: AMOUNT_OUT,
		priceImpactPct: '0',
		computeBudgetInstructions: [asApiInstruction(ROUTE[0])],
		setupInstructions: [asApiInstruction(ROUTE[1]), asApiInstruction(ROUTE[2])],
		swapInstruction: asApiInstruction(ROUTE[3]),
		cleanupInstruction: null,
		otherInstructions: [],
		tipInstruction: null,
		addressesByLookupTableAddress: {},
	};
};

/** Titan's msgpack quote reply carrying the same route. */
const titanQuoteBuffer = (): ArrayBuffer => {
	const encoded = encode({
		id: 'quote-1',
		inputMint: INPUT_MINT.toBytes(),
		outputMint: OUTPUT_MINT.toBytes(),
		swapMode: 'ExactIn',
		amount: Number(AMOUNT_IN),
		quotes: {
			best: {
				inAmount: Number(AMOUNT_IN),
				outAmount: Number(AMOUNT_OUT),
				slippageBps: SLIPPAGE_BPS,
				steps: [],
				addressLookupTables: [],
				instructions: ROUTE.map(({ programId, keys, data }) => ({
					p: programId.toBytes(),
					a: keys.map((pubkey) => ({
						p: pubkey.toBytes(),
						s: pubkey.equals(USER),
						w: true,
					})),
					d: new Uint8Array(data),
				})),
			},
		},
	});

	return encoded.slice().buffer;
};

/**
 * Runs both real clients over the same route.
 *
 * The point of the `SwapProvider` interface is that a caller gets the same
 * semantics whichever provider is configured — so this exercises both clients'
 * own quote parsing, route extraction and filtering rather than stubbing them
 * out. Stubbing the provider only tests that the unified client forwards.
 */
describe('SwapProvider parity', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let jupiter: JupiterClient;
	let jupiterV2: JupiterClient;
	let titan: TitanClient;
	let providers: Array<{ name: string; provider: SwapProvider }>;

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);

		sinon.stub(nodeFetch, 'default').callsFake(async (url: unknown) => {
			const path = String(url);
			const body = path.includes('/v2/build')
				? jupiterBuildBody()
				: path.includes('/quote')
				? jupiterQuoteBody
				: { swapTransaction: jupiterSwapTransaction() };

			return { ok: true, status: 200, json: async () => body } as never;
		});

		sinon.stub(global, 'fetch').callsFake(
			async () =>
				({
					ok: true,
					status: 200,
					arrayBuffer: async () => titanQuoteBuffer(),
				}) as never
		);

		jupiter = new JupiterClient({
			connection: connection as unknown as Connection,
		});
		jupiterV2 = new JupiterClient({
			connection: connection as unknown as Connection,
			apiVersion: 'v2',
		});
		titan = new TitanClient({
			connection: connection as unknown as Connection,
			authToken: '',
		});

		providers = [
			{ name: 'jupiter-v1', provider: jupiter },
			{ name: 'jupiter-v2', provider: jupiterV2 },
			{ name: 'titan', provider: titan },
		];
	});

	afterEach(() => {
		sinon.restore();
	});

	const quoteFor = (provider: SwapProvider): Promise<SwapQuote> =>
		provider.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(AMOUNT_IN),
			userPublicKey: USER,
			slippageBps: SLIPPAGE_BPS,
		});

	/** Asserts every provider produced the same thing, naming the odd one out. */
	const expectAgreement = <T>(results: T[]) => {
		const [first, ...rest] = results;

		rest.forEach((result, index) =>
			expect(result, `${providers[index + 1].name} differs`).to.deep.equal(
				first
			)
		);

		return first;
	};

	it('normalizes the same quote fields from every provider', async () => {
		const quotes = await Promise.all(
			providers.map(({ provider }) => quoteFor(provider))
		);

		const normalized = (quote: SwapQuote) => ({
			inputMint: quote.inputMint,
			outputMint: quote.outputMint,
			inAmount: quote.inAmount,
			outAmount: quote.outAmount,
			swapMode: quote.swapMode,
			slippageBps: quote.slippageBps,
		});

		expectAgreement(quotes.map(normalized));
	});

	it('strips the same setup and keeps the same hops from every provider', async () => {
		// The filter used to be duplicated per client and had already drifted;
		// this fails if either copy is reintroduced or diverges again.
		const filtered = await Promise.all(
			providers.map(async ({ provider }) => {
				const quote = await quoteFor(provider);
				const { instructions } = await provider.getRouteInstructions({
					quote,
					userPublicKey: USER,
				});
				return summarize(instructions);
			})
		);

		expect(expectAgreement(filtered).map((ix) => ix.programId)).to.deep.equal([
			ATA_PROGRAM.toString(),
			AMM_PROGRAM.toString(),
		]);
	});

	it('builds the same standalone transaction, setup included, from every provider', async () => {
		// Unlike getRouteInstructions, nothing is stripped — the caller signs and
		// sends this transaction itself, so it needs the provider's own setup.
		connection.getLatestBlockhash.resolves({
			blockhash: BLOCKHASH,
			lastValidBlockHeight: 1,
		});

		const built = await Promise.all(
			providers.map(async ({ provider }) => {
				const quote = await quoteFor(provider);
				const transaction = await provider.getSwapTransaction({
					quote,
					userPublicKey: USER,
				});

				expect(transaction.message.staticAccountKeys[0].equals(USER)).to.equal(
					true
				);

				return summarize(
					TransactionMessage.decompile(transaction.message).instructions
				);
			})
		);

		expect(expectAgreement(built).map((ix) => ix.programId)).to.deep.equal(
			ROUTE.map((ix) => ix.programId.toString())
		);
	});

	it('builds at the quoted slippage, not a provider default', async () => {
		// Jupiter's /swap defaulted to 50bps, so a 175bps quote silently executed
		// at 50 while Titan honoured what it had baked in.
		const quote = await quoteFor(jupiter);
		await jupiter.getRouteInstructions({ quote, userPublicKey: USER });

		// Matched on the method, not the path — Jupiter's base URL already ends
		// in `/swap`, so every call's URL contains it.
		const swapCall = (nodeFetch.default as sinon.SinonStub)
			.getCalls()
			.find((call) => call.args[1]?.method === 'POST');
		const body = JSON.parse(swapCall?.args[1]?.body as string);

		expect(body.slippageBps).to.equal(SLIPPAGE_BPS);
		// Our own wrapper, not part of Jupiter's request schema.
		expect(body.quoteResponse).to.not.have.property('providerRoute');
	});

	it('builds at the quoted slippage on jupiter v2, in one request', async () => {
		// v2 has no second request to re-state slippage on: the build the quote
		// carries was priced at what /build was asked for.
		const quote = await quoteFor(jupiterV2);
		await jupiterV2.getRouteInstructions({ quote, userPublicKey: USER });

		const calls = (nodeFetch.default as sinon.SinonStub).getCalls();
		expect(calls).to.have.length(1);
		expect(String(calls[0].args[0])).to.contain(`slippageBps=${SLIPPAGE_BPS}`);
		expect(quote.slippageBps).to.equal(SLIPPAGE_BPS);
	});

	(['jupiter-v1', 'jupiter-v2', 'titan'] as const).forEach((name) => {
		const clientFor = (): SwapProvider =>
			name === 'jupiter-v1'
				? jupiter
				: name === 'jupiter-v2'
				? jupiterV2
				: titan;
		/** A quote the client under test must refuse: from the other vendor. */
		const otherVendor = (): SwapProvider =>
			name === 'titan' ? jupiter : titan;
		const vendor = name === 'titan' ? 'titan' : 'jupiter';

		it(`${name} rejects a quote from the other provider`, async () => {
			const provider = clientFor();

			const foreign = await quoteFor(otherVendor());
			const err = await captureError(
				provider.getRouteInstructions({ quote: foreign, userPublicKey: USER })
			);

			expect(err.message).to.contain(vendor);
		});

		it(`${name} rejects a quote whose pair was rewritten after quoting`, async () => {
			const provider = clientFor();

			// The route is untouched, so it still swaps the pair it was quoted for
			// — but every guard that reads the quote's own mints (velocity's spot
			// market check, the instruction filter) now sees a different pair.
			const edited = {
				...(await quoteFor(provider)),
				outputMint: HOP_MINT.toString(),
			} as SwapQuote;

			const err = await captureError(
				provider.getRouteInstructions({ quote: edited, userPublicKey: USER })
			);

			expect(err.message).to.contain('outputMint');
			expect(err.message).to.contain('modified after it was returned');
		});

		it(`${name} rejects a quote whose size was rewritten after quoting`, async () => {
			const provider = clientFor();

			// `beginSwap` is funded from the quote's `inAmount`, so a rewritten one
			// releases an amount the route was never priced to consume.
			const edited = {
				...(await quoteFor(provider)),
				inAmount: '1',
			} as SwapQuote;

			const err = await captureError(
				provider.getRouteInstructions({ quote: edited, userPublicKey: USER })
			);

			expect(err.message).to.contain('inAmount');
			expect(err.message).to.contain(AMOUNT_IN);
		});

		it(`${name} rejects a quote with no route payload`, async () => {
			const provider = clientFor();

			const bare = { ...(await quoteFor(provider)) } as Record<string, unknown>;
			delete bare.providerRoute;

			const err = await captureError(
				provider.getRouteInstructions({
					quote: bare as unknown as SwapQuote,
					userPublicKey: USER,
				})
			);

			expect(err.message).to.contain('missing its provider route');
		});
	});

	// Wallet-bound providers: Titan resolves the user's token accounts at quote
	// time, and Jupiter v2 builds the route for a named `taker`. Jupiter v1 quotes
	// are wallet-independent by construction — it builds per-wallet at swap time —
	// so it has nothing to bind and is deliberately absent here.
	(['jupiter-v2', 'titan'] as const).forEach((name) => {
		it(`${name} rejects a route quoted for a different wallet`, async () => {
			const provider: SwapProvider = name === 'titan' ? titan : jupiterV2;

			const quote = await quoteFor(provider);
			const err = await captureError(
				provider.getRouteInstructions({ quote, userPublicKey: OTHER_WALLET })
			);

			expect(err.message).to.contain(USER.toString());
			expect(err.message).to.contain(OTHER_WALLET.toString());
		});
	});
});
