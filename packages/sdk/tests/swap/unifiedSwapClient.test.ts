import { expect } from 'chai';
import sinon from 'sinon';
import { Connection, PublicKey } from '@solana/web3.js';
import { BN } from '../../src/isomorphic/anchor';
import { UnifiedSwapClient } from '../../src/swap/UnifiedSwapClient';
import { MAX_TX_BYTE_SIZE } from '../../src/tx/utils';

const INPUT_MINT = new PublicKey('So11111111111111111111111111111111111111112');
const OUTPUT_MINT = new PublicKey(
	'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'
);
const USER = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');

/** 375 bytes are reserved for the velocity begin/end swap instructions. */
const EXPECTED_DEFAULT_SIZE_CONSTRAINT = MAX_TX_BYTE_SIZE - 375;

describe('UnifiedSwapClient Titan route size constraint', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: UnifiedSwapClient;
	let titanGetQuote: sinon.SinonStub;

	const getQuote = (sizeConstraint?: number) =>
		client.getQuote({
			inputMint: INPUT_MINT,
			outputMint: OUTPUT_MINT,
			amount: new BN(153200000),
			userPublicKey: USER,
			...(sizeConstraint === undefined ? {} : { sizeConstraint }),
		});

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);
		client = new UnifiedSwapClient({
			clientType: 'titan',
			connection: connection as unknown as Connection,
		});

		titanGetQuote = sinon.stub().resolves({});
		// Swap the underlying provider for a stub so we can inspect what the
		// unified layer forwards.
		(client as unknown as { client: { getQuote: sinon.SinonStub } }).client = {
			getQuote: titanGetQuote,
		} as never;
	});

	afterEach(() => {
		sinon.restore();
	});

	it('derives the default size constraint from the real tx size limit', async () => {
		await getQuote();

		// Guards against the previous `1280 - 375`, which over-allocated by 48
		// bytes because 1280 is the IPv6 MTU, not the tx size limit.
		expect(EXPECTED_DEFAULT_SIZE_CONSTRAINT).to.equal(857);
		expect(titanGetQuote.firstCall.args[0].sizeConstraint).to.equal(
			EXPECTED_DEFAULT_SIZE_CONSTRAINT
		);
	});

	it('forwards an explicit size constraint unchanged', async () => {
		await getQuote(512);

		expect(titanGetQuote.firstCall.args[0].sizeConstraint).to.equal(512);
	});
});
