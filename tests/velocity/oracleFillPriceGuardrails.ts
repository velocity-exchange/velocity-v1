import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	TestClient,
	BN,
	PRICE_PRECISION,
	PositionDirection,
	EventSubscriber,
	MarketStatus,
	BASE_PRECISION,
	isVariant,
	OracleSource,
	PEG_PRECISION,
} from '../../packages/sdk/src';

import {
	createUserWithUSDCAccount,
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	setFeedPriceNoProgram,
} from './testHelpers';
import {
	MARGIN_PRECISION,
	OrderType,
	PerpOperation,
	PostOnlyParams,
} from '../../packages/sdk';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('oracle fill guardrails', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let fillerVelocityClient: TestClient;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint;
	let userUSDCAccount;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(100000);
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(100000 * 10 ** 6);

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

		solUsd = await mockOracleNoProgram(svmContextWrapper, 20);

		marketIndexes = [0, 1];
		spotMarketIndexes = [0];
		oracleInfos = [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }];

		fillerVelocityClient = new TestClient({
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
		await fillerVelocityClient.initialize(usdcMint.publicKey, true);
		await fillerVelocityClient.subscribe();
		await initializeQuoteSpotMarket(fillerVelocityClient, usdcMint.publicKey);
		// dont fill against the vamm
		await fillerVelocityClient.updatePerpAuctionDuration(new BN(100));

		const periodicity = new BN(60 * 60); // 1 HOUR

		await fillerVelocityClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(20 * PEG_PRECISION.toNumber())
		);
		await fillerVelocityClient.updatePerpMarketStatus(0, MarketStatus.ACTIVE);

		await fillerVelocityClient.updatePerpMarketBaseSpread(
			0,
			PRICE_PRECISION.toNumber() / 8
		);

		await fillerVelocityClient.updatePerpMarketMarginRatio(
			0,
			MARGIN_PRECISION.toNumber() / 2,
			MARGIN_PRECISION.toNumber() / 3
		);

		await fillerVelocityClient.updatePerpMarketMaxSpread(
			0,
			PRICE_PRECISION.toNumber() / 5
		);

		await fillerVelocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);

		await fillerVelocityClient.updatePerpMarketPausedOperations(
			0,
			PerpOperation.AMM_FILL
		);
	});

	beforeEach(async () => {
		await fillerVelocityClient.moveAmmPrice(
			0,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve
		);
	});

	after(async () => {
		await fillerVelocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('taker long solUsd', async () => {
		const [takerVelocityClient, takerUSDCAccount] =
			await createUserWithUSDCAccount(
				svmContextWrapper,
				usdcMint,
				chProgram,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);

		await takerVelocityClient.deposit(usdcAmount, 0, takerUSDCAccount);

		const [makerVelocityClient, makerUSDCAccount] =
			await createUserWithUSDCAccount(
				svmContextWrapper,
				usdcMint,
				chProgram,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);

		await makerVelocityClient.deposit(usdcAmount, 0, makerUSDCAccount);

		await setFeedPriceNoProgram(svmContextWrapper, 14, solUsd);
		await makerVelocityClient.placePerpOrder({
			marketIndex: 0,
			direction: PositionDirection.SHORT,
			price: new BN(14).mul(PRICE_PRECISION),
			orderType: OrderType.LIMIT,
			baseAssetAmount: BASE_PRECISION,
		});

		await setFeedPriceNoProgram(svmContextWrapper, 31, solUsd);

		await takerVelocityClient.placePerpOrder({
			marketIndex: 0,
			orderType: OrderType.LIMIT,
			auctionStartPrice: new BN(100).mul(PRICE_PRECISION),
			auctionEndPrice: new BN(100).mul(PRICE_PRECISION),
			auctionDuration: 100,
			price: new BN(100).mul(PRICE_PRECISION),
			direction: PositionDirection.LONG,
			baseAssetAmount: BASE_PRECISION,
		});

		// move price to $30
		await setFeedPriceNoProgram(svmContextWrapper, 30, solUsd);

		const makerInfo = [
			{
				maker: await makerVelocityClient.getUserAccountPublicKey(),
				makerUserAccount: makerVelocityClient.getUserAccount(),
				makerStats: await makerVelocityClient.getUserStatsAccountPublicKey(),
			},
		];
		const firstFillTxSig = await fillerVelocityClient.fillPerpOrder(
			await takerVelocityClient.getUserAccountPublicKey(),
			takerVelocityClient.getUserAccount(),
			takerVelocityClient.getOrder(1),
			makerInfo
		);

		svmContextWrapper.printTxLogs(firstFillTxSig);

		// assert that the
		const orderActionRecord =
			eventSubscriber.getEventsArray('OrderActionRecord')[0];
		// console.log(eventSubscriber.getEventsArray('OrderActionRecord'));
		assert(isVariant(orderActionRecord.action, 'cancel'));

		await makerVelocityClient.placePerpOrder({
			marketIndex: 0,
			direction: PositionDirection.SHORT,
			price: new BN(31).mul(PRICE_PRECISION),
			orderType: OrderType.LIMIT,
			baseAssetAmount: BASE_PRECISION,
			postOnly: PostOnlyParams.MUST_POST_ONLY,
		});

		let error = false;
		try {
			const txSig = await fillerVelocityClient.fillPerpOrder(
				await takerVelocityClient.getUserAccountPublicKey(),
				takerVelocityClient.getUserAccount(),
				takerVelocityClient.getOrder(1),
				makerInfo
			);

			svmContextWrapper.printTxLogs(txSig);
		} catch (e) {
			error = true;
			assert(e.message.includes('0x1787'));
		}

		assert(error);

		await takerVelocityClient.unsubscribe();
		await makerVelocityClient.unsubscribe();
	});
});
