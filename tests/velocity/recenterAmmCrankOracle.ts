import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { startAnchor } from 'solana-bankrun';
import { Keypair, PublicKey } from '@solana/web3.js';
import {
	BN,
	getPerpMarketPublicKey,
	getSpotMarketPublicKey,
	HotRole,
	OracleSource,
	QUOTE_SPOT_MARKET_INDEX,
	TestClient,
} from '../../packages/sdk/src';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
} from './testHelpers';

// `recenter_perp_market_amm_crank` sets the market peg from the oracle account
// the caller passes, under the `AmmCrank` hot role. The market must bind that
// account. Without the binding, the hot key sets the peg from any feed it
// chooses, which is the power the warm-admin `recenter_perp_market_amm` holds.
describe('recenter amm crank oracle binding', () => {
	const chProgram = anchor.workspace.Velocity as Program;
	const INVALID_ORACLE = '0x1793'; // ErrorCode::InvalidOracle (6035)

	let velocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let usdcMint: Keypair;
	let marketOracle: PublicKey;
	let otherOracle: PublicKey;

	const MARKET_INDEX = 0;

	const mantissaSqrtScale = new BN(100000);
	const ammInitialQuoteAssetAmount = new BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetAmount = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);

	// The instruction takes the oracle as an `UncheckedAccount`, so the ix has
	// to be built by hand to substitute one. The SDK helper always passes
	// `perpMarketAccount.oracle`.
	const crankIxWithOracle = async (oracle: PublicKey) =>
		await velocityClient.program.instruction.recenterPerpMarketAmmCrank(null, {
			accounts: {
				admin: velocityClient.wallet.publicKey,
				state: await velocityClient.getStatePublicKey(),
				perpMarket: await getPerpMarketPublicKey(
					chProgram.programId,
					MARKET_INDEX
				),
				spotMarket: await getSpotMarketPublicKey(
					chProgram.programId,
					QUOTE_SPOT_MARKET_INDEX
				),
				oracle,
			},
		});

	const sendCrank = async (oracle: PublicKey) => {
		const tx = await velocityClient.buildTransaction(
			await crankIxWithOracle(oracle)
		);
		await velocityClient.sendTransaction(tx, [], velocityClient.opts);
	};

	before(async () => {
		const context = await startAnchor('', [], []);
		bankrunContextWrapper = new BankrunContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			bankrunContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(bankrunContextWrapper);
		marketOracle = await mockOracleNoProgram(
			bankrunContextWrapper,
			1,
			-7,
			undefined,
			10000
		);
		// A second real feed at a much higher price. The binding must refuse this
		// substitution, because it is a valid oracle for a different asset.
		otherOracle = await mockOracleNoProgram(
			bankrunContextWrapper,
			50000,
			-7,
			undefined,
			10000
		);

		velocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: bankrunContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [MARKET_INDEX],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [
				{ publicKey: marketOracle, source: OracleSource.PYTH_LAZER },
				{ publicKey: otherOracle, source: OracleSource.PYTH_LAZER },
			],
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
			MARKET_INDEX,
			marketOracle,
			ammInitialBaseAssetAmount,
			ammInitialQuoteAssetAmount,
			new BN(60 * 60)
		);

		// The wallet doubles as the AmmCrank hot key.
		await velocityClient.updateHotAdmin(
			HotRole.AmmCrank,
			velocityClient.wallet.publicKey
		);
		await velocityClient.fetchAccounts();
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('refuses an oracle that is not the market oracle, and leaves the peg alone', async () => {
		const pegBefore =
			velocityClient.getPerpMarketAccount(MARKET_INDEX).amm.pegMultiplier;

		let threw = false;
		try {
			await sendCrank(otherOracle);
		} catch (e) {
			threw = true;
			assert(
				e.message.includes(INVALID_ORACLE) ||
					e.message.includes('InvalidOracle'),
				`expected InvalidOracle, got: ${e.message}`
			);
		}
		assert(threw, 'the crank accepted an oracle the market does not name');

		await velocityClient.fetchAccounts();
		const pegAfter =
			velocityClient.getPerpMarketAccount(MARKET_INDEX).amm.pegMultiplier;
		assert(pegAfter.eq(pegBefore), `peg moved: ${pegBefore} -> ${pegAfter}`);
	});

	// A control. The rejection above must come from the oracle binding, and not
	// from another failure the same call would hit anyway.
	it('does not report InvalidOracle for the market oracle', async () => {
		try {
			await sendCrank(marketOracle);
		} catch (e) {
			assert(
				!e.message.includes(INVALID_ORACLE),
				`the market oracle was refused as invalid: ${e.message}`
			);
		}
	});
});
