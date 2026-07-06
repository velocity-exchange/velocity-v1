import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { Keypair, PublicKey } from '@solana/web3.js';

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
	PostOnlyParams,
} from '../../packages/sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { PEG_PRECISION, PerpOperation } from '../../packages/sdk';
import { JitProxyClient, PriceType } from '../../packages/jit-proxy/src';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';

// jit-proxy program id (unchanged across the drift -> velocity migration)
const JIT_PROXY_PROGRAM_ID = new PublicKey(
	'J1TnP8zvVxbtF5KFp5xRmWuvG9McnhzmBd9XGfCyuxFP'
);

describe('jit proxy smoke test', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let makerVelocityClient: TestClient;
	let makerVelocityClientUser: User;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

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
		// startAnchor loads every program in Anchor.toml's workspace from
		// target/deploy — velocity AND jit_proxy (now a workspace member).
		const context = await startAnchor('', [], []);
		bankrunContextWrapper = new BankrunContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			bankrunContextWrapper.connection,
			'processed',
			1
		);

		eventSubscriber = new EventSubscriber(
			bankrunContextWrapper.connection.toConnection(),
			chProgram
		);
		await eventSubscriber.subscribe();

		usdcMint = await mockUSDCMint(bankrunContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper
		);

		solUsd = await mockOracleNoProgram(bankrunContextWrapper, 32.821);

		marketIndexes = [0];
		spotMarketIndexes = [0, 1];
		oracleInfos = [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }];

		makerVelocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: bankrunContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
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

		// Pause the AMM as a filler so the JIT maker (not the vAMM) fills the taker.
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

	it('JIT maker fills a taker perp auction via jit-proxy', async () => {
		// --- taker setup ---
		const keypair = new Keypair();
		await bankrunContextWrapper.fundKeypair(keypair, 10 ** 9);
		await bulkAccountLoader.load();
		const wallet = new Wallet(keypair);
		const takerUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper,
			keypair.publicKey
		);
		const takerVelocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
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
			takerUSDCAccount.publicKey
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

		// --- taker places a LONG perp order with an auction ---
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
		const takerOrder = takerVelocityClientUser.getOrderByUserOrderId(1);
		assert(!takerOrder.postOnly);

		// --- JIT maker fills the taker's auction through the jit-proxy program ---
		const jitProxyClient = new JitProxyClient({
			driftClient: makerVelocityClient,
			programId: JIT_PROXY_PROGRAM_ID,
		});

		const txSig = await jitProxyClient.jit({
			takerKey: await takerVelocityClient.getUserAccountPublicKey(),
			takerStatsKey: takerVelocityClient.getUserStatsAccountPublicKey(),
			taker: takerVelocityClient.getUserAccount(),
			takerOrderId: takerOrder.orderId,
			maxPosition: baseAssetAmount,
			minPosition: baseAssetAmount.neg(),
			// SHORT maker (taker is LONG): `ask` is the maker's sell price and
			// must sit at/below the taker's ascending auction (33 -> 34) to cross.
			bid: new BN(33).mul(PRICE_PRECISION),
			ask: new BN(33).mul(PRICE_PRECISION),
			postOnly: PostOnlyParams.MUST_POST_ONLY,
			priceType: PriceType.LIMIT,
		});

		bankrunContextWrapper.printTxLogs(txSig.txSig);

		// --- assert the fill happened on both sides ---
		await makerVelocityClientUser.fetchAccounts();
		await takerVelocityClientUser.fetchAccounts();

		const makerPosition = makerVelocityClient.getUser().getPerpPosition(0);
		assert(
			makerPosition.baseAssetAmount.eq(BASE_PRECISION.neg()),
			`maker base ${makerPosition.baseAssetAmount.toString()} != ${BASE_PRECISION.neg().toString()}`
		);

		const takerPosition = takerVelocityClient.getUser().getPerpPosition(0);
		assert(
			takerPosition.baseAssetAmount.eq(BASE_PRECISION),
			`taker base ${takerPosition.baseAssetAmount.toString()} != ${BASE_PRECISION.toString()}`
		);

		await takerVelocityClientUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
	});
});
