import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import {
	BN,
	BASE_PRECISION,
	getMarketOrderParams,
	loadKeypair,
	PositionDirection,
	TestClient,
	Wallet,
} from '../../packages/sdk/src';
import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { VelocityCore } from '../../packages/sdk/src/core/VelocityCore';
import { findComputeUnitConsumption } from '../../packages/sdk/src/util/computeUnits';

type ComputeUnitMeasurement = {
	cu: number;
};

type BenchmarkResult = {
	instruction: string;
	path: string;
	measurement: ComputeUnitMeasurement;
};

function assertOptionalMax(
	label: string,
	measurement: ComputeUnitMeasurement,
	envName: string
): void {
	const rawLimit = process.env[envName];
	if (rawLimit === undefined) {
		return;
	}

	const limit = Number(rawLimit);
	assert(
		measurement.cu <= limit,
		`${label} CU ${measurement.cu} exceeded ${envName}=${limit}`
	);
}

function pad(value: string | number, width: number): string {
	return String(value).padEnd(width, ' ');
}

function printComputeUnitTable(
	title: string,
	results: BenchmarkResult[]
): void {
	const columns = [
		['instruction', 35],
		['path', 27],
		['cu', 9],
	] as const;

	const renderRow = (values: Array<string | number>): string =>
		values.map((value, index) => pad(value, columns[index][1])).join(' ');

	const width =
		columns.reduce((sum, [, columnWidth]) => sum + columnWidth, 0) +
		columns.length -
		1;

	console.log('');
	console.log(title);
	console.log('-'.repeat(width));
	console.log(renderRow(columns.map(([label]) => label)));
	console.log('-'.repeat(width));

	for (const result of results) {
		console.log(
			renderRow([result.instruction, result.path, result.measurement.cu])
		);
	}
	console.log('-'.repeat(width));
	console.log('');
}

