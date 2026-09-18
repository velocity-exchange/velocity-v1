import * as anchor from '@coral-xyz/anchor';
import {
	BASE_PRECISION,
	BN,
	OracleSource,
	TestClient,
	EventSubscriber,
	PRICE_PRECISION,
	PositionDirection,
} from '../../packages/sdk/src';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	initializeQuoteSpotMarket,
} from './testHelpers';
import { OrderType, TWO } from '../../packages/sdk';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('modify orders', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint;
	let userUSDCAccount;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(10 * 10 ** 6);

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

		const oracle = await mockOracleNoProgram(svmContextWrapper, 1);

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

		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));

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
	});

	after(async () => {
		await velocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('modify order by order id', async () => {
		await velocityClient.placePerpOrder({
			marketIndex: 0,
			baseAssetAmount: BASE_PRECISION,
			direction: PositionDirection.LONG,
			orderType: OrderType.LIMIT,
			price: PRICE_PRECISION,
		});

		await velocityClient.modifyOrder({
			orderId: 1,
			newBaseAmount: BASE_PRECISION.mul(TWO),
		});

		assert(
			velocityClient
				.getUser()
				.getUserAccount()
				.orders[0].baseAssetAmount.eq(BASE_PRECISION.mul(TWO))
		);
	});

	it('modify order by user order id', async () => {
		await velocityClient.placePerpOrder({
			userOrderId: 1,
			marketIndex: 0,
			baseAssetAmount: BASE_PRECISION,
			direction: PositionDirection.LONG,
			orderType: OrderType.LIMIT,
			price: PRICE_PRECISION,
		});

		await velocityClient.modifyOrderByUserOrderId({
			userOrderId: 1,
			newBaseAmount: BASE_PRECISION.mul(TWO),
		});

		assert(
			velocityClient
				.getUser()
				.getUserAccount()
				.orders[1].baseAssetAmount.eq(BASE_PRECISION.mul(TWO))
		);
	});
});
