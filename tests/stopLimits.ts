import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { Keypair, PublicKey, Transaction } from '@solana/web3.js';

import {
	TestClient,
	BN,
	PRICE_PRECISION,
	PositionDirection,
	User,
	Wallet,
	getMarketOrderParams,
	OrderTriggerCondition,
	getTriggerLimitOrderParams,
	EventSubscriber,
	MarketStatus,
} from '../sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	setFeedPriceNoProgram,
} from './testHelpers';
import { AMM_RESERVE_PRECISION, OracleSource, ZERO, isVariant } from '../sdk';
import {
	createAssociatedTokenAccountIdempotentInstruction,
	createMintToInstruction,
	getAssociatedTokenAddressSync,
} from '@solana/spl-token';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';

describe('stop limit', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let driftClient: TestClient;
	let driftClientUser: User;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let contextWrapper: LiteSVMContextWrapper;

	let userAccountPublicKey: PublicKey;

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

	const usdcAmount = new BN(10 * 10 ** 6);

	let discountMint: PublicKey;

	const fillerKeyPair = new Keypair();
	let fillerUSDCAccount: Keypair;
	let fillerDriftClient: TestClient;
	let fillerUser: User;

	const marketIndex = 0;
	let solUsd;
	let btcUsd;

	before(async () => {
		const context = await startLiteSVM('', [], []);

		contextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		eventSubscriber = new EventSubscriber(
			contextWrapper.connection.toConnection(),
			chProgram
		);

		await eventSubscriber.subscribe();

		usdcMint = await mockUSDCMint(contextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			contextWrapper
		);

		solUsd = await mockOracleNoProgram(
			contextWrapper,
			1,
			-7,
			undefined,
			10000
		);
		btcUsd = await mockOracleNoProgram(
			contextWrapper,
			60000,
			-7,
			undefined,
			10000
		);

		const marketIndexes = [marketIndex];
		const spotMarketIndexes = [0];
		const oracleInfos = [
			{
				publicKey: solUsd,
				source: OracleSource.PYTH_LAZER,
			},
			{
				publicKey: btcUsd,
				source: OracleSource.PYTH_LAZER,
			},
		];

		driftClient = new TestClient({
			connection: contextWrapper.connection.toConnection(),
			wallet: contextWrapper.provider.wallet,
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
		await driftClient.initialize(usdcMint.publicKey, true);
		await driftClient.subscribe();
		await initializeQuoteSpotMarket(driftClient, usdcMint.publicKey);
		await driftClient.updatePerpAuctionDuration(new BN(0));

		const periodicity = new BN(60 * 60); // 1 HOUR

		await driftClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity
		);
		await driftClient.updatePerpMarketStatus(0, MarketStatus.ACTIVE);

		await driftClient.initializePerpMarket(
			1,
			btcUsd,
			ammInitialBaseAssetReserve.div(new BN(3000)),
			ammInitialQuoteAssetReserve.div(new BN(3000)),
			periodicity,
			new BN(60000000) // btc-ish price level
		);
		await driftClient.updatePerpMarketStatus(1, MarketStatus.ACTIVE);

		[, userAccountPublicKey] =
			await driftClient.initializeUserAccountAndDepositCollateral(
				usdcAmount,
				userUSDCAccount.publicKey
			);

		driftClientUser = new User({
			driftClient,
			userAccountPublicKey: await driftClient.getUserAccountPublicKey(),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await driftClientUser.subscribe();

		const discountMintKeypair = await mockUSDCMint(contextWrapper);

		discountMint = discountMintKeypair.publicKey;

		await driftClient.updateDiscountMint(discountMint);

		const discountMintAta = getAssociatedTokenAddressSync(
			discountMint,
			contextWrapper.provider.wallet.publicKey
		);
		const ix = createAssociatedTokenAccountIdempotentInstruction(
			contextWrapper.context.payer.publicKey,
			discountMintAta,
			contextWrapper.provider.wallet.publicKey,
			discountMint
		);
		const mintToIx = createMintToInstruction(
			discountMint,
			discountMintAta,
			contextWrapper.provider.wallet.publicKey,
			1000 * 10 ** 6
		);
		await contextWrapper.sendTransaction(
			new Transaction().add(ix, mintToIx)
		);

		await contextWrapper.fundKeypair(fillerKeyPair, 10 ** 9);
		fillerUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			contextWrapper,
			fillerKeyPair.publicKey
		);
		fillerDriftClient = new TestClient({
			connection: contextWrapper.connection.toConnection(),
			wallet: new Wallet(fillerKeyPair),
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
		await fillerDriftClient.subscribe();

		await fillerDriftClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			fillerUSDCAccount.publicKey
		);

		fillerUser = new User({
			driftClient: fillerDriftClient,
			userAccountPublicKey: await fillerDriftClient.getUserAccountPublicKey(),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await fillerUser.subscribe();
	});

	after(async () => {
		await driftClient.unsubscribe();
		await driftClientUser.unsubscribe();
		await fillerUser.unsubscribe();
		await fillerDriftClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('Fill stop limit short order', async () => {
		const direction = PositionDirection.SHORT;
		const baseAssetAmount = new BN(AMM_RESERVE_PRECISION);
		const triggerPrice = PRICE_PRECISION;
		const limitPrice = PRICE_PRECISION.sub(
			driftClient.getPerpMarketAccount(marketIndex).orderTickSize
		);
		const triggerCondition = OrderTriggerCondition.ABOVE;

		await driftClient.placeAndTakePerpOrder(
			getMarketOrderParams({
				marketIndex,
				direction: PositionDirection.LONG,
				baseAssetAmount,
			})
		);

		const orderParams = getTriggerLimitOrderParams({
			marketIndex,
			direction,
			baseAssetAmount,
			price: limitPrice,
			triggerPrice,
			triggerCondition,
		});

		await driftClient.placePerpOrder(orderParams);
		const orderId = 2;
		const orderIndex = new BN(0);
		await driftClientUser.fetchAccounts();
		let order = driftClientUser.getOrder(orderId);

		await setFeedPriceNoProgram(contextWrapper, 1.01, solUsd, 10000);
		await driftClient.moveAmmToPrice(
			marketIndex,
			new BN(1.01 * PRICE_PRECISION.toNumber())
		);
		await driftClient.triggerOrder(
			userAccountPublicKey,
			driftClientUser.getUserAccount(),
			order
		);

		await fillerDriftClient.fillPerpOrder(
			userAccountPublicKey,
			driftClientUser.getUserAccount(),
			order
		);

		await driftClient.fetchAccounts();
		await driftClientUser.fetchAccounts();
		await fillerUser.fetchAccounts();

		order = driftClientUser.getUserAccount().orders[orderIndex.toString()];

		assert(isVariant(order.status, 'filled'));

		const firstPosition = driftClientUser.getUserAccount().perpPositions[0];
		const expectedBaseAssetAmount = new BN(0);
		assert(firstPosition.baseAssetAmount.eq(expectedBaseAssetAmount));

		const expectedQuoteAssetAmount = new BN(0);
		assert(firstPosition.quoteBreakEvenAmount.eq(expectedQuoteAssetAmount));

		const orderRecord = eventSubscriber.getEventsArray('OrderActionRecord')[0];

		assert.ok(orderRecord.baseAssetAmountFilled.eq(baseAssetAmount));
		const expectedTradeQuoteAssetAmount = new BN(1010000);
		assert.ok(
			orderRecord.quoteAssetAmountFilled.eq(expectedTradeQuoteAssetAmount)
		);

		const expectedOrderId = 2;
		const expectedFillRecordId = new BN(2);
		assert(orderRecord.ts.gt(ZERO));
		assert(orderRecord.takerOrderId === expectedOrderId);
		assert(isVariant(orderRecord.action, 'fill'));
		assert(
			orderRecord.taker.equals(await driftClientUser.getUserAccountPublicKey())
		);
		assert(
			orderRecord.filler.equals(await fillerUser.getUserAccountPublicKey())
		);
		assert(orderRecord.fillRecordId.eq(expectedFillRecordId));
	});

	it('Fill stop limit long order', async () => {
		const direction = PositionDirection.LONG;
		const baseAssetAmount = new BN(AMM_RESERVE_PRECISION);
		const triggerPrice = PRICE_PRECISION;
		const limitPrice = PRICE_PRECISION.add(
			driftClient.getPerpMarketAccount(marketIndex).orderTickSize
		);
		const triggerCondition = OrderTriggerCondition.BELOW;

		await driftClient.placeAndTakePerpOrder(
			getMarketOrderParams({
				marketIndex,
				direction: PositionDirection.SHORT,
				baseAssetAmount,
			})
		);

		const orderParams = getTriggerLimitOrderParams({
			marketIndex,
			direction,
			baseAssetAmount,
			price: limitPrice,
			triggerPrice,
			triggerCondition,
		});

		await driftClient.placePerpOrder(orderParams);
		const orderId = 4;
		const orderIndex = new BN(0);
		driftClientUser.getUserAccount();
		let order = driftClientUser.getOrder(orderId);

		await setFeedPriceNoProgram(contextWrapper, 0.99, solUsd, 10000);
		await driftClient.moveAmmToPrice(
			marketIndex,
			new BN(0.99 * PRICE_PRECISION.toNumber())
		);
		await driftClient.triggerOrder(
			userAccountPublicKey,
			driftClientUser.getUserAccount(),
			order
		);

		await fillerDriftClient.fillPerpOrder(
			userAccountPublicKey,
			driftClientUser.getUserAccount(),
			order
		);

		await driftClient.fetchAccounts();
		await driftClientUser.fetchAccounts();
		await fillerUser.fetchAccounts();

		order = driftClientUser.getUserAccount().orders[orderIndex.toString()];

		assert(isVariant(order.status, 'filled'));

		const firstPosition = driftClientUser.getUserAccount().perpPositions[0];
		const expectedBaseAssetAmount = new BN(0);
		assert(firstPosition.baseAssetAmount.eq(expectedBaseAssetAmount));

		const expectedQuoteAssetAmount = new BN(0);
		assert(firstPosition.quoteBreakEvenAmount.eq(expectedQuoteAssetAmount));

		const expectedTradeQuoteAssetAmount = new BN(990001);
		const orderRecord = eventSubscriber.getEventsArray('OrderActionRecord')[0];

		const expectedOrderId = 4;
		const expectedFillRecord = new BN(4);
		assert(orderRecord.ts.gt(ZERO));
		assert(orderRecord.takerOrderId === expectedOrderId);
		assert(isVariant(orderRecord.action, 'fill'));
		assert(
			orderRecord.taker.equals(await driftClientUser.getUserAccountPublicKey())
		);
		assert(
			orderRecord.filler.equals(await fillerUser.getUserAccountPublicKey())
		);
		assert(orderRecord.baseAssetAmountFilled.eq(baseAssetAmount));
		assert(
			orderRecord.quoteAssetAmountFilled.eq(expectedTradeQuoteAssetAmount)
		);
		assert(orderRecord.fillRecordId.eq(expectedFillRecord));
	});
});
