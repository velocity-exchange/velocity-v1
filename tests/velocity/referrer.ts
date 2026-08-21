import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { Keypair, PublicKey, Transaction } from '@solana/web3.js';

import {
	TestClient,
	BN,
	OracleSource,
	EventSubscriber,
	Wallet,
	PRICE_PRECISION,
	ReferrerStatus,
	AcceleratedReferralStatus,
	ACCELERATED_REFERRER_REWARD_PERCENT,
	RevenueShareAccount,
	RevenueShareEscrowAccount,
	RevenueShareEscrowMap,
	UserStatsAccount,
	getRevenueShareAccountPublicKey,
	isBuilderOrderReferral,
	ZERO,
} from '../../packages/sdk/src';

import {
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	initializeQuoteSpotMarket,
	createFundedKeyPair,
	createUserWithUSDCAccount,
} from './testHelpers';
import {
	BASE_PRECISION,
	getLimitOrderParams,
	PEG_PRECISION,
	PositionDirection,
} from '../../packages/sdk/src';
import { decodeName } from '../../packages/sdk/src/userName';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';

// `calculate_taker_fee` ceils the tier fee (safe_div_ceil), then each referral
// proportion floors. Mirror both or the expectation drifts by one for quotes that do
// not divide evenly.
const takerFeeFor = (
	quoteAssetAmountFilled: BN | number | string,
	feeTier: { feeNumerator: number; feeDenominator: number }
): BN =>
	new BN(quoteAssetAmountFilled)
		.muln(feeTier.feeNumerator)
		.addn(feeTier.feeDenominator - 1)
		.divn(feeTier.feeDenominator);

