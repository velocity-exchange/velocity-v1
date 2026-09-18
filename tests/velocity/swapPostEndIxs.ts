import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import { Keypair, LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';
import { Transaction } from '@solana/web3.js';

import {
	BN,
	TestClient,
	OracleSource,
	OracleInfo,
	QUOTE_PRECISION,
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
import {
	TOKEN_PROGRAM_ID,
	createCloseAccountInstruction,
	createTransferInstruction,
} from '@solana/spl-token';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

// `begin_swap` introspects every instruction that follows it. Instructions after
// `end_swap` must be inert (no writable accounts), with a carve-out for closing
// the swap's own token accounts. These tests cover that tail of the loop; the
// swap route itself is simulated with plain token transfers so no DEX is needed.
describe('swap post-end instructions', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let makerVelocityClient: TestClient;
	let makerWSOL: PublicKey;

	let bulkAccountLoader: TestBulkAccountLoader;
	let svmContextWrapper: LiteSVMContextWrapper;

	let solOracle: PublicKey;

	let usdcMint;
	let makerUSDC;

	let takerVelocityClient: TestClient;
	let takerWSOL: PublicKey;
	let takerUSDC: PublicKey;
	let takerKeypair: Keypair;

	const usdcAmount = new BN(200 * 10 ** 6);
	const solAmount = new BN(2 * 10 ** 9);

	let marketIndexes: number[];
	let spotMarketIndexes: number[];
	let oracleInfos: OracleInfo[];

	before(async () => {
		const context = startLiteSVM();

		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		makerUSDC = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);
		makerWSOL = await createWSolTokenAccountForUser(
			svmContextWrapper,
			// @ts-ignore
			svmContextWrapper.provider.wallet,
			solAmount
		);

		solOracle = await mockOracleNoProgram(svmContextWrapper, 100);

		marketIndexes = [];
		spotMarketIndexes = [0, 1];
		oracleInfos = [{ publicKey: solOracle, source: OracleSource.PYTH_LAZER }];

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
		await makerVelocityClient.initializeUserAccount();

		await initializeQuoteSpotMarket(makerVelocityClient, usdcMint.publicKey);
		await initializeSolSpotMarket(makerVelocityClient, solOracle);
		await makerVelocityClient.updateSpotMarketStepSizeAndTickSize(
			1,
			new BN(100000000),
			new BN(100)
		);
		await makerVelocityClient.updateSpotAuctionDuration(0);

		[takerVelocityClient, takerWSOL, takerUSDC, takerKeypair] =
			await createUserWithUSDCAndWSOLAccount(
				svmContextWrapper,
				usdcMint,
				chProgram,
				solAmount,
				usdcAmount,
				[],
				[0, 1],
				[
					{
						publicKey: solOracle,
						source: OracleSource.PYTH_LAZER,
					},
				],
				bulkAccountLoader
			);

		await svmContextWrapper.fundKeypair(takerKeypair, 10 * LAMPORTS_PER_SOL);
		await takerVelocityClient.deposit(usdcAmount, 0, takerUSDC);
	});

	after(async () => {
		await takerVelocityClient.unsubscribe();
		await makerVelocityClient.unsubscribe();
	});

	// Simulates a route: `amountIn` leaves the in token account, 1 SOL arrives in
	// the out token account, so `end_swap` sees a completed swap to reconcile.
	const buildSwapIxs = async (amountIn: BN) => {
		const { beginSwapIx, endSwapIx } = await takerVelocityClient.getSwapIx({
			amountIn,
			inMarketIndex: 0,
			outMarketIndex: 1,
			inTokenAccount: takerUSDC,
			outTokenAccount: takerWSOL,
		});

		const transferIn = createTransferInstruction(
			takerUSDC,
			makerUSDC.publicKey,
			takerVelocityClient.wallet.publicKey,
			amountIn.toNumber()
		);

		const transferOut = createTransferInstruction(
			makerWSOL,
			takerWSOL,
			makerVelocityClient.wallet.publicKey,
			LAMPORTS_PER_SOL
		);

		return { beginSwapIx, endSwapIx, transferIn, transferOut };
	};

	it('rejects closing a non-swap token account after end_swap', async () => {
		const amountIn = new BN(50).mul(QUOTE_PRECISION);
		const { beginSwapIx, endSwapIx, transferIn, transferOut } =
			await buildSwapIxs(amountIn);

		const closeIx = createCloseAccountInstruction(
			makerUSDC.publicKey,
			takerVelocityClient.wallet.publicKey,
			takerVelocityClient.wallet.publicKey,
			undefined,
			TOKEN_PROGRAM_ID
		);

		const tx = new Transaction()
			.add(beginSwapIx)
			.add(transferIn)
			.add(transferOut)
			.add(endSwapIx)
			.add(closeIx);

		let failed = false;
		try {
			await takerVelocityClient.sendTransaction(tx, [
				// @ts-ignore
				makerVelocityClient.wallet.payer,
			]);
		} catch (e) {
			const err = e as Error;
			if (err.toString().includes('0x1868')) {
				failed = true;
			}
		}
		assert(failed, 'expected InvalidSwap');
	});
	it('closes the swap token account after end_swap', async () => {
		const amountIn = new BN(100).mul(QUOTE_PRECISION);
		const { beginSwapIx, endSwapIx, transferIn, transferOut } =
			await buildSwapIxs(amountIn);

		// takerUSDC is drained by transferIn, so it can be closed after end_swap
		const closeIx = createCloseAccountInstruction(
			takerUSDC,
			takerVelocityClient.wallet.publicKey,
			takerVelocityClient.wallet.publicKey,
			undefined,
			TOKEN_PROGRAM_ID
		);

		const tx = new Transaction()
			.add(beginSwapIx)
			.add(transferIn)
			.add(transferOut)
			.add(endSwapIx)
			.add(closeIx);

		const { txSig } = await takerVelocityClient.sendTransaction(tx, [
			// @ts-ignore
			makerVelocityClient.wallet.payer,
		]);

		const txLogs = await svmContextWrapper.connection.getTransaction(txSig, {
			commitment: 'confirmed',
			maxSupportedTransactionVersion: 1,
		});
		// A loop that fails to advance past the close ix exhausts the bump heap
		// instead of finishing introspection.
		assert(
			!txLogs.meta.logMessages.some((log) =>
				log.includes('memory allocation failed')
			),
			'begin_swap must not exhaust the heap walking past end_swap'
		);

		const accountInfo = await svmContextWrapper.connection.getAccountInfo(
			takerUSDC
		);
		assert(accountInfo === null, 'takerUSDC should be closed');
	});
});
