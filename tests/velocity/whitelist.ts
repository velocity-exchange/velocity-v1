import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';
import { BASE_PRECISION, BN, OracleSource } from '../../packages/sdk';

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

import { TestClient, PRICE_PRECISION } from '../../packages/sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('whitelist', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	let velocityClient: TestClient;

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
		const context = startLiteSVM();

		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);

		const solUsd = await mockOracleNoProgram(svmContextWrapper, 1);
		const periodicity = new BN(60 * 60); // 1 HOUR

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
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
		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);

		await velocityClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity
		);

		const keypair = Keypair.generate();
		const transaction = new Transaction().add(
			SystemProgram.createAccount({
				fromPubkey: svmContextWrapper.provider.wallet.publicKey,
				newAccountPubkey: keypair.publicKey,
				space: MINT_SIZE,
				lamports: 10_000_000_000,
				programId: TOKEN_PROGRAM_ID,
			}),
			createInitializeMint2Instruction(
				keypair.publicKey,
				0,
				svmContextWrapper.provider.wallet.publicKey,
				svmContextWrapper.provider.wallet.publicKey,
				TOKEN_PROGRAM_ID
			)
		);

		await svmContextWrapper.sendTransaction(transaction, [keypair]);

		whitelistMint = keypair.publicKey;
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('Assert whitelist mint null', async () => {
		const state = velocityClient.getStateAccount();
		assert(state.whitelistMint.equals(PublicKey.default));
	});

	it('enable whitelist mint', async () => {
		await velocityClient.updateWhitelistMint(whitelistMint);
		const state = velocityClient.getStateAccount();
		console.assert(state.whitelistMint.equals(whitelistMint));
	});

	it('block initialize user', async () => {
		try {
			[, userAccountPublicKey] =
				await velocityClient.initializeUserAccountAndDepositCollateral(
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
			svmContextWrapper.provider.wallet.publicKey
		);
		const ix = createAssociatedTokenAccountIdempotentInstruction(
			svmContextWrapper.context.payer.publicKey,
			whitelistMintAta,
			svmContextWrapper.provider.wallet.publicKey,
			whitelistMint
		);
		const mintToIx = createMintToInstruction(
			whitelistMint,
			whitelistMintAta,
			svmContextWrapper.provider.wallet.publicKey,
			1
		);
		await svmContextWrapper.sendTransaction(
			new Transaction().add(ix, mintToIx)
		);

		[, userAccountPublicKey] =
			await velocityClient.initializeUserAccountAndDepositCollateral(
				usdcAmount,
				userUSDCAccount.publicKey
			);

		const user: any = await velocityClient.program.account.user.fetch(
			userAccountPublicKey
		);

		assert.ok(
			user.authority.equals(svmContextWrapper.provider.wallet.publicKey)
		);
	});

	it('disable whitelist mint', async () => {
		await velocityClient.updateWhitelistMint(PublicKey.default);
		const state = velocityClient.getStateAccount();
		console.assert(state.whitelistMint.equals(PublicKey.default));
	});
});