describe('referrer', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let referrerVelocityClient: TestClient;

	let refereeKeyPair: Keypair;
	let refereeVelocityClient: TestClient;
	let refereeUSDCAccount: Keypair;

	let fillerVelocityClient: TestClient;

	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;

	let escrowMap: RevenueShareEscrowMap;

	let bankrunContextWrapper: BankrunContextWrapper;

	let usdcMint;
	let referrerUSDCAccount;

	let solOracle: PublicKey;

	// ammInvariant == k == x * y
	const ammReservePrecision = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		ammReservePrecision
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		ammReservePrecision
	);

	const usdcAmount = new BN(100 * 10 ** 6);

	before(async () => {
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
		referrerUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper
		);

		solOracle = await mockOracleNoProgram(
			bankrunContextWrapper,
			100,
			-7,
			undefined,
			10000
		);

		const marketIndexes = [0];
		const spotMarketIndexes = [0];
		const oracleInfos = [
			{
				publicKey: solOracle,
				source: OracleSource.PYTH_LAZER,
			},
		];
		referrerVelocityClient = new TestClient({
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
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await referrerVelocityClient.initialize(usdcMint.publicKey, true);
		await referrerVelocityClient.subscribe();
		await referrerVelocityClient.updatePerpAuctionDuration(0);
		// Enable builder-codes so the RevenueShareEscrow referral path is active.
		await referrerVelocityClient.updateFeatureBitFlagsBuilderCodes(true);

		const periodicity = new BN(60 * 60); // 1 HOUR

		await referrerVelocityClient.initializePerpMarket(
			0,
			solOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(100).mul(PEG_PRECISION)
		);

		await initializeQuoteSpotMarket(referrerVelocityClient, usdcMint.publicKey);

		await referrerVelocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			referrerUSDCAccount.publicKey
		);

		refereeKeyPair = await createFundedKeyPair(bankrunContextWrapper);
		refereeUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper,
			refereeKeyPair.publicKey
		);

		refereeVelocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: new Wallet(refereeKeyPair),
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
		await refereeVelocityClient.subscribe();

		[fillerVelocityClient] = await createUserWithUSDCAccount(
			bankrunContextWrapper,
			usdcMint,
			chProgram,
			usdcAmount,
			marketIndexes,
			spotMarketIndexes,
			oracleInfos,
			bulkAccountLoader
		);

		escrowMap = new RevenueShareEscrowMap(refereeVelocityClient, false);
	});

	after(async () => {
		await referrerVelocityClient.unsubscribe();
		await refereeVelocityClient.unsubscribe();
		await fillerVelocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('automatically accelerates new user creation', async () => {
		// ACCELERATED_REFERRAL_ENROLLMENT_ENABLED is a beta-scoped constant, so every new
		// account is enrolled. There is no runtime switch to toggle.
		const [acceleratedClient] = await createUserWithUSDCAccount(
			bankrunContextWrapper,
			usdcMint,
			chProgram,
			usdcAmount,
			[0],
			[0],
			[
				{
					publicKey: solOracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			bulkAccountLoader
		);

		try {
			const acceleratedStats =
				(await acceleratedClient.program.account.userStats.fetch(
					acceleratedClient.getUserStatsAccountPublicKey()
				)) as UserStatsAccount;
			assert(
				(acceleratedStats.acceleratedReferralStatus &
					AcceleratedReferralStatus.Accelerated) >
					0
			);
		} finally {
			await acceleratedClient.unsubscribe();
		}
	});

	it('initialize referrer name account', async () => {
		await referrerVelocityClient.initializeReferrerName('crisp');
		const referrerNameAccount =
			await referrerVelocityClient.fetchReferrerNameAccount('crisp');
		assert(decodeName(referrerNameAccount.name) === 'crisp');
		assert(
			referrerNameAccount.authority.equals(referrerVelocityClient.authority)
		);
		assert(
			referrerNameAccount.user.equals(
				await referrerVelocityClient.getUserAccountPublicKey()
			)
		);
	});

	it('initialize with referrer', async () => {
		const [txSig] =
			await refereeVelocityClient.initializeUserAccountAndDepositCollateral(
				usdcAmount,
				refereeUSDCAccount.publicKey,
				0,
				0,
				'crisp',
				undefined,
				{
					referrer: await referrerVelocityClient.getUserAccountPublicKey(),
					referrerStats: referrerVelocityClient.getUserStatsAccountPublicKey(),
				}
			);

		await eventSubscriber.awaitTx(txSig);

		const newUserRecord = eventSubscriber.getEventsArray('NewUserRecord')[0];
		assert(
			newUserRecord.referrer.equals(
				bankrunContextWrapper.provider.wallet.publicKey
			)
		);

		await refereeVelocityClient.fetchAccounts();
		const refereeStats = refereeVelocityClient.getUserStats().getAccount();
		assert(
			refereeStats.referrer.equals(
				bankrunContextWrapper.provider.wallet.publicKey
			)
		);
		assert((refereeStats.referrerStatus & ReferrerStatus.IsReferred) > 0);

		const referrerStats = referrerVelocityClient.getUserStats().getAccount();
		assert((referrerStats.referrerStatus & ReferrerStatus.IsReferrer) > 0);
	});

	it('referrer can initialize a RevenueShare account', async () => {
		await referrerVelocityClient.initializeRevenueShare(
			referrerVelocityClient.wallet.publicKey
		);

		const accountInfo = await bankrunContextWrapper.connection.getAccountInfo(
			getRevenueShareAccountPublicKey(
				referrerVelocityClient.program.programId,
				referrerVelocityClient.wallet.publicKey
			)
		);
		assert(accountInfo !== null, 'RevenueShare account should exist');

		const revShare: RevenueShareAccount =
			referrerVelocityClient.program.account.revenueShare.coder.accounts.decodeUnchecked(
				'revenueShare',
				accountInfo.data
			);
		assert(
			revShare.authority.toBase58() ===
				referrerVelocityClient.wallet.publicKey.toBase58()
		);
		assert(revShare.totalReferrerRewards.toNumber() === 0);
	});

	it('cannot initialize a RevenueShareEscrow before the authority has a user', async () => {
		// escrow.referrer is snapshotted once at init and never rewritten, and
		// UserStats.referrer is only ever set when the first sub-account is created.
		// An escrow created in between would therefore freeze a defaulted referrer
		// forever, suppressing that authority's referral rewards with no way to
		// repair the field. Anyone can pay for anyone's escrow — the authority does
		// not sign — so the window has to be closed on chain.
		const strandedKeyPair = await createFundedKeyPair(bankrunContextWrapper);
		const strandedClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: new Wallet(strandedKeyPair),
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
					publicKey: solOracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await strandedClient.subscribe();

		// UserStats exists, no sub-account yet: exactly the state the escrow refuses.
		const statsTx = await strandedClient.buildTransaction([
			await strandedClient.getInitializeUserStatsIx(),
		]);
		await strandedClient.sendTransaction(statsTx);

		try {
			// A third party paying for the escrow is what made this griefable.
			await referrerVelocityClient.initializeRevenueShareEscrow(
				strandedKeyPair.publicKey,
				3
			);
			assert(false, 'escrow init should have failed with no user created');
		} catch (e) {
			assert(
				e.message.includes('0x185a'), // UserNotFound
				`expected UserNotFound (0x185a), got ${e.message}`
			);
		}

		await strandedClient.unsubscribe();
	});

	it('referee can initialize a RevenueShareEscrow', async () => {
		// The referee already has its referrer set on UserStats (from the
		// 'initialize with referrer' test), so initializing the escrow stamps
		// escrow.referrer with the referrer's authority.
		await refereeVelocityClient.initializeRevenueShareEscrow(
			refereeVelocityClient.wallet.publicKey,
			3
		);

		await escrowMap.slowSync();
		const escrow = (await escrowMap.mustGet(
			refereeVelocityClient.wallet.publicKey.toBase58()
		)) as RevenueShareEscrowAccount;
		assert(
			escrow.authority.toBase58() ===
				refereeVelocityClient.wallet.publicKey.toBase58()
		);
		assert(
			escrow.referrer.toBase58() ===
				referrerVelocityClient.wallet.publicKey.toBase58(),
			`escrow.referrer ${escrow.referrer.toBase58()} !== referrer ${referrerVelocityClient.wallet.publicKey.toBase58()}`
		);
	});

	it('fill order accrues a referral reward to the escrow and settles it', async () => {
		const marketIndex = 0;
		// Enrollment is a const during the beta, so the referrer was auto-accelerated when
		// their account was created. An admin revoke is the only way back to the Standard
		// rate, and it blocks reenrollment so the revoke survives their next fill.
		await referrerVelocityClient.updateUserAcceleratedReferralStatus(
			referrerVelocityClient.authority,
			false
		);
		await referrerVelocityClient.fetchAccounts();

		// Referee places a crossing limit long order, filled against the vAMM by
		// the filler. The SDK detects referral status and attaches the escrow plus
		// referrer UserStats without caller hints.
		const price = new BN(101).mul(PRICE_PRECISION);
		await refereeVelocityClient.placePerpOrder(
			getLimitOrderParams({
				baseAssetAmount: BASE_PRECISION,
				direction: PositionDirection.LONG,
				marketIndex,
				price,
			})
		);

		await refereeVelocityClient.fetchAccounts();
		const order = refereeVelocityClient.getUser().getOpenOrders()[0];

		const refereeDiscountBefore = refereeVelocityClient
			.getUserStats()
			.getAccount().fees.totalRefereeDiscount;
		const txSig = await fillerVelocityClient.fillPerpOrder(
			await refereeVelocityClient.getUserAccountPublicKey(),
			refereeVelocityClient.getUserAccount(),
			{ marketIndex, orderId: order.orderId }
		);

		await eventSubscriber.awaitTx(txSig);

		const eventRecord = eventSubscriber.getEventsArray('OrderActionRecord')[0];
		const referrerReward = new BN(eventRecord.referrerReward);
		const feeTier =
			refereeVelocityClient.getStateAccount().perpFeeStructure.feeTiers[0];
		const grossFee = takerFeeFor(eventRecord.quoteAssetAmountFilled, feeTier);
		const expectedStandardReward = grossFee
			.muln(feeTier.referrerRewardNumerator)
			.divn(feeTier.referrerRewardDenominator);
		assert(
			referrerReward.eq(expectedStandardReward),
			`standard reward ${referrerReward.toString()} !== ${expectedStandardReward.toString()}`
		);

		await refereeVelocityClient.fetchAccounts();
		const refereeDiscount = refereeVelocityClient
			.getUserStats()
			.getAccount()
			.fees.totalRefereeDiscount.sub(refereeDiscountBefore);
		const expectedDiscount = grossFee
			.muln(feeTier.refereeFeeNumerator)
			.divn(feeTier.refereeFeeDenominator);
		assert(refereeDiscount.eq(expectedDiscount));

		// The referral reward should now be sitting in a Referral-flagged order
		// in the referee's escrow, waiting to be settled.
		await escrowMap.slowSync();
		let escrow = (await escrowMap.mustGet(
			refereeVelocityClient.wallet.publicKey.toBase58()
		)) as RevenueShareEscrowAccount;
		const referralOrder = escrow.orders.find(
			(o) => isBuilderOrderReferral(o) && o.marketIndex === marketIndex
		);
		assert(referralOrder !== undefined, 'expected a referral order in escrow');
		assert(
			referralOrder.feesAccrued.eq(referrerReward),
			`referralOrder.feesAccrued ${referralOrder.feesAccrued.toString()} !== referrerReward ${referrerReward.toString()}`
		);
		assert(referralOrder.feesAccrued.gt(ZERO));

		// Snapshot the referrer's RevenueShare before settle.
		const revShareBeforeInfo =
			await bankrunContextWrapper.connection.getAccountInfo(
				getRevenueShareAccountPublicKey(
					referrerVelocityClient.program.programId,
					referrerVelocityClient.wallet.publicKey
				)
			);
		const revShareBefore: RevenueShareAccount =
			referrerVelocityClient.program.account.revenueShare.coder.accounts.decodeUnchecked(
				'revenueShare',
				revShareBeforeInfo.data
			);

		await bankrunContextWrapper.moveTimeForward(100);

		// Settle the referee's pnl; the escrow map drives the SDK to include the
		// referrer's User + RevenueShare accounts so the sweep can credit them.
		await refereeVelocityClient.fetchAccounts();
		await referrerVelocityClient.settlePNL(
			await refereeVelocityClient.getUserAccountPublicKey(),
			refereeVelocityClient.getUserAccount(),
			marketIndex,
			undefined,
			undefined,
			escrowMap
		);

		// Referral slot in the escrow is reset after sweep.
		await escrowMap.slowSync();
		escrow = (await escrowMap.mustGet(
			refereeVelocityClient.wallet.publicKey.toBase58()
		)) as RevenueShareEscrowAccount;
		const referralOrderAfter = escrow.orders.find(
			(o) => isBuilderOrderReferral(o) && o.marketIndex === marketIndex
		);
		assert(referralOrderAfter !== undefined);
		assert(
			referralOrderAfter.feesAccrued.eq(ZERO),
			`referralOrderAfter.feesAccrued ${referralOrderAfter.feesAccrued.toString()} !== 0`
		);

		// Referrer's RevenueShare.totalReferrerRewards increased by the reward.
		const revShareAfterInfo =
			await bankrunContextWrapper.connection.getAccountInfo(
				getRevenueShareAccountPublicKey(
					referrerVelocityClient.program.programId,
					referrerVelocityClient.wallet.publicKey
				)
			);
		const revShareAfter: RevenueShareAccount =
			referrerVelocityClient.program.account.revenueShare.coder.accounts.decodeUnchecked(
				'revenueShare',
				revShareAfterInfo.data
			);
		const referrerRewardChange = revShareAfter.totalReferrerRewards.sub(
			revShareBefore.totalReferrerRewards
		);
		assert(
			referrerRewardChange.eq(referrerReward),
			`referrerRewardChange ${referrerRewardChange.toString()} !== referrerReward ${referrerReward.toString()}`
		);
	});

	it('Accelerated referrer receives the fixed Accelerated reward', async () => {
		const marketIndex = 0;
		await referrerVelocityClient.updateUserAcceleratedReferralStatus(
			referrerVelocityClient.authority,
			true
		);
		await referrerVelocityClient.fetchAccounts();
		assert(
			(referrerVelocityClient.getUserStats().getAccount()
				.acceleratedReferralStatus &
				AcceleratedReferralStatus.Accelerated) >
				0
		);

		await refereeVelocityClient.placePerpOrder(
			getLimitOrderParams({
				baseAssetAmount: BASE_PRECISION,
				direction: PositionDirection.SHORT,
				marketIndex,
				price: new BN(99).mul(PRICE_PRECISION),
			})
		);
		await refereeVelocityClient.fetchAccounts();
		const order = refereeVelocityClient.getUser().getOpenOrders()[0];
		const discountBefore = refereeVelocityClient.getUserStats().getAccount()
			.fees.totalRefereeDiscount;

		const txSig = await fillerVelocityClient.fillPerpOrder(
			await refereeVelocityClient.getUserAccountPublicKey(),
			refereeVelocityClient.getUserAccount(),
			{ marketIndex, orderId: order.orderId }
		);
		await eventSubscriber.awaitTx(txSig);

		const eventRecord = eventSubscriber.getEventsArray('OrderActionRecord')[0];
		const feeTier =
			refereeVelocityClient.getStateAccount().perpFeeStructure.feeTiers[0];
		const grossFee = takerFeeFor(eventRecord.quoteAssetAmountFilled, feeTier);
		const expectedAcceleratedReward = grossFee
			.muln(ACCELERATED_REFERRER_REWARD_PERCENT)
			.divn(feeTier.referrerRewardDenominator);
		assert(new BN(eventRecord.referrerReward).eq(expectedAcceleratedReward));

		await refereeVelocityClient.fetchAccounts();
		const discount = refereeVelocityClient
			.getUserStats()
			.getAccount()
			.fees.totalRefereeDiscount.sub(discountBefore);
		const expectedDiscount = grossFee
			.muln(feeTier.refereeFeeNumerator)
			.divn(feeTier.refereeFeeDenominator);
		assert(discount.eq(expectedDiscount));
	});

	it('Standard reward applies when the fill omits the referrer UserStats', async () => {
		// A client built before the accelerated-referral upgrade sends the taker's escrow with no
		// referrer UserStats behind it. The fill must still land, at the Standard rate.
		const marketIndex = 0;
		await referrerVelocityClient.fetchAccounts();
		assert(
			(referrerVelocityClient.getUserStats().getAccount()
				.acceleratedReferralStatus &
				AcceleratedReferralStatus.Accelerated) >
				0
		);

		await refereeVelocityClient.placePerpOrder(
			getLimitOrderParams({
				baseAssetAmount: BASE_PRECISION,
				direction: PositionDirection.SHORT,
				marketIndex,
				price: new BN(99).mul(PRICE_PRECISION),
			})
		);
		await refereeVelocityClient.fetchAccounts();
		const order = refereeVelocityClient.getUser().getOpenOrders()[0];

		const fillIx = await fillerVelocityClient.getFillPerpOrderIx(
			await refereeVelocityClient.getUserAccountPublicKey(),
			refereeVelocityClient.getUserAccount(),
			{ marketIndex, orderId: order.orderId },
			undefined, // makerInfo
			undefined, // fillerSubAccountId
			undefined, // isSignedMsg
			undefined, // fillerAuthority
			undefined, // hasBuilderFee
			undefined, // takerEscrow
			true, // takerIsReferred: attach the escrow
			PublicKey.default // takerReferrer: and nothing behind it
		);
		const { txSig } = await fillerVelocityClient.sendTransaction(
			(await fillerVelocityClient.buildTransaction([fillIx])) as Transaction
		);
		await eventSubscriber.awaitTx(txSig);

		const eventRecord = eventSubscriber.getEventsArray('OrderActionRecord')[0];
		const feeTier =
			refereeVelocityClient.getStateAccount().perpFeeStructure.feeTiers[0];
		const grossFee = takerFeeFor(eventRecord.quoteAssetAmountFilled, feeTier);
		const expectedStandardReward = grossFee
			.muln(feeTier.referrerRewardNumerator)
			.divn(feeTier.referrerRewardDenominator);
		assert(new BN(eventRecord.referrerReward).eq(expectedStandardReward));
		assert(
			!new BN(eventRecord.referrerReward).eq(
				grossFee
					.muln(ACCELERATED_REFERRER_REWARD_PERCENT)
					.divn(feeTier.referrerRewardDenominator)
			)
		);
	});

	it('withdraw', async () => {
		const txSig = await refereeVelocityClient.withdraw(
			usdcAmount.div(new BN(2)),
			0,
			refereeUSDCAccount.publicKey
		);

		await eventSubscriber.awaitTx(txSig);
	});
});
