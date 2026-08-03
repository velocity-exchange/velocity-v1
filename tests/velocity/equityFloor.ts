import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { PublicKey } from '@solana/web3.js';

import {
	BN,
	TestClient,
	PositionDirection,
	User,
	Wallet,
	getMarketOrderParams,
	EventSubscriber,
	PRICE_PRECISION,
} from '../../packages/sdk/src';

import {
	createFundedKeyPair,
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
// InvalidEquityFloorTransfer
const INVALID_FLOOR_TRANSFER_HEX = '0x18d7';
// SufficientCollateral
const SUFFICIENT_COLLATERAL_HEX = '0x1774';
// InvalidEquityBreakerReset
const INVALID_BREAKER_RESET_HEX = '0x18e0';

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
		await delegateVelocityClient?.unsubscribe();
	});

	it('warm admin sets the equity floor', async () => {
		const floor = new BN(20 * 10 ** 6); // above the 10 USDC of equity
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			floor,
			ZERO
		);

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
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			floor,
			ZERO
		);

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
			new BN(20 * 10 ** 6),
			ZERO
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

		// the reducing fill succeeded on a subaccount below its raw floor, so
		// it armed the authority-wide breaker inline (lazy trip)
		assert((await fetchBreakerTripped()) !== 0);

		// clear it so the tests below start from an unarmed authority
		await velocityClient.resetEquityFloorBreaker(
			velocityClient.getUserStatsAccountPublicKey()
		);
		assert((await fetchBreakerTripped()) === 0);
	});

	it('clearing the floor disables the check', async () => {
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			ZERO,
			ZERO
		);

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

	// ---- floor-carrying delegate transfers ----
	// sub 0 equity here is ~8 USDC (10 deposited, 1 withdrawn, ~2000 in fees)

	let delegateVelocityClient: TestClient;

	const floorOf = async (subAccountId: number): Promise<BN> => {
		await velocityClient.fetchAccounts();
		return velocityClient.getUser(subAccountId).getUserAccount().equityFloor;
	};

	const bufferOf = async (subAccountId: number): Promise<BN> => {
		await velocityClient.fetchAccounts();
		return velocityClient.getUser(subAccountId).getUserAccount()
			.equityFloorBuffer;
	};

	it('sets up second subaccount and delegate', async () => {
		await velocityClient.initializeUserAccount(1);
		await velocityClient.switchActiveUser(0);

		const delegateKeyPair = await createFundedKeyPair(bankrunContextWrapper);
		await velocityClient.updateUserDelegate(delegateKeyPair.publicKey);
		await velocityClient.switchActiveUser(1);
		await velocityClient.updateUserDelegate(delegateKeyPair.publicKey, 1);
		await velocityClient.switchActiveUser(0);
		await velocityClient.updateUserAllowDelegateTransfer(true);

		delegateVelocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: new Wallet(delegateKeyPair),
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			oracleInfos: [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }],
			authority: bankrunContextWrapper.provider.wallet.publicKey,
			authoritySubAccountMap: new Map().set(
				bankrunContextWrapper.provider.wallet.publicKey,
				[0, 1]
			),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await delegateVelocityClient.subscribe();

		// floor sub 0 at 4 USDC; equity ~8 so above floor
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			new BN(4 * 10 ** 6),
			ZERO
		);
	});

	it('delegate transfer without floor delta leaves floors in place', async () => {
		await delegateVelocityClient.transferDepositByDelegate(
			new BN(3 * 10 ** 6),
			0,
			0,
			1
		);

		assert((await floorOf(0)).eq(new BN(4 * 10 ** 6)));
		assert((await floorOf(1)).eq(ZERO));
	});

	it('from side cannot transfer past its floor', async () => {
		// sub 0 equity ~5, floor 4: moving 4 more would breach
		let err: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(4 * 10 ** 6),
				0,
				0,
				1
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'transfer should have been rejected');
		assert(err.message.includes(EQUITY_BELOW_FLOOR_HEX));
	});

	it('floor moves with the funds when a delta is passed', async () => {
		// move 3 USDC and 2 USDC of floor: sub 0 keeps equity ~2 >= floor 2,
		// sub 1 gets equity ~6 >= floor 2; sum of floors stays 4
		await delegateVelocityClient.transferDepositByDelegate(
			new BN(3 * 10 ** 6),
			0,
			0,
			1,
			new BN(2 * 10 ** 6)
		);

		assert((await floorOf(0)).eq(new BN(2 * 10 ** 6)));
		assert((await floorOf(1)).eq(new BN(2 * 10 ** 6)));
	});

	it('cannot move more floor than the from side holds', async () => {
		let err: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(1 * 10 ** 6),
				0,
				0,
				1,
				new BN(5 * 10 ** 6)
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'transfer should have been rejected');
		assert(err.message.includes(INVALID_FLOOR_TRANSFER_HEX));
	});

	it('receiving side must back its increased floor with equity', async () => {
		// sub 0 has equity ~2 and floor 2; pushing 2 more floor onto it with
		// only 0.4 of funds leaves floor 4 > equity ~2.4
		let err: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(0.4 * 10 ** 6),
				0,
				1,
				0,
				new BN(2 * 10 ** 6)
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'transfer should have been rejected');
		assert(err.message.includes(INVALID_FLOOR_TRANSFER_HEX));
	});

	it('funds and floor can migrate back together', async () => {
		await delegateVelocityClient.transferDepositByDelegate(
			new BN(2 * 10 ** 6),
			0,
			1,
			0,
			new BN(2 * 10 ** 6)
		);

		// sum of floors preserved through every shuffle
		assert((await floorOf(0)).eq(new BN(4 * 10 ** 6)));
		assert((await floorOf(1)).eq(ZERO));
	});

	it("'auto' computes the minimal floor delta", async () => {
		// sub 0: equity ~5, floor 4 -> excess ~1. moving 3 needs ~2 of floor;
		// auto should land the transfer without the caller doing the math
		await delegateVelocityClient.transferDepositByDelegate(
			new BN(3 * 10 ** 6),
			0,
			0,
			1,
			'auto'
		);

		const floor0 = await floorOf(0);
		const floor1 = await floorOf(1);

		// sum still conserved, and some floor actually moved
		assert(floor0.add(floor1).eq(new BN(4 * 10 ** 6)));
		assert(floor1.gt(ZERO));
	});

	// ---- authority-wide breaker ----

	const fetchBreakerTripped = async (): Promise<number> => {
		const statsPk = velocityClient.getUserStatsAccountPublicKey();
		const stats = await (velocityClient.program.account as any).userStats.fetch(
			statsPk
		);
		return stats.equityBreakerTripped;
	};

	it('breaker cannot be tripped while above the floor', async () => {
		await velocityClient.fetchAccounts();

		let err: Error | undefined;
		try {
			await delegateVelocityClient.tripEquityFloorBreaker(
				userAccountPublicKey,
				velocityClient.getUser(0).getUserAccount()
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'trip should have been rejected');
		assert(err.message.includes(SUFFICIENT_COLLATERAL_HEX));
		assert((await fetchBreakerTripped()) === 0);
	});

	it('breaker trips permissionlessly and freezes every subaccount', async () => {
		// simulate a drawdown breach by raising sub 0's floor above its equity
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			new BN(50 * 10 ** 6),
			ZERO
		);
		await velocityClient.fetchAccounts();

		// the delegate wallet is "anyone" here: trip is permissionless
		await delegateVelocityClient.tripEquityFloorBreaker(
			userAccountPublicKey,
			velocityClient.getUser(0).getUserAccount()
		);
		assert((await fetchBreakerTripped()) !== 0);

		// sub 1 is comfortably above its own floor, yet frozen too
		await velocityClient.switchActiveUser(1);
		let err: Error | undefined;
		try {
			await velocityClient.withdraw(
				new BN(1 * 10 ** 6),
				0,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			err = e as Error;
		}
		await velocityClient.switchActiveUser(0);
		assert(err, 'withdraw from healthy subaccount should have been rejected');
		assert(err.message.includes(EQUITY_BELOW_FLOOR_HEX));

		// delegate transfers are frozen as well: floor cannot move
		let transferErr: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(1 * 10 ** 6),
				0,
				1,
				0,
				new BN(1 * 10 ** 6)
			);
		} catch (e) {
			transferErr = e as Error;
		}
		assert(transferErr, 'floor-carrying transfer should have been rejected');
		assert(transferErr.message.includes(EQUITY_BELOW_FLOOR_HEX));

		// and funds may not move toward a subaccount that is not breached
		let outboundErr: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(1 * 10 ** 6),
				0,
				0,
				1
			);
		} catch (e) {
			outboundErr = e as Error;
		}
		assert(
			outboundErr,
			'transfer to a healthy subaccount should have been rejected'
		);
		assert(outboundErr.message.includes(EQUITY_BELOW_FLOOR_HEX));
	});

	it('cure transfers into the breached subaccount stay allowed while tripped', async () => {
		const floor0Before = await floorOf(0);
		const floor1Before = await floorOf(1);
		await velocityClient.fetchAccounts();
		const equity0Before = velocityClient.getUser(0).getNetUsdValue();

		// funds-only transfer into breached sub 0: the one delegate transfer
		// the breaker allows, so internal surplus can cure a breach
		await delegateVelocityClient.transferDepositByDelegate(
			new BN(1 * 10 ** 6),
			0,
			1,
			0
		);

		// funds moved, floors did not, and the flag did not clear
		await velocityClient.fetchAccounts();
		assert(velocityClient.getUser(0).getNetUsdValue().gt(equity0Before));
		assert((await floorOf(0)).eq(floor0Before));
		assert((await floorOf(1)).eq(floor1Before));
		assert((await fetchBreakerTripped()) !== 0);

		// deposits also stay allowed under the freeze; restore sub 1's equity
		// so the later buffer-band arithmetic keeps its transfer history
		await velocityClient.deposit(
			new BN(1 * 10 ** 6),
			0,
			userUSDCAccount.publicKey,
			1
		);
		assert((await fetchBreakerTripped()) !== 0);
	});

	it('reset is refused while any subaccount is below its buffered floor', async () => {
		await velocityClient.fetchAccounts();
		const userAccounts = [
			velocityClient.getUser(0).getUserAccount(),
			velocityClient.getUser(1).getUserAccount(),
		];

		// sub 0 is still below its 50 floor: the self-verifying reset reverts
		let err: Error | undefined;
		try {
			await velocityClient.resetEquityFloorBreaker(
				velocityClient.getUserStatsAccountPublicKey(),
				undefined,
				userAccounts
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'reset with a breached subaccount should have been rejected');
		assert(err.message.includes(INVALID_BREAKER_RESET_HEX));
		assert((await fetchBreakerTripped()) !== 0);

		// omitting the breached subaccount does not help: the count is pinned
		// to the authority's live subaccounts
		let incompleteErr: Error | undefined;
		try {
			await velocityClient.resetEquityFloorBreaker(
				velocityClient.getUserStatsAccountPublicKey(),
				undefined,
				[velocityClient.getUser(1).getUserAccount()]
			);
		} catch (e) {
			incompleteErr = e as Error;
		}
		assert(incompleteErr, 'incomplete reset should have been rejected');
		assert(incompleteErr.message.includes(INVALID_BREAKER_RESET_HEX));
		assert((await fetchBreakerTripped()) !== 0);
	});

	it('warm admin resets the breaker and unfreezes', async () => {
		// sub 0 cannot back its simulated 50 floor; resuming anyway means
		// lowering the floor first, explicitly, then the reset verifies clean
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			ZERO,
			ZERO
		);
		await velocityClient.fetchAccounts();

		await velocityClient.resetEquityFloorBreaker(
			velocityClient.getUserStatsAccountPublicKey(),
			undefined,
			[
				velocityClient.getUser(0).getUserAccount(),
				velocityClient.getUser(1).getUserAccount(),
			]
		);
		assert((await fetchBreakerTripped()) === 0);

		// healthy subaccount can act again
		await velocityClient.switchActiveUser(1);
		await velocityClient.withdraw(
			new BN(1 * 10 ** 6),
			0,
			userUSDCAccount.publicKey
		);
		await velocityClient.switchActiveUser(0);
	});

	// ---- buffer band ----
	// sub 1 has ~6 USDC of equity here (integral transfers +3 +3 -2 +3 -1,
	// minus a unit or two of interest-index rounding dust), so tests measure
	// equity instead of assuming exact values, and avoid landing transfers
	// exactly on the buffered line (the dust makes that fragile on-chain).

	let sub1UserPublicKey: PublicKey;

	const tripAttempt = async (
		userPk: PublicKey,
		subAccountId: number
	): Promise<Error | undefined> => {
		await velocityClient.fetchAccounts();
		try {
			await delegateVelocityClient.tripEquityFloorBreaker(
				userPk,
				velocityClient.getUser(subAccountId).getUserAccount()
			);
			return undefined;
		} catch (e) {
			return e as Error;
		}
	};

	it('warm admin sets floor and buffer together', async () => {
		sub1UserPublicKey = await velocityClient.getUserAccountPublicKey(1);

		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			ZERO,
			ZERO
		);
		await velocityClient.updateUserEquityFloor(
			sub1UserPublicKey,
			new BN(3 * 10 ** 6),
			new BN(2 * 10 ** 6)
		);

		await velocityClient.fetchAccounts();
		const sub1 = velocityClient.getUser(1);
		assert(sub1.getUserAccount().equityFloor.eq(new BN(3 * 10 ** 6)));
		assert(sub1.getUserAccount().equityFloorBuffer.eq(new BN(2 * 10 ** 6)));
		assert(sub1.getBufferedEquityFloor().eq(new BN(5 * 10 ** 6)));
		// equity ~6 vs buffered floor 5: healthy, ~1 of headroom above the gate
		assert(!sub1.isBelowEquityFloor());
		assert(!sub1.isBelowBufferedEquityFloor());
		const aboveBuffered = sub1.getEquityAboveBufferedFloor()!;
		assert(aboveBuffered.gt(new BN(0.9 * 10 ** 6)));
		assert(aboveBuffered.lte(new BN(1 * 10 ** 6)));
	});

	it('withdrawals may not dip into the buffer band', async () => {
		await velocityClient.switchActiveUser(1);

		// ~6 - 2 = ~4 < 5 (floor + buffer): rejected even though 4 > floor 3
		let err: Error | undefined;
		try {
			await velocityClient.withdraw(
				new BN(2 * 10 ** 6),
				0,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'withdraw into the band should have been rejected');
		assert(err.message.includes(EQUITY_BELOW_FLOOR_HEX));

		// withdrawing everything above the buffered floor except a sliver of
		// slack is allowed, parking sub 1 just above floor + buffer
		await velocityClient.fetchAccounts();
		const drainTo = new BN(5 * 10 ** 6).addn(20);
		await velocityClient.withdraw(
			velocityClient
				.getUser(1)
				.getTotalCollateral('Initial', true)
				.sub(drainTo),
			0,
			userUSDCAccount.publicKey
		);

		// now essentially at the buffered floor: nothing meaningful may leave
		let err2: Error | undefined;
		try {
			await velocityClient.withdraw(
				new BN(0.1 * 10 ** 6),
				0,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			err2 = e as Error;
		}
		await velocityClient.switchActiveUser(0);
		assert(err2, 'withdraw below the buffered floor should have been rejected');
		assert(err2.message.includes(EQUITY_BELOW_FLOOR_HEX));
	});

	it('the buffer band gates actions but does not arm the breaker', async () => {
		// simulate a drawdown into the band: equity 5, floor 4.5, buffered 6.5
		await velocityClient.updateUserEquityFloor(
			sub1UserPublicKey,
			new BN(4.5 * 10 ** 6),
			new BN(2 * 10 ** 6)
		);
		await velocityClient.switchActiveUser(1);

		// risk-increasing order rejected
		let orderErr: Error | undefined;
		try {
			await velocityClient.placeAndTakePerpOrder(
				getMarketOrderParams({
					marketIndex,
					direction: PositionDirection.LONG,
					baseAssetAmount: new BN(AMM_RESERVE_PRECISION),
					price: PRICE_PRECISION.mul(new BN(1049)).div(new BN(1000)),
				})
			);
		} catch (e) {
			orderErr = e as Error;
		}
		assert(orderErr, 'order in the band should have been rejected');
		assert(orderErr.message.includes(EQUITY_BELOW_FLOOR_HEX));

		// withdrawal rejected
		let withdrawErr: Error | undefined;
		try {
			await velocityClient.withdraw(
				new BN(0.5 * 10 ** 6),
				0,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			withdrawErr = e as Error;
		}
		assert(withdrawErr, 'withdraw in the band should have been rejected');
		assert(withdrawErr.message.includes(EQUITY_BELOW_FLOOR_HEX));

		await velocityClient.switchActiveUser(0);

		// but the trip is impossible: equity 5 >= raw floor 4.5
		const tripErr = await tripAttempt(sub1UserPublicKey, 1);
		assert(tripErr, 'trip inside the band should have been rejected');
		assert(tripErr.message.includes(SUFFICIENT_COLLATERAL_HEX));
		assert((await fetchBreakerTripped()) === 0);
	});

	it("'auto' transfers carry no floor while headroom covers the amount", async () => {
		// sub 1: equity ~5, floor 1, buffer 1 -> ~3 of excess above the gate
		await velocityClient.updateUserEquityFloor(
			sub1UserPublicKey,
			new BN(1 * 10 ** 6),
			new BN(1 * 10 ** 6)
		);
		await delegateVelocityClient.fetchAccounts();

		// moving 1 fits entirely inside the excess: no floor should move
		await delegateVelocityClient.transferDepositByDelegate(
			new BN(1 * 10 ** 6),
			0,
			1,
			0,
			'auto'
		);

		assert((await floorOf(1)).eq(new BN(1 * 10 ** 6)));
		assert((await floorOf(0)).eq(ZERO));

		const tripErr = await tripAttempt(sub1UserPublicKey, 1);
		assert(tripErr, 'trip after an auto transfer should have been rejected');
		assert(tripErr.message.includes(SUFFICIENT_COLLATERAL_HEX));
		assert((await fetchBreakerTripped()) === 0);
	});

	it("'auto' transfers shed the whole floor when the cap binds, disabling the check", async () => {
		// sub 1: equity ~4, floor 1, buffer 1 -> excess ~2; moving 3.5 wants
		// ~1.5 of floor but the cap is the 1 of floor sub 1 holds, so the
		// whole floor migrates and sub 1's check turns off (floor 0)
		const buffer0Before = await bufferOf(0);
		await delegateVelocityClient.fetchAccounts();
		await delegateVelocityClient.transferDepositByDelegate(
			new BN(3.5 * 10 ** 6),
			0,
			1,
			0,
			'auto'
		);

		assert((await floorOf(1)).eq(ZERO));
		assert((await floorOf(0)).eq(new BN(1 * 10 ** 6)));

		// the whole buffer travels with the whole floor: no orphan buffer is
		// left on the now check-disabled sub 1
		assert((await bufferOf(1)).eq(ZERO));
		assert((await bufferOf(0)).eq(buffer0Before.add(new BN(1 * 10 ** 6))));

		// neither side is trippable: sub 0 backs its floor, sub 1 has none
		const trip0 = await tripAttempt(userAccountPublicKey, 0);
		assert(trip0, 'trip on the floor-holding side should have been rejected');
		assert(trip0.message.includes(SUFFICIENT_COLLATERAL_HEX));
		const trip1 = await tripAttempt(sub1UserPublicKey, 1);
		assert(trip1, 'trip on the floorless side should have been rejected');
		assert(trip1.message.includes(SUFFICIENT_COLLATERAL_HEX));
		assert((await fetchBreakerTripped()) === 0);

		// with the floor fully shed, sub 1 is unrestricted again
		await velocityClient.switchActiveUser(1);
		await velocityClient.withdraw(
			new BN(0.1 * 10 ** 6),
			0,
			userUSDCAccount.publicKey
		);
		await velocityClient.switchActiveUser(0);
	});

	it('floor rebalances without funds, inside the same rules', async () => {
		// sub 1 (~0.4 equity) cannot take 0.3 of floor: the buffer share the
		// move carries (0.3 of sub 0's 1 of buffer) makes the credited side
		// back 0.6 of floor + buffer with only ~0.4 of equity
		await delegateVelocityClient.fetchAccounts();
		let err: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				ZERO,
				0,
				0,
				1,
				new BN(0.3 * 10 ** 6)
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'unbacked floor-only move should have been rejected');
		assert(err.message.includes(INVALID_FLOOR_TRANSFER_HEX));

		// with the buffers cleared, the same zero-amount move is backed and lands
		await velocityClient.updateUserEquityFloor(sub1UserPublicKey, ZERO, ZERO);
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			new BN(1 * 10 ** 6),
			ZERO
		);
		await delegateVelocityClient.fetchAccounts();
		await delegateVelocityClient.transferDepositByDelegate(
			ZERO,
			0,
			0,
			1,
			new BN(0.3 * 10 ** 6)
		);

		assert((await floorOf(0)).eq(new BN(0.7 * 10 ** 6)));
		assert((await floorOf(1)).eq(new BN(0.3 * 10 ** 6)));

		const trip0 = await tripAttempt(userAccountPublicKey, 0);
		assert(trip0, 'trip on sub 0 should have been rejected');
		assert(trip0.message.includes(SUFFICIENT_COLLATERAL_HEX));
		const trip1 = await tripAttempt(sub1UserPublicKey, 1);
		assert(trip1, 'trip on sub 1 should have been rejected');
		assert(trip1.message.includes(SUFFICIENT_COLLATERAL_HEX));
		assert((await fetchBreakerTripped()) === 0);
	});

	it('clearing floors ends buffer enforcement', async () => {
		await velocityClient.updateUserEquityFloor(
			userAccountPublicKey,
			ZERO,
			ZERO
		);
		await velocityClient.updateUserEquityFloor(sub1UserPublicKey, ZERO, ZERO);

		await velocityClient.fetchAccounts();
		assert(!velocityClient.getUser(0).isBelowBufferedEquityFloor());
		assert(!velocityClient.getUser(1).isBelowBufferedEquityFloor());

		await velocityClient.switchActiveUser(1);
		await velocityClient.withdraw(
			new BN(0.1 * 10 ** 6),
			0,
			userUSDCAccount.publicKey
		);
		await velocityClient.switchActiveUser(0);
	});
});
