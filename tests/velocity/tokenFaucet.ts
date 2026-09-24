import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';
import { Program } from '@coral-xyz/anchor';
import { TestClient, TokenFaucet } from '../../packages/sdk/src';
import { BN } from '../../packages/sdk';
import { Keypair, PublicKey } from '@solana/web3.js';
import { initializeQuoteSpotMarket, mockUSDCMint } from './testHelpers';
import {
	createAssociatedTokenAccountIdempotentInstruction,
	getAssociatedTokenAddressSync,
	unpackAccount,
	unpackMint,
} from '@solana/spl-token';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('token faucet', () => {
	const program = anchor.workspace.TokenFaucet as Program;

	let tokenFaucet: TokenFaucet;

	let usdcMint: Keypair;

	const chProgram = anchor.workspace.Velocity as Program;
	let velocityClient: TestClient;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	const amount = new BN(10 * 10 ** 6);

	before(async () => {
		const context = startLiteSVM();

		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			spotMarketIndexes: [],
			perpMarketIndexes: [],
			subAccountIds: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		usdcMint = await mockUSDCMint(svmContextWrapper);

		tokenFaucet = new TokenFaucet(
			svmContextWrapper.connection.toConnection(),
			svmContextWrapper.provider.wallet,
			program.programId,
			usdcMint.publicKey,
			undefined,
			svmContextWrapper
		);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('Initialize State', async () => {
		await tokenFaucet.initialize();
		const state: any = await tokenFaucet.fetchState();

		assert.ok(
			state.coldAdmin.equals(svmContextWrapper.provider.wallet.publicKey)
		);

		const [mintAuthority, mintAuthorityNonce] =
			await PublicKey.findProgramAddress(
				[
					Buffer.from(anchor.utils.bytes.utf8.encode('mint_authority')),
					state.mint.toBuffer(),
				],
				tokenFaucet.program.programId
			);

		assert.ok(state.mintAuthority.equals(mintAuthority));
		assert.ok(mintAuthorityNonce === state.mintAuthorityNonce);

		const mintInfoRaw = await svmContextWrapper.connection.getAccountInfo(
			tokenFaucet.mint
		);
		const mintInfo = unpackMint(tokenFaucet.mint, mintInfoRaw);
		assert.ok(state.mintAuthority.equals(mintInfo.mintAuthority));
	});

	it('mint to user', async () => {
		const keyPair = new Keypair();
		const ata = getAssociatedTokenAddressSync(
			tokenFaucet.mint,
			keyPair.publicKey
		);
		const userTokenAccountIx =
			await createAssociatedTokenAccountIdempotentInstruction(
				svmContextWrapper.provider.wallet.publicKey,
				ata,
				keyPair.publicKey,
				tokenFaucet.mint
			);
		await svmContextWrapper.sendTransaction(
			new anchor.web3.Transaction().add(userTokenAccountIx)
		);
		let userTokenAccountInfoRaw =
			await svmContextWrapper.connection.getAccountInfo(ata);
		let userTokenAccountInfo = unpackAccount(ata, userTokenAccountInfoRaw);
		try {
			await tokenFaucet.mintToUser(userTokenAccountInfo.address, amount);
		} catch (e) {
			console.error(e);
		}
		userTokenAccountInfoRaw =
			await svmContextWrapper.connection.getAccountInfo(ata);
		userTokenAccountInfo = unpackAccount(ata, userTokenAccountInfoRaw);
		assert.ok(new BN(userTokenAccountInfo.amount.toString()).eq(amount));
	});

	it('initialize user for dev net', async () => {
		const state: any = await tokenFaucet.fetchState();

		await velocityClient.initialize(state.mint, false);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.initializeUserAccountForDevnet(
			0,
			'crisp',
			0,
			tokenFaucet,
			amount
		);

		assert(velocityClient.getQuoteAssetTokenAmount().eq(amount));
	});

	it('transfer mint authority back', async () => {
		await tokenFaucet.transferMintAuthority();
		const mintInfoRaw = await svmContextWrapper.connection.getAccountInfo(
			tokenFaucet.mint
		);
		const mintInfo = unpackMint(tokenFaucet.mint, mintInfoRaw);
		assert.ok(
			svmContextWrapper.provider.wallet.publicKey.equals(
				mintInfo.mintAuthority
			)
		);
	});
});
