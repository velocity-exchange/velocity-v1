import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { PublicKey } from '@solana/web3.js';

import {
	BN,
	TestClient,
	PositionDirection,
	User,
	getMarketOrderParams,
	EventSubscriber,
	PRICE_PRECISION,
} from '../../packages/sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { AMM_RESERVE_PRECISION, OracleSource, ZERO } from '../../packages/sdk';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';

// EquityBelowFloor
const EQUITY_BELOW_FLOOR_HEX = '0x18d6';

describe('equity floor', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let velocityClientUser: User;
	let eventSubscriber: EventSubscriber;
	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let usdcMint;
	let userUSDCAccount;
	let userAccountPublicKey: PublicKey;

	const mantissaSqrtScale = new BN(100000);
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(10 * 10 ** 6);

	const marketIndex = 0;
	let solUsd;

	before(async () => {
		const context = await startAnchor('', [], []);

		bankrunContextWrapper = new BankrunContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			bankrunContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(bankrunContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper
		);

		eventSubscriber = new EventSubscriber(
			bankrunContextWrapper.connection.toConnection(),
			chProgram
		);
		await eventSubscriber.subscribe();

		solUsd = await mockOracleNoProgram(
			bankrunContextWrapper,
			1,
			-7,
			undefined,
			10000
		);

		const marketIndexes = [0];
		const spotMarketIndexes = [0];
		const oracleInfos = [
			{ publicKey: solUsd, source: OracleSource.PYTH_LAZER },
		];

		velocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: bankrunContextWrapper.provider.wallet,
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
		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));

		const periodicity = new BN(60 * 60);

		await velocityClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity
		);

		await velocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);

		userAccountPublicKey = await velocityClient.getUserAccountPublicKey();

		velocityClientUser = new User({
			velocityClient,
			userAccountPublicKey,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await velocityClientUser.subscribe();
	});

	after(async () => {
		await velocityClient.unsubscribe();
		await velocityClientUser.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('warm admin sets the equity floor', async () => {
		const floor = new BN(20 * 10 ** 6); // above the 10 USDC of equity
		await velocityClient.updateUserEquityFloor(userAccountPublicKey, floor);

		await velocityClient.fetchAccounts();
		await velocityClientUser.fetchAccounts();

		assert(velocityClientUser.getUserAccount().equityFloor.eq(floor));
		assert(velocityClientUser.isBelowEquityFloor());
		assert(velocityClientUser.getEquityAboveFloor().eq(ZERO));
		assert(velocityClientUser.getWithdrawalLimit(0).eq(ZERO));
	});

	it('blocks risk-increasing orders below the floor', async () => {
		const orderParams = getMarketOrderParams({
			marketIndex,
			direction: PositionDirection.LONG,
			baseAssetAmount: new BN(AMM_RESERVE_PRECISION),
			price: PRICE_PRECISION.mul(new BN(1049)).div(new BN(1000)),
		});

		let err: Error | undefined;
		try {
			await velocityClient.placeAndTakePerpOrder(orderParams);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'order should have been rejected');
		assert(err.message.includes(EQUITY_BELOW_FLOOR_HEX));

		await velocityClientUser.fetchAccounts();
		assert(
			velocityClientUser
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.eq(ZERO)
		);
	});

	it('blocks withdrawals below the floor', async () => {
		let err: Error | undefined;
		try {
			await velocityClient.withdraw(
				new BN(5 * 10 ** 6),
				0,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'withdraw should have been rejected');
		assert(err.message.includes(EQUITY_BELOW_FLOOR_HEX));
	});

	it('lowering the floor re-enables trading', async () => {
		const floor = new BN(2 * 10 ** 6); // below the 10 USDC of equity
		await velocityClient.updateUserEquityFloor(userAccountPublicKey, floor);

		await velocityClient.fetchAccounts();
		await velocityClientUser.fetchAccounts();
		assert(!velocityClientUser.isBelowEquityFloor());

		const orderParams = getMarketOrderParams({
			marketIndex,
			direction: PositionDirection.LONG,
			baseAssetAmount: new BN(AMM_RESERVE_PRECISION),
			price: PRICE_PRECISION.mul(new BN(1049)).div(new BN(1000)),
		});
		await velocityClient.placeAndTakePerpOrder(orderParams);

		await velocityClientUser.fetchAccounts();
		assert(
			velocityClientUser
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.gt(ZERO)
		);
	});

	it('reduce-only orders stay allowed below the floor', async () => {
		// push the floor back above equity with a position open
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			new BN(20 * 10 ** 6)
		);

		const orderParams = getMarketOrderParams({
			marketIndex,
			direction: PositionDirection.SHORT,
			baseAssetAmount: new BN(AMM_RESERVE_PRECISION),
			reduceOnly: true,
		});
		await velocityClient.placeAndTakePerpOrder(orderParams);

		await velocityClientUser.fetchAccounts();
		assert(
			velocityClientUser
				.getUserAccount()
				.perpPositions[0].baseAssetAmount.eq(ZERO)
		);
	});

	it('clearing the floor disables the check', async () => {
		await velocityClient.updateUserEquityFloor(userAccountPublicKey, ZERO);

		await velocityClient.fetchAccounts();
		await velocityClientUser.fetchAccounts();
		assert(velocityClientUser.getUserAccount().equityFloor.eq(ZERO));
		assert(!velocityClientUser.isBelowEquityFloor());
		assert(velocityClientUser.getEquityAboveFloor() === null);

		await velocityClient.withdraw(
			new BN(1 * 10 ** 6),
			0,
			userUSDCAccount.publicKey
		);
	});
});
