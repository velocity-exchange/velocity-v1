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

	// ---- floor-carrying delegate transfers ----
	// sub 0 equity here is ~8 USDC (10 deposited, 1 withdrawn, ~2000 in fees)

	let delegateVelocityClient: TestClient;

	const floorOf = async (subAccountId: number): Promise<BN> => {
		await velocityClient.fetchAccounts();
		return velocityClient.getUser(subAccountId).getUserAccount().equityFloor;
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
			new BN(4 * 10 ** 6)
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
		const stats = await (
			velocityClient.program.account as any
		).userStats.fetch(statsPk);
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
			new BN(50 * 10 ** 6)
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

		// delegate transfers are frozen as well
		let transferErr: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(1 * 10 ** 6),
				0,
				1,
				0
			);
		} catch (e) {
			transferErr = e as Error;
		}
		assert(transferErr, 'delegate transfer should have been rejected');
		assert(transferErr.message.includes(EQUITY_BELOW_FLOOR_HEX));
	});

	it('warm admin resets the breaker and unfreezes', async () => {
		await velocityClient.resetEquityFloorBreaker(
			velocityClient.getUserStatsAccountPublicKey()
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

		await delegateVelocityClient.unsubscribe();
	});
});
