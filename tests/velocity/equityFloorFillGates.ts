import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';

import {
	BN,
	TestClient,
	EventSubscriber,
	OracleSource,
	OracleInfo,
	QUOTE_PRECISION,
	PRICE_PRECISION,
	BASE_PRECISION,
	PEG_PRECISION,
	PositionDirection,
	OrderType,
	OrderTriggerCondition,
	MarketStatus,
	PerpOperation,
	getTriggerMarketOrderParams,
	isVariant,
	ZERO,
} from '../../packages/sdk/src';

import {
	createUserWithUSDCAccount,
	createUserWithUSDCAndWSOLAccount,
	initializeQuoteSpotMarket,
	initializeSolSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	setFeedConfidenceNoProgram,
	setFeedPriceNoProgram,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

// InvalidOracle
const INVALID_ORACLE_HEX = '0x1793';

// Fill-path and trigger-path halves of the equity-floor oracle handling.
//
// The perp market prices off its own oracle. The floored accounts also hold a
// wsol deposit priced by a separate spot oracle. Each test can therefore
// invalidate one leg and leave the other alone:
//
//  - an invalid perp oracle blocks the match itself, so a match fill yields
//    zero while the expired-maker cleanup still lands,
//  - an invalid spot oracle makes a floored account's equity unverifiable
//    while the fill market stays healthy, which is what the maker prune and
//    the trigger reject cover.
describe('equity floor fill gates', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let fillerVelocityClient: TestClient;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;
	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint;
	let fillerUSDCAccount;

	const mantissaSqrtScale = new BN(100000);
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(10_000).mul(QUOTE_PRECISION);
	const solAmount = new BN(LAMPORTS_PER_SOL);
	// floored accounts hold 10,000 usdc + 1 sol at 100 = 10,100 equity; a
	// 1,000 floor clears comfortably whenever the oracles are valid, so the
	// only thing that can fail a gate in these tests is validity itself
	const floor = new BN(1_000).mul(QUOTE_PRECISION);

	let perpOracle: PublicKey;
	let solSpotOracle: PublicKey;

	let marketIndexes: number[];
	let spotMarketIndexes: number[];
	let oracleInfos: OracleInfo[];

	// Invalidate only the floored accounts' spot deposit oracle: everything
	// goes stale with the clock, then the perp oracle is re-stamped fresh.
	const staleSpotOracleOnly = async () => {
		await svmContextWrapper.moveTimeForward(400);
		await setFeedPriceNoProgram(svmContextWrapper, 100, perpOracle);
	};

	const refreshSpotOracle = async () => {
		await setFeedPriceNoProgram(svmContextWrapper, 100, solSpotOracle);
	};

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
		fillerUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);

		perpOracle = await mockOracleNoProgram(svmContextWrapper, 100);
		solSpotOracle = await mockOracleNoProgram(svmContextWrapper, 100);

		marketIndexes = [0];
		spotMarketIndexes = [0, 1];
		oracleInfos = [
			{ publicKey: perpOracle, source: OracleSource.PYTH_LAZER },
			{ publicKey: solSpotOracle, source: OracleSource.PYTH_LAZER },
		];

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
		await initializeSolSpotMarket(fillerVelocityClient, solSpotOracle);

		// long auctions + paused AMM fills, so the only fill source in these
		// tests is the DLOB match against the resting maker
		await fillerVelocityClient.updatePerpAuctionDuration(new BN(100));

		const periodicity = new BN(60 * 60);
		await fillerVelocityClient.initializePerpMarket(
			0,
			perpOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(100).mul(PEG_PRECISION)
		);
		await fillerVelocityClient.updatePerpMarketStatus(0, MarketStatus.ACTIVE);
		await fillerVelocityClient.updatePerpMarketPausedOperations(
			0,
			PerpOperation.AMM_FILL
		);

		await fillerVelocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			fillerUSDCAccount.publicKey
		);
	});

	after(async () => {
		await fillerVelocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('an uncertain perp oracle yields a zero match fill and still cleans up expired makers', async () => {
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

		// resting maker liquidity the taker would match against
		await makerVelocityClient.placePerpOrder({
			marketIndex: 0,
			direction: PositionDirection.SHORT,
			price: new BN(100).mul(PRICE_PRECISION),
			orderType: OrderType.LIMIT,
			baseAssetAmount: BASE_PRECISION,
		});

		// a second maker order that expires before the fill, so the fill's
		// maker sweep has cleanup to do even while matching is withheld
		const now = svmContextWrapper.connection.getTime();
		await makerVelocityClient.placePerpOrder({
			marketIndex: 0,
			direction: PositionDirection.SHORT,
			price: new BN(100).mul(PRICE_PRECISION),
			orderType: OrderType.LIMIT,
			baseAssetAmount: BASE_PRECISION,
			maxTs: new BN(now + 5),
		});

		await takerVelocityClient.placePerpOrder({
			marketIndex: 0,
			orderType: OrderType.LIMIT,
			auctionStartPrice: new BN(102).mul(PRICE_PRECISION),
			auctionEndPrice: new BN(102).mul(PRICE_PRECISION),
			auctionDuration: 100,
			price: new BN(102).mul(PRICE_PRECISION),
			direction: PositionDirection.LONG,
			baseAssetAmount: BASE_PRECISION,
		});

		// half the price as confidence: TooUncertain for every consumer
		await setFeedConfidenceNoProgram(svmContextWrapper, 50, perpOracle);
		// let the maxTs order expire (well inside oracle staleness bounds)
		await svmContextWrapper.moveTimeForward(10);

		const makerInfo = [
			{
				maker: await makerVelocityClient.getUserAccountPublicKey(),
				makerUserAccount: makerVelocityClient.getUserAccount(),
				makerStats: await makerVelocityClient.getUserStatsAccountPublicKey(),
			},
		];

		// must not revert: matching is withheld, not failed
		await fillerVelocityClient.fillPerpOrder(
			await takerVelocityClient.getUserAccountPublicKey(),
			takerVelocityClient.getUserAccount(),
			takerVelocityClient.getOrder(1),
			makerInfo
		);

		await takerVelocityClient.fetchAccounts();
		await makerVelocityClient.fetchAccounts();

		// zero filled for the taker
		const takerOrder = takerVelocityClient.getOrder(1);
		assert(takerOrder !== undefined, 'taker order should still be open');
		assert(
			takerOrder.baseAssetAmountFilled.eq(ZERO),
			'taker should have filled nothing against an uncertain oracle'
		);
		assert(
			takerVelocityClient
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.eq(ZERO),
			'taker should have no position'
		);

		// the expired maker order was still swept (cleanup precedes the gate)
		const cancelRecord = eventSubscriber
			.getEventsArray('OrderActionRecord')
			.find(
				(record) =>
					isVariant(record.action, 'cancel') &&
					isVariant(record.actionExplanation, 'orderExpired')
			);
		assert(
			cancelRecord !== undefined,
			'the expired maker order should have been cancelled by the fill'
		);
		const makerOrders = makerVelocityClient
			.getUserAccount()
			.orders.filter((order) => isVariant(order.status, 'open'));
		assert(
			makerOrders.length === 1,
			'only the non-expired maker order should remain open'
		);

		// restore a tight confidence for the next tests
		await setFeedConfidenceNoProgram(svmContextWrapper, 0.1, perpOracle);

		await takerVelocityClient.unsubscribe();
		await makerVelocityClient.unsubscribe();
	});

	it('a floored maker with an invalid oracle is pruned instead of poisoning the fill', async () => {
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

		const [makerVelocityClient, makerWSOL, makerUSDC] =
			await createUserWithUSDCAndWSOLAccount(
				svmContextWrapper,
				usdcMint,
				chProgram,
				solAmount,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await makerVelocityClient.deposit(usdcAmount, 0, makerUSDC);
		await makerVelocityClient.deposit(solAmount, 1, makerWSOL);

		const makerUserPublicKey =
			await makerVelocityClient.getUserAccountPublicKey();
		await fillerVelocityClient.updateUserEquityFloor(
			makerUserPublicKey,
			floor,
			ZERO
		);

		// risk-increasing resting maker order (no position behind it)
		await makerVelocityClient.placePerpOrder({
			marketIndex: 0,
			direction: PositionDirection.SHORT,
			price: new BN(100).mul(PRICE_PRECISION),
			orderType: OrderType.LIMIT,
			baseAssetAmount: BASE_PRECISION,
		});

		await takerVelocityClient.placePerpOrder({
			marketIndex: 0,
			orderType: OrderType.LIMIT,
			auctionStartPrice: new BN(102).mul(PRICE_PRECISION),
			auctionEndPrice: new BN(102).mul(PRICE_PRECISION),
			auctionDuration: 100,
			price: new BN(102).mul(PRICE_PRECISION),
			direction: PositionDirection.LONG,
			baseAssetAmount: BASE_PRECISION,
		});

		// the maker's wsol deposit goes unpriceable; the fill market's own
		// oracle stays fresh
		await staleSpotOracleOnly();

		const makerInfo = [
			{
				maker: makerUserPublicKey,
				makerUserAccount: makerVelocityClient.getUserAccount(),
				makerStats: await makerVelocityClient.getUserStatsAccountPublicKey(),
			},
		];

		// must not revert: the floored maker is pruned from the maker set
		await fillerVelocityClient.fillPerpOrder(
			await takerVelocityClient.getUserAccountPublicKey(),
			takerVelocityClient.getUserAccount(),
			takerVelocityClient.getOrder(1),
			makerInfo
		);

		await takerVelocityClient.fetchAccounts();
		await makerVelocityClient.fetchAccounts();

		assert(
			takerVelocityClient
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.eq(ZERO),
			'nothing should have matched against the pruned maker'
		);
		// pruned, not cancelled: the order survives the incident
		const makerOrder = makerVelocityClient
			.getUserAccount()
			.orders.find((order) => isVariant(order.status, 'open'));
		assert(
			makerOrder !== undefined,
			'the pruned maker order should still be resting'
		);

		// feed recovers -> the same maker set fills
		await refreshSpotOracle();
		await fillerVelocityClient.fillPerpOrder(
			await takerVelocityClient.getUserAccountPublicKey(),
			takerVelocityClient.getUserAccount(),
			takerVelocityClient.getOrder(1),
			makerInfo
		);

		await takerVelocityClient.fetchAccounts();
		assert(
			takerVelocityClient
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.gt(ZERO),
			'the fill should land once the oracle recovers'
		);

		await takerVelocityClient.unsubscribe();
		await makerVelocityClient.unsubscribe();
	});

	it('a floored account with an invalid oracle rejects the trigger and keeps the order', async () => {
		const [userVelocityClient, userWSOL, userUSDC] =
			await createUserWithUSDCAndWSOLAccount(
				svmContextWrapper,
				usdcMint,
				chProgram,
				solAmount,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await userVelocityClient.deposit(usdcAmount, 0, userUSDC);
		await userVelocityClient.deposit(solAmount, 1, userWSOL);

		const userPublicKey = await userVelocityClient.getUserAccountPublicKey();
		await fillerVelocityClient.updateUserEquityFloor(
			userPublicKey,
			floor,
			ZERO
		);

		// a resting risk-increasing stop: triggers when the oracle is above
		// 50, which it already is
		await userVelocityClient.placePerpOrder(
			getTriggerMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: BASE_PRECISION,
				triggerPrice: new BN(50).mul(PRICE_PRECISION),
				triggerCondition: OrderTriggerCondition.ABOVE,
				userOrderId: 1,
			})
		);

		await userVelocityClient.fetchAccounts();
		const order = userVelocityClient.getOrderByUserId(1);

		// the account's wsol deposit goes unpriceable; the trigger market's
		// own oracle stays fresh
		await staleSpotOracleOnly();

		// rejected, not cancelled: an oracle blip must not destroy the order
		let err: Error | undefined;
		try {
			await fillerVelocityClient.triggerOrder(
				userPublicKey,
				userVelocityClient.getUserAccount(),
				order
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'the trigger should have been rejected');
		assert(
			err.message.includes(INVALID_ORACLE_HEX),
			`expected InvalidOracle, got: ${err.message}`
		);

		await userVelocityClient.fetchAccounts();
		const survivingOrder = userVelocityClient.getOrderByUserId(1);
		assert(
			survivingOrder !== undefined,
			'the order should have survived the rejected trigger'
		);

		// feed recovers -> the same trigger goes through. The retry must not be
		// byte-identical to the rejected transaction: nothing advanced the
		// blockhash since (a failed send skips the slot bump, and the client
		// caches blockhashes for 2s anyway), so an identical retry lands on the
		// same signature and is dropped as a duplicate before it reaches the
		// program. A different compute-unit limit changes the message bytes
		// deterministically, with no dependence on cache timing.
		await refreshSpotOracle();
		await fillerVelocityClient.triggerOrder(
			userPublicKey,
			userVelocityClient.getUserAccount(),
			survivingOrder,
			{ computeUnits: 599_999 }
		);

		await userVelocityClient.fetchAccounts();
		const triggeredOrder = userVelocityClient.getOrderByUserId(1);
		assert(
			triggeredOrder !== undefined &&
				isVariant(triggeredOrder.triggerCondition, 'triggeredAbove'),
			'the order should have triggered once the oracle recovered'
		);

		await userVelocityClient.unsubscribe();
	});
});
