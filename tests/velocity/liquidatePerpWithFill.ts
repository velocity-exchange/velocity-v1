import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import {
	BASE_PRECISION,
	BN,
	EventSubscriber,
	isVariant,
	LIQUIDATION_PCT_PRECISION,
	OracleGuardRails,
	OracleSource,
	PositionDirection,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	TestClient,
	Wallet,
} from '../../packages/sdk/src';
import { assert } from 'chai';

import { Keypair, LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';

import {
	createUserWithUSDCAccount,
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	setFeedPriceNoProgram,
} from './testHelpers';
import {
	OrderType,
	PERCENTAGE_PRECISION,
	PerpOperation,
} from '../../packages/sdk';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('liquidate perp (no open orders)', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint;
	let userUSDCAccount;

	const liquidatorKeyPair = new Keypair();
	let liquidatorUSDCAccount: Keypair;
	let liquidatorVelocityClient: TestClient;

	let makerVelocityClient: TestClient;
	let makerUSDCAccount: PublicKey;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(10 * 10 ** 6);
	const makerUsdcAmount = new BN(1000 * 10 ** 6);

	let oracle: PublicKey;

	before(async () => {
		const context = startLiteSVM();

		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		eventSubscriber = new EventSubscriber(
			svmContextWrapper.connection.toConnection(),
			//@ts-ignore
			chProgram
		);

		await eventSubscriber.subscribe();

		usdcMint = await mockUSDCMint(svmContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);

		oracle = await mockOracleNoProgram(
			svmContextWrapper,
			1,
			-7,
			undefined,
			10000
		);

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [
				{
					publicKey: oracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();

		await velocityClient.updateInitialPctToLiquidate(
			LIQUIDATION_PCT_PRECISION.toNumber()
		);

		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));

		const oracleGuardRails: OracleGuardRails = {
			priceDivergence: {
				markOraclePercentDivergence: PERCENTAGE_PRECISION.muln(100),
				oracleTwap5MinPercentDivergence: PERCENTAGE_PRECISION.muln(100),
			},
			validity: {
				slotsBeforeStaleForAmm: new BN(100),
				slotsBeforeStaleForMargin: new BN(100),
				confidenceIntervalMaxSize: new BN(100000),
				tooVolatileRatio: new BN(11), // allow 11x change
			},
		};

		await velocityClient.updateOracleGuardRails(oracleGuardRails);

		const periodicity = new BN(0);

		await velocityClient.initializePerpMarket(
			0,

			oracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity
		);

		await velocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);

		await velocityClient.openPosition(
			PositionDirection.LONG,
			new BN(175).mul(BASE_PRECISION).div(new BN(10)), // 17.5 SOL
			0,
			new BN(0)
		);

		svmContextWrapper.fundKeypair(liquidatorKeyPair, LAMPORTS_PER_SOL);
		liquidatorUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper,
			liquidatorKeyPair.publicKey
		);
		liquidatorVelocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: new Wallet(liquidatorKeyPair),
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [
				{
					publicKey: oracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await liquidatorVelocityClient.subscribe();

		await liquidatorVelocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			liquidatorUSDCAccount.publicKey
		);

		[makerVelocityClient, makerUSDCAccount] = await createUserWithUSDCAccount(
			svmContextWrapper,
			usdcMint,
			chProgram,
			makerUsdcAmount,
			[0],
			[0],
			[
				{
					publicKey: oracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			bulkAccountLoader
		);

		await makerVelocityClient.deposit(makerUsdcAmount, 0, makerUSDCAccount);
	});

	after(async () => {
		await velocityClient.unsubscribe();
		await liquidatorVelocityClient.unsubscribe();
		await makerVelocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('liquidate', async () => {
		await setFeedPriceNoProgram(svmContextWrapper, 0.1, oracle, 10000);
		await velocityClient.updatePerpMarketPausedOperations(
			0,
			PerpOperation.AMM_FILL
		);

		try {
			const failToPlaceTxSig = await velocityClient.placePerpOrder({
				direction: PositionDirection.SHORT,
				baseAssetAmount: BASE_PRECISION,
				price: PRICE_PRECISION.divn(10),
				orderType: OrderType.LIMIT,
				reduceOnly: true,
				marketIndex: 0,
			});
			svmContextWrapper.connection.printTxLogs(failToPlaceTxSig);
			throw new Error('Expected placePerpOrder to throw an error');
		} catch (error) {
			if (
				error.message !==
				'Error processing Instruction 1: custom program error: 0x1773'
			) {
				throw new Error(`Unexpected error message: ${error.message}`);
			}
		}

		await makerVelocityClient.placePerpOrder({
			direction: PositionDirection.LONG,
			baseAssetAmount: new BN(175).mul(BASE_PRECISION),
			price: PRICE_PRECISION.divn(10),
			orderType: OrderType.LIMIT,
			marketIndex: 0,
		});

		const makerInfos = [
			{
				maker: await makerVelocityClient.getUserAccountPublicKey(),
				makerStats: makerVelocityClient.getUserStatsAccountPublicKey(),
				makerUserAccount: makerVelocityClient.getUserAccount(),
			},
		];

		const txSig = await liquidatorVelocityClient.liquidatePerpWithFill(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount(),
			0,
			makerInfos
		);

		svmContextWrapper.connection.printTxLogs(txSig);

		for (let i = 0; i < 32; i++) {
			assert(
				!isVariant(velocityClient.getUserAccount().orders[i].status, 'open')
			);
		}

		assert(
			liquidatorVelocityClient
				.getUserAccount()
				.perpPositions[0].quoteAssetAmount.eq(new BN(70))
		);

		assert(
			velocityClient
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.eq(new BN(0))
		);

		assert(
			velocityClient
				.getUserAccount()
				.perpPositions[0].quoteAssetAmount.eq(new BN(-15757943))
		);

		assert(
			liquidatorVelocityClient.getPerpMarketAccount(0).ifLiquidationFee ===
				10000
		);

		assert(
			makerVelocityClient
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.eq(new BN(17500000000))
		);

		assert(
			makerVelocityClient
				.getUserAccount()
				.perpPositions[0].quoteAssetAmount.eq(new BN(-1749957))
		);

		assert(
			liquidatorVelocityClient.getPerpMarketAccount(0).ifLiquidationFee ===
				10000
		);

		await makerVelocityClient.liquidatePerpPnlForDeposit(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount(),
			0,
			0,
			QUOTE_PRECISION.muln(20)
		);

		await makerVelocityClient.resolvePerpBankruptcy(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount(),
			0
		);
	});
});
