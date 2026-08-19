import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	Transaction,
} from '@solana/web3.js';

import {
	BN,
	TestClient,
	EventSubscriber,
	OracleSource,
	OracleInfo,
	QUOTE_PRECISION,
	User,
	ZERO,
} from '../../packages/sdk/src';

import {
	createUserWithUSDCAndWSOLAccount,
	createWSolTokenAccountForUser,
	initializeQuoteSpotMarket,
	initializeSolSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { createTransferInstruction } from '@solana/spl-token';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';

// EquityBelowFloor
const EQUITY_BELOW_FLOOR_HEX = '0x18d6';

// Lazy breaker trip: a taker holds 200 USDC and owes 1 SOL (tokens spent
// externally), so net equity (100) sits below the floor (150) while nobody
// has sent the permissionless trip. The strictly reducing swap that repays
// the borrow is allowed, and its success must arm the authority-wide breaker
// inline because the account is still below its raw floor afterwards.
describe('equity floor lazy trip', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let adminVelocityClient: TestClient;
	let adminWSOL: PublicKey;
	let adminUSDC;
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

	const usdcAmount = new BN(200).mul(QUOTE_PRECISION);
	const solAmount = new BN(10).mul(new BN(LAMPORTS_PER_SOL));
	const floor = new BN(150).mul(QUOTE_PRECISION);

	let marketIndexes: number[];
	let spotMarketIndexes: number[];
	let oracleInfos: OracleInfo[];

	const fetchBreakerTripped = async (): Promise<number> => {
		const statsPk = takerVelocityClient.getUserStatsAccountPublicKey();
		const stats = await (
			takerVelocityClient.program.account as any
		).userStats.fetch(statsPk);
		return stats.equityBreakerTripped;
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
		adminUSDC = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper
		);
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

		// SOL liquidity so the taker can borrow within the daily withdraw guard
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
				ZERO,
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
	});

	after(async () => {
		await takerUser.unsubscribe();
		await takerVelocityClient.unsubscribe();
		await adminVelocityClient.unsubscribe();
		await eventSubscriber.unsubscribe();
	});

	it('taker drops below the floor with the breaker unarmed', async () => {
		// borrow 1 SOL against the 200 USDC deposit; tokens leave the protocol
		await takerVelocityClient.withdraw(
			new BN(LAMPORTS_PER_SOL),
			1,
			takerWSOL,
			false
		);

		await adminVelocityClient.updateUserEquityFloor(
			takerUserPublicKey,
			floor,
			ZERO
		);

		await takerVelocityClient.fetchAccounts();
		await takerUser.fetchAccounts();
		assert(takerUser.getNetUsdValue().lt(floor));
		assert(takerUser.isBelowEquityFloor());

		// nobody sent the permissionless trip
		assert((await fetchBreakerTripped()) === 0);
	});

	it('reducing swap succeeds and arms the breaker inline', async () => {
		const amountIn = new BN(100).mul(QUOTE_PRECISION);
		const { beginSwapIx, endSwapIx } = await takerVelocityClient.getSwapIx({
			amountIn,
			inMarketIndex: 0,
			outMarketIndex: 1,
			inTokenAccount: takerUSDC,
			outTokenAccount: takerWSOL,
		});

		const transferIn = createTransferInstruction(
			takerUSDC,
			adminUSDC.publicKey,
			takerVelocityClient.wallet.publicKey,
			amountIn.toNumber()
		);
		const transferOut = createTransferInstruction(
			adminWSOL,
			takerWSOL,
			adminVelocityClient.wallet.publicKey,
			LAMPORTS_PER_SOL
		);

		const tx = new Transaction()
			.add(beginSwapIx)
			.add(transferIn)
			.add(transferOut)
			.add(endSwapIx);

		const { txSig } = await takerVelocityClient.sendTransaction(tx, [
			// @ts-ignore
			adminVelocityClient.wallet.payer,
		]);
		bankrunContextWrapper.printTxLogs(txSig);

		await takerVelocityClient.fetchAccounts();
		await takerUser.fetchAccounts();

		// the swap went through: usdc consumed, sol debt cleared
		const solPosition = takerUser.getTokenAmount(1);
		assert(solPosition.abs().lt(new BN(LAMPORTS_PER_SOL).div(new BN(100))));

		// still below the raw floor afterwards, so the swap's success armed
		// the authority-wide breaker without a keeper trip transaction
		assert(takerUser.getNetUsdValue().lt(floor));
		assert((await fetchBreakerTripped()) !== 0);
	});

	it('the lazily armed breaker freezes withdrawals', async () => {
		let err: Error | undefined;
		try {
			await takerVelocityClient.withdraw(
				new BN(10).mul(QUOTE_PRECISION),
				0,
				takerUSDC
			);
		} catch (e) {
			err = e as Error;
		}
		assert(err, 'withdraw should have been rejected while tripped');
		assert(err.message.includes(EQUITY_BELOW_FLOOR_HEX));
	});
});
