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
} from '../sdk/src';
import {
	initializeQuoteSpotMarket,
	mockUSDCMint,
	mockUserUSDCAccount,
	mockOracleNoProgram,
	overWritePerpMarket,
} from './testHelpers';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';
import dotenv from 'dotenv';
import {
	CustomBorshAccountsCoder,
	CustomBorshCoder,
} from '../sdk/src/decode/customCoder';
dotenv.config();

describe('LiteSVM Overwrite Accounts', () => {
	const program = anchor.workspace.Drift as Program;
	// @ts-ignore
	program.coder.accounts = new CustomBorshAccountsCoder(program.idl);
	let contextWrapper: LiteSVMContextWrapper;
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
		const context = await startLiteSVM('', [], []);

		// @ts-ignore
		contextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(contextWrapper);
		spotMarketOracle = await mockOracleNoProgram(contextWrapper, 80);

		const keypair = new Keypair();
		await contextWrapper.fundKeypair(keypair, 50 * LAMPORTS_PER_SOL);

		adminClient = new TestClient({
			connection: contextWrapper.connection.toConnection(),
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
			contextWrapper,
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
			contextWrapper,
			perpMarket.pubkey,
			perpMarket
		);

		const updatedPerpMarket = adminClient.getPerpMarketAccount(0);
		expect(updatedPerpMarket.amm.baseAssetAmountWithAmm.eq(newBaseAssetAmount))
			.to.be.true;
	});
});
