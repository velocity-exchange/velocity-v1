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
	TestClient,
	ZERO,
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

	// Lets the SOL borrow index go stale, expects the transfer to revert, then
	// cranks the market and expects the same transfer to land.
	async function assertRejectedUntilCranked(
		fromSub: number,
		toSub: number,
		amount: BN
	) {
		await svmContextWrapper.moveTimeForward(2 * 60 * 60);
		await setFeedPriceNoProgram(svmContextWrapper, 1, perpOracle);
		await setFeedPriceNoProgram(svmContextWrapper, 100, solOracle);
		await velocityClient.fetchAccounts();

		let error = '';
		try {
			await velocityClient.transferPerpPosition(
				fromSub,
				toSub,
				PERP_MARKET_INDEX,
				amount
			);
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
		await velocityClient.transferPerpPosition(
			fromSub,
			toSub,
			PERP_MARKET_INDEX,
			amount
		);
		await velocityClient.fetchAccounts();
	}

	function perpBase(sub: number): BN | undefined {
		return velocityClient
			.getUserAccount(sub)
			.perpPositions.find((p) => p.marketIndex === PERP_MARKET_INDEX)
			?.baseAssetAmount;
	}

	it('rejects the transfer until the recipient borrow market is cranked', async () => {
		await assertRejectedUntilCranked(FROM_SUB, TO_SUB, BASE_PRECISION);
		assert(perpBase(TO_SUB)?.eq(BASE_PRECISION));
	});

	it('rejects the transfer until the sender borrow market is cranked', async () => {
		// TO_SUB now holds the long and the borrow; the recipient has no borrows,
		// so only the sender gate can reject.
		await assertRejectedUntilCranked(TO_SUB, FROM_SUB, BASE_PRECISION);
		assert(perpBase(FROM_SUB)?.eq(BASE_PRECISION));
	});
});
