import * as anchor from '@coral-xyz/anchor';

import { Program } from '@coral-xyz/anchor';

import { assert } from 'chai';

import {
	LAMPORTS_PER_SOL,
	Keypair,
	PublicKey,
	SYSVAR_INSTRUCTIONS_PUBKEY,
	Transaction,
} from '@solana/web3.js';

import {
	BN,
	TestClient,
	OracleSource,
	OracleInfo,
	PERCENTAGE_PRECISION,
} from '../../packages/sdk/src';

import {
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

// Regression guard for `liquidate_spot_with_swap`: the begin handler introspects
// the matching end instruction and binds accounts by hard-coded index, with the
// swap (remaining) accounts assumed to start right after the fixed accounts.
// `LiquidateSpotWithSwap` has 12 fixed accounts (indexes 0..=11), the last of
// them the liquidator_stats the equity-breaker gate reads; a stale guard
// previously used the old 13-account Drift layout, which made the begin/end
// account-count check unsatisfiable and bricked the route.
// This test builds begin + end through the generated SDK/IDL and asserts the account
// order the program guard depends on, so any future struct reshuffle that desyncs the
// guard is caught here rather than on-chain.
// InvalidLiquidateSpotWithSwap
const INVALID_LIQUIDATE_SPOT_WITH_SWAP_HEX = '0x18a4';

describe('liquidate spot with swap account bindings', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let liquidatorClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let solOracle: PublicKey;
	let usdcMint;

	let userClient: TestClient;
	let liquidatorUSDC: PublicKey;
	let liquidatorWSOL: PublicKey;
	let liquidatorKeypair: Keypair;
	let userUSDCAccount;
	let userWSOLAccount: PublicKey;

	const usdcAmount = new BN(200 * 10 ** 6).muln(10);
	const solAmount = new BN(10 * 10 ** 9).muln(10);

	const spotMarketIndexes = [0, 1];

	// Fixed-account indexes the begin handler's introspection guard binds against
	// (see programs/velocity/src/instructions/keeper.rs). These mirror the IDL
	// account order of `LiquidateSpotWithSwap` and must stay in sync with it.
	const AUTHORITY_IX_INDEX = 1;
	const LIQUIDATOR_IX_INDEX = 2;
	const USER_IX_INDEX = 3;
	const LIABILITY_VAULT_IX_INDEX = 4;
	const ASSET_VAULT_IX_INDEX = 5;
	const LIABILITY_TOKEN_ACCOUNT_IX_INDEX = 6;
	const ASSET_TOKEN_ACCOUNT_IX_INDEX = 7;
	const TOKEN_PROGRAM_IX_INDEX = 8;
	const VELOCITY_SIGNER_IX_INDEX = 9;
	const INSTRUCTIONS_SYSVAR_IX_INDEX = 10;
	// last, not beside the liquidator as in the direct liquidation contexts,
	// so that adding it renumbered none of the accounts above
	const LIQUIDATOR_STATS_IX_INDEX = 11;
	const NUM_FIXED_ACCOUNTS = 12;

	before(async () => {
		const context = await startAnchor('', [], []);

		bankrunContextWrapper = new BankrunContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			bankrunContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(bankrunContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper
		);
		userWSOLAccount = await createWSolTokenAccountForUser(
			bankrunContextWrapper,
			// @ts-ignore
			bankrunContextWrapper.provider.wallet,
			solAmount
		);

		solOracle = await mockOracleNoProgram(bankrunContextWrapper, 100);

		const oracleInfos: OracleInfo[] = [
			{ publicKey: solOracle, source: OracleSource.PYTH_LAZER },
		];

		userClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: bankrunContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [],
			spotMarketIndexes,
			subAccountIds: [],
			oracleInfos,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await userClient.initialize(usdcMint.publicKey, true);
		await userClient.subscribe();
		await userClient.initializeUserAccount();

		const oracleGuardrails = await userClient.getStateAccount()
			.oracleGuardRails;
		oracleGuardrails.validity.tooVolatileRatio = new BN(10000);
		oracleGuardrails.priceDivergence.oracleTwap5MinPercentDivergence = new BN(
			100
		).mul(PERCENTAGE_PRECISION);
		await userClient.updateOracleGuardRails(oracleGuardrails);

		await initializeQuoteSpotMarket(userClient, usdcMint.publicKey);
		await initializeSolSpotMarket(userClient, solOracle);

		[liquidatorClient, liquidatorWSOL, liquidatorUSDC, liquidatorKeypair] =
			await createUserWithUSDCAndWSOLAccount(
				bankrunContextWrapper,
				usdcMint,
				chProgram,
				solAmount,
				usdcAmount,
				[],
				spotMarketIndexes,
				oracleInfos,
				bulkAccountLoader
			);

		await bankrunContextWrapper.fundKeypair(
			liquidatorKeypair,
			10 * LAMPORTS_PER_SOL
		);

		// A liquidatable user, so `liquidate_spot_with_swap_begin` gets past the
		// controller and reaches the begin/end introspection guard the second
		// test exercises. The liquidator supplies the sol the user borrows.
		await liquidatorClient.deposit(solAmount, 1, liquidatorWSOL);
		await userClient.deposit(usdcAmount, 0, userUSDCAccount.publicKey);
		await userClient.withdraw(new BN(95 * 10 ** 8), 1, userWSOLAccount);

		// sol doubles: the borrow outgrows the usdc collateral backing it
		await setFeedPriceNoProgram(bankrunContextWrapper, 200, solOracle);
	});

	after(async () => {
		await userClient.unsubscribe();
		await liquidatorClient.unsubscribe();
	});

	it('begin/end account order matches the program guard indexes', async () => {
		const assetMarketIndex = 0; // USDC
		const liabilityMarketIndex = 1; // SOL

		const { beginSwapIx, endSwapIx } =
			await liquidatorClient.getLiquidateSpotWithSwapIx({
				swapAmount: new BN(10 ** 6),
				assetMarketIndex,
				liabilityMarketIndex,
				assetTokenAccount: liquidatorUSDC,
				liabilityTokenAccount: liquidatorWSOL,
				userAccount: userClient.getUserAccount(),
				userAccountPublicKey: await userClient.getUserAccountPublicKey(),
			});

		const authority = liquidatorClient.wallet.publicKey;
		const liquidator = await liquidatorClient.getUserAccountPublicKey();
		const user = await userClient.getUserAccountPublicKey();
		const liabilitySpotMarket =
			liquidatorClient.getSpotMarketAccountOrThrow(liabilityMarketIndex);
		const assetSpotMarket =
			liquidatorClient.getSpotMarketAccountOrThrow(assetMarketIndex);

		// The guard runs on the BEGIN instruction's accounts (ctx) against the
		// END instruction's accounts (introspected). Both are built from the same
		// `LiquidateSpotWithSwap` struct, so they must produce an identical layout.
		for (const ix of [beginSwapIx, endSwapIx]) {
			assert.ok(
				ix.keys[AUTHORITY_IX_INDEX].pubkey.equals(authority),
				'authority binding index mismatch'
			);
			assert.ok(
				ix.keys[LIQUIDATOR_IX_INDEX].pubkey.equals(liquidator),
				'liquidator binding index mismatch'
			);
			assert.ok(
				ix.keys[USER_IX_INDEX].pubkey.equals(user),
				'user binding index mismatch'
			);
			assert.ok(
				ix.keys[LIABILITY_VAULT_IX_INDEX].pubkey.equals(
					liabilitySpotMarket.vault
				),
				'liability_spot_market_vault binding index mismatch'
			);
			assert.ok(
				ix.keys[ASSET_VAULT_IX_INDEX].pubkey.equals(assetSpotMarket.vault),
				'asset_spot_market_vault binding index mismatch'
			);
			assert.ok(
				ix.keys[LIABILITY_TOKEN_ACCOUNT_IX_INDEX].pubkey.equals(liquidatorWSOL),
				'liability_token_account binding index mismatch'
			);
			assert.ok(
				ix.keys[ASSET_TOKEN_ACCOUNT_IX_INDEX].pubkey.equals(liquidatorUSDC),
				'asset_token_account binding index mismatch'
			);
		}

		// Begin and end must carry the same accounts (the guard compares them 1:1),
		// and the remaining (swap) accounts must start right after the fixed block.
		assert.equal(
			beginSwapIx.keys.length,
			endSwapIx.keys.length,
			'begin and end must have the same number of accounts'
		);
		assert.isAtLeast(
			beginSwapIx.keys.length,
			NUM_FIXED_ACCOUNTS,
			'expected at least the fixed accounts'
		);
		for (let i = 0; i < beginSwapIx.keys.length; i++) {
			assert.ok(
				beginSwapIx.keys[i].pubkey.equals(endSwapIx.keys[i].pubkey),
				`begin/end account mismatch at index ${i}`
			);
		}

		// The guard counts remaining accounts as `ix.accounts.len() - 12` and
		// loops from index 12, so the last fixed account must sit at index 11.
		// Pinning the tail is what catches a 13th fixed account being appended:
		// that shifts nothing at 0..=8, so every binding assertion above would
		// still pass while the program read the first swap account as a fixed
		// one and the count check went off by one.
		for (const ix of [beginSwapIx, endSwapIx]) {
			assert.ok(
				ix.keys[TOKEN_PROGRAM_IX_INDEX].pubkey.equals(
					liquidatorClient.getTokenProgramForSpotMarket(assetSpotMarket)
				),
				'token_program binding index mismatch'
			);
			assert.ok(
				ix.keys[VELOCITY_SIGNER_IX_INDEX].pubkey.equals(
					liquidatorClient.getStateAccount().signer
				),
				'velocity_signer binding index mismatch'
			);
			assert.ok(
				ix.keys[INSTRUCTIONS_SYSVAR_IX_INDEX].pubkey.equals(
					SYSVAR_INSTRUCTIONS_PUBKEY
				),
				'instructions sysvar binding index mismatch'
			);
			assert.ok(
				ix.keys[LIQUIDATOR_STATS_IX_INDEX].pubkey.equals(
					liquidatorClient.getUserStatsAccountPublicKey()
				),
				'liquidator_stats binding index mismatch'
			);
		}

		const remainingCount = beginSwapIx.keys.length - NUM_FIXED_ACCOUNTS;
		assert.isAtLeast(
			remainingCount,
			0,
			'fixed account count must not exceed total accounts'
		);
	});

	it('the program accepts the layout the sdk builds', async () => {
		// Everything above compares the built instructions against the IDL,
		// which cannot catch the program's own copy of these indexes being
		// wrong: `begin` introspects the matching `end` and compares the two
		// lists by position, and no test had ever executed that block. The
		// other suites that send this pair reject earlier, at the equity
		// breaker check or an account constraint.
		//
		// The fixture's user is genuinely liquidatable, so `begin` runs to
		// completion, introspection included. The pair still fails, at `end`
		// with InvalidSwap, because nothing swaps between the two
		// instructions and the flash loan is never repaid. That is the point:
		// reaching `end` at all means the guard accepted this account order,
		// while any mis-numbered index or an off-by-one fixed/remaining
		// boundary would have stopped it in `begin` with the error below.
		const { beginSwapIx, endSwapIx } =
			await liquidatorClient.getLiquidateSpotWithSwapIx({
				swapAmount: new BN(10 ** 6),
				assetMarketIndex: 0,
				liabilityMarketIndex: 1,
				assetTokenAccount: liquidatorUSDC,
				liabilityTokenAccount: liquidatorWSOL,
				userAccount: userClient.getUserAccount(),
				userAccountPublicKey: await userClient.getUserAccountPublicKey(),
			});

		let err: Error | undefined;
		try {
			await liquidatorClient.sendTransaction(
				new Transaction().add(beginSwapIx, endSwapIx)
			);
		} catch (e) {
			err = e as Error;
		}

		assert(err, 'the liquidation itself is expected to fail');
		assert(
			!err.message.includes(INVALID_LIQUIDATE_SPOT_WITH_SWAP_HEX),
			`the begin/end introspection guard rejected the sdk-built account order, so the program's indexes disagree with the IDL: ${err.message}`
		);
	});
});
