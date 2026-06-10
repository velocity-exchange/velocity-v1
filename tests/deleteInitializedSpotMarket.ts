import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	TestClient,
	BN,
	OracleSource,
	SPOT_MARKET_RATE_PRECISION,
	SPOT_MARKET_WEIGHT_PRECISION,
} from '../sdk/src';

import {
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import {
	getInsuranceFundVaultPublicKey,
	getSpotMarketPublicKey,
	getSpotMarketVaultPublicKey,
} from '../sdk';
import { PublicKey } from '@solana/web3.js';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';

describe('max deposit', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let driftClient: TestClient;

	let contextWrapper: LiteSVMContextWrapper;

	let bulkAccountLoader: TestBulkAccountLoader;

	let usdcMint;
	let _userUSDCAccount;

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
		_userUSDCAccount = await mockUserUSDCAccount(
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

		await driftClient.initializeSpotMarket(
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
		await driftClient.unsubscribe();
	});

	it('delete', async () => {
		const txSig = await driftClient.deleteInitializedSpotMarket(0);

		contextWrapper.connection.printTxLogs(txSig);

		const spotMarketKey = await getSpotMarketPublicKey(
			driftClient.program.programId,
			0
		);

		let result =
			await contextWrapper.connection.getAccountInfoAndContext(
				spotMarketKey,
				'processed'
			);
		assert(result.value === null);

		const spotMarketVaultKey = await getSpotMarketVaultPublicKey(
			driftClient.program.programId,
			0
		);

		result = await contextWrapper.connection.getAccountInfoAndContext(
			spotMarketVaultKey,
			'processed'
		);
		assert(result.value === null);

		const ifVaultKey = await getInsuranceFundVaultPublicKey(
			driftClient.program.programId,
			0
		);

		result = await contextWrapper.connection.getAccountInfoAndContext(
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
			await driftClient.initializeSpotMarket(
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
