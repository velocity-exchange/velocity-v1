import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import {
	BN,
	getTokenAmount,
	loadKeypair,
	InitializePerpMarketArgs,
	InitializeSpotMarketArgs,
	OracleSource,
	SpotBalanceType,
	PERCENTAGE_PRECISION,
	TestClient,
	Wallet,
	ZERO,
} from '../../packages/sdk/src';
import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	perpMarketParams,
	spotMarketParams,
} from './testHelpers';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';

/**
 * `initialize_{spot,perp}_market` take an args struct. The SDK also keeps a
 * positional convenience form, which maps twenty and twenty-eight arguments
 * onto that struct, so the risk is not that the handler is wrong, it is that
 * the mapping puts an argument in the wrong field. These tests build the same
 * market both ways and compare the resulting accounts field by field, which is
 * the only check that actually catches a transposition.
 */
describe('initialize market', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let bulkAccountLoader: TestBulkAccountLoader;
	let velocityClient: TestClient;
	let svmContextWrapper: LiteSVMContextWrapper;
	let usdcMint;
	let userUSDCAccount;
	let solUsd;

	const usdcAmount = new BN(1000 * 10 ** 6);
	const periodicity = new BN(60 * 60);

	before(async () => {
		const context = startLiteSVM();
		svmContextWrapper = new LiteSVMContextWrapper(context);
		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);

		const wallet = new Wallet(loadKeypair(process.env.ANCHOR_WALLET));
		//@ts-ignore
		await svmContextWrapper.fundKeypair(wallet, 10 ** 9);

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [0, 1],
			spotMarketIndexes: [0, 1],
			subAccountIds: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			usdcAmount,
			svmContextWrapper,
			velocityClient.wallet.publicKey
		);

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await velocityClient.initializeUserAccount(0);
		await velocityClient.fetchAccounts();

		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));
		await velocityClient.fetchAccounts();

		solUsd = await mockOracleNoProgram(svmContextWrapper, 1);
	});

	/** Sends an args-struct init, which the SDK only exposes as an instruction. */
	const sendInitPerp = async (args: InitializePerpMarketArgs, oracle) => {
		const ix = await velocityClient.getInitializePerpMarketIx(args, oracle);
		const tx = await velocityClient.buildTransaction(ix);
		await velocityClient.sendTransaction(tx, [], velocityClient.opts);
	};

	const sendInitSpot = async (args: InitializeSpotMarketArgs, mint, oracle) => {
		const ix = await velocityClient.getInitializeSpotMarketIx(
			args,
			mint,
			oracle
		);
		const tx = await velocityClient.buildTransaction(ix);
		await velocityClient.sendTransaction(tx, [], velocityClient.opts);
	};

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('positional and args forms build the same perp market', async () => {
		const reserve = new BN(1000);

		await velocityClient.initializePerpMarket(
			0,
			solUsd,
			reserve,
			reserve,
			periodicity
		);
		await velocityClient.fetchAccounts();

		await sendInitPerp(
			perpMarketParams({
				marketIndex: 1,
				ammBaseAssetReserve: reserve,
				ammQuoteAssetReserve: reserve,
				ammPeriodicity: periodicity,
			}),
			solUsd
		);
		await velocityClient.fetchAccounts();

		const v1 = velocityClient.getPerpMarketAccount(0);
		const v2 = velocityClient.getPerpMarketAccount(1);

		// Every field a wrapper could transpose. Compared by name so a mismatch
		// says which one moved.
		assert.equal(v1.marginRatioInitial, v2.marginRatioInitial);
		assert.equal(v1.marginRatioMaintenance, v2.marginRatioMaintenance);
		assert.equal(v1.liquidatorFee, v2.liquidatorFee);
		assert.equal(v1.ifLiquidationFee, v2.ifLiquidationFee);
		assert.equal(v1.imfFactor, v2.imfFactor);
		assert.equal(v1.amm.baseSpread, v2.amm.baseSpread);
		assert.equal(v1.amm.maxSpread, v2.amm.maxSpread);
		assert.equal(v1.amm.curveUpdateIntensity, v2.amm.curveUpdateIntensity);
		assert.equal(v1.amm.ammJitIntensity, v2.amm.ammJitIntensity);
		assert.ok(v1.orderStepSize.eq(v2.orderStepSize));
		assert.ok(v1.orderTickSize.eq(v2.orderTickSize));
		assert.ok(v1.maxOpenInterest.eq(v2.maxOpenInterest));
		assert.ok(v1.amm.baseAssetReserve.eq(v2.amm.baseAssetReserve));
		assert.ok(v1.amm.quoteAssetReserve.eq(v2.amm.quoteAssetReserve));
		assert.ok(v1.amm.pegMultiplier.eq(v2.amm.pegMultiplier));
		assert.ok(
			v1.insuranceClaim.quoteMaxInsurance.eq(
				v2.insuranceClaim.quoteMaxInsurance
			)
		);
		assert.deepEqual(v1.contractTier, v2.contractTier);
		assert.deepEqual(v1.oracleSource, v2.oracleSource);
		assert.deepEqual(Array.from(v1.name), Array.from(v2.name));
	});

	it('args form sets the fields the positional form hardcodes to zero', async () => {
		const oracle = await mockOracleNoProgram(svmContextWrapper, 1);
		const maxTokenDeposits = new BN(123_456_789);
		const minBorrowRate = 1; // 1/200 = 0.5%

		await sendInitSpot(
			spotMarketParams({
				oracleSource: OracleSource.PYTH_LAZER,
				minBorrowRate,
				maxTokenDeposits,
			}),
			usdcMint.publicKey,
			oracle
		);
		await velocityClient.fetchAccounts();

		const market = velocityClient.getSpotMarketAccount(1);
		assert.ok(
			market.maxTokenDeposits.eq(maxTokenDeposits),
			`maxTokenDeposits ${market.maxTokenDeposits.toString()}`
		);
		assert.equal(market.minBorrowRate, minBorrowRate);

		// market 0 went through the positional form, which hardcodes both.
		const quote = velocityClient.getSpotMarketAccount(0);
		assert.ok(quote.maxTokenDeposits.eq(ZERO));
		assert.equal(quote.minBorrowRate, 0);
	});

	it('rejects a borrow floor above the max, in the right units', async () => {
		// `minBorrowRate` is X/200 (1 => 0.5%), the other three are
		// PERCENTAGE_PRECISION. Validating without scaling makes 255 read as
		// 0.0255% instead of 127.5%, so any floor passes against any max.
		const oracle = await mockOracleNoProgram(svmContextWrapper, 1);
		let threw = false;
		try {
			await sendInitSpot(
				spotMarketParams({
					oracleSource: OracleSource.PYTH_LAZER,
					minBorrowRate: 255, // 127.5%
					optimalBorrowRate: PERCENTAGE_PRECISION.divn(100).toNumber(), // 1%
					maxBorrowRate: PERCENTAGE_PRECISION.divn(50).toNumber(), // 2%
				}),
				usdcMint.publicKey,
				oracle
			);
		} catch {
			threw = true;
		}
		assert.ok(
			threw,
			'a 127.5% floor under a 2% max must be rejected at init, as the update instruction rejects it'
		);
	});

	it('depositIntoPerpMarketPnlPool moves tokens and credits the pool', async () => {
		await velocityClient.fetchAccounts();
		const before = velocityClient.getPerpMarketAccount(0);
		const quoteBefore = velocityClient.getSpotMarketAccount(0);
		const pnlBefore = getTokenAmount(
			before.pnlPool.scaledBalance,
			quoteBefore,
			SpotBalanceType.DEPOSIT
		);
		const vaultBefore = (
			await svmContextWrapper.connection.getTokenAccountBalance(
				quoteBefore.vault
			)
		).amount;

		const amount = new BN(5 * 10 ** 6);
		await velocityClient.depositIntoPerpMarketPnlPool(
			0,
			amount,
			userUSDCAccount.publicKey
		);
		await velocityClient.fetchAccounts();

		const after = velocityClient.getPerpMarketAccount(0);
		const quoteAfter = velocityClient.getSpotMarketAccount(0);
		const pnlAfter = getTokenAmount(
			after.pnlPool.scaledBalance,
			quoteAfter,
			SpotBalanceType.DEPOSIT
		);
		const vaultAfter = (
			await svmContextWrapper.connection.getTokenAccountBalance(
				quoteAfter.vault
			)
		).amount;

		// The accounting moved and the tokens moved, by the same amount. That
		// pairing is the whole reason this instruction exists.
		assert.ok(
			pnlAfter.sub(pnlBefore).eq(amount),
			`pnl pool moved ${pnlAfter
				.sub(pnlBefore)
				.toString()}, expected ${amount.toString()}`
		);
		assert.equal(
			(vaultAfter - vaultBefore).toString(),
			amount.toString(),
			'vault token balance moved by the credited amount'
		);
	});
});
