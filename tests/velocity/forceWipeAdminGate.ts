import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';
import { Program } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import { TOKEN_PROGRAM_ID } from '@solana/spl-token';

import {
	TestClient,
	BN,
	OracleSource,
	MarketStatus,
	SPOT_MARKET_RATE_PRECISION,
	SPOT_MARKET_WEIGHT_PRECISION,
	getUserStatsAccountPublicKey,
	getVelocitySignerPublicKey,
} from '../../packages/sdk/src';

import {
	createUserWithUSDCAccount,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('force wipe admin gate', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let svmContextWrapper: LiteSVMContextWrapper;
	let bulkAccountLoader: TestBulkAccountLoader;
	let usdcMint;

	let admin: TestClient;
	let user: TestClient;

	const usdcAmount = new BN(10 * 10 ** 6);

	before(async () => {
		svmContextWrapper = new LiteSVMContextWrapper(startLiteSVM());
		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		await mockUserUSDCAccount(usdcMint, usdcAmount, svmContextWrapper);

		admin = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await admin.initialize(usdcMint.publicKey, true);
		await admin.subscribe();

		await admin.initializeSpotMarket(
			usdcMint.publicKey,
			SPOT_MARKET_RATE_PRECISION.div(new BN(2)).toNumber(),
			SPOT_MARKET_RATE_PRECISION.mul(new BN(20)).toNumber(),
			SPOT_MARKET_RATE_PRECISION.mul(new BN(50)).toNumber(),
			PublicKey.default,
			OracleSource.QUOTE_ASSET,
			SPOT_MARKET_WEIGHT_PRECISION.toNumber(),
			SPOT_MARKET_WEIGHT_PRECISION.toNumber(),
			SPOT_MARKET_WEIGHT_PRECISION.toNumber(),
			SPOT_MARKET_WEIGHT_PRECISION.toNumber()
		);
		await admin.updateSpotMarketStatus(0, MarketStatus.ACTIVE);

		let usdcAccount: PublicKey;
		[user, usdcAccount] = await createUserWithUSDCAccount(
			svmContextWrapper,
			usdcMint,
			chProgram,
			usdcAmount,
			[],
			[0],
			[],
			bulkAccountLoader
		);
		await user.deposit(usdcAmount, 0, usdcAccount);
	});

	after(async () => {
		await admin.unsubscribe();
		await user.unsubscribe();
	});

	it('refuses a force wipe gated by an account other than State', async () => {
		const attacker = user;
		const attackerUserStats = getUserStatsAccountPublicKey(
			chProgram.programId,
			attacker.wallet.publicKey
		);
		const velocitySigner = getVelocitySignerPublicKey(chProgram.programId);
		const signerNonce = admin.getStateAccount().signerNonce;

		try {
			await attacker.program.methods
				.forceWipeAccountsDevnet(signerNonce)
				.accountsStrict({
					admin: attacker.wallet.publicKey,
					state: attackerUserStats,
					velocitySigner,
					tokenProgram: TOKEN_PROGRAM_ID,
				})
				.remainingAccounts([
					{
						pubkey: admin.getSpotMarketAccount(0).vault,
						isSigner: false,
						isWritable: true,
					},
					{ pubkey: usdcMint.publicKey, isSigner: false, isWritable: true },
				])
				.rpc();
			assert.fail('force wipe with a UserStats in the state slot succeeded');
		} catch (e) {
			assert.include(String(e), 'ConstraintSeeds');
		}

		const vault = await svmContextWrapper.connection
			.toConnection()
			.getAccountInfo(admin.getSpotMarketAccount(0).vault);
		assert.isNotNull(vault, 'the spot vault survived');
	});
});
