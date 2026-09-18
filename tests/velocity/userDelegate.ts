import * as anchor from '@coral-xyz/anchor';

import { Program } from '@coral-xyz/anchor';

import {
	QUOTE_SPOT_MARKET_INDEX,
	TestClient,
	BN,
	EventSubscriber,
	PRICE_PRECISION,
	OracleSource,
	PositionDirection,
	Wallet,
	MarketStatus,
} from '../../packages/sdk/src';

import {
	createFundedKeyPair,
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { assert } from 'chai';
import { Keypair } from '@solana/web3.js';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('user delegate', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint;

	const usdcAmount = new BN(10 * 10 ** 6);

	let delegateKeyPair: Keypair;
	let secondDelegateKeyPair: Keypair;
	let delegateVelocityClient: TestClient;
	let delegateUsdcAccount: Keypair;

	const marketIndexes = [0];
	const spotMarketIndexes = [0];

	let solUsd;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetAmount = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetAmount = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

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

		solUsd = await mockOracleNoProgram(svmContextWrapper, 1);
		velocityClient = new TestClient({
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
			oracleInfos: [
				{
					source: OracleSource.PYTH_LAZER,
					publicKey: solUsd,
				},
			],
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));

		const periodicity = new BN(60 * 60); // 1 HOUR
		await velocityClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetAmount,
			ammInitialQuoteAssetAmount,
			periodicity
		);
		await velocityClient.updatePerpMarketStatus(0, MarketStatus.ACTIVE);

		const subAccountId = 0;
		const name = 'CRISP';
		await velocityClient.initializeUserAccount(subAccountId, name);
		await velocityClient.initializeUserAccount(1, 'CRISP 1');
		await velocityClient.switchActiveUser(0);

		delegateKeyPair = await createFundedKeyPair(svmContextWrapper);
		secondDelegateKeyPair = await createFundedKeyPair(svmContextWrapper);
	});

	after(async () => {
		await velocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
		await delegateVelocityClient.unsubscribe();
	});

	it('Update delegate', async () => {
		await velocityClient.updateUserDelegate(delegateKeyPair.publicKey);
		await velocityClient.switchActiveUser(1);
		await velocityClient.updateUserDelegate(delegateKeyPair.publicKey, 1);
		await velocityClient.switchActiveUser(0);

		await velocityClient.fetchAccounts();
		assert(
			velocityClient.getUserAccount().delegate.equals(delegateKeyPair.publicKey)
		);
		assert(
			velocityClient
				.getUser(1)
				.getUserAccount()
				.delegate.equals(delegateKeyPair.publicKey)
		);

		delegateVelocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: new Wallet(delegateKeyPair),
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: marketIndexes,
			spotMarketIndexes: spotMarketIndexes,
			oracleInfos: [
				{
					source: OracleSource.PYTH_LAZER,
					publicKey: solUsd,
				},
			],
			authority: svmContextWrapper.provider.wallet.publicKey,
			authoritySubAccountMap: new Map().set(
				svmContextWrapper.provider.wallet.publicKey,
				[0, 1]
			),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await delegateVelocityClient.subscribe();
	});

	it('Deposit', async () => {
		delegateUsdcAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper,
			delegateKeyPair.publicKey
		);

		await delegateVelocityClient.deposit(
			usdcAmount,
			QUOTE_SPOT_MARKET_INDEX,
			delegateUsdcAccount.publicKey
		);

		assert(delegateVelocityClient.getQuoteAssetTokenAmount().eq(usdcAmount));
	});

	it('Cannot transfer deposits by delegate without permission', async () => {
		let caughtError = false;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(1 * 10 ** 6),
				QUOTE_SPOT_MARKET_INDEX,
				0,
				1
			);
		} catch (e) {
			caughtError = true;
		}
		assert(caughtError);
	});

	it('Update allow delegate transfer', async () => {
		await velocityClient.updateUserAllowDelegateTransfer(true);
	});

	it('Transfer deposits by delegate', async () => {
		const transferAmount = new BN(3 * 10 ** 6);

		await delegateVelocityClient.transferDepositByDelegate(
			transferAmount,
			QUOTE_SPOT_MARKET_INDEX,
			0,
			1
		);
		await velocityClient.getUser(0).fetchAccounts();
		await velocityClient.getUser(1).fetchAccounts();

		const fromAmount = velocityClient
			.getUser(0)
			.getTokenAmount(QUOTE_SPOT_MARKET_INDEX);
		const toAmount = velocityClient
			.getUser(1)
			.getTokenAmount(QUOTE_SPOT_MARKET_INDEX);
		const expectedFromAmount = usdcAmount.sub(transferAmount);

		assert(
			fromAmount.gte(expectedFromAmount.subn(1)) &&
				fromAmount.lte(expectedFromAmount),
			`from amount ${fromAmount.toString()}`
		);
		assert(toAmount.eq(transferAmount), `to amount ${toAmount.toString()}`);
	});

	it('Cannot transfer deposits unless both subaccounts use the same delegate', async () => {
		await velocityClient.switchActiveUser(1);
		await velocityClient.updateUserDelegate(secondDelegateKeyPair.publicKey, 1);
		await velocityClient.switchActiveUser(0);
		await velocityClient.fetchAccounts();

		let caughtError = false;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(1 * 10 ** 6),
				QUOTE_SPOT_MARKET_INDEX,
				0,
				1
			);
		} catch (e) {
			caughtError = true;
		}
		assert(caughtError);
	});

	it('Withdraw', async () => {
		let caughtError = false;
		try {
			await delegateVelocityClient.withdraw(
				usdcAmount,
				QUOTE_SPOT_MARKET_INDEX,
				delegateUsdcAccount.publicKey
			);
		} catch (e) {
			caughtError = true;
		}
		assert(caughtError);
	});

	it('Open position', async () => {
		await delegateVelocityClient.openPosition(
			PositionDirection.LONG,
			usdcAmount,
			0
		);
	});
});
