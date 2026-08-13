import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	LAMPORTS_PER_SOL,
	PublicKey,
	Transaction,
	TransactionInstruction,
} from '@solana/web3.js';

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
	OrderTriggerCondition,
	MarketStatus,
	getMarketOrderParams,
	getTriggerMarketOrderParams,
	getLimitOrderParams,
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
	setFeedPriceNoProgram,
} from './testHelpers';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';

// EquityBelowFloor
const EQUITY_BELOW_FLOOR_HEX = '0x18d6';
// position of liquidator_stats in the LiquidateSpotWithSwap account list
const LIQUIDATOR_STATS_IX_INDEX = 3;

// The authority-wide equity breaker as a freeze over every route the audit
// issues flagged (#54 trigger, #57 perp transfer, #68 liquidator routes).
// One authority trips the breaker on subaccount 0 and every test drives the
// frozen action from its HEALTHY subaccount 1; the shared-freeze semantics
// are the whole point.
describe('equity breaker freeze', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let adminVelocityClient: TestClient;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let usdcMint;
	let adminUSDCAccount;

	const mantissaSqrtScale = new BN(100000);
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(10_000).mul(QUOTE_PRECISION);
	const solAmount = new BN(LAMPORTS_PER_SOL);

	let perpOracle: PublicKey;
	let solSpotOracle: PublicKey;

	let marketIndexes: number[];
	let spotMarketIndexes: number[];
	let oracleInfos: OracleInfo[];

	// tripped authority: sub 0 breaches, sub 1 stays healthy
	let trippedClient: TestClient;
	let trippedWSOL: PublicKey;
	let trippedSub0PublicKey: PublicKey;

	// victim for the liquidation routes: only needs to be a valid user, the
	// breaker gate runs before liquidation eligibility
	let victimClient: TestClient;
	let victimPublicKey: PublicKey;

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
		adminUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper
		);

		perpOracle = await mockOracleNoProgram(bankrunContextWrapper, 100);
		solSpotOracle = await mockOracleNoProgram(bankrunContextWrapper, 100);

		marketIndexes = [0];
		spotMarketIndexes = [0, 1];
		oracleInfos = [
			{ publicKey: perpOracle, source: OracleSource.PYTH_LAZER },
			{ publicKey: solSpotOracle, source: OracleSource.PYTH_LAZER },
		];

		adminVelocityClient = new TestClient({
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
		await adminVelocityClient.initialize(usdcMint.publicKey, true);
		await adminVelocityClient.subscribe();
		await initializeQuoteSpotMarket(adminVelocityClient, usdcMint.publicKey);
		await initializeSolSpotMarket(adminVelocityClient, solSpotOracle);

		// zero auction duration: market orders fill against the AMM at once,
		// which is how the perp-transfer test builds its position
		await adminVelocityClient.updatePerpAuctionDuration(new BN(0));

		const periodicity = new BN(60 * 60);
		await adminVelocityClient.initializePerpMarket(
			0,
			perpOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(100).mul(PEG_PRECISION)
		);
		await adminVelocityClient.updatePerpMarketStatus(0, MarketStatus.ACTIVE);

		await adminVelocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			adminUSDCAccount.publicKey
		);

		// the authority that gets frozen: sub 0 (floored, breached) + sub 1
		// (healthy, the one every frozen action is driven from)
		let trippedUSDC: PublicKey;
		[trippedClient, trippedWSOL, trippedUSDC] =
			await createUserWithUSDCAndWSOLAccount(
				bankrunContextWrapper,
				usdcMint,
				chProgram,
				solAmount,
				usdcAmount,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await trippedClient.deposit(usdcAmount.div(new BN(2)), 0, trippedUSDC);
		trippedSub0PublicKey = await trippedClient.getUserAccountPublicKey();

		await trippedClient.initializeUserAccount(1);
		await trippedClient.switchActiveUser(1);
		await trippedClient.deposit(usdcAmount.div(new BN(2)), 0, trippedUSDC);
		await trippedClient.switchActiveUser(0);

		[victimClient] = await createUserWithUSDCAccount(
			bankrunContextWrapper,
			usdcMint,
			chProgram,
			usdcAmount,
			marketIndexes,
			spotMarketIndexes,
			oracleInfos,
			bulkAccountLoader
		);
		await victimClient.fetchAccounts();
		victimPublicKey = await victimClient.getUserAccountPublicKey();
	});

	after(async () => {
		await adminVelocityClient.unsubscribe();
		await trippedClient.unsubscribe();
		await victimClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('rests a trigger order, breaches sub 0 and trips the breaker', async () => {
		// the trigger order must exist BEFORE the freeze: the test is what a
		// keeper can do to it afterwards
		await trippedClient.switchActiveUser(1);
		await trippedClient.placePerpOrder(
			getTriggerMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: BASE_PRECISION,
				triggerPrice: new BN(50).mul(PRICE_PRECISION),
				triggerCondition: OrderTriggerCondition.ABOVE,
				userOrderId: 1,
			})
		);
		await trippedClient.switchActiveUser(0);

		// raise sub 0's floor above its equity and trip permissionlessly
		await adminVelocityClient.updateUserEquityFloor(
			trippedSub0PublicKey,
			usdcAmount.mul(new BN(2)),
			ZERO
		);
		await trippedClient.fetchAccounts();
		await adminVelocityClient.tripEquityFloorBreaker(
			trippedSub0PublicKey,
			trippedClient.getUserAccount(0)
		);

		const statsPk = trippedClient.getUserStatsAccountPublicKey();
		const stats = await (trippedClient.program.account as any).userStats.fetch(
			statsPk
		);
		assert(stats.equityBreakerTripped !== 0, 'the breaker should have tripped');
	});

	it('placement stays allowed on a healthy subaccount while tripped', async () => {
		// pins the #54 policy: the freeze blocks execution (fills, triggers,
		// liquidations, transfers out), not resting intent. plain placement:
		await trippedClient.switchActiveUser(1);
		await trippedClient.placePerpOrder(
			getLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: BASE_PRECISION,
				price: new BN(90).mul(PRICE_PRECISION),
				userOrderId: 2,
			})
		);

		// batch placement:
		await trippedClient.placeOrders([
			getLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: BASE_PRECISION,
				price: new BN(89).mul(PRICE_PRECISION),
				userOrderId: 3,
			}),
			getLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: BASE_PRECISION,
				price: new BN(88).mul(PRICE_PRECISION),
				userOrderId: 4,
			}),
		]);

		await trippedClient.fetchAccounts();
		for (const userOrderId of [2, 3, 4]) {
			assert(
				trippedClient.getOrderByUserId(userOrderId) !== undefined,
				`order ${userOrderId} should have been placed`
			);
		}

		// clean up the resting limit orders; the trigger order stays for the
		// next test
		await trippedClient.cancelOrderByUserId(2);
		await trippedClient.cancelOrderByUserId(3);
		await trippedClient.cancelOrderByUserId(4);
		await trippedClient.switchActiveUser(0);
	});

	it('triggering while tripped cancels the order instead of activating it, with no keeper reward', async () => {
		await trippedClient.fetchAccounts();
		const order = trippedClient.getOrderByUserId(1, 1);
		assert(order !== undefined, 'trigger order should be resting');

		const fillerQuoteBefore = adminVelocityClient
			.getUserAccount()
			.perpPositions.map((p) => p.quoteAssetAmount.toString());

		// the trigger itself succeeds as a transaction: the risk gate resolves
		// it into a reward-free cancel rather than an activation
		await adminVelocityClient.triggerOrder(
			await trippedClient.getUserAccountPublicKey(1),
			trippedClient.getUserAccount(1),
			order
		);

		// read the account raw; a cancelled slot keeps its userOrderId with
		// status flipped to `canceled`, so assert on the status
		const sub1Raw = await (trippedClient.program.account as any).user.fetch(
			await trippedClient.getUserAccountPublicKey(1)
		);
		const triggerOrderSlot = sub1Raw.orders.find(
			(o: { userOrderId: number }) => o.userOrderId === 1
		);
		assert(
			triggerOrderSlot !== undefined &&
				isVariant(triggerOrderSlot.status, 'canceled'),
			'the trigger order should have been cancelled, not activated'
		);
		await adminVelocityClient.fetchAccounts();
		const cancelRecord = eventSubscriber
			.getEventsArray('OrderActionRecord')
			.find((record) => isVariant(record.action, 'cancel'));
		assert(
			cancelRecord !== undefined,
			'a cancel should have been recorded for the trigger'
		);
		assert(
			cancelRecord.fillerReward === null || cancelRecord.fillerReward.eq(ZERO),
			'the triggering keeper must not be paid for flipping a frozen order into a cancel'
		);
		const fillerQuoteAfter = adminVelocityClient
			.getUserAccount()
			.perpPositions.map((p) => p.quoteAssetAmount.toString());
		assert(
			JSON.stringify(fillerQuoteBefore) === JSON.stringify(fillerQuoteAfter),
			'no value should have moved to the filler'
		);
	});

	it('every liquidation route rejects the tripped liquidator', async () => {
		// driven from the HEALTHY subaccount: the freeze is authority-wide.
		// the victim does not need to be liquidatable, the breaker gate runs
		// before liquidation eligibility, and that ordering is part of the pin.
		await trippedClient.switchActiveUser(1);
		await victimClient.fetchAccounts();
		const victimAccount = victimClient.getUserAccount();

		const expectBreakerReject = async (
			name: string,
			run: () => Promise<unknown>
		) => {
			let err: Error | undefined;
			try {
				await run();
			} catch (e) {
				err = e as Error;
			}
			assert(err, `${name} should have been rejected`);
			assert(
				err.message.includes(EQUITY_BELOW_FLOOR_HEX),
				`${name}: expected EquityBelowFloor, got: ${err.message}`
			);
		};

		await expectBreakerReject('liquidatePerp', () =>
			trippedClient.liquidatePerp(
				victimPublicKey,
				victimAccount,
				0,
				BASE_PRECISION
			)
		);

		await expectBreakerReject('liquidateSpot', () =>
			trippedClient.liquidateSpot(
				victimPublicKey,
				victimAccount,
				0,
				1,
				new BN(1)
			)
		);

		await expectBreakerReject('liquidateBorrowForPerpPnl', () =>
			trippedClient.liquidateBorrowForPerpPnl(
				victimPublicKey,
				victimAccount,
				0,
				1,
				new BN(1)
			)
		);

		await expectBreakerReject('liquidatePerpPnlForDeposit', () =>
			trippedClient.liquidatePerpPnlForDeposit(
				victimPublicKey,
				victimAccount,
				0,
				0,
				new BN(1)
			)
		);

		// swap-backed: the route this PR closes. the tokens flow through the
		// authority's wallet, but the gate at `begin` bars it the same way.
		const { beginSwapIx, endSwapIx } =
			await trippedClient.getLiquidateSpotWithSwapIx({
				liabilityMarketIndex: 0,
				assetMarketIndex: 1,
				swapAmount: new BN(1),
				assetTokenAccount: trippedWSOL,
				liabilityTokenAccount: await (async () => {
					// any usdc token account of the tripped authority works;
					// begin rejects before any token movement
					const account = await mockUserUSDCAccount(
						usdcMint,
						ZERO,
						bankrunContextWrapper,
						trippedClient.wallet.publicKey
					);
					return account.publicKey;
				})(),
				userAccount: victimAccount,
				userAccountPublicKey: victimPublicKey,
			});
		await expectBreakerReject('liquidateSpotWithSwap', () =>
			trippedClient.sendTransaction(
				new Transaction().add(beginSwapIx, endSwapIx)
			)
		);

		// the obvious bypass of the gate above: pass some OTHER authority's
		// untripped UserStats in the liquidator_stats slot. `is_stats_for_user`
		// is the only thing standing between the new check and being
		// cosmetic, so pin it rather than trusting the constraint by eye.
		const foreignStats = victimClient.getUserStatsAccountPublicKey();
		const swapWithForeignStats = (ix: TransactionInstruction) => {
			const keys = ix.keys.map((meta, i) =>
				i === LIQUIDATOR_STATS_IX_INDEX
					? { ...meta, pubkey: foreignStats }
					: meta
			);
			return new TransactionInstruction({
				programId: ix.programId,
				keys,
				data: ix.data,
			});
		};

		let foreignErr: Error | undefined;
		try {
			await trippedClient.sendTransaction(
				new Transaction().add(
					swapWithForeignStats(beginSwapIx),
					swapWithForeignStats(endSwapIx)
				)
			);
		} catch (e) {
			foreignErr = e as Error;
		}
		assert(
			foreignErr,
			'a foreign liquidator_stats should not satisfy the breaker gate'
		);
		assert(
			!foreignErr.message.includes(EQUITY_BELOW_FLOOR_HEX),
			`expected the constraint to reject before the breaker check, got: ${foreignErr.message}`
		);

		await trippedClient.switchActiveUser(0);
	});

	it('perp transfer cannot land exposure on a recipient below its floor', async () => {
		// #57: recipient passes initial margin but sits below its configured
		// floor, so the transfer must reject on the recipient's floor, not just
		// the sender's. fresh, untripped authority.
		const [transferClient, transferUSDC] = await createUserWithUSDCAccount(
			bankrunContextWrapper,
			usdcMint,
			chProgram,
			usdcAmount,
			marketIndexes,
			spotMarketIndexes,
			oracleInfos,
			bulkAccountLoader
		);
		await transferClient.deposit(usdcAmount.div(new BN(2)), 0, transferUSDC);

		// re-stamp the perp oracle: enough slots have passed for it to read
		// stale-for-amm, which would silently withhold the AMM fill below
		await setFeedPriceNoProgram(bankrunContextWrapper, 100, perpOracle);

		// sub 0 opens the position to transfer (fills against the AMM)
		await transferClient.placeAndTakePerpOrder(
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: BASE_PRECISION,
			})
		);
		await transferClient.fetchAccounts();
		assert(
			transferClient.getUserAccount().perpPositions[0].baseAssetAmount.gt(ZERO),
			'the AMM fill should have opened the position to transfer'
		);

		await transferClient.initializeUserAccount(1);
		await transferClient.switchActiveUser(1);
		await transferClient.deposit(usdcAmount.div(new BN(2)), 0, transferUSDC);
		await transferClient.switchActiveUser(0);

		// recipient's floor above its equity: initial margin passes trivially
		// (it holds only deposits), the floor does not
		const recipientPublicKey = await transferClient.getUserAccountPublicKey(1);
		await adminVelocityClient.updateUserEquityFloor(
			recipientPublicKey,
			usdcAmount.mul(new BN(2)),
			ZERO
		);
		await transferClient.fetchAccounts();

		let err: Error | undefined;
		try {
			await transferClient.transferPerpPosition(0, 1, 0, BASE_PRECISION);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'the perp transfer should have been rejected');
		assert(
			err.message.includes(EQUITY_BELOW_FLOOR_HEX),
			`expected EquityBelowFloor on the recipient, got: ${err.message}`
		);

		await transferClient.unsubscribe();
	});
});
