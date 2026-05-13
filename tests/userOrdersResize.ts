import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	AccountInfo,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
} from '@solana/web3.js';

import {
	BN,
	PRICE_PRECISION,
	TestClient,
	User,
	OracleSource,
	PYTH_LAZER_STORAGE_ACCOUNT_KEY,
	PTYH_LAZER_PROGRAM_ID,
} from '../sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	sleep,
} from './testHelpers';
import { PEG_PRECISION } from '../sdk/src';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../sdk/src/bankrun/bankrunConnection';
import dotenv from 'dotenv';
import { PYTH_STORAGE_DATA } from './pythLazerData';
dotenv.config();

const PYTH_STORAGE_ACCOUNT_INFO: AccountInfo<Buffer> = {
	executable: false,
	lamports: LAMPORTS_PER_SOL,
	owner: new PublicKey(PTYH_LAZER_PROGRAM_ID),
	rentEpoch: 0,
	data: Buffer.from(PYTH_STORAGE_DATA, 'base64'),
};

// User layout: [8B disc][1296B header][orders_len * 96B Order]
const USER_FIXED_LEN = 8 + 1296;
const ORDER_LEN = 96;
const DEFAULT_USER_ORDERS = 8;
const MAX_USER_ORDERS = 128;

function expectedUserAccountSize(numOrders: number): number {
	return USER_FIXED_LEN + numOrders * ORDER_LEN;
}

describe('resize user orders', () => {
	const chProgram = anchor.workspace.Drift as Program;

	let driftClient: TestClient;
	let driftClientUser: User;

	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	let usdcMint: Keypair;
	let userUSDCAccount: { publicKey: anchor.web3.PublicKey };

	const usdcAmount = new BN(100 * 10 ** 6);

	let solUsd: anchor.web3.PublicKey;
	let marketIndexes: number[];
	let spotMarketIndexes: number[];
	let oracleInfos: { publicKey: anchor.web3.PublicKey; source: OracleSource }[];

	before(async () => {
		const context = await startAnchor(
			'',
			[],
			[
				{
					address: PYTH_LAZER_STORAGE_ACCOUNT_KEY,
					info: PYTH_STORAGE_ACCOUNT_INFO,
				},
			]
		);

		// @ts-ignore
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

		solUsd = await mockOracleNoProgram(bankrunContextWrapper, 32.821);

		marketIndexes = [0];
		spotMarketIndexes = [0, 1];
		oracleInfos = [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }];

		driftClient = new TestClient({
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
		await driftClient.initialize(usdcMint.publicKey, true);
		await driftClient.subscribe();
		await initializeQuoteSpotMarket(driftClient, usdcMint.publicKey);

		await driftClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			new BN(0),
			new BN(33 * PEG_PRECISION.toNumber())
		);

		await driftClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);

		driftClientUser = new User({
			driftClient,
			userAccountPublicKey: await driftClient.getUserAccountPublicKey(),
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await driftClientUser.subscribe();
	});

	after(async () => {
		await driftClient.unsubscribe();
		await driftClientUser.unsubscribe();
	});

	async function getUserAccountDataLen(): Promise<number> {
		const userPda = await driftClient.getUserAccountPublicKey();
		const info = await bankrunContextWrapper.connection.getAccountInfo(userPda);
		assert.ok(info, 'user account missing');
		return info!.data.length;
	}

	it('initializes user with DEFAULT_USER_ORDERS slots', async () => {
		const len = await getUserAccountDataLen();
		assert.equal(
			len,
			expectedUserAccountSize(DEFAULT_USER_ORDERS),
			`init account size should be ${expectedUserAccountSize(
				DEFAULT_USER_ORDERS
			)}`
		);
	});

	it('grows to 16 orders', async () => {
		await driftClient.resizeUserOrders(16);
		await sleep(200);
		const len = await getUserAccountDataLen();
		assert.equal(len, expectedUserAccountSize(16));
	});

	it('grows to MAX_USER_ORDERS in two steps', async () => {
		// Solana's MAX_PERMITTED_DATA_INCREASE caps a single realloc at 10_240B.
		// (128 - 16) * 96B = 10_752B > 10_240B → must split.
		// Step from 16 → 100 orders: delta = (100-16) * 96 = 8_064B (under cap).
		await driftClient.resizeUserOrders(100);
		await sleep(200);
		assert.equal(await getUserAccountDataLen(), expectedUserAccountSize(100));

		// Step from 100 → 128 orders: delta = (128-100) * 96 = 2_688B (under cap).
		await driftClient.resizeUserOrders(MAX_USER_ORDERS);
		await sleep(200);
		assert.equal(
			await getUserAccountDataLen(),
			expectedUserAccountSize(MAX_USER_ORDERS)
		);
	});

	// Anchor error codes from programs/drift/src/error.rs (decimal → hex).
	const ERR_INVALID_USER_ORDERS_RESIZE = '0x18cf'; // 6351
	// Anchor's declarative `realloc` shrinks the account before the handler
	// body runs, so by the time `load_user_mut!` reads `orders_len` it exceeds
	// the new tail capacity and fails with UnableToLoadAccountLoader. The tx
	// still reverts atomically — the shrink is rejected — but the surfaced
	// code is the inner load failure rather than the grow-only check.
	const ERR_UNABLE_TO_LOAD_ACCOUNT_LOADER = '0x17b1'; // 6065

	it('rejects shrink (grow-only)', async () => {
		try {
			await driftClient.resizeUserOrders(8);
			assert.fail('expected resize to fail');
		} catch (e) {
			const msg = (e as Error).toString();
			const rejected =
				msg.includes(ERR_INVALID_USER_ORDERS_RESIZE) ||
				msg.includes(ERR_UNABLE_TO_LOAD_ACCOUNT_LOADER);
			assert.ok(rejected, `expected shrink to be rejected, got: ${msg}`);
		}
		const len = await getUserAccountDataLen();
		assert.equal(
			len,
			expectedUserAccountSize(MAX_USER_ORDERS),
			'failed resize should leave size unchanged'
		);
	});

	it('rejects grow past MAX_USER_ORDERS', async () => {
		try {
			await driftClient.resizeUserOrders(MAX_USER_ORDERS + 1);
			assert.fail('expected resize to fail');
		} catch (e) {
			assert.include(
				(e as Error).toString(),
				ERR_INVALID_USER_ORDERS_RESIZE,
				`expected InvalidUserOrdersResize (${ERR_INVALID_USER_ORDERS_RESIZE}), got: ${e}`
			);
		}
	});
});
