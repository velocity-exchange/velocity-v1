import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import {
	BN,
	OracleSource,
	PEG_PRECISION,
	PRICE_PRECISION,
	PYTH_LAZER_PROGRAM_ID,
	PYTH_LAZER_STORAGE_ACCOUNT_KEY,
	TestClient,
	assert,
	getPythLazerOraclePublicKey,
	isVariant,
} from '../../packages/sdk/src';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';
import { startAnchor } from 'solana-bankrun';
import { AccountInfo, LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';
import { initializeQuoteSpotMarket, mockUSDCMint } from './testHelpers';
import { PYTH_LAZER_HEX_STRING_MULTI } from './pythLazerData';
import { freshLazerSolHex, mockLazerStorageData } from './pythLazerMock';

// set up account infos to load into banks client
const PYTH_STORAGE_ACCOUNT_INFO: AccountInfo<Buffer> = {
	executable: false,
	lamports: LAMPORTS_PER_SOL,
	owner: new PublicKey(PYTH_LAZER_PROGRAM_ID),
	rentEpoch: 0,
	data: Buffer.from(mockLazerStorageData(), 'base64'),
};

describe('pyth lazer oracles', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;

	let bulkAccountLoader: TestBulkAccountLoader;

	let bankrunContextWrapper: BankrunContextWrapper;
	let usdcMint;

	const feedId = 6;

	let feedAddress: PublicKey;

	before(async () => {
		// use bankrun builtin function to start solana program test
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

		// wrap the context to use it with the test helpers
		bankrunContextWrapper = new BankrunContextWrapper(context);

		// don't use regular bulk account loader, use test
		bulkAccountLoader = new TestBulkAccountLoader(
			bankrunContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(bankrunContextWrapper);
		feedAddress = getPythLazerOraclePublicKey(chProgram.programId, feedId);

		velocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: bankrunContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [
				{
					publicKey: feedAddress,
					source: OracleSource.PYTH_LAZER,
				},
			],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();

		await velocityClient.initializePythLazerOracle(feedId);
		await velocityClient.postPythLazerOracleUpdate(
			[feedId],
			freshLazerSolHex(bankrunContextWrapper.connection.getTime())
		);

		const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
		const ammInitialQuoteAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
			mantissaSqrtScale
		);
		const ammInitialBaseAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
			mantissaSqrtScale
		);
		const periodicity = new BN(0);
		await velocityClient.initializePerpMarket(
			0,
			feedAddress,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(82 * PEG_PRECISION.toNumber()),
			OracleSource.PYTH_LAZER
		);
		await velocityClient.initializeAmmCache();

		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('init feed', async () => {
		await velocityClient.initializePythLazerOracle(1);
		await velocityClient.initializePythLazerOracle(2);
		// await velocityClient.initializePythLazerOracle(6); before hook already initialized SOL oracle
	});

	it('crank single', async () => {
		await velocityClient.postPythLazerOracleUpdate(
			[6],
			freshLazerSolHex(bankrunContextWrapper.connection.getTime())
		);
		await velocityClient.updatePerpMarketOracle(
			0,
			getPythLazerOraclePublicKey(velocityClient.program.programId, 6),
			OracleSource.PYTH_LAZER
		);
		await velocityClient.fetchAccounts();
		assert(
			isVariant(
				velocityClient.getPerpMarketAccount(0).oracleSource,
				'pythLazer'
			)
		);
	});

	it('crank multi', async () => {
		// MULTI stays a frozen Pyth-signed fixture: it still verifies (Pyth's signer is kept
		// trusted) but its stale feeds are skipped by the max-age check, so the crank is a
		// no-op update here (this case only asserts the tx succeeds).
		const tx = await velocityClient.postPythLazerOracleUpdate(
			[1, 2, 6],
			PYTH_LAZER_HEX_STRING_MULTI
		);
		console.log(tx);
	});
});
