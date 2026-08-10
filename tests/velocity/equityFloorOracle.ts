import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { Keypair, LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';

import {
	BN,
	TestClient,
	EventSubscriber,
	OracleSource,
	OracleInfo,
	QUOTE_PRECISION,
	User,
	Wallet,
	ZERO,
} from '../../packages/sdk/src';

import {
	createFundedKeyPair,
	createUserWithUSDCAndWSOLAccount,
	createWSolTokenAccountForUser,
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
// InvalidOracle
const INVALID_ORACLE_HEX = '0x1793';

// Equity-floor gates against an invalid oracle (OtterSec #131/#139/#142).
//
// `calculate_user_equity` prices every position at the raw live oracle price
// and reports the oracle-validity verdict separately. Every floor gate used to
// discard that verdict. These tests pin the two fixes:
//
//  - Gates that restrict the user judge on the LOWER bound, which prices an
//    unpriceable asset at `min(live, twap_5min)`. A stale-high price can no
//    longer buy a withdrawal down through the floor.
//  - The floor-shed defusal guard rejects an invalid oracle outright, so it
//    agrees with `trip_equity_floor_breaker`, which already did.
describe('equity floor oracle validity', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let adminVelocityClient: TestClient;
	let adminWSOL: PublicKey;
	let eventSubscriber: EventSubscriber;

	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let solOracle: PublicKey;
	let usdcMint;

	let takerVelocityClient: TestClient;
	let takerUser: User;
	let takerWSOL: PublicKey;
	let takerUSDC: PublicKey;
	let takerKeypair: Keypair;
	let takerUserPublicKey: PublicKey;
	let delegateVelocityClient: TestClient;

	let dustVelocityClient: TestClient;
	let dustUserPublicKey: PublicKey;

	const usdcAmount = new BN(200).mul(QUOTE_PRECISION);
	const solAmount = new BN(20).mul(new BN(LAMPORTS_PER_SOL));
	// net equity at a fair sol price is 200 usdc + 1 sol at 100 = 300
	const floor = new BN(250).mul(QUOTE_PRECISION);

	let marketIndexes: number[];
	let spotMarketIndexes: number[];
	let oracleInfos: OracleInfo[];

	// Pushes the sol oracle to `price` and then past the guard rails, so every
	// read of it is invalid. The spot market's `last_oracle_price_twap_5min`
	// stays near the price the market was initialized at, which is what gives
	// the two bounds room to differ.
	const staleSolOracleAt = async (price: number) => {
		await setFeedPriceNoProgram(bankrunContextWrapper, price, solOracle);
		await bankrunContextWrapper.moveTimeForward(400);
	};

	const refreshSolOracle = async (price: number) => {
		await setFeedPriceNoProgram(bankrunContextWrapper, price, solOracle);
	};

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
		await mockUserUSDCAccount(usdcMint, usdcAmount, bankrunContextWrapper);
		adminWSOL = await createWSolTokenAccountForUser(
			bankrunContextWrapper,
			// @ts-ignore
			bankrunContextWrapper.provider.wallet,
			solAmount
		);

		solOracle = await mockOracleNoProgram(bankrunContextWrapper, 100);

		marketIndexes = [];
		spotMarketIndexes = [0, 1];
		oracleInfos = [{ publicKey: solOracle, source: OracleSource.PYTH_LAZER }];

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
		await adminVelocityClient.initializeUserAccount();

		await initializeQuoteSpotMarket(adminVelocityClient, usdcMint.publicKey);
		await initializeSolSpotMarket(adminVelocityClient, solOracle);

		// sol liquidity so the vault can cover the taker's own sol deposit
		await adminVelocityClient.deposit(
			new BN(5).mul(new BN(LAMPORTS_PER_SOL)),
			1,
			adminWSOL
		);

		[takerVelocityClient, takerWSOL, takerUSDC, takerKeypair] =
			await createUserWithUSDCAndWSOLAccount(
				bankrunContextWrapper,
				usdcMint,
				chProgram,
				new BN(2).mul(new BN(LAMPORTS_PER_SOL)),
				usdcAmount,
				[],
				[0, 1],
				oracleInfos,
				bulkAccountLoader
			);

		await bankrunContextWrapper.fundKeypair(
			takerKeypair,
			10 * LAMPORTS_PER_SOL
		);
		await takerVelocityClient.deposit(usdcAmount, 0, takerUSDC);
		await takerVelocityClient.deposit(new BN(LAMPORTS_PER_SOL), 1, takerWSOL);
		takerUserPublicKey = await takerVelocityClient.getUserAccountPublicKey();

		takerUser = new User({
			velocityClient: takerVelocityClient,
			userAccountPublicKey: takerUserPublicKey,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await takerUser.subscribe();

		// second subaccount plus a delegate, so the floor-shed path is callable
		await takerVelocityClient.initializeUserAccount(1);
		await takerVelocityClient.switchActiveUser(0);

		const delegateKeyPair = await createFundedKeyPair(bankrunContextWrapper);
		await takerVelocityClient.updateUserDelegate(delegateKeyPair.publicKey);
		await takerVelocityClient.switchActiveUser(1);
		await takerVelocityClient.updateUserDelegate(delegateKeyPair.publicKey, 1);
		await takerVelocityClient.switchActiveUser(0);
		await takerVelocityClient.updateUserAllowDelegateTransfer(true);

		delegateVelocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: new Wallet(delegateKeyPair),
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: marketIndexes,
			spotMarketIndexes: spotMarketIndexes,
			oracleInfos,
			authority: takerVelocityClient.wallet.publicKey,
			authoritySubAccountMap: new Map().set(
				takerVelocityClient.wallet.publicKey,
				[0, 1]
			),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await delegateVelocityClient.subscribe();

		// a separate dust account for force_delete_user; it holds only a sol
		// position, so its equity depends entirely on the sol oracle
		const [dustClient, dustWSOL] = await createUserWithUSDCAndWSOLAccount(
			bankrunContextWrapper,
			usdcMint,
			chProgram,
			new BN(LAMPORTS_PER_SOL),
			ZERO,
			[],
			[0, 1],
			oracleInfos,
			bulkAccountLoader
		);
		dustVelocityClient = dustClient;
		await dustVelocityClient.deposit(new BN(100_000), 1, dustWSOL);
		dustUserPublicKey = await dustVelocityClient.getUserAccountPublicKey();

		await adminVelocityClient.updateUserEquityFloor(
			takerUserPublicKey,
			floor,
			ZERO
		);
	});

	after(async () => {
		await takerUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
		await delegateVelocityClient.unsubscribe();
		await dustVelocityClient.unsubscribe();
		await adminVelocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('withdraws are allowed while the deposit oracle is priceable', async () => {
		// positive control at a fair sol price: equity 300, floor 250, so a
		// 40 usdc withdrawal leaves 260 and is permitted
		await takerVelocityClient.fetchAccounts();
		await takerUser.fetchAccounts();

		assert(takerUser.getNetUsdValue().gt(floor));

		const before = await bankrunContextWrapper.connection.getTokenAccount(
			takerUSDC
		);

		await takerVelocityClient.withdraw(
			new BN(40).mul(QUOTE_PRECISION),
			0,
			takerUSDC
		);

		const after = await bankrunContextWrapper.connection.getTokenAccount(
			takerUSDC
		);
		assert(
			after.amount - before.amount === BigInt(40 * 10 ** 6),
			'the fair-price withdrawal should have paid out 40 usdc'
		);
	});

	it('a stale-high deposit oracle cannot buy a withdrawal through the floor', async () => {
		// The sol deposit is now unpriceable and the live price reads 500. The
		// old code valued it at 500 and let the withdrawal through. The lower
		// bound values it at min(500, twap_5min) instead, which keeps the
		// subaccount under its floor and refuses.
		await staleSolOracleAt(500);

		const before = await bankrunContextWrapper.connection.getTokenAccount(
			takerUSDC
		);

		let err: Error | undefined;
		try {
			await takerVelocityClient.withdraw(
				new BN(40).mul(QUOTE_PRECISION),
				0,
				takerUSDC
			);
		} catch (e) {
			err = e as Error;
		}

		assert(err, 'stale-high withdrawal should have been rejected');
		assert(
			err.message.includes(EQUITY_BELOW_FLOOR_HEX),
			`expected EquityBelowFloor, got: ${err.message}`
		);

		const after = await bankrunContextWrapper.connection.getTokenAccount(
			takerUSDC
		);
		assert(
			after.amount === before.amount,
			'no usdc should have left the protocol'
		);
	});

	it('the trip and the floor-shed defusal guard agree on an invalid oracle', async () => {
		// Both halves belong in one test. The entire defect was that these two
		// disagreed: the trip required valid oracles while the defusal guard did
		// not, so an owner could shed the floor off a breached subaccount in the
		// one slot the oracle was bad and defuse the trip permanently.
		//
		// Raise the floor to 400 so the subaccount is genuinely breached at an
		// honest price and merely looks solvent at the stale one. It holds
		// 160 usdc and 1 sol: 260 at the 5 minute twap, which is under the
		// floor and therefore provable, against 660 at the stale live price of
		// 500, which is over it. The shed below moves 100 usdc of funds with
		// 100 usdc of floor, so the credited side can back its new floor and
		// the only thing that can stop the transfer is the oracle.
		await adminVelocityClient.updateUserEquityFloor(
			takerUserPublicKey,
			new BN(400).mul(QUOTE_PRECISION),
			ZERO
		);
		await takerVelocityClient.fetchAccounts();

		const floorBefore = takerVelocityClient
			.getUser(0)
			.getUserAccount().equityFloor;

		// half one: the permissionless trip refuses to arm off an invalid price
		let tripErr: Error | undefined;
		try {
			await adminVelocityClient.tripEquityFloorBreaker(
				takerUserPublicKey,
				takerVelocityClient.getUserAccount()
			);
		} catch (e) {
			tripErr = e as Error;
		}
		assert(tripErr, 'trip should have been rejected');
		assert(
			tripErr.message.includes(INVALID_ORACLE_HEX),
			`expected InvalidOracle from the trip, got: ${tripErr.message}`
		);

		// half two: shedding floor off the same subaccount, in the same state,
		// must refuse for the same reason
		let shedErr: Error | undefined;
		try {
			await delegateVelocityClient.transferDepositByDelegate(
				new BN(100).mul(QUOTE_PRECISION),
				0,
				0,
				1,
				new BN(100).mul(QUOTE_PRECISION)
			);
		} catch (e) {
			shedErr = e as Error;
		}
		assert(shedErr, 'floor shed should have been rejected');
		assert(
			shedErr.message.includes(INVALID_ORACLE_HEX),
			`expected InvalidOracle from the floor shed, got: ${shedErr.message}`
		);

		// the floor did not move, so the trip stays provable once the oracle
		// recovers
		await takerVelocityClient.fetchAccounts();
		assert(
			takerVelocityClient
				.getUser(0)
				.getUserAccount()
				.equityFloor.eq(floorBefore),
			'no floor should have been shed'
		);
	});

	it('force delete user refuses on an invalid oracle', async () => {
		// Deletion sends the account's remaining deposits to the keeper's own
		// token account, so an understated equity would hand away real funds.
		await dustVelocityClient.fetchAccounts();

		let err: Error | undefined;
		try {
			await adminVelocityClient.forceDeleteUser(
				dustUserPublicKey,
				dustVelocityClient.getUserAccount()
			);
		} catch (e) {
			err = e as Error;
		}

		assert(err, 'force delete should have been rejected');
		assert(
			err.message.includes(INVALID_ORACLE_HEX),
			`expected InvalidOracle, got: ${err.message}`
		);

		const stillThere = await bankrunContextWrapper.connection.getAccountInfo(
			dustUserPublicKey
		);
		assert(stillThere !== null, 'the user account should still exist');

		await refreshSolOracle(100);
	});
});
