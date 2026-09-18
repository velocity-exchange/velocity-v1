import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert, expect } from 'chai';
import {
	BN,
	ExchangeStatus,
	HotRole,
	getPythLazerOraclePublicKey,
	getTokenAmount,
	loadKeypair,
	OracleGuardRails,
	OracleSource,
	SpotBalanceType,
	TestClient,
	Wallet,
} from '../../packages/sdk/src';

import {
	decodeName,
	DEFAULT_MARKET_NAME,
} from '../../packages/sdk/src/userName';

import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
} from './testHelpers';
import { Keypair, PublicKey } from '@solana/web3.js';
import {
	LiteSVMContextWrapper,
	Connection,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import { createTransferCheckedInstruction } from '@solana/spl-token';

describe('admin', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let bulkAccountLoader: TestBulkAccountLoader;

	let velocityClient: TestClient;

	let usdcMint;

	let userUSDCAccount;

	const usdcAmount = new BN(10 * 10 ** 6);

	let svmContextWrapper: LiteSVMContextWrapper;

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
			connection: svmContextWrapper.connection.toConnection(), // ugh.
			wallet,
			programID: chProgram.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
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

		const periodicity = new BN(60 * 60); // 1 HOUR

		const solUsd = await mockOracleNoProgram(svmContextWrapper, 1);
		await velocityClient.initializePerpMarket(
			0,
			solUsd,
			new BN(1000),
			new BN(1000),
			periodicity
		);

		await velocityClient.initializeAmmCache();
	});

	it('checks market name', async () => {
		const market = velocityClient.getPerpMarketAccount(0);
		const name = decodeName(market.name);
		assert(name == DEFAULT_MARKET_NAME);

		const newName = 'Glory t0 the DAmm';
		await velocityClient.updatePerpMarketName(0, newName);

		await velocityClient.fetchAccounts();
		const newMarket = velocityClient.getPerpMarketAccount(0);
		assert(
			decodeName(newMarket.name) == newName,
			`market name does not match \n actual: ${decodeName(
				newMarket.name
			)} \n expected: ${newName}`
		);
	});

	it('Update Amm Jit', async () => {
		await velocityClient.fetchAccounts();
		assert(
			velocityClient.getPerpMarketAccount(0).amm.ammJitIntensity == 0,
			`amm jit intensity does not match \n actual: ${
				velocityClient.getPerpMarketAccount(0).amm.ammJitIntensity
			} \n expected: 0`
		);

		await velocityClient.updateAmmJitIntensity(0, 100);
		await velocityClient.fetchAccounts();
		assert(
			velocityClient.getPerpMarketAccount(0).amm.ammJitIntensity == 100,
			`amm jit intensity does not match \n actual: ${
				velocityClient.getPerpMarketAccount(0).amm.ammJitIntensity
			} \n expected: 100`
		);

		await velocityClient.updateAmmJitIntensity(0, 50);
		await velocityClient.fetchAccounts();
		assert(
			velocityClient.getPerpMarketAccount(0).amm.ammJitIntensity == 50,
			`amm jit intensity does not match \n actual: ${
				velocityClient.getPerpMarketAccount(0).amm.ammJitIntensity
			} \n expected: 50`
		);
	});

	it('allows the vAMM active-management role without granting broad admin', async () => {
		const activeManagementKey = Keypair.generate();
		await svmContextWrapper.fundKeypair(activeManagementKey, 10 ** 9);

		const activeManagementClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: new Wallet(activeManagementKey),
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			useHotWalletAdmin: true,
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
		});
		await activeManagementClient.subscribe();

		try {
			const expectRejected = async (call: () => Promise<unknown>) => {
				let rejected = false;
				try {
					await call();
				} catch (_) {
					rejected = true;
				}
				expect(rejected).to.equal(true);
			};

			let rejectedWhileUnassigned = false;
			try {
				await activeManagementClient.updatePerpMarketCurveUpdateIntensity(
					0,
					42
				);
			} catch (_) {
				rejectedWhileUnassigned = true;
			}
			expect(rejectedWhileUnassigned).to.equal(true);

			// The existing native-spread bot role is separate and must not gain
			// access to quote management setters.
			await velocityClient.updateHotAdmin(
				HotRole.AmmSpreadAdjust,
				activeManagementKey.publicKey
			);
			await velocityClient.fetchAccounts();
			expect(
				velocityClient
					.getStateAccount()
					.hotAmmSpreadAdjust.equals(activeManagementKey.publicKey)
			).to.equal(true);
			let spreadBotRejected = false;
			try {
				await activeManagementClient.updatePerpMarketCurveUpdateIntensity(
					0,
					42
				);
			} catch (_) {
				spreadBotRejected = true;
			}
			expect(spreadBotRejected).to.equal(true);

			await velocityClient.updateHotAdmin(
				HotRole.VammQuoteManagement,
				activeManagementKey.publicKey
			);
			await velocityClient.fetchAccounts();
			expect(
				velocityClient
					.getStateAccount()
					.hotVammQuoteManagement.equals(activeManagementKey.publicKey)
			).to.equal(true);

			await activeManagementClient.updatePerpMarketCurveUpdateIntensity(0, 120);
			await activeManagementClient.updatePerpMarketReferencePriceOffsetDeadbandPct(
				0,
				7
			);
			await activeManagementClient.updateAmmJitIntensity(0, 20);
			await activeManagementClient.updatePerpMarketMaxSpread(0, 10_000);
			// Both spread adjustment levers expose their full meaningful band.
			await activeManagementClient.updatePerpMarketAmmSpreadAdjustment(
				0,
				-100,
				100,
				0
			);
			await activeManagementClient.updatePerpMarketAmmSpreadAdjustment(
				0,
				-5,
				12,
				0
			);
			await activeManagementClient.updatePerpMarketFundingBiasSensitivity(
				0,
				25
			);
			await velocityClient.fetchAccounts();
			const updatedAmm = velocityClient.getPerpMarketAccount(0).amm;
			expect(updatedAmm.curveUpdateIntensity).to.equal(120);
			expect(updatedAmm.referencePriceOffsetDeadbandPct).to.equal(7);
			expect(updatedAmm.ammJitIntensity).to.equal(20);
			expect(updatedAmm.maxSpread).to.equal(10_000);
			expect(updatedAmm.ammSpreadAdjustment).to.equal(-5);
			expect(updatedAmm.ammInventorySpreadAdjustment).to.equal(12);
			expect(updatedAmm.fundingBiasSensitivity).to.equal(25);

			await expectRejected(() =>
				activeManagementClient.updatePerpMarketCurveUpdateIntensity(0, 99)
			);
			await expectRejected(() =>
				activeManagementClient.updatePerpMarketReferencePriceOffsetDeadbandPct(
					0,
					26
				)
			);
			await expectRejected(() =>
				activeManagementClient.updateAmmJitIntensity(0, 26)
			);
			await expectRejected(() =>
				activeManagementClient.updatePerpMarketMaxSpread(0, 20_001)
			);
			await expectRejected(() =>
				activeManagementClient.updatePerpMarketAmmSpreadAdjustment(0, 101, 0, 0)
			);
			await expectRejected(() =>
				activeManagementClient.updatePerpMarketAmmSpreadAdjustment(0, 0, 101, 0)
			);
			await expectRejected(() =>
				activeManagementClient.updatePerpMarketFundingBiasSensitivity(0, 101)
			);
			await velocityClient.fetchAccounts();
			expect(
				velocityClient.getPerpMarketAccount(0).amm.ammSpreadAdjustment
			).to.equal(-5);
			expect(
				velocityClient.getPerpMarketAccount(0).amm.ammInventorySpreadAdjustment
			).to.equal(12);

			// Warm/cold governance is intentionally not trapped by the hot role
			// bounds and can still apply the setter's wider semantic range.
			await velocityClient.updatePerpMarketCurveUpdateIntensity(0, 100);
			await velocityClient.fetchAccounts();
			expect(
				velocityClient.getPerpMarketAccount(0).amm.curveUpdateIntensity
			).to.equal(100);

			// A broad warm-admin instruction remains unavailable to this role.
			let broadAdminRejected = false;
			try {
				const ix =
					await activeManagementClient.program.instruction.updatePerpMarketBaseSpread(
						velocityClient.getPerpMarketAccountOrThrow(0).amm.baseSpread,
						{
							accounts: {
								admin: activeManagementKey.publicKey,
								state: await activeManagementClient.getStatePublicKey(),
								perpMarket:
									velocityClient.getPerpMarketAccountOrThrow(0).pubkey,
							},
						}
					);
				const tx = await activeManagementClient.buildTransaction(ix);
				await activeManagementClient.sendTransaction(tx, []);
			} catch (_) {
				broadAdminRejected = true;
			}
			expect(broadAdminRejected).to.equal(true);
		} finally {
			await activeManagementClient.unsubscribe();
		}
	});

	it('Update Margin Ratio', async () => {
		const marginRatioInitial = 3000;
		const marginRatioMaintenance = 1000;

		await velocityClient.updatePerpMarketMarginRatio(
			0,
			marginRatioInitial,
			marginRatioMaintenance
		);

		await velocityClient.fetchAccounts();
		const market = velocityClient.getPerpMarketAccount(0);

		assert(
			market.marginRatioInitial === marginRatioInitial,
			`margin ratio initial does not match \n actual: ${market.marginRatioInitial} \n expected: ${marginRatioInitial}`
		);
		assert(
			market.marginRatioMaintenance === marginRatioMaintenance,
			`margin ratio maintenance does not match \n actual: ${market.marginRatioMaintenance} \n expected: ${marginRatioMaintenance}`
		);
	});

	it('Update perp fee structure', async () => {
		const newFeeStructure = velocityClient.getStateAccount().perpFeeStructure;
		newFeeStructure.flatFillerFee = new BN(0);

		await velocityClient.updatePerpFeeStructure(newFeeStructure);

		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();

		assert(
			JSON.stringify(newFeeStructure) ===
				JSON.stringify(state.perpFeeStructure),
			`fee structure does not match \n actual: ${JSON.stringify(
				state.perpFeeStructure
			)} \n expected: ${JSON.stringify(newFeeStructure)}`
		);
	});

	it('Update spot fee structure', async () => {
		const newFeeStructure = velocityClient.getStateAccount().spotFeeStructure;
		newFeeStructure.flatFillerFee = new BN(1);

		await velocityClient.updateSpotFeeStructure(newFeeStructure);

		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();

		assert(
			JSON.stringify(newFeeStructure) ===
				JSON.stringify(state.spotFeeStructure),
			`fee structure does not match \n actual: ${JSON.stringify(
				state.spotFeeStructure
			)} \n expected: ${JSON.stringify(newFeeStructure)}`
		);
	});

	it('Update oracle guard rails', async () => {
		const oracleGuardRails: OracleGuardRails = {
			priceDivergence: {
				markOraclePercentDivergence: new BN(1000000),
				oracleTwap5MinPercentDivergence: new BN(1000000),
			},
			validity: {
				slotsBeforeStaleForAmm: new BN(1),
				slotsBeforeStaleForMargin: new BN(1),
				confidenceIntervalMaxSize: new BN(1),
				tooVolatileRatio: new BN(1),
			},
		};

		await velocityClient.updateOracleGuardRails(oracleGuardRails);

		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();

		assert(
			JSON.stringify(oracleGuardRails) ===
				JSON.stringify(state.oracleGuardRails),
			`oracle guard rails does not match \n actual: ${JSON.stringify(
				state.oracleGuardRails
			)} \n expected: ${JSON.stringify(oracleGuardRails)}`
		);
	});

	it('Update protocol mint', async () => {
		const mint = new PublicKey('2fvh6hkCYfpNqke9N48x6HcrW92uZVU3QSiXZX4A5L27');

		await velocityClient.updateDiscountMint(mint);

		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();

		assert(
			state.discountMint.equals(mint),
			`discount mint does not match \n actual: ${state.discountMint} \n expected: ${mint}`
		);
	});

	// it('Update max deposit', async () => {
	//  const maxDeposit = new BN(10);

	//  await velocityClient.updateMaxDeposit(maxDeposit);

	//  await velocityClient.fetchAccounts();
	//  const state = velocityClient.getStateAccount();

	//  assert(state.maxDeposit.eq(maxDeposit));
	// });

	it('Update market oracle', async () => {
		const newOracle = PublicKey.default;
		const newOracleSource = OracleSource.QUOTE_ASSET;

		await velocityClient.updatePerpMarketOracle(0, newOracle, newOracleSource);

		await velocityClient.fetchAccounts();
		const market = velocityClient.getPerpMarketAccount(0);
		assert(
			market.oracle.equals(PublicKey.default),
			`oracle does not match \n actual: ${market.oracle} \n expected: ${PublicKey.default}`
		);
		assert(
			JSON.stringify(market.oracleSource) === JSON.stringify(newOracleSource),
			`oracle source does not match \n actual: ${JSON.stringify(
				market.oracleSource
			)} \n expected: ${JSON.stringify(newOracleSource)}`
		);
	});

	it('Update market base asset step size', async () => {
		const stepSize = new BN(2);
		const tickSize = new BN(2);

		await velocityClient.updatePerpMarketStepSizeAndTickSize(
			0,
			stepSize,
			tickSize
		);

		await velocityClient.fetchAccounts();
		const market = velocityClient.getPerpMarketAccount(0);
		assert(
			market.orderStepSize.eq(stepSize),
			`step size does not match \n actual: ${market.orderStepSize} \n expected: ${stepSize}`
		);
		assert(
			market.orderTickSize.eq(tickSize),
			`tick size does not match \n actual: ${market.orderTickSize} \n expected: ${tickSize}`
		);
	});

	it('Pause liq', async () => {
		await velocityClient.updateExchangeStatus(ExchangeStatus.LIQ_PAUSED);
		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();
		assert(
			state.exchangeStatus === ExchangeStatus.LIQ_PAUSED,
			`exchange status does not match \n actual: ${state.exchangeStatus} \n expected: ${ExchangeStatus.LIQ_PAUSED}`
		);

		console.log('paused liq!');
		// unpause
		await velocityClient.updateExchangeStatus(ExchangeStatus.ACTIVE);
		await velocityClient.fetchAccounts();
		const state2 = velocityClient.getStateAccount();
		assert(
			state2.exchangeStatus === ExchangeStatus.ACTIVE,
			`exchange status does not match \n actual: ${state2.exchangeStatus} \n expected: ${ExchangeStatus.ACTIVE}`
		);
		console.log('unpaused liq!');
	});

	it('Pause amm', async () => {
		await velocityClient.updateExchangeStatus(ExchangeStatus.AMM_PAUSED);
		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();
		assert(
			state.exchangeStatus === ExchangeStatus.AMM_PAUSED,
			`exchange status does not match \n actual: ${state.exchangeStatus} \n expected: ${ExchangeStatus.AMM_PAUSED}`
		);

		console.log('paused amm!');
		// unpause
		await velocityClient.updateExchangeStatus(ExchangeStatus.ACTIVE);
		await velocityClient.fetchAccounts();
		const state2 = velocityClient.getStateAccount();
		assert(
			state2.exchangeStatus === ExchangeStatus.ACTIVE,
			`exchange status does not match \n actual: ${state2.exchangeStatus} \n expected: ${ExchangeStatus.ACTIVE}`
		);
		console.log('unpaused amm!');
	});

	it('Pause funding', async () => {
		await velocityClient.updateExchangeStatus(ExchangeStatus.FUNDING_PAUSED);
		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();
		assert(
			state.exchangeStatus === ExchangeStatus.FUNDING_PAUSED,
			`exchange status does not match \n actual: ${state.exchangeStatus} \n expected: ${ExchangeStatus.FUNDING_PAUSED}`
		);

		console.log('paused funding!');
		// unpause
		await velocityClient.updateExchangeStatus(ExchangeStatus.ACTIVE);
		await velocityClient.fetchAccounts();
		const state2 = velocityClient.getStateAccount();
		assert(
			state2.exchangeStatus === ExchangeStatus.ACTIVE,
			`exchange status does not match \n actual: ${state2.exchangeStatus} \n expected: ${ExchangeStatus.ACTIVE}`
		);
		console.log('unpaused funding!');
	});

	it('Pause deposts and withdraws', async () => {
		await velocityClient.updateExchangeStatus(
			ExchangeStatus.DEPOSIT_PAUSED | ExchangeStatus.WITHDRAW_PAUSED
		);
		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();
		assert(
			state.exchangeStatus ===
				(ExchangeStatus.DEPOSIT_PAUSED | ExchangeStatus.WITHDRAW_PAUSED),
			`exchange status does not match \n actual: ${
				state.exchangeStatus
			} \n expected: ${
				ExchangeStatus.DEPOSIT_PAUSED | ExchangeStatus.WITHDRAW_PAUSED
			}`
		);

		console.log('paused deposits and withdraw!');
		// unpause
		await velocityClient.updateExchangeStatus(ExchangeStatus.ACTIVE);
		await velocityClient.fetchAccounts();
		const state2 = velocityClient.getStateAccount();
		assert(
			state2.exchangeStatus === ExchangeStatus.ACTIVE,
			`exchange status does not match \n actual: ${state2.exchangeStatus} \n expected: ${ExchangeStatus.ACTIVE}`
		);
		console.log('unpaused deposits and withdraws!');
	});

	it('Init pyth lazer', async () => {
		await velocityClient.fetchAccounts();
		const tx = await velocityClient.initializePythLazerOracle(0);
		console.log(tx);

		assert(
			await checkIfAccountExists(
				velocityClient.connection,
				getPythLazerOraclePublicKey(velocityClient.program.programId, 0)
			)
		);
	});

	it('update MM oracle native', async () => {
		// Use a realistic baseline in PRICE_PRECISION so the small numeric increments
		// stay well under the 1% step cap enforced by the native handler.
		const oraclePrice = new BN(100_000_000);
		const oracleTS = new BN(Date.now());
		const sourceSlot = async () =>
			new BN((await svmContextWrapper.connection.getSlot()).toString());
		await velocityClient.updateFeatureBitFlagsMMOracle(true);
		await velocityClient.updateMmOracleNative(
			0,
			oraclePrice,
			oracleTS,
			await sourceSlot()
		);
		await velocityClient.fetchAccounts();

		let perpMarket = velocityClient.getPerpMarketAccount(0);
		assert(perpMarket.marketStats.mmOraclePrice.eq(oraclePrice));
		const slot = (await svmContextWrapper.connection.getSlot()).toString();
		expect(perpMarket.marketStats.mmOracleSlot.toNumber()).to.be.approximately(
			+slot,
			1
		);
		assert(perpMarket.marketStats.mmOracleSequenceId.eq(oracleTS));

		// Doesnt change if id doesnt increase
		await velocityClient.updateMmOracleNative(
			0,
			oraclePrice.addn(1),
			oracleTS,
			await sourceSlot()
		);
		assert(perpMarket.marketStats.mmOraclePrice.eq(oraclePrice));

		// The builder rejects a zero price before it reaches the chain (the
		// program hard-errors on any non-positive price).
		try {
			await velocityClient.updateMmOracleNative(
				0,
				new BN(0),
				oracleTS,
				await sourceSlot()
			);
			assert.fail('Should have thrown');
		} catch (e) {
			console.log(e.message);
			assert(e.message.includes('non-positive price'));
		}

		// So does a negative price, which BN's little-endian serialization
		// would otherwise silently send as its magnitude.
		try {
			await velocityClient.updateMmOracleNative(
				0,
				new BN(-1),
				oracleTS,
				await sourceSlot()
			);
			assert.fail('Should have thrown');
		} catch (e) {
			assert(e.message.includes('non-positive price'));
		}

		// Skipped (not an error) when the source slot is too old: the update
		// landed later than MM_ORACLE_MAX_SOURCE_AGE_SLOTS after observation.
		await svmContextWrapper.connection.updateSlotAndClock();
		await svmContextWrapper.connection.updateSlotAndClock();
		const staleSource = (await sourceSlot()).subn(3);
		await velocityClient.updateMmOracleNative(
			0,
			oraclePrice.addn(5),
			oracleTS.addn(5),
			staleSource
		);
		await velocityClient.fetchAccounts();
		perpMarket = velocityClient.getPerpMarketAccount(0);
		assert(
			perpMarket.marketStats.mmOracleSequenceId.eq(oracleTS),
			'stale-source update must be skipped'
		);

		// Doesnt update if we flip the admin switch
		await velocityClient.updateFeatureBitFlagsMMOracle(false);
		try {
			await velocityClient.updateMmOracleNative(
				0,
				oraclePrice,
				oracleTS,
				await sourceSlot()
			);
			assert.fail('Should have thrown');
		} catch (e) {
			console.log(e.message);
			// Typed error (MmOracleUpdateDisabled) rather than the old panic.
			assert(e.message.includes('custom program error'));
		}

		// Re-enable and update
		await velocityClient.updateFeatureBitFlagsMMOracle(true);
		await velocityClient.updateMmOracleNative(
			0,
			oraclePrice.addn(2),
			oracleTS.addn(1),
			await sourceSlot()
		);
		await velocityClient.fetchAccounts();
		perpMarket = velocityClient.getPerpMarketAccount(0);
		assert(perpMarket.marketStats.mmOraclePrice.eq(oraclePrice.addn(2)));
		assert(perpMarket.marketStats.mmOracleSequenceId.eq(oracleTS.addn(1)));
	});

	it('mm oracle step cap clamps a too-large jump and converges', async () => {
		// Each send advances the LiteSVM slot by one; a second advance clears the
		// program's MM_ORACLE_MIN_SLOT_GAP of 2 so the write reaches the step cap
		// instead of being skipped by the rate limit.
		const advancePastRateLimit = () =>
			svmContextWrapper.connection.updateSlotAndClock();

		await velocityClient.fetchAccounts();
		const before = velocityClient.getPerpMarketAccount(0);
		const baselinePrice = before.marketStats.mmOraclePrice;
		const baselineSeqId = before.marketStats.mmOracleSequenceId;

		// 5% jump from the last accepted price exceeds the 1% step cap. The
		// write is clamped to the cap rather than rejected: rejecting left the
		// stored price where it was, so every subsequent update was still beyond
		// the cap against the same stale value and the oracle froze permanently.
		const tooLargePrice = baselinePrice.muln(105).divn(100);
		const freshSeqId = baselineSeqId.addn(1000);
		const expectedFirstStep = baselinePrice.muln(101).divn(100);

		const sourceSlot = async () =>
			new BN((await svmContextWrapper.connection.getSlot()).toString());
		await advancePastRateLimit();
		await velocityClient.updateMmOracleNative(
			0,
			tooLargePrice,
			freshSeqId,
			await sourceSlot()
		);
		await velocityClient.fetchAccounts();

		const after = velocityClient.getPerpMarketAccount(0);
		assert(
			after.marketStats.mmOraclePrice.eq(expectedFirstStep),
			`expected clamp to ${expectedFirstStep.toString()}, got ${after.marketStats.mmOraclePrice.toString()}`
		);
		assert(
			after.marketStats.mmOracleSequenceId.eq(freshSeqId),
			'sequence id should advance: the update was consumed, not dropped'
		);

		// And it keeps closing the gap. Resending the same target walks another
		// cap-width, where the old behaviour would have stalled forever.
		let current = after.marketStats.mmOraclePrice;
		let seqId = freshSeqId;
		for (let i = 0; i < 5 && !current.eq(tooLargePrice); i++) {
			seqId = seqId.addn(1);
			await advancePastRateLimit();
			await velocityClient.updateMmOracleNative(
				0,
				tooLargePrice,
				seqId,
				await sourceSlot()
			);
			await velocityClient.fetchAccounts();
			const next =
				velocityClient.getPerpMarketAccount(0).marketStats.mmOraclePrice;
			assert(next.gt(current), 'price must keep moving toward the target');
			current = next;
		}
		assert(
			current.eq(tooLargePrice),
			`should have converged on ${tooLargePrice.toString()}, got ${current.toString()}`
		);
	});

	it('update amm adjustment oracle native', async () => {
		const ammSpreadAdjustment = 5;
		await velocityClient.updateAmmSpreadAdjustmentNative(
			0,
			ammSpreadAdjustment
		);
		await velocityClient.fetchAccounts();
		const perpMarket = velocityClient.getPerpMarketAccount(0);
		assert(perpMarket.amm.ammSpreadAdjustment == ammSpreadAdjustment);
	});

	it('update perp market reference offset deadband pct', async () => {
		const referenceOffsetDeadbandPct = 5;
		await velocityClient.updatePerpMarketReferencePriceOffsetDeadbandPct(
			0,
			referenceOffsetDeadbandPct
		);
		await velocityClient.fetchAccounts();
		const perpMarket = velocityClient.getPerpMarketAccount(0);
		assert(
			perpMarket.amm.referencePriceOffsetDeadbandPct ==
				referenceOffsetDeadbandPct
		);
	});

	it('update pnl pool', async () => {
		const quoteVault = velocityClient.getSpotMarketAccount(0).vault;

		const splTransferIx = createTransferCheckedInstruction(
			userUSDCAccount.publicKey,
			usdcMint.publicKey,
			quoteVault,
			velocityClient.wallet.publicKey,
			usdcAmount.toNumber(),
			6
		);

		const tx = await velocityClient.buildTransaction(splTransferIx);
		// @ts-ignore
		await velocityClient.sendTransaction(tx);

		await velocityClient.updatePerpMarketPnlPool(0, usdcAmount);

		await velocityClient.fetchAccounts();

		const perpMarket = velocityClient.getPerpMarketAccount(0);
		const spotMarket = velocityClient.getSpotMarketAccount(0);

		const tokenAmount = getTokenAmount(
			perpMarket.pnlPool.scaledBalance,
			spotMarket,
			SpotBalanceType.DEPOSIT
		);

		assert(tokenAmount.eq(usdcAmount));
	});

	it('Update admin', async () => {
		const newAdminKey = PublicKey.default;

		await velocityClient.updateAdmin(newAdminKey);

		await velocityClient.fetchAccounts();
		const state = velocityClient.getStateAccount();

		assert(
			state.coldAdmin.equals(newAdminKey),
			`admin does not match \n actual: ${state.coldAdmin} \n expected: ${newAdminKey}`
		);
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});
});

async function checkIfAccountExists(
	connection: Connection,
	account: PublicKey
): Promise<boolean> {
	try {
		const accountInfo = await connection.getAccountInfo(account);
		return accountInfo != null;
	} catch (e) {
		// Doesn't already exist
		return false;
	}
}
