import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { TestClient, QUOTE_PRECISION, BN, OracleSource } from '../sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';

describe('max deposit', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let driftClient: TestClient;

	let bulkAccountLoader: TestBulkAccountLoader;

	let contextWrapper: LiteSVMContextWrapper;

	let usdcMint;
	let userUSDCAccount;

	const usdcAmount = new BN(10 * 10 ** 6);

	before(async () => {
		const context = await startLiteSVM('', [], []);

		contextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(contextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			contextWrapper
		);

		const solUsd = await mockOracleNoProgram(contextWrapper, 1);

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
			oracleInfos: [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }],
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await driftClient.initialize(usdcMint.publicKey, true);
		await driftClient.subscribe();
		await initializeQuoteSpotMarket(driftClient, usdcMint.publicKey);
	});

	after(async () => {
		await driftClient.unsubscribe();
	});

	it('update max deposit', async () => {
		await driftClient.updateSpotMarketMaxTokenDeposits(0, QUOTE_PRECISION);
		const market = driftClient.getSpotMarketAccount(0);
		console.assert(market.maxTokenDeposits.eq(QUOTE_PRECISION));
	});

	it('block deposit', async () => {
		try {
			await driftClient.initializeUserAccountAndDepositCollateral(
				usdcAmount,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			return;
		}
		assert(false);
	});
});
