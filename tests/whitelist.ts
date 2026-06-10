import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';
import { BASE_PRECISION, BN, OracleSource } from '../sdk';

import { Program } from '@coral-xyz/anchor';

import {
	Keypair,
	PublicKey,
	SystemProgram,
	Transaction,
} from '@solana/web3.js';
import {
	MINT_SIZE,
	TOKEN_PROGRAM_ID,
	createAssociatedTokenAccountIdempotentInstruction,
	createInitializeMint2Instruction,
	createMintToInstruction,
	getAssociatedTokenAddressSync,
} from '@solana/spl-token';

import { TestClient, PRICE_PRECISION } from '../sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';

describe('whitelist', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let bulkAccountLoader: TestBulkAccountLoader;

	let contextWrapper: LiteSVMContextWrapper;

	let driftClient: TestClient;

	let userAccountPublicKey: PublicKey;

	let usdcMint;
	let userUSDCAccount;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(
		5 * BASE_PRECISION.toNumber()
	).mul(mantissaSqrtScale);
	const ammInitialBaseAssetReserve = new anchor.BN(
		5 * BASE_PRECISION.toNumber()
	).mul(mantissaSqrtScale);

	const usdcAmount = new BN(10 * 10 ** 6);

	let whitelistMint: PublicKey;

	before(async () => {
		const context = await startLiteSVM('', [], []);

		contextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(contextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			contextWrapper
		);

		const solUsd = await mockOracleNoProgram(contextWrapper, 1);
		const periodicity = new BN(60 * 60); // 1 HOUR

		driftClient = new TestClient({
			connection: contextWrapper.connection.toConnection(),
			wallet: contextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }],
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await driftClient.initialize(usdcMint.publicKey, true);
		await driftClient.subscribe();
		await initializeQuoteSpotMarket(driftClient, usdcMint.publicKey);

		await driftClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity
		);

		const keypair = Keypair.generate();
		const transaction = new Transaction().add(
			SystemProgram.createAccount({
				fromPubkey: contextWrapper.provider.wallet.publicKey,
				newAccountPubkey: keypair.publicKey,
				space: MINT_SIZE,
				lamports: 10_000_000_000,
				programId: TOKEN_PROGRAM_ID,
			}),
			createInitializeMint2Instruction(
				keypair.publicKey,
				0,
				contextWrapper.provider.wallet.publicKey,
				contextWrapper.provider.wallet.publicKey,
				TOKEN_PROGRAM_ID
			)
		);

		await contextWrapper.sendTransaction(transaction, [keypair]);

		whitelistMint = keypair.publicKey;
	});

	after(async () => {
		await driftClient.unsubscribe();
	});

	it('Assert whitelist mint null', async () => {
		const state = driftClient.getStateAccount();
		assert(state.whitelistMint.equals(PublicKey.default));
	});

	it('enable whitelist mint', async () => {
		await driftClient.updateWhitelistMint(whitelistMint);
		const state = driftClient.getStateAccount();
		console.assert(state.whitelistMint.equals(whitelistMint));
	});

	it('block initialize user', async () => {
		try {
			[, userAccountPublicKey] =
				await driftClient.initializeUserAccountAndDepositCollateral(
					usdcAmount,
					userUSDCAccount.publicKey
				);
		} catch (e) {
			console.log(e);
			return;
		}
		assert(false);
	});

	it('successful initialize user', async () => {
		const whitelistMintAta = getAssociatedTokenAddressSync(
			whitelistMint,
			contextWrapper.provider.wallet.publicKey
		);
		const ix = createAssociatedTokenAccountIdempotentInstruction(
			contextWrapper.context.payer.publicKey,
			whitelistMintAta,
			contextWrapper.provider.wallet.publicKey,
			whitelistMint
		);
		const mintToIx = createMintToInstruction(
			whitelistMint,
			whitelistMintAta,
			contextWrapper.provider.wallet.publicKey,
			1
		);
		await contextWrapper.sendTransaction(
			new Transaction().add(ix, mintToIx)
		);

		[, userAccountPublicKey] =
			await driftClient.initializeUserAccountAndDepositCollateral(
				usdcAmount,
				userUSDCAccount.publicKey
			);

		const user: any = await driftClient.program.account.user.fetch(
			userAccountPublicKey
		);

		assert.ok(
			user.authority.equals(contextWrapper.provider.wallet.publicKey)
		);
	});

	it('disable whitelist mint', async () => {
		await driftClient.updateWhitelistMint(PublicKey.default);
		const state = driftClient.getStateAccount();
		console.assert(state.whitelistMint.equals(PublicKey.default));
	});
});
