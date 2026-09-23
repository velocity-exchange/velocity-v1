import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { Keypair } from '@solana/web3.js';

import {
	BN,
	PRICE_PRECISION,
	TestClient,
	PositionDirection,
	User,
	Wallet,
	EventSubscriber,
	BASE_PRECISION,
	getLimitOrderParams,
	OracleSource,
} from '../../packages/sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import {
	PEG_PRECISION,
	PerpOperation,
	PostOnlyParams,
} from '../../packages/sdk';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('place and make perp order', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let makerVelocityClient: TestClient;
	let makerVelocityClientUser: User;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	let usdcMint;
	let userUSDCAccount;

	const usdcAmount = new BN(100 * 10 ** 6);

	let solUsd;
	let marketIndexes;
	let spotMarketIndexes;
	let oracleInfos;

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
			chProgram
		);

		await eventSubscriber.subscribe();

		usdcMint = await mockUSDCMint(svmContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);

		solUsd = await mockOracleNoProgram(svmContextWrapper, 32.821);

		marketIndexes = [0];
		spotMarketIndexes = [0, 1];
		oracleInfos = [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }];

		makerVelocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: marketIndexes,
			spotMarketIndexes: spotMarketIndexes,
			subAccountIds: [],
			oracleInfos,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await makerVelocityClient.initialize(usdcMint.publicKey, true);
		await makerVelocityClient.subscribe();
		await initializeQuoteSpotMarket(makerVelocityClient, usdcMint.publicKey);

		const periodicity = new BN(0);
		await makerVelocityClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(32 * PEG_PRECISION.toNumber())
		);

		await makerVelocityClient.updatePerpMarketPausedOperations(
			0,
			PerpOperation.AMM_FILL
		);

		await makerVelocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);

		makerVelocityClientUser = new User({
			velocityClient: makerVelocityClient,
			userAccountPublicKey: await makerVelocityClient.getUserAccountPublicKey(),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await makerVelocityClientUser.subscribe();
	});

	after(async () => {
		await makerVelocityClient.unsubscribe();
		await makerVelocityClientUser.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('make', async () => {
		const keypair = new Keypair();
		await svmContextWrapper.fundKeypair(keypair, 10 ** 9);
		await bulkAccountLoader.load();
		const wallet = new Wallet(keypair);
		const userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper,
			keypair.publicKey
		);
		const takerVelocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: marketIndexes,
			spotMarketIndexes: spotMarketIndexes,
			subAccountIds: [],
			oracleInfos,
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await takerVelocityClient.subscribe();
		await takerVelocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);
		const takerVelocityClientUser = new User({
			velocityClient: takerVelocityClient,
			userAccountPublicKey: await takerVelocityClient.getUserAccountPublicKey(),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await takerVelocityClientUser.subscribe();

		const marketIndex = 0;
		const baseAssetAmount = BASE_PRECISION;
		const takerOrderParams = getLimitOrderParams({
			marketIndex,
			direction: PositionDirection.LONG,
			baseAssetAmount,
			price: new BN(34).mul(PRICE_PRECISION),
			auctionStartPrice: new BN(33).mul(PRICE_PRECISION),
			auctionEndPrice: new BN(34).mul(PRICE_PRECISION),
			auctionDuration: 10,
			userOrderId: 1,
			postOnly: PostOnlyParams.NONE,
		});
		await takerVelocityClient.placePerpOrder(takerOrderParams);
		await takerVelocityClientUser.fetchAccounts();
		const order = takerVelocityClientUser.getOrderByUserOrderId(1);
		assert(!order.postOnly);

		const makerOrderParams = getLimitOrderParams({
			marketIndex,
			direction: PositionDirection.SHORT,
			baseAssetAmount,
			price: new BN(33).mul(PRICE_PRECISION),
			userOrderId: 1,
			postOnly: PostOnlyParams.MUST_POST_ONLY,
			bitFlags: 1,
		});

		const txSig = await makerVelocityClient.placeAndMakePerpOrder(
			makerOrderParams,
			{
				taker: await takerVelocityClient.getUserAccountPublicKey(),
				order: takerVelocityClient.getOrderByUserId(1),
				takerUserAccount: takerVelocityClient.getUserAccount(),
				takerStats: takerVelocityClient.getUserStatsAccountPublicKey(),
			}
		);

		svmContextWrapper.printTxLogs(txSig);

		const makerPosition = makerVelocityClient.getUser().getPerpPosition(0);
		assert(makerPosition.baseAssetAmount.eq(BASE_PRECISION.neg()));

		const takerPosition = takerVelocityClient.getUser().getPerpPosition(0);
		assert(takerPosition.baseAssetAmount.eq(BASE_PRECISION));

		await takerVelocityClientUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
	});
});
