import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	TestClient,
	BN,
	OracleSource,
	SPOT_MARKET_RATE_PRECISION,
	SPOT_MARKET_WEIGHT_PRECISION,
} from '../../packages/sdk/src';

import {
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import {
	getInsuranceFundVaultPublicKey,
	getSpotMarketPublicKey,
	getSpotMarketVaultPublicKey,
} from '../../packages/sdk';
import { PublicKey } from '@solana/web3.js';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('max deposit', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;

	let svmContextWrapper: LiteSVMContextWrapper;

	let bulkAccountLoader: TestBulkAccountLoader;

	let usdcMint;
	let _userUSDCAccount;

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
		_userUSDCAccount = await mockUserUSDCAccount(
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

		const optimalUtilization = SPOT_MARKET_RATE_PRECISION.div(
			new BN(2)
		).toNumber(); // 50% utilization
		const optimalRate = SPOT_MARKET_RATE_PRECISION.toNumber();
		const maxRate = SPOT_MARKET_RATE_PRECISION.toNumber();
		const initialAssetWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const maintenanceAssetWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const initialLiabilityWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const maintenanceLiabilityWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const imfFactor = 0;

		await velocityClient.initializeSpotMarket(
			usdcMint.publicKey,
			optimalUtilization,
			optimalRate,
			maxRate,
			PublicKey.default,
			OracleSource.QUOTE_ASSET,
			initialAssetWeight,
			maintenanceAssetWeight,
			initialLiabilityWeight,
			maintenanceLiabilityWeight,
			imfFactor,
			undefined,
			undefined,
			false
		);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('delete', async () => {
		const txSig = await velocityClient.deleteInitializedSpotMarket(0);

		svmContextWrapper.connection.printTxLogs(txSig);

		const spotMarketKey = await getSpotMarketPublicKey(
			velocityClient.program.programId,
			0
		);

		let result = await svmContextWrapper.connection.getAccountInfoAndContext(
			spotMarketKey,
			'processed'
		);
		assert(result.value === null);

		const spotMarketVaultKey = await getSpotMarketVaultPublicKey(
			velocityClient.program.programId,
			0
		);

		result = await svmContextWrapper.connection.getAccountInfoAndContext(
			spotMarketVaultKey,
			'processed'
		);
		assert(result.value === null);

		const ifVaultKey = await getInsuranceFundVaultPublicKey(
			velocityClient.program.programId,
			0
		);

		result = await svmContextWrapper.connection.getAccountInfoAndContext(
			ifVaultKey,
			'processed'
		);
		assert(result.value === null);
	});

	it('re initialize', async () => {
		const optimalUtilization = SPOT_MARKET_RATE_PRECISION.div(
			new BN(2)
		).toNumber(); // 50% utilization
		const optimalRate = SPOT_MARKET_RATE_PRECISION.toNumber();
		const maxRate = SPOT_MARKET_RATE_PRECISION.toNumber();
		const initialAssetWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const maintenanceAssetWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const initialLiabilityWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const maintenanceLiabilityWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const imfFactor = 0;

		try {
			await velocityClient.initializeSpotMarket(
				usdcMint.publicKey,
				optimalUtilization,
				optimalRate,
				maxRate,
				PublicKey.default,
				OracleSource.QUOTE_ASSET,
				initialAssetWeight,
				maintenanceAssetWeight,
				initialLiabilityWeight,
				maintenanceLiabilityWeight,
				imfFactor,
				undefined,
				undefined,
				false
			);
		} catch (e) {
			console.error(e);
		}
	});
});
