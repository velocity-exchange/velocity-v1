import { expect } from 'chai';
import sinon from 'sinon';
import { Connection, PublicKey } from '@solana/web3.js';
import { BN } from '../../src/isomorphic/anchor';
import { UnifiedSwapClient } from '../../src/swap/UnifiedSwapClient';
import { SwapQuote } from '../../src/swap/types';
import { MAX_TX_BYTE_SIZE } from '../../src/tx/utils';

// jupiterClient does `import fetch from 'node-fetch'`, which compiles to a
// `.default` property read on the module object at call time — so stubbing that
// property intercepts it. Stubbing `global.fetch` would not.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const nodeFetch = require('node-fetch');

const INPUT_MINT = new PublicKey('So11111111111111111111111111111111111111112');
const OUTPUT_MINT = new PublicKey(
	'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'
);
const USER = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');

/** 375 bytes are reserved for the velocity begin/end swap instructions. */
const EXPECTED_DEFAULT_SIZE_CONSTRAINT = MAX_TX_BYTE_SIZE - 375;

const stubQuote = (provider: 'jupiter' | 'titan'): SwapQuote =>
	({
		inputMint: INPUT_MINT.toString(),
		outputMint: OUTPUT_MINT.toString(),
		inAmount: '1000000',
		outAmount: '13131908',
		swapMode: 'ExactIn',
		slippageBps: 50,
		routePlan: [],
		providerRoute:
			provider === 'jupiter'
				? { provider, quote: {} }
				: { provider, route: {} },
	}) as unknown as SwapQuote;

/** Replaces the underlying provider so we can inspect what gets forwarded. */
const stubProvider = (client: UnifiedSwapClient, quote: SwapQuote) => {
	const getQuote = sinon.stub().resolves(quote);
	const getRouteInstructions = sinon
		.stub()
		.resolves({ instructions: [], lookupTables: [] });

	(
		client as unknown as {
			client: {
				getQuote: sinon.SinonStub;
				getRouteInstructions: sinon.SinonStub;
			};
		}
	).client = { getQuote, getRouteInstructions } as never;

	return { getQuote, getRouteInstructions };
};

describe('UnifiedSwapClient Titan route size constraint', () => {
	let client: UnifiedSwapClient;
	let getQuote: sinon.SinonStub;

	beforeEach(() => {
		client = new UnifiedSwapClient({
			clientType: 'titan',
			connection: sinon.createStubInstance(Connection) as unknown as Connection,
		});
		({ getQuote } = stubProvider(client, stubQuote('titan')));
	});

	afterEach(() => {
		sinon.restore();
	});

	it('derives the default size constraint from the real tx size limit', async () => {
		await client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(153200000),
			userPublicKey: USER,
		});

		// Guards against the previous `1280 - 375`, which over-allocated by 48
		// bytes because 1280 is the IPv6 MTU, not the tx size limit.
		expect(EXPECTED_DEFAULT_SIZE_CONSTRAINT).to.equal(857);
		expect(getQuote.firstCall.args[0].sizeConstraint).to.equal(
			EXPECTED_DEFAULT_SIZE_CONSTRAINT
		);
	});

	it('forwards an explicit size constraint unchanged', async () => {
		await client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(153200000),
			userPublicKey: USER,
			sizeConstraint: 512,
		});

		expect(getQuote.firstCall.args[0].sizeConstraint).to.equal(512);
	});
});

// Forwarding only — the provider is stubbed out, so these say nothing about the
// two clients agreeing. That parity is covered in providerParity.test.ts, which
// runs both real clients over the same route.
(['jupiter', 'titan'] as const).forEach((provider) => {
	describe(`UnifiedSwapClient.getRouteInstructions (${provider})`, () => {
		let client: UnifiedSwapClient;
		let quote: SwapQuote;
		let getQuote: sinon.SinonStub;
		let getRouteInstructions: sinon.SinonStub;

		const params = {
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(153200000),
			userPublicKey: USER,
		};

		beforeEach(() => {
			client = new UnifiedSwapClient({
				clientType: provider,
				connection: sinon.createStubInstance(
					Connection
				) as unknown as Connection,
			});
			quote = stubQuote(provider);
			({ getQuote, getRouteInstructions } = stubProvider(client, quote));
		});

		afterEach(() => {
			sinon.restore();
		});

		it('builds from the supplied quote without re-quoting', async () => {
			// Building must be a pure function of the quote passed in: quoting here
			// would build a route the user was never shown.
			await client.getRouteInstructions({ ...params, quote });

			expect(getQuote.called).to.be.false;
			expect(getRouteInstructions.firstCall.args[0].quote).to.equal(quote);
		});

		it('reports itself as the configured provider', () => {
			expect(client.providerName).to.equal(provider);
		});
	});
});

describe('UnifiedSwapClient Jupiter API version', () => {
	let fetchStub: sinon.SinonStub;

	const jupiterClient = (jupiterApiVersion?: 'v1' | 'v2') =>
		new UnifiedSwapClient({
			clientType: 'jupiter',
			connection: sinon.createStubInstance(Connection) as unknown as Connection,
			jupiterApiVersion,
		});

	/**
	 * The version is private on the Jupiter client, so read the forwarding the
	 * way a caller experiences it: which endpoint the quote request goes to. The
	 * body is deliberately unusable — the request has already been made by the
	 * time the client rejects it.
	 */
	const quotedUrl = async (client: UnifiedSwapClient) => {
		fetchStub.resolves({
			ok: true,
			status: 200,
			json: async () => ({}),
		} as unknown as Response);

		await client
			.getQuote({
				inputMint: INPUT_MINT,
				outputMint: OUTPUT_MINT,
				amount: new BN(1000000),
				userPublicKey: USER,
			})
			.catch(() => undefined);

		return String(fetchStub.firstCall.args[0]);
	};

	beforeEach(() => {
		fetchStub = sinon.stub(nodeFetch, 'default');
	});

	afterEach(() => {
		sinon.restore();
	});

	it('forwards an explicit version to the Jupiter client', async () => {
		expect(await quotedUrl(jupiterClient('v2'))).to.contain('/v2/build');
	});

	it('leaves the Jupiter client on its own default when unset', async () => {
		expect(await quotedUrl(jupiterClient())).to.contain('/v1/quote');
	});
});
