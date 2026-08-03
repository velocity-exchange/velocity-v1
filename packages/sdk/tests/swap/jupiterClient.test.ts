import { expect } from 'chai';
import sinon from 'sinon';
import {
	AddressLookupTableAccount,
	Connection,
	PublicKey,
	TransactionMessage,
} from '@solana/web3.js';
import { BN } from '../../src/isomorphic/anchor';
import {
	JupiterApiInstruction,
	JupiterBuildResponse,
	JupiterClient,
} from '../../src/jupiter/jupiterClient';

// jupiterClient does `import fetch from 'node-fetch'`, which compiles to a
// `.default` property read on the module object at call time — so stubbing that
// property intercepts it. Stubbing `global.fetch` would not.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const nodeFetch = require('node-fetch');

/** Resolves to the rejection reason, or fails if the promise resolves. */
const captureError = async (promise: Promise<unknown>): Promise<Error> => {
	try {
		await promise;
	} catch (err) {
		return err as Error;
	}

	throw new Error('expected the promise to reject, but it resolved');
};

const INPUT_MINT = new PublicKey('So11111111111111111111111111111111111111112');
const OUTPUT_MINT = new PublicKey(
	'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'
);

const validQuoteBody = {
	inputMint: INPUT_MINT.toString(),
	outputMint: OUTPUT_MINT.toString(),
	inAmount: '153200000',
	outAmount: '1999997166',
	swapMode: 'ExactIn',
	slippageBps: 10,
	routePlan: [],
};

const jsonResponse = (
	body: unknown,
	init?: { ok?: boolean; status?: number }
) =>
	({
		ok: init?.ok ?? true,
		status: init?.status ?? 200,
		statusText: 'Error',
		json: async () => body,
	}) as unknown as Response;

describe('JupiterClient.getQuote', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: JupiterClient;
	let fetchStub: sinon.SinonStub;

	const getQuote = () =>
		client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(153200000),
		});

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);
		client = new JupiterClient({
			connection: connection as unknown as Connection,
		});
		fetchStub = sinon.stub(nodeFetch, 'default');
	});

	afterEach(() => {
		sinon.restore();
	});

	it('returns a valid quote unchanged', async () => {
		fetchStub.resolves(jsonResponse(validQuoteBody));

		const quote = await getQuote();

		expect(quote.inputMint).to.equal(INPUT_MINT.toString());
		expect(quote.outAmount).to.equal('1999997166');
	});

	it('attaches the route payload the swap step needs', async () => {
		// Carried on the quote rather than cached on the client, so building a
		// swap can't pick up a route from some other in-flight quote.
		fetchStub.resolves(jsonResponse(validQuoteBody));

		const quote = await getQuote();

		expect(quote.providerRoute.provider).to.equal('jupiter');
		expect(quote.providerRoute).to.have.property('quote');
	});

	it('throws on a non-OK response, surfacing the provider error', async () => {
		fetchStub.resolves(
			jsonResponse(
				{ error: 'Route not found', errorCode: 'ROUTE_NOT_FOUND' },
				{ ok: false, status: 422 }
			)
		);

		const err = await captureError(getQuote());

		expect(err.message).to.contain('422');
		expect(err.message).to.contain('Route not found');
	});

	it('preserves errorCode when a non-OK response has no error message', async () => {
		fetchStub.resolves(
			jsonResponse({ errorCode: 'ROUTE_NOT_FOUND' }, { ok: false, status: 422 })
		);

		const err = await captureError(getQuote());

		expect(err.message).to.contain('ROUTE_NOT_FOUND');
	});

	it('throws when a 200 response carries an error payload', async () => {
		// The failure mode that produced "missing field `inputMint`" downstream:
		// a parseable error body that passes a truthiness check.
		fetchStub.resolves(
			jsonResponse({ error: 'Route not found', errorCode: 'ROUTE_NOT_FOUND' })
		);

		const err = await captureError(getQuote());

		expect(err.message).to.contain('Route not found');
	});

	it('throws when the response is missing route fields', async () => {
		fetchStub.resolves(jsonResponse({ swapMode: 'ExactIn', routePlan: [] }));

		const err = await captureError(getQuote());

		expect(err.message).to.contain('missing route fields');
	});

	it('throws when the body is not parseable JSON', async () => {
		fetchStub.resolves({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => {
				throw new Error('Unexpected token < in JSON');
			},
		} as unknown as Response);

		const err = await captureError(getQuote());

		expect(err.message).to.contain('Jupiter quote failed');
	});
});

