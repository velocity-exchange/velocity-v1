import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { PublicKey } from '@solana/web3.js';
import {
	AMM_RESERVE_PRECISION,
	BN,
	ContractTier,
	OracleSource,
	PEG_PRECISION,
	QUOTE_PRECISION,
	TestClient,
} from '../../packages/sdk/src';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	setFeedPriceNoProgram,
} from './testHelpers';

// update_perp_bid_ask_twap must project the curve onto the current oracle
// before sampling the AMM quote into the mark TWAP.
describe('update perp bid ask twap', () => {
	const program = anchor.workspace.Velocity as Program;

	let svm: LiteSVMContextWrapper;
	let client: TestClient;
	let oracle: PublicKey;

	before(async () => {
		svm = new LiteSVMContextWrapper(startLiteSVM());
		const loader = new TestBulkAccountLoader(svm.connection, 'processed', 1);
		const usdcMint = await mockUSDCMint(svm);
		oracle = await mockOracleNoProgram(svm, 100);

		client = new TestClient({
			connection: svm.connection.toConnection(),
			wallet: svm.provider.wallet,
			programID: program.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [{ publicKey: oracle, source: OracleSource.PYTH_LAZER }],
			userStats: true,
			accountSubscription: { type: 'polling', accountLoader: loader },
		});
		await client.initialize(usdcMint.publicKey, true);
		await client.subscribe();
		await initializeQuoteSpotMarket(client, usdcMint.publicKey);

		const reserves = AMM_RESERVE_PRECISION.mul(new BN(1_000_000));
		await client.initializePerpMarket(
			0,
			oracle,
			reserves,
			reserves,
			new BN(3600),
			new BN(100).mul(PEG_PRECISION),
			undefined,
			ContractTier.A
		);
		await client.updatePerpMarketBaseSpread(0, 100);
		await client.updatePerpMarketCurveUpdateIntensity(0, 100);

		// The crank requires the keeper to hold at least 1000 USDC of IF stake.
		await client.initializeUserAccount();
		const statsKey = client.getUserStatsAccountPublicKey();
		const stats = await client.program.account.userStats.fetch(statsKey);
		stats.ifStakedQuoteAssetAmount = QUOTE_PRECISION.mul(new BN(1000));
		const info = await svm.connection.getAccountInfo(statsKey);
		svm.context.setAccount(statsKey, {
			executable: false,
			owner: program.programId,
			lamports: info.lamports,
			data: await client.program.coder.accounts.encode('userStats', stats),
		});
	});

	after(async () => {
		await client.unsubscribe();
	});

	it('samples the curve repegged to the current oracle', async () => {
		await client.fetchAccounts();
		const before = client.getPerpMarketAccount(0);

		// Leave the stored peg 2% behind a fresh oracle.
		await svm.moveTimeForward(120);
		await setFeedPriceNoProgram(svm, 102, oracle);

		await client.updatePerpBidAskTwap(0, []);
		await client.fetchAccounts();
		const after = client.getPerpMarketAccount(0);

		assert(
			after.amm.pegMultiplier.gt(before.amm.pegMultiplier),
			`peg did not move: ${before.amm.pegMultiplier} -> ${after.amm.pegMultiplier}`
		);
		// A stale peg quotes a bid below 100 and drags the bid TWAP down. The
		// repegged curve quotes it near 102.
		assert(
			after.marketStats.lastBidPriceTwap.gt(
				before.marketStats.lastBidPriceTwap
			),
			`bid twap did not rise: ${before.marketStats.lastBidPriceTwap} -> ${after.marketStats.lastBidPriceTwap}`
		);
		assert(
			after.marketStats.lastAskPriceTwap.gt(before.marketStats.lastAskPriceTwap)
		);
	});
});
