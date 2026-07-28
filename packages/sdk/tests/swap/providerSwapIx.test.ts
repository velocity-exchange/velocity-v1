import { expect } from 'chai';
import sinon from 'sinon';
import { PublicKey, TransactionInstruction } from '@solana/web3.js';
import { BN } from '../../src/isomorphic/anchor';
import { VelocityClient } from '../../src/velocityClient';
import { DEFAULT_ROUTE_SIZE_CONSTRAINT } from '../../src/swap/UnifiedSwapClient';
import { SwapMode, SwapProvider, SwapQuote } from '../../src/swap/types';

const IN_MINT = new PublicKey('So11111111111111111111111111111111111111112');
const OUT_MINT = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const OTHER_MINT = new PublicKey('mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So');
const USER = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');
const IN_ATA = new PublicKey('4kSjWQnPCFCkzKnFuNCMhutFsPzWqMbEnJyxgAJLwLjE');
const OUT_ATA = new PublicKey('AVfynEVFCUiCCzhCkyDoQBQpVCEyPqmSUDmYjNvJVKmL');

const IN_MARKET_INDEX = 1;
const OUT_MARKET_INDEX = 2;

const AMOUNT_IN = '153200000';
const AMOUNT_OUT = '1999997166';

const marker = (tag: string) =>
	new TransactionInstruction({
		programId: new PublicKey('675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8'),
		keys: [],
		data: Buffer.from(tag),
	});

const BEGIN_IX = marker('begin');
const END_IX = marker('end');
const ROUTE_IX = marker('route');

const quoteFor = (
	swapMode: SwapMode,
	overrides: Partial<SwapQuote> = {}
): SwapQuote =>
	({
		inputMint: IN_MINT.toString(),
		outputMint: OUT_MINT.toString(),
		inAmount: AMOUNT_IN,
		outAmount: AMOUNT_OUT,
		swapMode,
		slippageBps: 50,
		routePlan: [],
		providerRoute: { provider: 'jupiter', quote: {} },
		...overrides,
	}) as unknown as SwapQuote;

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
 * The builder itself, over stubbed collaborators.
 *
 * `getProviderSwapIx` is the one place a quote is checked against the swap
 * `beginSwap`/`endSwap` is being built for, and the only thing that decides how
 * much `beginSwap` releases — both worth pinning without standing up a client
 * against a validator.
 */
