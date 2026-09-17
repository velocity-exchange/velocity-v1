import * as anchor from '@coral-xyz/anchor';

import { Program } from '@coral-xyz/anchor';

import { assert } from 'chai';

import {
	BN,
	PRICE_PRECISION,
	TestClient,
	PositionDirection,
	getLimitOrderParams,
	MarketStatus,
	AMM_RESERVE_PRECISION,
	OracleSource,
	ZERO,
} from '../../packages/sdk/src';

import {
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	initializeQuoteSpotMarket,
} from './testHelpers';
import { PostOnlyParams } from '../../packages/sdk';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';

// An oracle-floating limit price cannot rest on a CLOB. The program therefore
// refuses every limit order that carries an oracle offset, with
// InvalidOrderOracleOffset. An oracle-relative maker quote belongs to a PropAMM
// quoter instead.
describe('oracle offset limit orders are refused', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let bulkAccountLoader: TestBulkAccountLoader;

	let bankrunContextWrapper: BankrunContextWrapper;

	let velocityClient: TestClient;

	let usdcMint;
	let userUSDCAccount;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(100000);
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 9).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 9).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(10 * 10 ** 6);

	const marketIndex = 0;
	let solUsd;

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

		solUsd = await mockOracleNoProgram(
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
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [{ publicKey: solUsd, source: OracleSource.PYTH_LAZER }],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));

		const periodicity = new BN(60 * 60); // 1 HOUR

		await velocityClient.initializePerpMarket(
			0,
			solUsd,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity
		);
		await velocityClient.updatePerpMarketStatus(0, MarketStatus.ACTIVE);

		await velocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('refuses a limit order with an oracle offset', async () => {
		try {
			await velocityClient.placePerpOrder(
				getLimitOrderParams({
					marketIndex,
					direction: PositionDirection.LONG,
					baseAssetAmount: AMM_RESERVE_PRECISION,
					price: ZERO,
					oraclePriceOffset: PRICE_PRECISION.div(new BN(50)).neg(),
				})
			);
			assert(false, 'oracle offset limit order placed');
		} catch (e) {
			assert(
				e.message.includes('0x17a7'), // InvalidOrderOracleOffset
				`expected InvalidOrderOracleOffset (0x17a7), got ${e.message}`
			);
		}
	});

	it('refuses a post-only limit order with an oracle offset', async () => {
		try {
			await velocityClient.placePerpOrder(
				getLimitOrderParams({
					marketIndex,
					direction: PositionDirection.SHORT,
					baseAssetAmount: AMM_RESERVE_PRECISION,
					price: ZERO,
					oraclePriceOffset: PRICE_PRECISION.div(new BN(50)),
					postOnly: PostOnlyParams.MUST_POST_ONLY,
				})
			);
			assert(false, 'oracle offset post-only limit order placed');
		} catch (e) {
			assert(
				e.message.includes('0x17a7'), // InvalidOrderOracleOffset
				`expected InvalidOrderOracleOffset (0x17a7), got ${e.message}`
			);
		}
	});

	it('places the same order with a fixed price', async () => {
		await velocityClient.placePerpOrder(
			getLimitOrderParams({
				marketIndex,
				direction: PositionDirection.LONG,
				baseAssetAmount: AMM_RESERVE_PRECISION,
				price: PRICE_PRECISION.sub(PRICE_PRECISION.div(new BN(50))),
			})
		);

		await velocityClient.fetchAccounts();
		const order = velocityClient
			.getUserAccount()
			.orders.find((order) => order.baseAssetAmount.gt(ZERO));
		assert(order !== undefined);
		assert(order.oraclePriceOffset.eq(ZERO));
	});
});
