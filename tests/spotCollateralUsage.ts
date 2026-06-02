import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { PublicKey } from '@solana/web3.js';

import {
	TestClient,
	BN,
	EventSubscriber,
	SPOT_MARKET_RATE_PRECISION,
	OracleSource,
	SPOT_MARKET_WEIGHT_PRECISION,
	OracleInfo,
} from '../sdk/src';

import {
	createUserWithUSDCAccount,
	createUserWithUSDCAndWSOLAccount,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	sleep,
} from './testHelpers';
import { NATIVE_MINT } from '@solana/spl-token';
import { QUOTE_PRECISION, ZERO } from '../sdk';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../sdk/src/bankrun/bankrunConnection';

describe('spot collateral usage tracking', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let admin: TestClient;
	let eventSubscriber: EventSubscriber;
	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let solOracle: PublicKey;
	let usdcMint;

	// lender: deposits USDC so there's something to borrow
	let lenderDriftClient: TestClient;
	let lenderUSDCAccount: PublicKey;

	// borrower: deposits SOL collateral, borrows USDC against it
	let borrowerDriftClient: TestClient;
	let borrowerWSOLAccount: PublicKey;
	let borrowerUSDCAccount: PublicKey;

	const usdcAmount = new BN(10 * 10 ** 6);
	const largeUsdcAmount = new BN(10_000 * 10 ** 6);
	const solAmount = new BN(1 * 10 ** 9);
	const borrowAmount = new BN(5 * 10 ** 6); // 5 USDC

	const USDC_MARKET_INDEX = 0;
	const SOL_MARKET_INDEX = 1;

	let marketIndexes: number[];
	let spotMarketIndexes: number[];
	let oracleInfos: OracleInfo[];

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
		await mockUserUSDCAccount(usdcMint, largeUsdcAmount, bankrunContextWrapper);

		solOracle = await mockOracleNoProgram(bankrunContextWrapper, 30);

		marketIndexes = [];
		spotMarketIndexes = [0, 1];
		oracleInfos = [{ publicKey: solOracle, source: OracleSource.PYTH_LAZER }];

		admin = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: bankrunContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
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

		await admin.initialize(usdcMint.publicKey, true);
		await admin.subscribe();

		const optimalUtilization = SPOT_MARKET_RATE_PRECISION.div(
			new BN(2)
		).toNumber();
		const optimalRate = SPOT_MARKET_RATE_PRECISION.mul(new BN(20)).toNumber();
		const maxRate = SPOT_MARKET_RATE_PRECISION.mul(new BN(50)).toNumber();
		const fullWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();

		// USDC market (quote): all weights 1.0
		await admin.initializeSpotMarket(
			usdcMint.publicKey,
			optimalUtilization,
			optimalRate,
			maxRate,
			PublicKey.default,
			OracleSource.QUOTE_ASSET,
			fullWeight,
			fullWeight,
			fullWeight,
			fullWeight
		);
		await admin.updateWithdrawGuardThreshold(
			USDC_MARKET_INDEX,
			new BN(10 ** 10).mul(QUOTE_PRECISION)
		);

		// SOL market: 0.8 initial asset weight, 1.2 initial liability weight
		await admin.initializeSpotMarket(
			NATIVE_MINT,
			optimalUtilization,
			optimalRate,
			maxRate,
			solOracle,
			OracleSource.PYTH_LAZER,
			SPOT_MARKET_WEIGHT_PRECISION.mul(new BN(8)).div(new BN(10)).toNumber(),
			SPOT_MARKET_WEIGHT_PRECISION.mul(new BN(9)).div(new BN(10)).toNumber(),
			SPOT_MARKET_WEIGHT_PRECISION.mul(new BN(12)).div(new BN(10)).toNumber(),
			SPOT_MARKET_WEIGHT_PRECISION.mul(new BN(11)).div(new BN(10)).toNumber()
		);
		await admin.updateWithdrawGuardThreshold(
			SOL_MARKET_INDEX,
			new BN(10 ** 10).mul(QUOTE_PRECISION)
		);
		await admin.fetchAccounts();
	});

	after(async () => {
		await admin.unsubscribe();
		await eventSubscriber.unsubscribe();
		await lenderDriftClient.unsubscribe();
		await borrowerDriftClient.unsubscribe();
	});

	it('no collateral usage from a plain deposit', async () => {
		[lenderDriftClient, lenderUSDCAccount] = await createUserWithUSDCAccount(
			bankrunContextWrapper,
			usdcMint,
			chProgram,
			usdcAmount,
			marketIndexes,
			spotMarketIndexes,
			oracleInfos,
			bulkAccountLoader
		);
		await lenderDriftClient.deposit(
			usdcAmount,
			USDC_MARKET_INDEX,
			lenderUSDCAccount
		);

		[borrowerDriftClient, borrowerWSOLAccount, borrowerUSDCAccount] =
			await createUserWithUSDCAndWSOLAccount(
				bankrunContextWrapper,
				usdcMint,
				chProgram,
				solAmount,
				ZERO,
				marketIndexes,
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);
		await borrowerDriftClient.deposit(
			solAmount,
			SOL_MARKET_INDEX,
			borrowerWSOLAccount
		);

		await admin.fetchAccounts();
		// neither depositor has a borrow, so nothing is "in use" as collateral
		assert(
			admin
				.getSpotMarketAccount(SOL_MARKET_INDEX)
				.totalUsageAsCollateral.eq(ZERO)
		);
		assert(
			admin
				.getSpotMarketAccount(USDC_MARKET_INDEX)
				.totalUsageAsCollateral.eq(ZERO)
		);
	});

	it('borrowing marks a proportional amount of the collateral in use', async () => {
		// refresh so the client sees the SOL deposit when picking the collateral
		// markets to mark writable
		await borrowerDriftClient.fetchAccounts();
		// borrow 5 USDC against 1 SOL ($30, weighted $24 at 0.8):
		// utilization ~= 5/24, so a fraction of the SOL deposit is in use
		await borrowerDriftClient.withdraw(
			borrowAmount,
			USDC_MARKET_INDEX,
			borrowerUSDCAccount
		);

		await admin.fetchAccounts();
		const solUsage =
			admin.getSpotMarketAccount(SOL_MARKET_INDEX).totalUsageAsCollateral;

		// some of the SOL is committed, but never more than the deposit
		assert(solUsage.gt(ZERO));
		assert(solUsage.lte(solAmount));
		// ~0.2 SOL expected; bound loosely to stay precision-agnostic
		assert(solUsage.gt(solAmount.div(new BN(10)))); // > 0.1 SOL
		assert(solUsage.lt(solAmount.div(new BN(2)))); // < 0.5 SOL

		// the borrowed asset is a liability, not collateral, so it stays zero
		assert(
			admin
				.getSpotMarketAccount(USDC_MARKET_INDEX)
				.totalUsageAsCollateral.eq(ZERO)
		);
	});

	it('repaying the borrow releases the collateral', async () => {
		await borrowerDriftClient.fetchAccounts();
		// deposit the borrowed USDC back to fully repay
		await borrowerDriftClient.deposit(
			borrowAmount,
			USDC_MARKET_INDEX,
			borrowerUSDCAccount
		);
		await sleep(100);

		await admin.fetchAccounts();
		assert(
			admin
				.getSpotMarketAccount(SOL_MARKET_INDEX)
				.totalUsageAsCollateral.eq(ZERO)
		);
	});
});
