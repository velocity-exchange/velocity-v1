import * as anchor from '@coral-xyz/anchor';
import { expect } from 'chai';

import { Program, Wallet } from '@coral-xyz/anchor';

import { Keypair, PublicKey } from '@solana/web3.js';

import {
	BN,
	HotRole,
	TestClient,
	decodeUser,
	getTokenAmount,
	getSignedTokenAmount,
} from '../../packages/sdk/src';

import {
	createFundedKeyPair,
	initializeQuoteSpotMarket,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import dotenv from 'dotenv';
dotenv.config();

describe('account extension', () => {
	const chProgram = anchor.workspace.Velocity as Program;
	let svmContextWrapper: LiteSVMContextWrapper;
	let bulkAccountLoader: TestBulkAccountLoader;

	let velocityClient: TestClient;
	let usdcAccount: Keypair;
	let userAccountPublicKey: PublicKey;

	let hotKeyPair: Keypair;
	let hotVelocityClient: TestClient;

	let usdcMint;
	const usdcAmount = new BN(100 * 10 ** 6);
	const depositAmount = new BN(50 * 10 ** 6);

	const EXTRA_BYTES = 128;
	let originalUserAccountSize: number;

	before(async () => {
		const context = startLiteSVM();

		// @ts-ignore
		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		usdcAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			subAccountIds: [],
			perpMarketIndexes: [],
			spotMarketIndexes: [0],
			oracleInfos: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		[, userAccountPublicKey] =
			await velocityClient.initializeUserAccountAndDepositCollateral(
				depositAmount,
				usdcAccount.publicKey
			);

		hotKeyPair = await createFundedKeyPair(svmContextWrapper);
		hotVelocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: new Wallet(hotKeyPair),
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			subAccountIds: [],
			perpMarketIndexes: [],
			spotMarketIndexes: [0],
			oracleInfos: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await hotVelocityClient.subscribe();
	});

	after(async () => {
		await velocityClient.unsubscribe();
		await hotVelocityClient.unsubscribe();
	});

	it('extend_account_devnet grows a user account and zero-fills the tail', async () => {
		const before = await svmContextWrapper.connection.getAccountInfo(
			userAccountPublicKey
		);
		originalUserAccountSize = before.data.length;

		await velocityClient.extendAccountDevnet(
			userAccountPublicKey,
			originalUserAccountSize + EXTRA_BYTES
		);

		const after = await svmContextWrapper.connection.getAccountInfo(
			userAccountPublicKey
		);
		expect(after.data.length).to.equal(originalUserAccountSize + EXTRA_BYTES);
		expect(
			after.data.subarray(originalUserAccountSize).every((b) => b === 0)
		).to.equal(true);
		// prefix untouched
		expect(
			after.data.subarray(0, originalUserAccountSize).equals(before.data)
		).to.equal(true);

		// The payer covered rent for the added bytes. The runtime rejects the
		// resize transaction if the account drops below rent exemption.
		expect(after.lamports).to.be.gt(before.lamports);
	});

	it('clients decode the extended account', async () => {
		const info = await svmContextWrapper.connection.getAccountInfo(
			userAccountPublicKey
		);

		const anchorDecoded = velocityClient.program.coder.accounts.decode(
			'user',
			Buffer.from(info.data)
		);
		expect(
			anchorDecoded.authority.equals(velocityClient.wallet.publicKey)
		).to.equal(true);

		const customDecoded = decodeUser(Buffer.from(info.data));
		expect(
			customDecoded.authority.equals(velocityClient.wallet.publicKey)
		).to.equal(true);
	});

	it('program still operates on the extended account', async () => {
		// grow the spot market too, so the deposit loads two extended accounts
		const spotMarketPublicKey = velocityClient.getSpotMarketAccount(0).pubkey;
		const spotMarketInfo =
			await svmContextWrapper.connection.getAccountInfo(
				spotMarketPublicKey
			);
		await velocityClient.extendAccountDevnet(
			spotMarketPublicKey,
			spotMarketInfo.data.length + EXTRA_BYTES
		);

		await velocityClient.deposit(depositAmount, 0, usdcAccount.publicKey);
		await velocityClient.fetchAccounts();

		const spotPosition = velocityClient.getSpotPosition(0);
		const spotMarket = velocityClient.getSpotMarketAccount(0);
		const balance = getSignedTokenAmount(
			getTokenAmount(
				spotPosition.scaledBalance,
				spotMarket,
				spotPosition.balanceType
			),
			spotPosition.balanceType
		);
		expect(balance.toString()).to.equal(usdcAmount.toString());
	});

	it('extend_account is a no-op for an account at (or beyond) target size', async () => {
		const before = await svmContextWrapper.connection.getAccountInfo(
			userAccountPublicKey
		);
		await velocityClient.extendAccount(userAccountPublicKey);
		const after = await svmContextWrapper.connection.getAccountInfo(
			userAccountPublicKey
		);
		expect(after.data.length).to.equal(before.data.length);
	});

	it('extend_account_devnet rejects shrinking', async () => {
		let failed = false;
		try {
			await velocityClient.extendAccountDevnet(
				userAccountPublicKey,
				originalUserAccountSize
			);
		} catch (e) {
			failed = true;
		}
		expect(failed).to.equal(true);
	});

	it('extend_account rejects non-velocity accounts', async () => {
		let failed = false;
		try {
			await velocityClient.extendAccount(usdcMint.publicKey);
		} catch (e) {
			failed = true;
		}
		expect(failed).to.equal(true);
	});

	it('extend_account requires the AccountExtension role', async () => {
		// unassigned role, non-admin signer -> rejected
		let failed = false;
		try {
			await hotVelocityClient.extendAccount(userAccountPublicKey);
		} catch (e) {
			failed = true;
		}
		expect(failed).to.equal(true);

		// The warm admin assigns the hot key. The same signer then passes, and the
		// instruction does nothing because the account is already at size.
		await velocityClient.updateHotAdmin(
			HotRole.AccountExtension,
			hotKeyPair.publicKey
		);
		await velocityClient.fetchAccounts();
		expect(
			velocityClient
				.getStateAccount()
				.hotAccountExtension.equals(hotKeyPair.publicKey)
		).to.equal(true);
		await hotVelocityClient.extendAccount(userAccountPublicKey);
	});
});
