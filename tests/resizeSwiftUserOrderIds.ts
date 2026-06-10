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
} from '../sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	sleep,
} from './testHelpers';
import { PEG_PRECISION } from '../sdk/src';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';
import dotenv from 'dotenv';
dotenv.config();

describe('place and make signedMsg order', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let makerDriftClient: TestClient;
	let makerDriftClientUser: User;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let contextWrapper: LiteSVMContextWrapper;

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
		const context = await startLiteSVM('', [], []);

		// @ts-ignore
		contextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		eventSubscriber = new EventSubscriber(
			contextWrapper.connection.toConnection(),
			// @ts-ignore
			chProgram
		);

		await eventSubscriber.subscribe();

		usdcMint = await mockUSDCMint(contextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			contextWrapper
		);

		solUsd = await mockOracleNoProgram(contextWrapper, 32.821);

		marketIndexes = [0];
		spotMarketIndexes = [0, 1];
		oracleInfos = [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }];

		makerDriftClient = new TestClient({
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
		await makerDriftClient.initialize(usdcMint.publicKey, true);
		await makerDriftClient.subscribe();
		await initializeQuoteSpotMarket(makerDriftClient, usdcMint.publicKey);

		const periodicity = new BN(0);
		await makerDriftClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(33 * PEG_PRECISION.toNumber())
		);

		await makerDriftClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);

		makerDriftClientUser = new User({
			driftClient: makerDriftClient,
			userAccountPublicKey: await makerDriftClient.getUserAccountPublicKey(),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await makerDriftClientUser.subscribe();
	});

	after(async () => {
		await makerDriftClient.unsubscribe();
		await makerDriftClientUser.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('increase size of signedMsg user orders', async () => {
		const [takerDriftClient, takerDriftClientUser] =
			await initializeNewTakerClientAndUser(
				contextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerDriftClientUser.fetchAccounts();

		await takerDriftClient.resizeSignedMsgUserOrders(
			takerDriftClientUser.getUserAccount().authority,
			100
		);

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerDriftClient.program.programId,
				takerDriftClientUser.getUserAccount().authority
			);
		const signedMsgUserOrders =
			(await takerDriftClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 100);

		await takerDriftClientUser.unsubscribe();
		await takerDriftClient.unsubscribe();
	});

	it('fails to decrease size if authority != payer', async () => {
		const [takerDriftClient, takerDriftClientUser] =
			await initializeNewTakerClientAndUser(
				contextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerDriftClientUser.fetchAccounts();

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerDriftClient.program.programId,
				takerDriftClientUser.getUserAccount().authority
			);

		try {
			await makerDriftClient.resizeSignedMsgUserOrders(
				takerDriftClientUser.getUserAccount().authority,
				4
			);
			assert.fail('Expected an error');
		} catch (error) {
			assert.include(error.toString(), '0x18a9');
		}

		const signedMsgUserOrders =
			(await takerDriftClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 32);

		await takerDriftClientUser.unsubscribe();
		await takerDriftClient.unsubscribe();
	});

	it('allows decrease size if authority is delegate', async () => {
		const [takerDriftClient, takerDriftClientUser] =
			await initializeNewTakerClientAndUser(
				contextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerDriftClientUser.fetchAccounts();

		await takerDriftClient.updateUserDelegate(
			makerDriftClient.wallet.publicKey
		);

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerDriftClient.program.programId,
				takerDriftClientUser.getUserAccount().authority
			);

		await makerDriftClient.resizeSignedMsgUserOrders(
			takerDriftClientUser.getUserAccount().authority,
			4
		);

		const signedMsgUserOrders =
			(await takerDriftClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 4);

		await takerDriftClientUser.unsubscribe();
		await takerDriftClient.unsubscribe();
	});

	it('decrease size of signedMsg user orders', async () => {
		const [takerDriftClient, takerDriftClientUser] =
			await initializeNewTakerClientAndUser(
				contextWrapper,
				chProgram,
				usdcMint,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await takerDriftClientUser.fetchAccounts();

		await takerDriftClient.resizeSignedMsgUserOrders(
			takerDriftClientUser.getUserAccount().authority,
			4
		);

		const signedMsgUserOrdersAccountPublicKey =
			getSignedMsgUserAccountPublicKey(
				takerDriftClient.program.programId,
				takerDriftClientUser.getUserAccount().authority
			);
		const signedMsgUserOrders =
			(await takerDriftClient.program.account.signedMsgUserOrders.fetch(
				signedMsgUserOrdersAccountPublicKey
			)) as any;

		assert.equal(signedMsgUserOrders.signedMsgOrderData.length, 4);

		await takerDriftClientUser.unsubscribe();
		await takerDriftClient.unsubscribe();
	});
});

async function initializeNewTakerClientAndUser(
	contextWrapper: LiteSVMContextWrapper,
	chProgram: Program,
	usdcMint: Keypair,
	usdcAmount: BN,
	marketIndexes: number[],
	spotMarketIndexes: number[],
	oracleInfos: { publicKey: PublicKey; source: OracleSource }[],
	bulkAccountLoader: TestBulkAccountLoader
): Promise<[TestClient, User]> {
	const keypair = new Keypair();
	await contextWrapper.fundKeypair(keypair, 10 ** 9);
	await sleep(1000);
	const wallet = new Wallet(keypair);
	const userUSDCAccount = await mockUserUSDCAccount(
		usdcMint,
		usdcAmount,
		contextWrapper,
		keypair.publicKey
	);
	const takerDriftClient = new TestClient({
		connection: contextWrapper.connection.toConnection(),
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
	await takerDriftClient.subscribe();
	await takerDriftClient.initializeUserAccountAndDepositCollateral(
		usdcAmount,
		userUSDCAccount.publicKey
	);
	const takerDriftClientUser = new User({
		driftClient: takerDriftClient,
		userAccountPublicKey: await takerDriftClient.getUserAccountPublicKey(),
		accountSubscription: {
			type: 'polling',
			accountLoader: bulkAccountLoader,
		},
	});
	await takerDriftClientUser.subscribe();
	await takerDriftClient.initializeSignedMsgUserOrders(
		takerDriftClientUser.getUserAccount().authority,
		32
	);
	return [takerDriftClient, takerDriftClientUser];
}
