import * as anchor from '@coral-xyz/anchor';
import { assert } from 'chai';

import { Program } from '@coral-xyz/anchor';

import {
	TestClient,
	QUOTE_PRECISION,
	PERCENTAGE_PRECISION,
	BN,
	OracleSource,
	getSpotMarketPublicKey,
} from '../../packages/sdk/src';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { startAnchor } from 'solana-bankrun';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';

describe('spot market deposit cap + configurable withdraw breaker', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;

	let usdcMint;
	let userUSDCAccount;

	// fund the user with 10 USDC
	const usdcAmount = new BN(10 * 10 ** 6);
	// deposit cap: no rate limit below 5 USDC of deposits
	const guardThreshold = new BN(5 * 10 ** 6);

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

		const solUsd = await mockOracleNoProgram(bankrunContextWrapper, 1);

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
			userStats: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	// Fetch + decode the spot market straight from chain via the program coder,
	// so the assertion validates the regenerated IDL layout end-to-end and does
	// not depend on the polling subscriber's refresh timing under bankrun.
	const fetchSpotMarket = async () => {
		const pk = await getSpotMarketPublicKey(chProgram.programId, 0);
		return (await velocityClient.program.account.spotMarket.fetch(pk)) as any;
	};

	it('update withdraw circuit breaker pct', async () => {
		const tenPct = PERCENTAGE_PRECISION.divn(10).toNumber();
		await velocityClient.updateSpotMarketWithdrawCircuitBreaker(0, tenPct);
		const market = await fetchSpotMarket();
		assert(market.withdrawCircuitBreakerPct === tenPct);
	});

	it('update deposit cap', async () => {
		const twentyPct = PERCENTAGE_PRECISION.divn(5).toNumber();
		await velocityClient.updateSpotMarketDepositCap(0, guardThreshold, twentyPct);
		const market = await fetchSpotMarket();
		assert(market.depositGuardThreshold.eq(guardThreshold));
		assert(market.maxDepositPctPerDay === twentyPct);
	});

	it('blocks a deposit above the daily cap', async () => {
		// deposit_token_twap is ~0 in a fresh market, so the cap == guardThreshold.
		// depositing 10 USDC (> 5 USDC cap) must revert with DailyDepositLimit.
		let threw = false;
		try {
			await velocityClient.initializeUserAccountAndDepositCollateral(
				usdcAmount,
				userUSDCAccount.publicKey
			);
		} catch (e) {
			threw = true;
		}
		assert(threw, 'expected deposit above the daily cap to revert');
	});

	it('allows a deposit at/below the guard threshold', async () => {
		// 5 USDC == guardThreshold == cap when twap ~0, so this must succeed.
		await velocityClient.initializeUserAccountAndDepositCollateral(
			guardThreshold,
			userUSDCAccount.publicKey
		);
		const user = velocityClient.getUserAccount();
		assert(user !== undefined);
	});
});