const USER = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');
const HOP_MINT = new PublicKey('mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So');
const ALT_ADDRESS = new PublicKey(
	'DttEs7CNMNwtH4gc5cfusPJn3xHvavEt8eAfDtDGTEFc'
);

const COMPUTE_BUDGET_PROGRAM = 'ComputeBudget111111111111111111111111111111';
const SYSTEM_PROGRAM = '11111111111111111111111111111111';
const TOKEN_PROGRAM = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';
const ATA_PROGRAM = 'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL';
/** Jupiter v6 — what every observed v2 route executes through. */
const JUPITER_V6_PROGRAM = 'JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4';

const apiIx = (
	programId: string,
	pubkeys: PublicKey[],
	data: number[]
): JupiterApiInstruction => ({
	programId,
	accounts: pubkeys.map((pubkey) => ({
		pubkey: pubkey.toString(),
		isSigner: pubkey.equals(USER),
		isWritable: true,
	})),
	data: Buffer.from(data).toString('base64'),
});

/** An ATA create for `mint`; the mint sits at key index 3. */
const ataCreate = (mint: PublicKey): JupiterApiInstruction =>
	apiIx(ATA_PROGRAM, [USER, USER, USER, mint], [1]);

/**
 * Shaped on a real `/swap/v2/build` response.
 *
 * `computeBudgetInstructions` holds a `SetComputeUnitPrice` (discriminator 3)
 * and nothing else, because that is all v2 sends — unlike v1's `/swap`, it never
 * supplies a `SetComputeUnitLimit`. A fixture with a limit would hide that.
 */
const validBuildBody: JupiterBuildResponse = {
	inputMint: INPUT_MINT.toString(),
	outputMint: OUTPUT_MINT.toString(),
	inAmount: '153200000',
	outAmount: '1999997166',
	otherAmountThreshold: '1997997169',
	swapMode: 'ExactIn',
	slippageBps: 10,
	priceImpactPct: '0',
	routePlan: [],
	computeBudgetInstructions: [
		apiIx(COMPUTE_BUDGET_PROGRAM, [], [3, 64, 66, 15, 0, 0, 0, 0, 0]),
	],
	setupInstructions: [],
	swapInstruction: apiIx(JUPITER_V6_PROGRAM, [USER], [9, 1, 2, 3]),
	cleanupInstruction: null,
	otherInstructions: [],
	tipInstruction: null,
	addressesByLookupTableAddress: {},
};