describe('compute units', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let svmContextWrapper: LiteSVMContextWrapper;
	let velocityClient: TestClient;
	let originalConsoleLog: typeof console.log;

	let acceptedMmOraclePrice = new BN(100_000_000);
	let acceptedMmOracleSequenceId = new BN(1_000_000);
	let ammSpreadAdjustment = 0;

	// Dedicated market for the fill bench. The oracle, the MM oracle, and the AMM
	// curve all sit at price 1, so a taker fills cleanly against the vAMM.
	// `curve_update_intensity` stays above 0, so the routing projection runs.
	// This market stays separate from market 0, whose MM oracle sits at 100 for
	// the admin noop benches.
	const fillMarketIndex = 1;
	const fillMmOraclePrice = new BN(1_000_000); // price 1, PRICE_PRECISION
	let fillMmOracleSequenceId = new BN(1_000_000);

	// Dedicated markets for the batch bench, kept off markets 0 and 1 so the
	// batch writes cannot perturb the single-market noop benches or the fill
	// bench. Four of them because that is the current mainnet perp market count.
	const batchMarketIndexes = [2, 3, 4, 5];
	let batchMmOraclePrice = new BN(100_000_000);
	let batchMmOracleSequenceId = new BN(1_000_000);

	before(async () => {
		originalConsoleLog = console.log;
		console.log = (...args: Parameters<typeof console.log>) => {
			if (String(args[0]).startsWith('Pausing to find oracle ')) {
				return;
			}
			originalConsoleLog(...args);
		};

		const context = startLiteSVM();
		svmContextWrapper = new LiteSVMContextWrapper(context);
		const defaultIdl = VelocityCore.defaultIdl();
		(VelocityCore as any).defaultIdl = () => ({
			...defaultIdl,
			address: chProgram.programId.toString(),
		});

		const bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection,
			'processed',
			0
		);

		const usdcMint = await mockUSDCMint(svmContextWrapper);
		const wallet = new Wallet(loadKeypair(process.env.ANCHOR_WALLET));
		await svmContextWrapper.fundKeypair(wallet, 10 ** 9);

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0, 1, ...batchMarketIndexes],
			spotMarketIndexes: [0],
			subAccountIds: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		const userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			new BN(10 * 10 ** 6),
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

		const solUsd = await mockOracleNoProgram(svmContextWrapper, 1);
		await velocityClient.initializePerpMarket(
			0,
			solUsd,
			new BN(1000),
			new BN(1000),
			new BN(60 * 60)
		);
		await velocityClient.initializeAmmCache();
		await velocityClient.updateFeatureBitFlagsMMOracle(true);

		await velocityClient.updateMmOracleNative(
			0,
			acceptedMmOraclePrice,
			acceptedMmOracleSequenceId,
			new BN((await svmContextWrapper.connection.getSlot()).toString())
		);

		// Fill-bench market: real AMM depth, oracle/MM-oracle/curve aligned at 1,
		// curve_update_intensity engaged so the fill's routing projection runs.
		const fillAmmReserve = new BN(5 * 10 ** 13).mul(new BN(100_000));
		await velocityClient.initializePerpMarket(
			fillMarketIndex,
			solUsd,
			fillAmmReserve,
			fillAmmReserve,
			new BN(60 * 60)
		);
		await velocityClient.updatePerpMarketCurveUpdateIntensity(
			fillMarketIndex,
			100
		);
		await velocityClient.updatePerpMarketBaseSpread(fillMarketIndex, 500);
		await velocityClient.updateMmOracleNative(
			fillMarketIndex,
			fillMmOraclePrice,
			fillMmOracleSequenceId,
			new BN((await svmContextWrapper.connection.getSlot()).toString())
		);
		// Batch-bench markets. No AMM depth or curve work needed: the batch
		// handler only touches `market_stats`, so a bare initialized market
		// exercises exactly the same path a mainnet market would.
		for (const marketIndex of batchMarketIndexes) {
			await velocityClient.initializePerpMarket(
				marketIndex,
				solUsd,
				new BN(1000),
				new BN(1000),
				new BN(60 * 60)
			);
		}

		await velocityClient.deposit(
			new BN(10 * 10 ** 6),
			0,
			userUSDCAccount.publicKey
		);
		await velocityClient.fetchAccounts();
	});

	after(async () => {
		if (velocityClient?.isSubscribed) {
			await velocityClient.unsubscribe();
		}
		if (originalConsoleLog) {
			console.log = originalConsoleLog;
		}
	});

	async function advancePastMmOracleRateLimit(): Promise<void> {
		await svmContextWrapper.connection.updateSlotAndClock();
	}

	/**
	 * The current LiteSVM slot, used as the source-observation slot. The program's
	 * `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` freshness gate then skips no write.
	 */
	async function sourceSlot(): Promise<BN> {
		return new BN(
			(await svmContextWrapper.connection.getSlot()).toString()
		);
	}

	async function getNativeInstructionComputeUnits(
		txSig: string
	): Promise<number> {
		const computeUnits = await findComputeUnitConsumption(
			chProgram.programId,
			svmContextWrapper.connection.toConnection(),
			txSig
		);
		assert.strictEqual(
			computeUnits.length,
			1,
			`expected one Velocity CU log, got ${computeUnits.length}`
		);
		return Number(computeUnits[0]);
	}

	async function sendAcceptedMmOracleUpdate(): Promise<string> {
		await advancePastMmOracleRateLimit();
		const nextPrice = acceptedMmOraclePrice.addn(1);
		const nextSequenceId = acceptedMmOracleSequenceId.addn(1);
		const txSig = await velocityClient.updateMmOracleNative(
			0,
			nextPrice,
			nextSequenceId,
			await sourceSlot()
		);
		acceptedMmOraclePrice = nextPrice;
		acceptedMmOracleSequenceId = nextSequenceId;
		return txSig;
	}

	/**
	 * Sends an `update_mm_oracle_batch_native` covering the first `count` batch
	 * markets, and asserts that the chain accepted every entry.
	 *
	 * A batch whose entries are all skipped still lands, still consumes CU, and
	 * costs less than the real path. Without the assertion, a regression that
	 * stops the writes landing makes this bench report a better number instead of
	 * failing. A raised `MM_ORACLE_MIN_SLOT_GAP`, a transposed price or
	 * sequence id in the payload, and a market-index mismatch all do that.
	 */
	async function sendAcceptedMmOracleBatch(count: number): Promise<string> {
		await advancePastMmOracleRateLimit();
		batchMmOraclePrice = batchMmOraclePrice.addn(1);
		batchMmOracleSequenceId = batchMmOracleSequenceId.addn(1);
		const marketIndexes = batchMarketIndexes.slice(0, count);

		const observedAt = await sourceSlot();
		const txSig = await velocityClient.updateMmOracleBatchNative(
			marketIndexes.map((marketIndex) => ({
				marketIndex,
				oraclePrice: batchMmOraclePrice,
				oracleSequenceId: batchMmOracleSequenceId,
				oracleSourceSlot: observedAt,
			})),
			// Generous limit: this measures consumption, not the budget.
			{ computeUnits: 50_000, computeUnitsPrice: 0 }
		);

		await velocityClient.fetchAccounts();
		for (const marketIndex of marketIndexes) {
			const stats =
				velocityClient.getPerpMarketAccountOrThrow(marketIndex).marketStats;
			assert.strictEqual(
				stats.mmOracleSequenceId.toString(),
				batchMmOracleSequenceId.toString(),
				`market ${marketIndex} did not accept the batch write`
			);
			assert.strictEqual(
				stats.mmOraclePrice.toString(),
				batchMmOraclePrice.toString(),
				`market ${marketIndex} kept a stale price`
			);
		}

		return txSig;
	}

	async function runBench(
		instruction: string,
		path: string,
		fn: () => Promise<number>
	): Promise<BenchmarkResult> {
		return {
			instruction,
			path,
			measurement: { cu: await fn() },
		};
	}

	it('native admin fast paths', async () => {
		const mmSuccess = await runBench(
			'update_mm_oracle_native',
			'success write',
			async () => {
				const txSig = await sendAcceptedMmOracleUpdate();
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		const mmStaleSequence = await runBench(
			'update_mm_oracle_native',
			'stale sequence noop',
			async () => {
				const txSig = await velocityClient.updateMmOracleNative(
					0,
					acceptedMmOraclePrice.addn(1),
					acceptedMmOracleSequenceId,
					await sourceSlot()
				);
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		const mmMinSlotGap = await runBench(
			'update_mm_oracle_native',
			'min slot gap noop',
			async () => {
				await sendAcceptedMmOracleUpdate();
				const txSig = await velocityClient.updateMmOracleNative(
					0,
					acceptedMmOraclePrice.addn(1),
					acceptedMmOracleSequenceId.addn(1),
					await sourceSlot()
				);
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		const mmStepCap = await runBench(
			'update_mm_oracle_native',
			'step cap clamp',
			async () => {
				await advancePastMmOracleRateLimit();
				// A 5% jump, beyond the 1% cap, so the program clamps the write to
				// the cap and does not drop it. Track what landed, so later benches
				// keep sending accepted updates.
				const nextSequenceId = acceptedMmOracleSequenceId.addn(1);
				const txSig = await velocityClient.updateMmOracleNative(
					0,
					acceptedMmOraclePrice.muln(105).divn(100),
					nextSequenceId,
					await sourceSlot()
				);
				acceptedMmOraclePrice = acceptedMmOraclePrice.muln(101).divn(100);
				acceptedMmOracleSequenceId = nextSequenceId;
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		const ammSpread = await runBench(
			'update_amm_spread_adjustment_native',
			'success write',
			async () => {
				ammSpreadAdjustment = (ammSpreadAdjustment + 1) % 100;
				const txSig = await velocityClient.updateAmmSpreadAdjustmentNative(
					0,
					ammSpreadAdjustment
				);
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		// Warm-up, and not measured. Markets 2-5 start with `mm_oracle_price == 0`,
		// which takes the bootstrap branch and skips the step-cap arithmetic.
		// Without this call every measured row holds some markets on the cheap
		// path, and fewer of them as n grows. The slope would then measure
		// bootstrap against steady state rather than the marginal cost.
		await sendAcceptedMmOracleBatch(batchMarketIndexes.length);

		// Batch handler at 1, 2 and 4 markets. The n=1 row is the control. It
		// isolates the batch framing overhead against the single-market handler.
		// The slope between the rows is the marginal per-market cost that the fixed
		// prologue amortises over.
		const mmBatchOne = await runBench(
			'update_mm_oracle_batch_native',
			'success write, 1 market',
			async () =>
				getNativeInstructionComputeUnits(await sendAcceptedMmOracleBatch(1))
		);
		const mmBatchTwo = await runBench(
			'update_mm_oracle_batch_native',
			'success write, 2 markets',
			async () =>
				getNativeInstructionComputeUnits(await sendAcceptedMmOracleBatch(2))
		);
		const mmBatchFour = await runBench(
			'update_mm_oracle_batch_native',
			'success write, 4 markets',
			async () =>
				getNativeInstructionComputeUnits(await sendAcceptedMmOracleBatch(4))
		);

		// Every entry rejected. Cheaper than the all-accepted row despite paying
		// for the reject-mask `msg!`, because it skips all four writes.
		const mmBatchAllRejected = await runBench(
			'update_mm_oracle_batch_native',
			'all rejected, 4 markets',
			async () => {
				await advancePastMmOracleRateLimit();
				const observedAt = await sourceSlot();
				const txSig = await velocityClient.updateMmOracleBatchNative(
					batchMarketIndexes.map((marketIndex) => ({
						marketIndex,
						oraclePrice: batchMmOraclePrice,
						oracleSequenceId: batchMmOracleSequenceId,
						oracleSourceSlot: observedAt,
					})),
					{ computeUnits: 50_000, computeUnitsPrice: 0 }
				);
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		// The worst case, and the row the SDK's default compute budget is fitted to.
		// A partly rejected batch pays for n-1 writes and for the reject-mask log.
		// It costs more than an all-accepted batch, which writes no log, and more
		// than an all-rejected batch, which performs no write. The setup cranks one
		// market on its own and then batches at once, which leaves that market
		// inside MM_ORACLE_MIN_SLOT_GAP while the rest clear it.
		const mmBatchPartial = await runBench(
			'update_mm_oracle_batch_native',
			'3 of 4 accepted, 4 markets',
			async () => {
				await advancePastMmOracleRateLimit();
				batchMmOraclePrice = batchMmOraclePrice.addn(1);
				batchMmOracleSequenceId = batchMmOracleSequenceId.addn(1);
				await velocityClient.updateMmOracleNative(
					batchMarketIndexes[0],
					batchMmOraclePrice,
					batchMmOracleSequenceId,
					await sourceSlot()
				);

				// Deliberately no slot advance: market[0] is now rate-limited.
				batchMmOraclePrice = batchMmOraclePrice.addn(1);
				batchMmOracleSequenceId = batchMmOracleSequenceId.addn(1);
				const observedAt = await sourceSlot();
				const txSig = await velocityClient.updateMmOracleBatchNative(
					batchMarketIndexes.map((marketIndex) => ({
						marketIndex,
						oraclePrice: batchMmOraclePrice,
						oracleSequenceId: batchMmOracleSequenceId,
						oracleSourceSlot: observedAt,
					})),
					{ computeUnits: 50_000, computeUnitsPrice: 0 }
				);
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		printComputeUnitTable('Fast paths', [
			mmSuccess,
			mmStaleSequence,
			mmMinSlotGap,
			mmStepCap,
			mmBatchOne,
			mmBatchTwo,
			mmBatchFour,
			mmBatchAllRejected,
			mmBatchPartial,
			ammSpread,
		]);

		// Four separate single-market transactions against one batch of four. The
		// assertion pins the direction, so a regression that erases the saving
		// fails the bench.
		const fourSingles = mmSuccess.measurement.cu * 4;
		assert(
			mmBatchFour.measurement.cu < fourSingles,
			`batch of 4 (${mmBatchFour.measurement.cu} CU) must beat 4 singles (${fourSingles} CU)`
		);
		console.log(
			`4 markets: ${fourSingles} CU as separate instructions vs ` +
				`${mmBatchFour.measurement.cu} CU batched ` +
				`(${(fourSingles / mmBatchFour.measurement.cu).toFixed(2)}x)`
		);
		console.log('');

		assertOptionalMax(
			'mm_oracle_success',
			mmSuccess.measurement,
			'NATIVE_CU_MAX_MM_SUCCESS'
		);
		assertOptionalMax(
			'mm_oracle_stale_sequence_noop',
			mmStaleSequence.measurement,
			'NATIVE_CU_MAX_MM_STALE_SEQUENCE'
		);
		assertOptionalMax(
			'mm_oracle_min_slot_gap_noop',
			mmMinSlotGap.measurement,
			'NATIVE_CU_MAX_MM_MIN_SLOT_GAP'
		);
		assertOptionalMax(
			'mm_oracle_step_cap_clamp',
			mmStepCap.measurement,
			'NATIVE_CU_MAX_MM_STEP_CAP'
		);
		assertOptionalMax(
			'amm_spread_adjustment_success',
			ammSpread.measurement,
			'NATIVE_CU_MAX_AMM_SPREAD'
		);
		assertOptionalMax(
			'mm_oracle_batch_four_markets',
			mmBatchFour.measurement,
			'NATIVE_CU_MAX_MM_BATCH_FOUR'
		);
		assertOptionalMax(
			'mm_oracle_batch_partial_four_markets',
			mmBatchPartial.measurement,
			'NATIVE_CU_MAX_MM_BATCH_PARTIAL'
		);
	});

	it('fill perp order against amm', async () => {
		// Reproduces the production hot path. A taker order fills against the vAMM
		// alone, which `fulfill_perp_order` reports as `orderFilledWithAmm`. No
		// crank touched the AMM this slot, so routing runs the curve projection
		// before it selects a fulfillment method.
		const fill = await runBench('fill_perp_order', 'amm fill', async () => {
			// Rest a taker market order (not measured).
			await velocityClient.placePerpOrder(
				getMarketOrderParams({
					marketIndex: fillMarketIndex,
					direction: PositionDirection.LONG,
					baseAssetAmount: BASE_PRECISION,
				})
			);
			await velocityClient.fetchAccounts();

			// Resolve the order just placed (most recent on this market).
			const orders = velocityClient
				.getUserAccount()
				.orders.filter((o) => o.marketIndex === fillMarketIndex);
			const order = orders.reduce((a, b) => (b.orderId > a.orderId ? b : a));

			// Advance the slot, then repost the MM oracle so it is fresh at the fill
			// slot while the AMM curve's `last_update_slot` still lags. No keeper
			// crank ran on this market. That is the production shape. The oracle
			// moved and the curve is stale, so routing must project before it can
			// select a fulfillment method.
			await advancePastMmOracleRateLimit();
			fillMmOracleSequenceId = fillMmOracleSequenceId.addn(1);
			await velocityClient.updateMmOracleNative(
				fillMarketIndex,
				fillMmOraclePrice,
				fillMmOracleSequenceId,
				await sourceSlot()
			);
			await velocityClient.fetchAccounts();

			const userAccountPublicKey =
				await velocityClient.getUserAccountPublicKey();
			const txSig = await velocityClient.fillPerpOrder(
				userAccountPublicKey,
				velocityClient.getUserAccount(),
				{ marketIndex: fillMarketIndex, orderId: order.orderId }
			);

			// Guard the measurement. A fill that returns zero base still consumes CU
			// without exercising the routing and settlement path this bench measures.
			// A tripped guardrail or a no-cross produces such a fill.
			await velocityClient.fetchAccounts();
			const position = velocityClient
				.getUserAccount()
				.perpPositions.find((p) => p.marketIndex === fillMarketIndex);
			assert(
				position !== undefined && !position.baseAssetAmount.isZero(),
				'fill bench did not fill: position base is zero'
			);

			return await getNativeInstructionComputeUnits(txSig);
		});

		printComputeUnitTable('Fill path', [fill]);

		assertOptionalMax(
			'fill_perp_order_amm',
			fill.measurement,
			'NATIVE_CU_MAX_FILL'
		);
	});
});
