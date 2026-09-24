import * as anchor from '@coral-xyz/anchor';
import { expect } from 'chai';
import { Program } from '@coral-xyz/anchor';
import { Keypair, LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';
import {
	BN,
	TestClient,
	QUOTE_PRECISION,
	PRICE_PRECISION,
	PEG_PRECISION,
	OracleSource,
} from '../../packages/sdk/src';
import {
	initializeQuoteSpotMarket,
	mockUSDCMint,
	mockUserUSDCAccount,
	mockOracleNoProgram,
	overWritePerpMarket,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import dotenv from 'dotenv';
import {
	CustomBorshAccountsCoder,
	CustomBorshCoder,
} from '../../packages/sdk/src/decode/customCoder';
dotenv.config();

describe('LiteSVM Overwrite Accounts', () => {
	const program = anchor.workspace.Velocity as Program;
	// @ts-ignore
	program.coder.accounts = new CustomBorshAccountsCoder(program.idl);
	let svmContextWrapper: LiteSVMContextWrapper;
	let bulkAccountLoader: TestBulkAccountLoader;

	let adminClient: TestClient;
	let usdcMint: Keypair;
	let spotMarketOracle: PublicKey;

	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	let userUSDCAccount: Keypair;

	before(async () => {
		const context = startLiteSVM();

		// @ts-ignore
		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		spotMarketOracle = await mockOracleNoProgram(svmContextWrapper, 80);

		const keypair = new Keypair();
		await svmContextWrapper.fundKeypair(keypair, 50 * LAMPORTS_PER_SOL);

		adminClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: new anchor.Wallet(keypair),
			programID: program.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			subAccountIds: [],
			perpMarketIndexes: [0, 1],
			spotMarketIndexes: [0, 1, 2],
			oracleInfos: [
				{
					publicKey: spotMarketOracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
			coder: new CustomBorshCoder(program.idl),
		});
		await adminClient.initialize(usdcMint.publicKey, true);
		await adminClient.subscribe();
		await initializeQuoteSpotMarket(adminClient, usdcMint.publicKey);

		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			new BN(10).mul(QUOTE_PRECISION),
			svmContextWrapper,
			keypair.publicKey
		);

		await adminClient.initializeUserAccountAndDepositCollateral(
			new BN(10).mul(QUOTE_PRECISION),
			userUSDCAccount.publicKey
		);

		const periodicity = new BN(0);

		await adminClient.initializePerpMarket(
			0,
			spotMarketOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(80 * PEG_PRECISION.toNumber())
		);
	});

	after(async () => {
		await adminClient.unsubscribe();
	});

	it('should overwrite perp accounts', async () => {
		const perpMarket = adminClient.getPerpMarketAccount(0);

		const newBaseAssetAmount = new BN(123);
		perpMarket.amm.baseAssetAmountWithAmm = newBaseAssetAmount;

		await overWritePerpMarket(
			adminClient,
			svmContextWrapper,
			perpMarket.pubkey,
			perpMarket
		);

		const updatedPerpMarket = adminClient.getPerpMarketAccount(0);
		expect(updatedPerpMarket.amm.baseAssetAmountWithAmm.eq(newBaseAssetAmount))
			.to.be.true;
	});
});
