import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';
import { BN, QUOTE_SPOT_MARKET_INDEX } from '../../packages/sdk';

import { Program } from '@coral-xyz/anchor';

import {
	TestClient,
	PRICE_PRECISION,
	PositionDirection,
	ExchangeStatus,
	OracleSource,
} from '../../packages/sdk/src';

import {
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	initializeQuoteSpotMarket,
} from './testHelpers';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';

describe('Pause exchange', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;

	let bulkAccountLoader: TestBulkAccountLoader;

	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint;
	let userUSDCAccount;

	// ammInvariant == k == x * y
	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(5 * 10 ** 9).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(5 * 10 ** 9).mul(
		mantissaSqrtScale
	);

	const usdcAmount = new BN(100 * 10 ** 6);

	before(async () => {
		const context = startLiteSVM();

		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper
		);

		const solOracle = await mockOracleNoProgram(svmContextWrapper, 1);
		const periodicity = new BN(60 * 60); // 1 HOUR

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0, 1],
			subAccountIds: [],
			oracleInfos: [
				{
					publicKey: solOracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();

		await velocityClient.initializePerpMarket(
			0,
			solOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity
		);

		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);

		await velocityClient.initializeUserAccountAndDepositCollateral(
			usdcAmount,
			userUSDCAccount.publicKey
		);

		const marketIndex = 0;
		const incrementalUSDCNotionalAmount = usdcAmount.mul(new BN(5));
		await velocityClient.openPosition(
			PositionDirection.LONG,
			incrementalUSDCNotionalAmount,
			marketIndex
		);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('Pause exchange', async () => {
		await velocityClient.updateExchangeStatus(ExchangeStatus.PAUSED);
		// `updateExchangeStatus` sends the transaction and returns. `getStateAccount` reads the
		// cached account, which the subscriber updates on its own schedule, so the assert below
		// races the poll under load. Fetch first, as the same assert in `admin.ts` does.
		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();
		assert(
			state.exchangeStatus === ExchangeStatus.PAUSED,
			`exchange status does not match \n actual: ${state.exchangeStatus} \n expected: ${ExchangeStatus.PAUSED}`
		);
	});

	it('Block open position', async () => {
		try {
			await velocityClient.openPosition(PositionDirection.LONG, usdcAmount, 0);
		} catch (e) {
			console.log(e);
			assert(e.message.includes('0x1788')); //Error Number: 6024. Error Message: Exchange is paused.
			return;
		}
		console.assert(false);
	});

	it('Block close position', async () => {
		try {
			await velocityClient.closePosition(0);
		} catch (e) {
			console.log(e.msg);

			assert(e.message.includes('0x1788'));
			return;
		}
		console.assert(false);
	});

	it('Block withdrawal', async () => {
		try {
			await velocityClient.withdraw(
				usdcAmount,
				QUOTE_SPOT_MARKET_INDEX,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			console.log(e.message);
			assert(e.message.includes('0x1788'));
			return;
		}
		console.assert(false);
	});
});
