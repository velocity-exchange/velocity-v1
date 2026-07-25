import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { startAnchor } from 'solana-bankrun';
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
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';
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

	let bankrunContextWrapper: BankrunContextWrapper;
	let velocityClient: TestClient;
	let originalConsoleLog: typeof console.log;

	let acceptedMmOraclePrice = new BN(100_000_000);
	let acceptedMmOracleSequenceId = new BN(1_000_000);
	let ammSpreadAdjustment = 0;

	// Dedicated market for the fill bench: oracle, MM oracle, and AMM curve all
	// aligned at price 1 so a taker fills cleanly against the vAMM, with
	// curve_update_intensity > 0 so the routing projection actually runs (that
	// is the path the dedup optimized). Kept separate from market 0, whose MM
	// oracle sits at 100 for the admin noop benches.
	const fillMarketIndex = 1;
	const fillMmOraclePrice = new BN(1_000_000); // price 1, PRICE_PRECISION
	let fillMmOracleSequenceId = new BN(1_000_000);

	before(async () => {
		originalConsoleLog = console.log;
		console.log = (...args: Parameters<typeof console.log>) => {
			if (String(args[0]).startsWith('Pausing to find oracle ')) {
				return;
			}
			originalConsoleLog(...args);
		};

		const context = await startAnchor('', [], []);
		bankrunContextWrapper = new BankrunContextWrapper(context as any);
		const defaultIdl = VelocityCore.defaultIdl();
		(VelocityCore as any).defaultIdl = () => ({
			...defaultIdl,
			address: chProgram.programId.toString(),
		});

		const bulkAccountLoader = new TestBulkAccountLoader(
			bankrunContextWrapper.connection,
			'processed',
			0
		);

		const usdcMint = await mockUSDCMint(bankrunContextWrapper);
		const wallet = new Wallet(loadKeypair(process.env.ANCHOR_WALLET));
		await bankrunContextWrapper.fundKeypair(wallet, 10 ** 9);

		velocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0, 1],
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
			bankrunContextWrapper,
			velocityClient.wallet.publicKey
		);

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await velocityClient.initializeUserAccount(0);
		await velocityClient.fetchAccounts();
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.updatePerpAuctionDuration(new BN(0));
		await velocityClient.fetchAccounts();

		const solUsd = await mockOracleNoProgram(bankrunContextWrapper, 1);
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
			acceptedMmOracleSequenceId
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
			fillMmOracleSequenceId
		);
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
		await bankrunContextWrapper.connection.updateSlotAndClock();
	}

	async function getNativeInstructionComputeUnits(
		txSig: string
	): Promise<number> {
		const computeUnits = await findComputeUnitConsumption(
			chProgram.programId,
			bankrunContextWrapper.connection.toConnection(),
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
			nextSequenceId
		);
		acceptedMmOraclePrice = nextPrice;
		acceptedMmOracleSequenceId = nextSequenceId;
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
					acceptedMmOracleSequenceId
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
					acceptedMmOracleSequenceId.addn(1)
				);
				return await getNativeInstructionComputeUnits(txSig);
			}
		);

		const mmStepCap = await runBench(
			'update_mm_oracle_native',
			'step cap noop',
			async () => {
				await advancePastMmOracleRateLimit();
				const txSig = await velocityClient.updateMmOracleNative(
					0,
					acceptedMmOraclePrice.muln(105).divn(100),
					acceptedMmOracleSequenceId.addn(1)
				);
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

		printComputeUnitTable('Fast paths', [
			mmSuccess,
			mmStaleSequence,
			mmMinSlotGap,
			mmStepCap,
			ammSpread,
		]);

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
			'mm_oracle_step_cap_noop',
			mmStepCap.measurement,
			'NATIVE_CU_MAX_MM_STEP_CAP'
		);
		assertOptionalMax(
			'amm_spread_adjustment_success',
			ammSpread.measurement,
			'NATIVE_CU_MAX_AMM_SPREAD'
		);
	});

	it('fill perp order against amm', async () => {
		// Reproduces the production hot path from the CU regression: a taker
		// order filled against the vAMM alone (`orderFilledWithAmm`), routed
		// through `fulfill_perp_order`. The AMM has not been cranked this slot,
		// so routing runs the curve projection before selecting a fulfillment
		// method. This is the shape the projection dedup targeted; run it on
		// master and on the optimized branch to read the before/after delta.
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

			// Advance the slot, then repost the MM oracle so it is fresh at the
			// fill slot while the AMM curve's `last_update_slot` still lags (no
			// keeper crank ran on this market). That is the exact production
			// shape: oracle moved, curve stale, so routing must project before
			// it can select a fulfillment method.
			await advancePastMmOracleRateLimit();
			fillMmOracleSequenceId = fillMmOracleSequenceId.addn(1);
			await velocityClient.updateMmOracleNative(
				fillMarketIndex,
				fillMmOraclePrice,
				fillMmOracleSequenceId
			);
			await velocityClient.fetchAccounts();

			const userAccountPublicKey =
				await velocityClient.getUserAccountPublicKey();
			const txSig = await velocityClient.fillPerpOrder(
				userAccountPublicKey,
				velocityClient.getUserAccount(),
				{ marketIndex: fillMarketIndex, orderId: order.orderId }
			);

			// Guard the measurement: a fill that silently zero-fills (a tripped
			// guardrail, a no-cross) would still consume CU but wouldn't exercise
			// the routing/settlement path this bench exists to measure.
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
