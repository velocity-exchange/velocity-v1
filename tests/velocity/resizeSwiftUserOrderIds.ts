import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { Keypair, PublicKey } from '@solana/web3.js';

import {
	BN,
	PRICE_PRECISION,
	TestClient,
	User,
	Wallet,
	EventSubscriber,
	OracleSource,
	getSignedMsgUserAccountPublicKey,
} from '../../packages/sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	sleep,
} from './testHelpers';
import { PEG_PRECISION } from '../../packages/sdk/src';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import dotenv from 'dotenv';
dotenv.config();

describe('place and make signedMsg order', () => {
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

		// @ts-ignore
		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		eventSubscriber = new EventSubscriber(
			svmContextWrapper.connection.toConnection(),
			// @ts-ignore
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
			new BN(33 * PEG_PRECISION.toNumber())
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

	it('increase size of signedMsg user orders', async () => {
		const [takerVelocityClient, takerVelocityClientUser] =
			await initializeNewTakerClientAndUser(
				svmContextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerVelocityClientUser.fetchAccounts();

		await takerVelocityClient.resizeSignedMsgUserOrders(
			takerVelocityClientUser.getUserAccount().authority,
			100
		);

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerVelocityClient.program.programId,
				takerVelocityClientUser.getUserAccount().authority
			);
		const signedMsgUserOrders =
			(await takerVelocityClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 100);

		await takerVelocityClientUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
	});

	it('fails to decrease size if authority != payer', async () => {
		const [takerVelocityClient, takerVelocityClientUser] =
			await initializeNewTakerClientAndUser(
				svmContextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerVelocityClientUser.fetchAccounts();

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerVelocityClient.program.programId,
				takerVelocityClientUser.getUserAccount().authority
			);

		try {
			await makerVelocityClient.resizeSignedMsgUserOrders(
				takerVelocityClientUser.getUserAccount().authority,
				4
			);
			assert.fail('Expected an error');
		} catch (error) {
			assert.include(error.toString(), '0x18a9');
		}

		const signedMsgUserOrders =
			(await takerVelocityClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 32);

		await takerVelocityClientUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
	});

	it('fails to decrease size if payer is a delegate (not the authority)', async () => {
		// The SignedMsgUserOrders account is authority-scoped and shared across every
		// subaccount of the authority. A per-subaccount delegate must not be able to shrink
		// it: shrinking evicts other subaccounts' active replay-protection UUIDs and re-enables
		// replay of their signed orders. Only the authority itself may shrink.
		const [takerVelocityClient, takerVelocityClientUser] =
			await initializeNewTakerClientAndUser(
				svmContextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerVelocityClientUser.fetchAccounts();

		await takerVelocityClient.updateUserDelegate(
			makerVelocityClient.wallet.publicKey
		);

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerVelocityClient.program.programId,
				takerVelocityClientUser.getUserAccount().authority
			);

		try {
			await makerVelocityClient.resizeSignedMsgUserOrders(
				takerVelocityClientUser.getUserAccount().authority,
				4
			);
			assert.fail('Expected an error');
		} catch (error) {
			assert.include(error.toString(), '0x18a9');
		}

		const signedMsgUserOrders =
			(await takerVelocityClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 32);

		await takerVelocityClientUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
	});

	it('decrease size of signedMsg user orders', async () => {
		const [takerVelocityClient, takerVelocityClientUser] =
			await initializeNewTakerClientAndUser(
				svmContextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerVelocityClientUser.fetchAccounts();

		await takerVelocityClient.resizeSignedMsgUserOrders(
			takerVelocityClientUser.getUserAccount().authority,
			4
		);

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerVelocityClient.program.programId,
				takerVelocityClientUser.getUserAccount().authority
			);
		const signedMsgUserOrders =
			(await takerVelocityClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 4);

		await takerVelocityClientUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
	});
});

async function initializeNewTakerClientAndUser(
	svmContextWrapper: LiteSVMContextWrapper,
	chProgram: Program,
	usdcMint: Keypair,
	usdcAmount: BN,
	marketIndexes: number[],
	spotMarketIndexes: number[],
	oracleInfos: { publicKey: PublicKey; source: OracleSource }[],
	bulkAccountLoader: TestBulkAccountLoader
): Promise<[TestClient, User]> {
	const keypair = new Keypair();
	await svmContextWrapper.fundKeypair(keypair, 10 ** 9);
	await sleep(1000);
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
	await takerVelocityClient.initializeSignedMsgUserOrders(
		takerVelocityClientUser.getUserAccount().authority,
		32
	);
	return [takerVelocityClient, takerVelocityClientUser];
}
