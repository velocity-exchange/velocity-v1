import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import { assert } from 'chai';

import {
	BASE_PRECISION,
	BN,
	OracleInfo,
	OracleSource,
	PERCENTAGE_PRECISION,
	PRICE_PRECISION,
	PositionDirection,
	QUOTE_PRECISION,
	SpecialUserStatus,
	TestClient,
	ZERO,
	getLimitOrderParams,
	getMarketOrderParams,
	isVariant,
} from '../../packages/sdk/src';

import {
	createWSolTokenAccountForUser,
	initializeQuoteSpotMarket,
	initializeSolSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	setFeedPriceNoProgram,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

// `transfer_perp_position` must not value the recipient's spot borrows through a
// stale `cumulative_borrow_interest`: an un-cranked borrow market understates the
// recipient's debt, so it could take on exposure that its real liability can't carry.
describe('transfer perp position with stale borrow interest', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let svmContextWrapper: LiteSVMContextWrapper;

	let perpOracle: PublicKey;
	let solOracle: PublicKey;
	let usdcMint;
	let usdcAccount: PublicKey;
	let wSolAccount: PublicKey;

	const PERP_MARKET_INDEX = 0;
	const QUOTE_MARKET_INDEX = 0;
	const SOL_MARKET_INDEX = 1;

	const FROM_SUB = 0;
	const TO_SUB = 1;

	const usdcCollateral = new BN(1_000).mul(QUOTE_PRECISION);
	const solDeposit = new BN(5).mul(new BN(10 ** 9));
	const solBorrow = new BN(10 ** 8);

	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammReserve = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);

	before(async () => {
		const context = startLiteSVM();
		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		usdcAccount = (
			await mockUserUSDCAccount(
				usdcMint,
				usdcCollateral.muln(10),
				svmContextWrapper
			)
		).publicKey;
		wSolAccount = await createWSolTokenAccountForUser(
			svmContextWrapper,
			// @ts-ignore
			svmContextWrapper.provider.wallet,
			solDeposit.muln(2)
		);

		perpOracle = await mockOracleNoProgram(svmContextWrapper, 1);
		solOracle = await mockOracleNoProgram(svmContextWrapper, 100);

		const oracleInfos: OracleInfo[] = [
			{ publicKey: perpOracle, source: OracleSource.PYTH_LAZER },
			{ publicKey: solOracle, source: OracleSource.PYTH_LAZER },
		];

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: FROM_SUB,
			perpMarketIndexes: [PERP_MARKET_INDEX],
			spotMarketIndexes: [QUOTE_MARKET_INDEX, SOL_MARKET_INDEX],
			subAccountIds: [],
			userStats: true,
			oracleInfos,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await initializeSolSpotMarket(velocityClient, solOracle);
		await velocityClient.updatePerpAuctionDuration(new BN(0));

		await velocityClient.updateOracleGuardRails({
			priceDivergence: {
				markOraclePercentDivergence: PERCENTAGE_PRECISION.mul(new BN(10)),
				oracleTwap5MinPercentDivergence: PERCENTAGE_PRECISION.mul(new BN(10)),
			},
			validity: {
				slotsBeforeStaleForAmm: new BN(100),
				slotsBeforeStaleForMargin: new BN(100),
				confidenceIntervalMaxSize: new BN(100000),
				tooVolatileRatio: new BN(55),
			},
		});

		await velocityClient.initializePerpMarket(
			PERP_MARKET_INDEX,
			perpOracle,
			ammReserve,
			ammReserve,
			new BN(60 * 60)
		);

		// from_user: USDC collateral plus the SOL liquidity the recipient borrows.
		await velocityClient.initializeUserAccountAndDepositCollateral(
			usdcCollateral,
			usdcAccount,
			QUOTE_MARKET_INDEX,
			FROM_SUB
		);
		await velocityClient.deposit(
			solDeposit,
			SOL_MARKET_INDEX,
			wSolAccount,
			FROM_SUB
		);

		// to_user: ample USDC collateral and a small SOL borrow.
		await velocityClient.initializeUserAccount(TO_SUB);
		await velocityClient.addUser(TO_SUB);
		await velocityClient.deposit(
			usdcCollateral,
			QUOTE_MARKET_INDEX,
			usdcAccount,
			TO_SUB
		);
		await velocityClient.updateUserMarginTradingEnabled([
			{ marginTradingEnabled: true, subAccountId: TO_SUB },
		]);
		await velocityClient.withdraw(
			solBorrow,
			SOL_MARKET_INDEX,
			wSolAccount,
			false,
			TO_SUB
		);

		await velocityClient.moveAmmPrice(
			PERP_MARKET_INDEX,
			ammReserve,
			ammReserve
		);
		await setFeedPriceNoProgram(svmContextWrapper, 1, perpOracle);
		await velocityClient.placeAndTakePerpOrder(
			getMarketOrderParams({
				marketIndex: PERP_MARKET_INDEX,
				direction: PositionDirection.LONG,
				baseAssetAmount: BASE_PRECISION,
			})
		);

		await velocityClient.fetchAccounts();
		const borrow = velocityClient
			.getUserAccount(TO_SUB)
			.spotPositions.find(
				(p) => p.marketIndex === SOL_MARKET_INDEX && !p.scaledBalance.eq(ZERO)
			);
		assert(borrow !== undefined && isVariant(borrow.balanceType, 'borrow'));
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	// Lets the SOL borrow index go stale, expects `send` to revert, then cranks
	// the market and expects the same call to land.
	async function letSolInterestGoStale() {
		await svmContextWrapper.moveTimeForward(2 * 60 * 60);
		await setFeedPriceNoProgram(svmContextWrapper, 1, perpOracle);
		await setFeedPriceNoProgram(svmContextWrapper, 100, solOracle);
		await velocityClient.fetchAccounts();
	}

	async function assertRejectedUntilCranked(send: () => Promise<unknown>) {
		await letSolInterestGoStale();

		let error = '';
		try {
			await send();
		} catch (e) {
			error = e.toString();
		}
		// SpotMarketInterestStaleForMargin (6371)
		assert(
			error.includes('0x18e3'),
			`expected SpotMarketInterestStaleForMargin, got: ${error || 'success'}`
		);

		await velocityClient.updateSpotMarketCumulativeInterest(SOL_MARKET_INDEX);
		await velocityClient.fetchAccounts();
		await send();
		await velocityClient.fetchAccounts();
	}

	const transfer = (fromSub: number, toSub: number) => () =>
		velocityClient.transferPerpPosition(
			fromSub,
			toSub,
			PERP_MARKET_INDEX,
			BASE_PRECISION
		);

	function perpBase(sub: number): BN | undefined {
		return velocityClient
			.getUserAccount(sub)
			.perpPositions.find((p) => p.marketIndex === PERP_MARKET_INDEX)
			?.baseAssetAmount;
	}

	it('rejects the transfer until the recipient borrow market is cranked', async () => {
		await assertRejectedUntilCranked(transfer(FROM_SUB, TO_SUB));
		assert(perpBase(TO_SUB)?.eq(BASE_PRECISION));
	});

	it('rejects the transfer until the sender borrow market is cranked', async () => {
		// TO_SUB now holds the long and the borrow; the recipient has no borrows,
		// so only the sender gate can reject.
		await assertRejectedUntilCranked(transfer(TO_SUB, FROM_SUB));
		assert(perpBase(FROM_SUB)?.eq(BASE_PRECISION));
	});

	it('rejects a vamm-hedger transfer until its borrow market is cranked', async () => {
		// Hand the long back to the borrower while the index is fresh, then flag it.
		await transfer(FROM_SUB, TO_SUB)();
		await velocityClient.fetchAccounts();
		const toUser = await velocityClient.getUserAccountPublicKey(TO_SUB);
		await velocityClient.updateSpecialUserStatus(
			toUser,
			SpecialUserStatus.VAMM_HEDGER
		);

		await assertRejectedUntilCranked(() =>
			velocityClient.specialTransferPerpPositionToVamm(
				toUser,
				PERP_MARKET_INDEX
			)
		);
		assert(perpBase(TO_SUB)?.isZero() ?? true);
	});
	// The SDK prepends the crank on cross-to-isolated transfers, so a borrower
	// with a stale market needs no extra step. The SDK measures staleness on the
	// local clock, so pin it to the warped chain clock for these cases.
	async function withChainClock(run: () => Promise<void>) {
		const realNow = Date.now;
		Date.now = () =>
			Number(svmContextWrapper.context.getClock().unixTimestamp) * 1000;
		try {
			await run();
		} finally {
			Date.now = realNow;
		}
	}

	it('cranks before a cross-to-isolated deposit transfer', async () => {
		await withChainClock(async () => {
			await letSolInterestGoStale();
			const isolatedBefore = velocityClient.getIsolatedPerpPositionTokenAmount(
				PERP_MARKET_INDEX,
				TO_SUB
			);

			await velocityClient.transferIsolatedPerpPositionDeposit(
				QUOTE_PRECISION,
				PERP_MARKET_INDEX,
				TO_SUB,
				undefined,
				undefined,
				true
			);
			await velocityClient.fetchAccounts();

			assert(
				velocityClient
					.getIsolatedPerpPositionTokenAmount(PERP_MARKET_INDEX, TO_SUB)
					.gt(isolatedBefore)
			);
		});
	});

	it('cranks before the isolated deposit an order prepends', async () => {
		await withChainClock(async () => {
			await letSolInterestGoStale();
			const isolatedBefore = velocityClient.getIsolatedPerpPositionTokenAmount(
				PERP_MARKET_INDEX,
				TO_SUB
			);

			// prepareMarketOrderTxs prepends the deposit as a required instruction;
			// placePerpOrder only adds it when there is room, so it can't pin this.
			const { marketOrderTx } = await velocityClient.prepareMarketOrderTxs(
				getLimitOrderParams({
					marketIndex: PERP_MARKET_INDEX,
					direction: PositionDirection.LONG,
					baseAssetAmount: BASE_PRECISION,
					price: PRICE_PRECISION.divn(2),
				}),
				await velocityClient.getUserAccountPublicKey(TO_SUB),
				velocityClient.getUserAccount(TO_SUB),
				undefined,
				undefined,
				undefined,
				undefined,
				undefined,
				undefined,
				QUOTE_PRECISION
			);
			await velocityClient.sendTransaction(marketOrderTx);
			await velocityClient.fetchAccounts();

			assert(
				velocityClient
					.getIsolatedPerpPositionTokenAmount(PERP_MARKET_INDEX, TO_SUB)
					.gt(isolatedBefore)
			);
		});
	});
});
