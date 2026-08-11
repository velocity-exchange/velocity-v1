import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert, expect } from 'chai';
import { startAnchor } from 'solana-bankrun';
import { BN, loadKeypair, TestClient, Wallet } from '../../packages/sdk/src';
import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
} from './testHelpers';
import { BankrunContextWrapper } from '../../packages/sdk/src/bankrun/bankrunConnection';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { VelocityCore } from '../../packages/sdk/src/core/VelocityCore';

/**
 * On-chain behaviour of `update_mm_oracle_batch_native` (native dispatch opcode 2).
 *
 * The Rust unit tests in `instructions::admin::native_batch_tests` cover every
 * branch of the handler against synthetic `AccountInfo`s. What they cannot cover
 * is the wire contract: that the bytes and account order the SDK builder emits
 * are the ones the handler expects. These tests run the real builder against the
 * real program, so a transposed field or a stride mistake shows up as a write
 * that did not land rather than as a green unit test.
 *
 * Lives in its own file (rather than `admin.ts`) because it needs four perp
 * markets, and adding markets to `admin.ts` would move the market counts its
 * other tests assert on.
 */
describe('mm oracle batch native', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let bankrunContextWrapper: BankrunContextWrapper;
	let velocityClient: TestClient;

	const marketIndexes = [0, 1, 2, 3];
	const BASE_PRICE = new BN(100_000_000);
	let sequenceId = new BN(1_000_000);

	/** Advance one slot; combined with the slot each send consumes this clears
	 * the program's `MM_ORACLE_MIN_SLOT_GAP` of 2. */
	async function advancePastRateLimit(): Promise<void> {
		await bankrunContextWrapper.connection.updateSlotAndClock();
	}

	function statsFor(marketIndex: number) {
		return velocityClient.getPerpMarketAccountOrThrow(marketIndex).marketStats;
	}

	/** Current bankrun slot as the source-observation slot, so the program's
	 * `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` freshness gate never skips a write. */
	async function sourceSlot(): Promise<BN> {
		return new BN(
			(await bankrunContextWrapper.connection.getSlot()).toString()
		);
	}

	before(async () => {
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
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: marketIndexes,
			spotMarketIndexes: [0],
			subAccountIds: [],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});

		await velocityClient.initialize(usdcMint.publicKey, true);
		await velocityClient.subscribe();
		await velocityClient.initializeUserAccount(0);
		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);
		await velocityClient.fetchAccounts();

		const solUsd = await mockOracleNoProgram(bankrunContextWrapper, 1);
		for (const marketIndex of marketIndexes) {
			await velocityClient.initializePerpMarket(
				marketIndex,
				solUsd,
				new BN(1000),
				new BN(1000),
				new BN(60 * 60)
			);
		}
		await velocityClient.initializeAmmCache();
		await velocityClient.updateFeatureBitFlagsMMOracle(true);
		await velocityClient.fetchAccounts();
	});

	after(async () => {
		if (velocityClient?.isSubscribed) {
			await velocityClient.unsubscribe();
		}
	});

	it('writes every market in one instruction', async () => {
		await advancePastRateLimit();
		sequenceId = sequenceId.addn(1);

		// A distinct price per market, so a positional mix-up between the payload
		// entries and the account list would be visible rather than symmetric.
		const priceFor = (marketIndex: number) =>
			BASE_PRICE.addn(1_000 * (marketIndex + 1));

		const observedAt = await sourceSlot();
		await velocityClient.updateMmOracleBatchNative(
			marketIndexes.map((marketIndex) => ({
				marketIndex,
				oraclePrice: priceFor(marketIndex),
				oracleSequenceId: sequenceId,
				oracleSourceSlot: observedAt,
			}))
		);
		await velocityClient.fetchAccounts();

		const slot = Number(await bankrunContextWrapper.connection.getSlot());
		for (const marketIndex of marketIndexes) {
			const stats = statsFor(marketIndex);
			assert(
				stats.mmOraclePrice.eq(priceFor(marketIndex)),
				`market ${marketIndex} price: got ${stats.mmOraclePrice.toString()}`
			);
			assert(
				stats.mmOracleSequenceId.eq(sequenceId),
				`market ${marketIndex} sequence id`
			);
			expect(stats.mmOracleSlot.toNumber()).to.be.approximately(slot, 1);
		}
	});

	it('skips a rate-limited market without disturbing the others', async () => {
		// Crank market 0 on its own, then immediately batch all four. Market 0 is
		// now inside MM_ORACLE_MIN_SLOT_GAP and must be skipped; the other three
		// must still be written, and the transaction must succeed.
		await advancePastRateLimit();
		sequenceId = sequenceId.addn(1);
		await velocityClient.updateMmOracleNative(
			0,
			BASE_PRICE.addn(500),
			sequenceId,
			await sourceSlot()
		);
		await velocityClient.fetchAccounts();
		const skippedBefore = statsFor(0);

		// Deliberately no advancePastRateLimit() here.
		sequenceId = sequenceId.addn(1);
		const observedAt = await sourceSlot();
		await velocityClient.updateMmOracleBatchNative(
			marketIndexes.map((marketIndex) => ({
				marketIndex,
				oraclePrice: BASE_PRICE.addn(2_000),
				oracleSequenceId: sequenceId,
				oracleSourceSlot: observedAt,
			}))
		);
		await velocityClient.fetchAccounts();

		const skippedAfter = statsFor(0);
		assert(
			skippedAfter.mmOracleSequenceId.eq(skippedBefore.mmOracleSequenceId),
			'rate-limited market must be untouched'
		);
		assert(skippedAfter.mmOraclePrice.eq(skippedBefore.mmOraclePrice));

		for (const marketIndex of [1, 2, 3]) {
			assert(
				statsFor(marketIndex).mmOracleSequenceId.eq(sequenceId),
				`market ${marketIndex} must still be written`
			);
		}
	});

	it('rejects the whole instruction when an entry names the wrong market', async () => {
		// Build a legitimate two-market batch, then point the first market slot at
		// market 2's account while the payload still says market 0. Without the
		// market-index field in the payload this would silently write market 0's
		// price onto market 2 and succeed.
		await advancePastRateLimit();
		sequenceId = sequenceId.addn(1);

		const observedAt = await sourceSlot();
		const ix = await velocityClient.getUpdateMmOracleBatchNativeIx([
			{
				marketIndex: 0,
				oraclePrice: BASE_PRICE.addn(3_000),
				oracleSequenceId: sequenceId,
				oracleSourceSlot: observedAt,
			},
			{
				marketIndex: 1,
				oraclePrice: BASE_PRICE.addn(3_000),
				oracleSequenceId: sequenceId,
				oracleSourceSlot: observedAt,
			},
		]);
		// Accounts are [signer, clock, state, market0, market1].
		ix.keys[3].pubkey = velocityClient.getPerpMarketAccountOrThrow(2).pubkey;

		const before = [0, 1, 2].map((i) =>
			statsFor(i).mmOracleSequenceId.toString()
		);

		try {
			const tx = await velocityClient.buildTransaction(ix, {
				computeUnits: 50_000,
				computeUnitsPrice: 0,
			});
			await velocityClient.sendTransaction(tx, [], velocityClient.opts);
			assert.fail('Should have thrown');
		} catch (e) {
			assert(
				e.message.includes('custom program error') ||
					e.message.includes('InvalidNativePerpMarketAccount'),
				`unexpected error: ${e.message}`
			);
		}

		await velocityClient.fetchAccounts();
		const after = [0, 1, 2].map((i) =>
			statsFor(i).mmOracleSequenceId.toString()
		);
		assert.deepStrictEqual(
			after,
			before,
			'a failed batch must leave every market untouched'
		);
	});

	it('rejects the batch when the admin kill switch is off', async () => {
		await velocityClient.updateFeatureBitFlagsMMOracle(false);
		await advancePastRateLimit();
		sequenceId = sequenceId.addn(1);

		const before = statsFor(1).mmOracleSequenceId.toString();
		try {
			await velocityClient.updateMmOracleBatchNative([
				{
					marketIndex: 1,
					oraclePrice: BASE_PRICE.addn(4_000),
					oracleSequenceId: sequenceId,
					oracleSourceSlot: await sourceSlot(),
				},
			]);
			assert.fail('Should have thrown');
		} catch (e) {
			// Opcode 0 panics here ("Program failed to complete"); the batch
			// returns a typed error instead, so the failure is identifiable.
			assert(
				e.message.includes('custom program error') ||
					e.message.includes('MmOracleUpdateDisabled'),
				`unexpected error: ${e.message}`
			);
		}

		await velocityClient.fetchAccounts();
		assert.strictEqual(statsFor(1).mmOracleSequenceId.toString(), before);

		await velocityClient.updateFeatureBitFlagsMMOracle(true);
	});

	it('skips an update whose source slot is too old', async () => {
		// The wire-level check for the payload's source-slot field: an update
		// observed more than MM_ORACLE_MAX_SOURCE_AGE_SLOTS before it lands is
		// skipped (transaction still succeeds), so a late-landing transaction
		// cannot make an old observation read as fresh.
		while ((await bankrunContextWrapper.connection.getSlot()) < 12n) {
			await advancePastRateLimit();
		}
		await advancePastRateLimit();
		sequenceId = sequenceId.addn(1);

		const before = statsFor(2).mmOracleSequenceId.toString();
		const staleSource = (await sourceSlot()).subn(11); // one past the 10-slot bound

		await velocityClient.updateMmOracleBatchNative([
			{
				marketIndex: 2,
				oraclePrice: BASE_PRICE.addn(5_000),
				oracleSequenceId: sequenceId,
				oracleSourceSlot: staleSource,
			},
		]);
		await velocityClient.fetchAccounts();

		assert.strictEqual(
			statsFor(2).mmOracleSequenceId.toString(),
			before,
			'stale-source update must be skipped'
		);
	});

	it('rejects malformed input in the builder before it reaches the chain', async () => {
		const valid = {
			marketIndex: 0,
			oraclePrice: BASE_PRICE,
			oracleSequenceId: sequenceId,
			oracleSourceSlot: await sourceSlot(),
		};

		const cases: [
			string,
			Parameters<typeof velocityClient.getUpdateMmOracleBatchNativeIx>[0],
		][] = [
			['empty batch', []],
			[
				'duplicate market index',
				[valid, { ...valid, oraclePrice: BASE_PRICE.addn(1) }],
			],
			['zero price', [{ ...valid, oraclePrice: new BN(0) }]],
			// BN's little-endian serialization drops the sign, so without this
			// guard a negative price reaches the program as its magnitude.
			['negative price', [{ ...valid, oraclePrice: BASE_PRICE.neg() }]],
			// A positive value above i64::MAX serializes into 8 bytes but is
			// parsed on chain as a negative i64.
			[
				'price above i64::MAX',
				[{ ...valid, oraclePrice: new BN(2).pow(new BN(63)) }],
			],
			[
				'source slot above u64::MAX',
				[{ ...valid, oracleSourceSlot: new BN(2).pow(new BN(64)) }],
			],
		];

		for (const [label, updates] of cases) {
			let threw = false;
			try {
				await velocityClient.getUpdateMmOracleBatchNativeIx(updates);
			} catch {
				threw = true;
			}
			assert(threw, `builder accepted ${label}`);
		}
	});
});