describe('VelocityClient.getProviderSwapIx', () => {
	let client: VelocityClient;
	let getSwapIx: sinon.SinonStub;
	let getQuote: sinon.SinonStub;
	let getRouteInstructions: sinon.SinonStub;
	let provider: SwapProvider;

	const market = (marketIndex: number, mint: PublicKey) =>
		({ marketIndex, mint }) as never;

	const build = (params: Record<string, unknown>) =>
		client.getProviderSwapIx({
			swapProvider: provider,
			outMarketIndex: OUT_MARKET_INDEX,
			inMarketIndex: IN_MARKET_INDEX,
			// Supplied so the ATA lookup/creation path stays out of these cases.
			outAssociatedTokenAccount: OUT_ATA,
			inAssociatedTokenAccount: IN_ATA,
			...params,
		} as never);

	beforeEach(() => {
		getQuote = sinon.stub();
		getRouteInstructions = sinon
			.stub()
			.resolves({ instructions: [ROUTE_IX], lookupTables: [] });
		provider = {
			providerName: 'jupiter',
			getQuote,
			getRouteInstructions,
		} as unknown as SwapProvider;

		getSwapIx = sinon
			.stub()
			.resolves({ beginSwapIx: BEGIN_IX, endSwapIx: END_IX });

		client = Object.create(VelocityClient.prototype) as VelocityClient;
		Object.assign(client as unknown as Record<string, unknown>, {
			provider: { wallet: { publicKey: USER } },
			getSpotMarketAccountOrThrow: (marketIndex: number) =>
				marketIndex === IN_MARKET_INDEX
					? market(IN_MARKET_INDEX, IN_MINT)
					: market(OUT_MARKET_INDEX, OUT_MINT),
			getSwapIx,
		});
	});

	afterEach(() => {
		sinon.restore();
	});

	it('brackets the route between the begin/end pair', async () => {
		const { ixs } = await build({
			amount: new BN(AMOUNT_IN),
			quote: quoteFor('ExactIn'),
		});

		expect(ixs).to.deep.equal([BEGIN_IX, ROUTE_IX, END_IX]);
	});

	it('releases exactly the quote input under ExactIn', async () => {
		await build({ amount: new BN(AMOUNT_IN), quote: quoteFor('ExactIn') });

		expect(getSwapIx.firstCall.args[0].amountIn.toString()).to.equal(AMOUNT_IN);
	});

	it('releases the quote input plus 10bp under ExactOut', async () => {
		await build({ amount: new BN(AMOUNT_OUT), quote: quoteFor('ExactOut') });

		expect(getSwapIx.firstCall.args[0].amountIn.toString()).to.equal(
			new BN(AMOUNT_IN).muln(1001).divn(1000).toString()
		);
	});

	it("takes the effective mode from the quote, not the caller's swapMode", async () => {
		// The quote is what beginSwap is sized off, so its mode is what decides
		// which side `amount` refers to.
		await build({
			amount: new BN(AMOUNT_OUT),
			swapMode: 'ExactIn',
			quote: quoteFor('ExactOut'),
		});

		expect(getSwapIx.firstCall.args[0].amountIn.toString()).to.equal(
			new BN(AMOUNT_IN).muln(1001).divn(1000).toString()
		);
	});

	it('rejects an ExactIn quote for a different input amount', async () => {
		const err = await captureError(
			build({ amount: new BN(1000), quote: quoteFor('ExactIn') })
		);

		expect(err.message).to.contain(AMOUNT_IN);
		expect(err.message).to.contain('1000');
	});

	it('rejects an ExactOut quote for a different output amount', async () => {
		const err = await captureError(
			build({ amount: new BN(1000), quote: quoteFor('ExactOut') })
		);

		expect(err.message).to.contain(AMOUNT_OUT);
		expect(err.message).to.contain('1000');
	});

	it('accepts an ExactOut quote whose input differs from the amount', async () => {
		// Only the output side is the caller's request under ExactOut; the input
		// is whatever the route consumes.
		await build({ amount: new BN(AMOUNT_OUT), quote: quoteFor('ExactOut') });

		expect(getRouteInstructions.calledOnce).to.be.true;
	});

	it('rejects a quote for a different pair', async () => {
		const err = await captureError(
			build({
				amount: new BN(AMOUNT_IN),
				quote: quoteFor('ExactIn', { outputMint: OTHER_MINT.toString() }),
			})
		);

		expect(err.message).to.contain(OTHER_MINT.toString());
	});

	it('quotes the markets it is building for when no quote is passed', async () => {
		getQuote.resolves(quoteFor('ExactIn'));

		await build({ amount: new BN(AMOUNT_IN), slippageBps: 175 });

		const args = getQuote.firstCall.args[0];
		expect(args.inputMint.equals(IN_MINT)).to.be.true;
		expect(args.outputMint.equals(OUT_MINT)).to.be.true;
		expect(args.userPublicKey.equals(USER)).to.be.true;
		expect(args.swapMode).to.equal('ExactIn');
		expect(args.slippageBps).to.equal(175);
		expect(args.sizeConstraint).to.equal(DEFAULT_ROUTE_SIZE_CONSTRAINT);
	});

	it('sizes a fetched ExactOut swap off the quote, not the requested output', async () => {
		// Previously buffered `amount` — an output-denominated number — and
		// released that as the input.
		getQuote.resolves(quoteFor('ExactOut'));

		await build({ amount: new BN(AMOUNT_OUT), swapMode: 'ExactOut' });

		expect(getQuote.firstCall.args[0].swapMode).to.equal('ExactOut');
		expect(getSwapIx.firstCall.args[0].amountIn.toString()).to.equal(
			new BN(AMOUNT_IN).muln(1001).divn(1000).toString()
		);
	});

	it('rejects a fetched quote for a different pair', async () => {
		getQuote.resolves(
			quoteFor('ExactIn', { inputMint: OTHER_MINT.toString() })
		);

		const err = await captureError(build({ amount: new BN(AMOUNT_IN) }));

		expect(err.message).to.contain(OTHER_MINT.toString());
	});
});