describe('JupiterClient v2 (/swap/v2/build)', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: JupiterClient;
	let fetchStub: sinon.SinonStub;

	const getQuote = (overrides: Record<string, unknown> = {}) =>
		client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(153200000),
			userPublicKey: USER,
			slippageBps: 10,
			...overrides,
		});

	const quoteUrl = () => String(fetchStub.firstCall.args[0]);

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);
		client = new JupiterClient({
			connection: connection as unknown as Connection,
			apiVersion: 'v2',
		});
		fetchStub = sinon.stub(nodeFetch, 'default');
	});

	afterEach(() => {
		sinon.restore();
	});

	it('quotes and builds in one request, bound to the taker', async () => {
		fetchStub.resolves(jsonResponse(validBuildBody));

		const quote = await getQuote();

		expect(fetchStub.callCount).to.equal(1);
		expect(quoteUrl()).to.contain('/v2/build?');

		const params = new URLSearchParams(quoteUrl().split('?')[1]);
		expect(params.get('taker')).to.equal(USER.toString());
		expect(params.get('slippageBps')).to.equal('10');
		// v1-only, and actively unsafe on v2 — see the autoSlippage case below.
		expect(params.has('autoSlippage')).to.be.false;
		expect(params.has('maxAutoSlippageBps')).to.be.false;
		expect(params.has('autoSlippageCollisionUsdValue')).to.be.false;

		expect(quote.outAmount).to.equal('1999997166');
		expect(quote.providerRoute.provider).to.equal('jupiter');
		expect(quote.providerRoute.quotedFor).to.equal(USER.toString());
	});

	// `/build` is ExactIn-only: sent ExactOut, the live API answers 200 with
	// `swapMode: 'ExactIn'` and spends `amount` as the input, so a caller asking
	// to receive `amount` would instead spend it. adminClient/velocityClient both
	// size `beginSwap` off an ExactOut quote, so inverting it silently is unsafe.
	it('rejects ExactOut rather than letting v2 reinterpret it as ExactIn', async () => {
		const err = await captureError(getQuote({ swapMode: 'ExactOut' }));

		expect(err.message).to.contain("swapMode 'ExactOut' is not supported");
		expect(err.message).to.contain('apiVersion: "v1"');
		expect(fetchStub.called).to.be.false;
	});

	it('does not send swapMode, which v2 removed from its contract', async () => {
		fetchStub.resolves(jsonResponse(validBuildBody));

		await getQuote({ swapMode: 'ExactIn' });

		expect(quoteUrl()).to.not.contain('swapMode');
	});

	// Each verified against the live v2 API to still bind, despite `onlyDirectRoutes`
	// being absent from v2's documented parameter list.
	it('forwards the routing constraints v2 still honours', async () => {
		fetchStub.resolves(jsonResponse(validBuildBody));

		await getQuote({
			onlyDirectRoutes: true,
			maxAccounts: 45,
			excludeDexes: ['Raydium CLMM'],
		});

		const params = new URLSearchParams(quoteUrl().split('?')[1]);
		expect(params.get('onlyDirectRoutes')).to.equal('true');
		expect(params.get('maxAccounts')).to.equal('45');
		expect(params.get('excludeDexes')).to.equal('Raydium CLMM');
	});

	it('rejects a quote request with no taker', async () => {
		const err = await captureError(
			client.getQuote({
				inputMint: INPUT_MINT,
				outputMint: OUTPUT_MINT,
				amount: new BN(153200000),
			})
		);

		expect(err.message).to.contain('userPublicKey is required');
		expect(fetchStub.called).to.be.false;
	});

	it('rejects autoSlippage rather than silently quoting at zero tolerance', async () => {
		// v2 ignores the auto-slippage params and answers `slippageBps: 0` with
		// `otherAmountThreshold == outAmount`, so any adverse move reverts the swap.
		const err = await captureError(
			getQuote({
				autoSlippage: true,
				maxAutoSlippageBps: 100,
				usdEstimate: 500,
			})
		);

		expect(err.message).to.contain('autoSlippage is not supported');
		expect(err.message).to.contain('apiVersion: "v1"');
		expect(fetchStub.called).to.be.false;
	});

	// v2 answers with three unrelated error shapes. Interpolating `error` renders
	// the ZodError one as `[object Object]`, which is how a rejected request used
	// to reach the caller.
	(
		[
			[
				'a ZodError validation body',
				{
					success: false,
					error: {
						issues: [
							{
								code: 'invalid_type',
								expected: 'string',
								received: 'undefined',
								path: ['taker'],
								message: 'Required',
							},
						],
						name: 'ZodError',
					},
				},
				{ ok: false, status: 400 },
				['ZodError', 'taker', 'Required'],
			],
			[
				'a rate-limit body',
				{ code: 429, message: 'Too many requests' },
				{ ok: false, status: 429 },
				['Too many requests'],
			],
			[
				'a v1-style routing failure',
				{ error: 'Route not found', errorCode: 'ROUTE_NOT_FOUND' },
				{ ok: false, status: 422 },
				['Route not found'],
			],
		] as const
	).forEach(([label, body, init, expected]) => {
		it(`describes ${label} readably`, async () => {
			fetchStub.resolves(jsonResponse(body, init));

			const err = await captureError(getQuote());

			expect(err.message).to.not.contain('[object Object]');
			expected.forEach((fragment) => expect(err.message).to.contain(fragment));
		});
	});

	it('builds route instructions without a second request', async () => {
		const lookupTable = { key: ALT_ADDRESS } as AddressLookupTableAccount;
		connection.getAddressLookupTable.resolves({
			context: { slot: 1 },
			value: lookupTable,
		});
		fetchStub.resolves(
			jsonResponse({
				...validBuildBody,
				addressesByLookupTableAddress: {
					[ALT_ADDRESS.toString()]: [USER.toString()],
				},
			})
		);

		const quote = await getQuote();
		const { instructions, lookupTables } = await client.getRouteInstructions({
			quote,
			userPublicKey: USER,
		});

		// The whole point of v2: the build already carries its instructions.
		expect(fetchStub.callCount).to.equal(1);
		expect(instructions.map((ix) => ix.programId.toString())).to.deep.equal([
			JUPITER_V6_PROGRAM,
		]);
		expect(lookupTables).to.deep.equal([lookupTable]);
		expect(
			connection.getAddressLookupTable.firstCall.args[0].equals(ALT_ADDRESS)
		).to.be.true;
	});

	it('strips every instruction velocity supplies itself, tip included', async () => {
		// A System-program transfer between beginSwap and endSwap fails the
		// program's instruction whitelist, so neither the Jito tip nor anything in
		// otherInstructions may survive into the bracket.
		fetchStub.resolves(
			jsonResponse({
				...validBuildBody,
				setupInstructions: [
					ataCreate(INPUT_MINT),
					ataCreate(OUTPUT_MINT),
					ataCreate(HOP_MINT),
				],
				cleanupInstruction: apiIx(TOKEN_PROGRAM, [USER], [9]),
				otherInstructions: [apiIx(SYSTEM_PROGRAM, [USER, USER], [2, 0])],
				tipInstruction: apiIx(SYSTEM_PROGRAM, [USER, USER], [2, 1]),
			})
		);

		const quote = await getQuote();
		const { instructions } = await client.getRouteInstructions({
			quote,
			userPublicKey: USER,
		});

		expect(
			instructions.map((ix) => ({
				programId: ix.programId.toString(),
				mint: ix.keys[3]?.pubkey.toString(),
			}))
		).to.deep.equal([
			// Nothing else creates the intermediate hop's account.
			{ programId: ATA_PROGRAM, mint: HOP_MINT.toString() },
			{ programId: JUPITER_V6_PROGRAM, mint: undefined },
		]);
	});

	// filterRouteInstructions is a denylist — it keeps what it doesn't recognize.
	// The bracket list is therefore selected from the route's own instructions, so
	// a tip that isn't a plain System transfer still cannot reach the chain.
	it('keeps a non-System tip out of the bracket', async () => {
		const JITO_TIP_PROGRAM = 'T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt';
		fetchStub.resolves(
			jsonResponse({
				...validBuildBody,
				otherInstructions: [apiIx(JITO_TIP_PROGRAM, [USER, USER], [7, 0])],
				tipInstruction: apiIx(JITO_TIP_PROGRAM, [USER, USER], [7, 1]),
			})
		);

		const quote = await getQuote();
		const { instructions } = await client.getRouteInstructions({
			quote,
			userPublicKey: USER,
		});

		expect(instructions.map((ix) => ix.programId.toString())).to.deep.equal([
			JUPITER_V6_PROGRAM,
		]);
	});

	it('rejects a 200 body that carries no build instructions', async () => {
		const {
			computeBudgetInstructions: _cb,
			swapInstruction: _swap,
			addressesByLookupTableAddress: _alts,
			...withoutInstructions
		} = validBuildBody;
		fetchStub.resolves(jsonResponse(withoutInstructions));

		const err = await captureError(getQuote());

		// Without this the quote succeeds and dies later inside the build step as
		// `build.computeBudgetInstructions is not iterable`.
		expect(err.message).to.contain('missing build instructions');
	});

	// A route needing no lookup tables reports the field as null, which must not
	// read as a malformed build.
	it('accepts a build with no address lookup tables', async () => {
		fetchStub.resolves(
			jsonResponse({ ...validBuildBody, addressesByLookupTableAddress: null })
		);

		const quote = await getQuote();
		const { lookupTables } = await client.getRouteInstructions({
			quote,
			userPublicKey: USER,
		});

		expect(lookupTables).to.deep.equal([]);
		expect(connection.getAddressLookupTable.called).to.be.false;
	});

	it('names an address lookup table it could not resolve', async () => {
		connection.getAddressLookupTable.resolves({
			context: { slot: 1 },
			value: null,
		});
		fetchStub.resolves(
			jsonResponse({
				...validBuildBody,
				addressesByLookupTableAddress: {
					[ALT_ADDRESS.toString()]: [USER.toString()],
				},
			})
		);

		const quote = await getQuote();
		const err = await captureError(
			client.getRouteInstructions({ quote, userPublicKey: USER })
		);

		// Compiling without the table silently falls back to static keys.
		expect(err.message).to.contain(ALT_ADDRESS.toString());
		expect(err.message).to.contain('missing address lookup table');
	});

	// v2 sends a CU price and no CU limit, so a standalone transaction is left on
	// the runtime default unless the caller sets one.
	it('sets an explicit compute unit limit when asked, and none otherwise', async () => {
		connection.getLatestBlockhash.resolves({
			blockhash: '11111111111111111111111111111111',
			lastValidBlockHeight: 1,
		});
		fetchStub.resolves(jsonResponse(validBuildBody));

		const quote = await getQuote();
		const computeBudgetData = async (computeUnitLimit?: number) => {
			const transaction = await client.getSwapTransaction({
				quote,
				userPublicKey: USER,
				computeUnitLimit,
			});

			return TransactionMessage.decompile(transaction.message)
				.instructions.filter(
					(ix) => ix.programId.toString() === COMPUTE_BUDGET_PROGRAM
				)
				.map((ix) => ix.data[0]);
		};

		// 3 = SetComputeUnitPrice, all Jupiter sends. 2 = SetComputeUnitLimit.
		expect(await computeBudgetData()).to.deep.equal([3]);
		expect(await computeBudgetData(400_000)).to.deep.equal([2, 3]);
	});

	it('compiles a standalone transaction from the full build, tip included', async () => {
		connection.getLatestBlockhash.resolves({
			blockhash: '11111111111111111111111111111111',
			lastValidBlockHeight: 1,
		});
		fetchStub.resolves(
			jsonResponse({
				...validBuildBody,
				setupInstructions: [ataCreate(OUTPUT_MINT)],
				cleanupInstruction: apiIx(TOKEN_PROGRAM, [USER], [9]),
				otherInstructions: [apiIx(SYSTEM_PROGRAM, [USER, USER], [2, 0])],
				tipInstruction: apiIx(SYSTEM_PROGRAM, [USER, USER], [2, 1]),
			})
		);

		const quote = await getQuote();
		const transaction = await client.getSwapTransaction({
			quote,
			userPublicKey: USER,
		});

		expect(transaction.message.staticAccountKeys[0].equals(USER)).to.be.true;
		expect(
			TransactionMessage.decompile(transaction.message).instructions.map((ix) =>
				ix.programId.toString()
			)
		).to.deep.equal([
			COMPUTE_BUDGET_PROGRAM,
			ATA_PROGRAM,
			JUPITER_V6_PROGRAM,
			TOKEN_PROGRAM,
			SYSTEM_PROGRAM,
			SYSTEM_PROGRAM,
		]);
	});

	it('rejects a route quoted for a different wallet', async () => {
		fetchStub.resolves(jsonResponse(validBuildBody));

		const quote = await getQuote();
		const err = await captureError(
			client.getRouteInstructions({
				quote,
				userPublicKey: new PublicKey(
					'4kSjWQnPCFCkzKnFuNCMhutFsPzWqMbEnJyxgAJLwLjE'
				),
			})
		);

		expect(err.message).to.contain(USER.toString());
	});
});

describe('JupiterClient.getLookupTable', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: JupiterClient;

	const accountKey = new PublicKey(
		'HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc'
	);
	const lookupTable = { key: accountKey } as AddressLookupTableAccount;

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);
		client = new JupiterClient({
			connection: connection as unknown as Connection,
		});
	});

	afterEach(() => {
		sinon.restore();
	});

	it('caches a fetched lookup table so repeat reads skip the RPC', async () => {
		connection.getAddressLookupTable.resolves({
			context: { slot: 1 },
			value: lookupTable,
		});

		const first = await client.getLookupTable(accountKey);
		const second = await client.getLookupTable(accountKey);

		expect(first).to.equal(lookupTable);
		expect(second).to.equal(lookupTable);
		expect(connection.getAddressLookupTable.calledOnce).to.be.true;
	});

	it('does not cache a miss', async () => {
		connection.getAddressLookupTable.resolves({
			context: { slot: 1 },
			value: null,
		});

		expect(await client.getLookupTable(accountKey)).to.be.undefined;
		expect(await client.getLookupTable(accountKey)).to.be.undefined;
		expect(connection.getAddressLookupTable.calledTwice).to.be.true;
	});
});
