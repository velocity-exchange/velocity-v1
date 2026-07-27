import { expect } from 'chai';
import sinon from 'sinon';
import {
	AddressLookupTableAccount,
	Connection,
	PublicKey,
} from '@solana/web3.js';
import { BN } from '../../src/isomorphic/anchor';
import { JupiterClient } from '../../src/jupiter/jupiterClient';

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
