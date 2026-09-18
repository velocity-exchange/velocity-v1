import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	TestClient,
	QUOTE_PRECISION,
	BN,
	OracleSource,
} from '../../packages/sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('max deposit', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint;
	let userUSDCAccount;

	const usdcAmount = new BN(10 * 10 ** 6);

	before(async () => {
		const context = startLiteSVM();

		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);

		const solUsd = await mockOracleNoProgram(svmContextWrapper, 1);

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }],
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('update max deposit', async () => {
		await velocityClient.updateSpotMarketMaxTokenDeposits(0, QUOTE_PRECISION);
		const market = velocityClient.getSpotMarketAccount(0);
		console.assert(market.maxTokenDeposits.eq(QUOTE_PRECISION));
	});

	it('block deposit', async () => {
		try {
			await velocityClient.initializeUserAccountAndDepositCollateral(
				usdcAmount,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			return;
		}
		assert(false);
	});
});
