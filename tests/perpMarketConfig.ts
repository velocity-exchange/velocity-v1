import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import {
	BN,
	PEG_PRECISION,
	PRICE_PRECISION,
	TestClient,
	assert,
	MarketConfigFlag,
} from '../sdk/src';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import {
	initializeQuoteSpotMarket,
	mockUSDCMint,
	mockOracleNoProgram,
} from './testHelpers';

describe('perp market config flag', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let driftClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let contextWrapper: LiteSVMContextWrapper;
	let usdcMint;

	before(async () => {
		const context = await startLiteSVM('', [], []);

		contextWrapper = new LiteSVMContextWrapper(context as any);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(contextWrapper);

		driftClient = new TestClient({
			connection: contextWrapper.connection.toConnection(),
			wallet: contextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await driftClient.initialize(usdcMint.publicKey, true);
		await driftClient.subscribe();

		const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
		const ammInitialQuoteAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
			mantissaSqrtScale
		);
		const ammInitialBaseAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
			mantissaSqrtScale
		);

		await driftClient.initializePerpMarket(
			0,
			await mockOracleNoProgram(contextWrapper, 100),
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			new BN(0),
			new BN(100 * PEG_PRECISION.toNumber())
		);
		await driftClient.initializeAmmCache();
		await initializeQuoteSpotMarket(driftClient, usdcMint.publicKey);
	});

	after(async () => {
		await driftClient.unsubscribe();
	});

	it('set disable formulaic k update flag', async () => {
		const marketIndex = 0;

		let market = driftClient.getPerpMarketAccount(marketIndex);
		assert(market.marketConfig === 0);

		await driftClient.updatePerpMarketConfig(
			marketIndex,
			MarketConfigFlag.DISABLE_FORMULAIC_K_UPDATE
		);

		await driftClient.fetchAccounts();
		market = driftClient.getPerpMarketAccount(marketIndex);
		assert(
			(market.marketConfig & MarketConfigFlag.DISABLE_FORMULAIC_K_UPDATE) !== 0
		);
	});

	it('clear disable formulaic k update flag', async () => {
		const marketIndex = 0;

		await driftClient.updatePerpMarketConfig(marketIndex, 0);

		await driftClient.fetchAccounts();
		const market = driftClient.getPerpMarketAccount(marketIndex);
		assert(market.marketConfig === 0);
	});

	it('reject unknown bits in market config', async () => {
		const marketIndex = 0;
		let threw = false;
		try {
			await driftClient.updatePerpMarketConfig(marketIndex, 0xff);
		} catch (e) {
			threw = true;
		}
		assert(threw, 'should have thrown for invalid bits');
	});
});
