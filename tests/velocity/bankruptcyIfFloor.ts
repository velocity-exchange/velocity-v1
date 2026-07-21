import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { startAnchor } from 'solana-bankrun';
import { Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';
import {
	BASE_PRECISION,
	BN,
	ContractTier,
	getTokenAmount,
	LIQUIDATION_PCT_PRECISION,
	OracleGuardRails,
	OracleSource,
	PositionDirection,
	QUOTE_PRECISION,
	SpotBalanceType,
	TestClient,
	TransferFeeAndPnlPoolDirection,
	Wallet,
	ZERO,
} from '../../packages/sdk/src';
import { PERCENTAGE_PRECISION, UserStatus } from '../../packages/sdk';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	setFeedPriceNoProgram,
} from './testHelpers';

// Regression for the pending-IF-fee sweep front-run: a permissionless
// sweepPerpMarketFees fired between a bankruptcy and its resolution must
// not clear the pending IF fee that resolvePerpBankruptcy consumes as its
// first-loss tranche. The market's `bankruptcyIfFloorPct` (a fraction of
// open-interest notional at the oracle TWAP, defaulted at market init)
// makes the sweep's IF drain leave that floor behind, so the tranche
// survives the front-run — and revenue settlement being "not due" (the
// quote market's revenue_settle_period is 0 here) can no longer turn the
// sweep into extra socialized loss.
describe('bankruptcy IF-fee floor', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let usdcMint: Keypair;
	let userUSDCAccount: Keypair;
	let oracle;

	const liquidatorKeyPair = new Keypair();
	let liquidatorUSDCAccount: Keypair;
	let liquidatorVelocityClient: TestClient;

	const MARKET_INDEX = 0;

	// ammInvariant == k == x * y ($1 SOL curve, as in liquidatePerp.ts)
	const mantissaSqrtScale = new BN(
		Math.sqrt(QUOTE_PRECISION.toNumber() /* PRICE_PRECISION */)
	);
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(200 * 10 ** 6);
	const depositAmount = new BN(10 * 10 ** 6);
	const pnlPoolSeed = new BN(100 * 10 ** 6);

	const readTokens = (pool: { scaledBalance: BN }) =>
		getTokenAmount(
			pool.scaledBalance,
			velocityClient.getSpotMarketAccount(0),
			SpotBalanceType.DEPOSIT
		);

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
		oracle = await mockOracleNoProgram(
			bankrunContextWrapper,
			1,
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
			oracleInfos: [{ publicKey: oracle, source: OracleSource.PYTH_LAZER }],
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();

		await velocityClient.updateInitialPctToLiquidate(
			LIQUIDATION_PCT_PRECISION.toNumber()
		);

		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));

		// keep the price band happy after the 90% oracle crash below (the
		// 5-min TWAP lags at ~$1 vs the $0.10 oracle → ~900% spread)
		const oracleGuardRails: OracleGuardRails = {
			priceDivergence: {
				markOraclePercentDivergence: PERCENTAGE_PRECISION.muln(10),
				oracleTwap5MinPercentDivergence: PERCENTAGE_PRECISION.muln(100),
			},
			validity: {
				slotsBeforeStaleForAmm: new BN(100),
				slotsBeforeStaleForMargin: new BN(100),
				confidenceIntervalMaxSize: new BN(100000),
				tooVolatileRatio: new BN(11),
			},
		};
		await velocityClient.updateOracleGuardRails(oracleGuardRails);

		await velocityClient.initializePerpMarket(
			MARKET_INDEX,
			oracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			new BN(0)
		);

		// route the entire taker fee to the IF carveout so the bankruptcy's
		// tranche-1 budget (pending_if_fee) is as large as the fills allow and
		// there is no protocol/AMM-provision noise in the waterfall assertions
		const feeStructure = velocityClient.getStateAccount().perpFeeStructure;
		feeStructure.ammFeeNumerator = 0;
		feeStructure.ifFeeNumerator = 100;
		await velocityClient.updatePerpFeeStructure(feeStructure);

		await velocityClient.initializeUserAccountAndDepositCollateral(
			depositAmount,
			userUSDCAccount.publicKey
		);

		await velocityClient.openPosition(
			PositionDirection.LONG,
			new BN(175).mul(BASE_PRECISION).div(new BN(10)), // 17.5 SOL
			MARKET_INDEX,
			new BN(0)
		);

		bankrunContextWrapper.fundKeypair(liquidatorKeyPair, LAMPORTS_PER_SOL);
		liquidatorUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			bankrunContextWrapper,
			liquidatorKeyPair.publicKey
		);
		liquidatorVelocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: new Wallet(liquidatorKeyPair),
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [MARKET_INDEX],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [{ publicKey: oracle, source: OracleSource.PYTH_LAZER }],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await liquidatorVelocityClient.subscribe();

		await liquidatorVelocityClient.initializeUserAccountAndDepositCollateral(
			depositAmount,
			liquidatorUSDCAccount.publicKey
		);
	});

	after(async () => {
		await velocityClient.unsubscribe();
		await liquidatorVelocityClient.unsubscribe();
	});

	it('floored sweep between bankruptcy and resolve cannot strip the tranche', async () => {
		// markets initialize with the 10 bps default
		await velocityClient.fetchAccounts();
		assert(
			velocityClient.getPerpMarketAccount(MARKET_INDEX).bankruptcyIfFloorPct ===
				1000,
			'new market should default to a 10 bps floor'
		);

		// raise the floor to 100% of OI notional so it covers the full (small)
		// pending IF fee accrued by the fills in this test
		await velocityClient.updatePerpMarketBankruptcyIfFloorPct(
			MARKET_INDEX,
			PERCENTAGE_PRECISION.toNumber()
		);

		// give the sweep real tokens to drain (absent the floor it WOULD
		// sweep the full pending IF fee out of this pool) and remove the
		// retention buffer so only the floor can hold anything back
		await velocityClient.updatePerpMarketFeePoolBufferTarget(
			MARKET_INDEX,
			ZERO
		);
		await velocityClient.depositIntoPerpMarketFeePool(
			MARKET_INDEX,
			pnlPoolSeed,
			userUSDCAccount.publicKey
		);
		await velocityClient.transferFeeAndPnlPool(
			MARKET_INDEX,
			MARKET_INDEX,
			pnlPoolSeed,
			TransferFeeAndPnlPoolDirection.FEE_TO_PNL_POOL
		);
		await velocityClient.fetchAccounts();

		const pendingIfSeed =
			velocityClient.getPerpMarketAccount(MARKET_INDEX).feeLedger.pendingIfFee;
		assert(
			pendingIfSeed.gt(ZERO),
			'setup should have accrued a pending IF fee from the open fill'
		);

		// crash the price and drive the user into bankruptcy
		await setFeedPriceNoProgram(bankrunContextWrapper, 0.1, oracle, 10000);
		await bulkAccountLoader.load();
		await velocityClient.fetchAccounts();

		await liquidatorVelocityClient.setUserStatusToBeingLiquidated(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount()
		);
		await liquidatorVelocityClient.liquidatePerp(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount(),
			MARKET_INDEX,
			new BN(175).mul(BASE_PRECISION).div(new BN(10))
		);
		await liquidatorVelocityClient.liquidatePerpPnlForDeposit(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount(),
			MARKET_INDEX,
			0,
			velocityClient.getUserAccount().perpPositions[0].quoteAssetAmount.abs()
		);

		await velocityClient.fetchAccounts();
		assert(velocityClient.getUserAccount().status === UserStatus.BANKRUPT);
		const loss = velocityClient
			.getUserAccount()
			.perpPositions[0].quoteAssetAmount.abs();
		assert(loss.gt(ZERO), 'bankrupt user should have a residual quote loss');

		const flagged = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		const pendingIfBefore = flagged.feeLedger.pendingIfFee;
		assert(pendingIfBefore.gt(ZERO));
		assert(
			loss.gt(pendingIfBefore),
			'loss should exceed the tranche so the withholding is total'
		);
		const revenuePoolBefore = readTokens(
			velocityClient.getSpotMarketAccount(0).revenuePool
		);

		// the front-run: a permissionless sweep while the bankruptcy is
		// unresolved (and revenue settlement to the IF vault is not due —
		// revenue_settle_period is 0). The floor (100% of OI notional)
		// exceeds the pending IF fee, so nothing may leave for the revenue
		// pool.
		await velocityClient.sweepPerpMarketFees(MARKET_INDEX);
		await velocityClient.fetchAccounts();

		const afterSweep = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		assert(
			afterSweep.feeLedger.pendingIfFee.eq(pendingIfBefore),
			`sweep drained the floored tranche: ${afterSweep.feeLedger.pendingIfFee} != ${pendingIfBefore}`
		);
		assert(
			readTokens(velocityClient.getSpotMarketAccount(0).revenuePool).eq(
				revenuePoolBefore
			),
			'sweep moved floored IF fees to the revenue pool'
		);

		// resolve: tranche 1 must still see the full pending IF fee, so the
		// socialized remainder is exactly loss - pendingIf (empty IF vault,
		// no AMM provision to claw back)
		await velocityClient.updatePerpMarketContractTier(
			MARKET_INDEX,
			ContractTier.A
		);
		await velocityClient.updatePerpMarketMaxImbalances(
			MARKET_INDEX,
			new BN(40000).mul(QUOTE_PRECISION),
			QUOTE_PRECISION,
			QUOTE_PRECISION
		);
		await velocityClient.fetchAccounts();

		await liquidatorVelocityClient.resolvePerpBankruptcy(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount(),
			MARKET_INDEX
		);

		await velocityClient.fetchAccounts();
		const afterResolve = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		assert(
			afterResolve.feeLedger.pendingIfFee.eq(ZERO),
			'tranche 1 should consume the full pending IF fee'
		);
		assert(
			afterResolve.totalSocialLoss.eq(loss.sub(pendingIfBefore)),
			`social loss ${afterResolve.totalSocialLoss} != loss ${loss} - tranche ${pendingIfBefore}`
		);
		assert(
			(velocityClient.getUserAccount().status &
				(UserStatus.BANKRUPT | UserStatus.BEING_LIQUIDATED)) ===
				0
		);

		// the sweep stays callable afterwards (nothing pending remains here)
		await velocityClient.sweepPerpMarketFees(MARKET_INDEX);
		await velocityClient.fetchAccounts();
		assert(
			velocityClient
				.getPerpMarketAccount(MARKET_INDEX)
				.feeLedger.pendingIfFee.eq(ZERO)
		);
	});
});
