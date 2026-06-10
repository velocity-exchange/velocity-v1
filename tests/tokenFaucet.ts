import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';
import { Program } from '@coral-xyz/anchor';
import { TestClient, TokenFaucet } from '../sdk/src';
import { BN } from '../sdk';
import { Keypair, PublicKey } from '@solana/web3.js';
import { initializeQuoteSpotMarket, mockUSDCMint } from './testHelpers';
import {
	createAssociatedTokenAccountIdempotentInstruction,
	getAssociatedTokenAddressSync,
	unpackAccount,
	unpackMint,
} from '@solana/spl-token';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';

describe('token faucet', () => {
	const program = anchor.workspace.TokenFaucet as Program;

	let tokenFaucet: TokenFaucet;

	let usdcMint: Keypair;

	const chProgram = anchor.workspace.Drift as Program;
	let driftClient: TestClient;

	let bulkAccountLoader: TestBulkAccountLoader;

	let contextWrapper: LiteSVMContextWrapper;

	const amount = new BN(10 * 10 ** 6);

	before(async () => {
		const context = await startLiteSVM('', [], []);

		contextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		driftClient = new TestClient({
			connection: contextWrapper.connection.toConnection(),
			wallet: contextWrapper.provider.wallet,
			programID: chProgram.programId,
			spotMarketIndexes: [],
			perpMarketIndexes: [],
			subAccountIds: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		usdcMint = await mockUSDCMint(contextWrapper);

		tokenFaucet = new TokenFaucet(
			contextWrapper.connection.toConnection(),
			contextWrapper.provider.wallet,
			program.programId,
			usdcMint.publicKey,
			undefined,
			contextWrapper
		);
	});

	after(async () => {
		await driftClient.unsubscribe();
	});

	it('Initialize State', async () => {
		await tokenFaucet.initialize();
		const state: any = await tokenFaucet.fetchState();

		assert.ok(
			state.coldAdmin.equals(contextWrapper.provider.wallet.publicKey)
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

		const mintInfoRaw = await contextWrapper.connection.getAccountInfo(
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
				contextWrapper.provider.wallet.publicKey,
				ata,
				keyPair.publicKey,
				tokenFaucet.mint
			);
		await contextWrapper.sendTransaction(
			new anchor.web3.Transaction().add(userTokenAccountIx)
		);
		let userTokenAccountInfoRaw =
			await contextWrapper.connection.getAccountInfo(ata);
		let userTokenAccountInfo = unpackAccount(ata, userTokenAccountInfoRaw);
		try {
			await tokenFaucet.mintToUser(userTokenAccountInfo.address, amount);
		} catch (e) {
			console.error(e);
		}
		userTokenAccountInfoRaw =
			await contextWrapper.connection.getAccountInfo(ata);
		userTokenAccountInfo = unpackAccount(ata, userTokenAccountInfoRaw);
		assert.ok(new BN(userTokenAccountInfo.amount.toString()).eq(amount));
	});

	it('initialize user for dev net', async () => {
		const state: any = await tokenFaucet.fetchState();

		await driftClient.initialize(state.mint, false);
		await driftClient.subscribe();
		await initializeQuoteSpotMarket(driftClient, usdcMint.publicKey);
		await driftClient.initializeUserAccountForDevnet(
			0,
			'crisp',
			0,
			tokenFaucet,
			amount
		);

		assert(driftClient.getQuoteAssetTokenAmount().eq(amount));
	});

	it('transfer mint authority back', async () => {
		await tokenFaucet.transferMintAuthority();
		const mintInfoRaw = await contextWrapper.connection.getAccountInfo(
			tokenFaucet.mint
		);
		const mintInfo = unpackMint(tokenFaucet.mint, mintInfoRaw);
		assert.ok(
			contextWrapper.provider.wallet.publicKey.equals(
				mintInfo.mintAuthority
			)
		);
	});
});
