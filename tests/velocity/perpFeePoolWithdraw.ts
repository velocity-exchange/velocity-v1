import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { IdlCoder } from '@coral-xyz/anchor/dist/cjs/coder/borsh/idl';
import { assert } from 'chai';
import { Keypair } from '@solana/web3.js';
import { getAssociatedTokenAddressSync } from '@solana/spl-token';
import {
	BANKRUPTCY_IF_FLOOR_DISABLED,
	BN,
	getTokenAmount,
	MarketStatus,
	MarketType,
	OracleSource,
	PositionDirection,
	PositionFlag,
	SpotBalanceType,
	TestClient,
	UserStatus,
	ZERO,
} from '../../packages/sdk/src';
import {
	LiteSVMContextWrapper,
	startLiteSVM,
} from '../../packages/sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../../packages/sdk/src/accounts/testBulkAccountLoader';
import {
	initializeQuoteSpotMarket,
	mockOracleNoProgram,
	mockUSDCMint,
	mockUserUSDCAccount,
	setFeedConfidenceNoProgram,
	setFeedPriceNoProgram,
} from './testHelpers';

// FeePoolWithdrawMarketNotEligible (6375) and FeePoolWithdrawInsufficientBalance (6376).
const NOT_ELIGIBLE = '0x18e7';
const INSUFFICIENT_BALANCE = '0x18e8';

describe('perp fee pool withdraw', () => {
	const chProgram = anchor.workspace.Velocity as Program;

	let velocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let svmContextWrapper: LiteSVMContextWrapper;

	let usdcMint: Keypair;
	let userUSDCAccount: Keypair;
	let solUsd;
	const recipient = Keypair.generate();
	let recipientTokenAccount;

	const MARKET_INDEX = 0;

	const mantissaSqrtScale = new BN(100000);
	const ammInitialQuoteAssetAmount = new BN(5 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetAmount = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);

	const usdcAmount = new BN(500 * 10 ** 6);
	const depositAmount = new BN(10 * 10 ** 6);
	const feePoolSeed = new BN(100 * 10 ** 6);

	const readFeePool = () =>
		getTokenAmount(
			velocityClient.getPerpMarketAccount(MARKET_INDEX).amm.feePool
				.scaledBalance,
			velocityClient.getSpotMarketAccount(0),
			SpotBalanceType.DEPOSIT
		);

	const readRecipient = async () =>
		new BN(
			Number(
				(
					await svmContextWrapper.connection.getTokenAccount(
						recipientTokenAccount
					)
				).amount
			)
		);

	const expectError = async (fn: () => Promise<unknown>, code: string) => {
		let threw = false;
		try {
			await fn();
		} catch (e) {
			threw = true;
			assert(
				e.message.toLowerCase().includes(code),
				`expected ${code}, got: ${e.message}`
			);
		}
		assert(threw, `expected ${code}, but the withdrawal succeeded`);
	};

	const writeAccount = async (name, address, account) => {
		const original = await svmContextWrapper.connection.getAccountInfo(address);
		const data = Buffer.from(original.data);
		const types = velocityClient.program.idl.types;
		const layout = IdlCoder.typeDefLayout({
			typeDef: types.find((type) => type.name === name),
			types,
		});
		layout.encode(account, data.subarray(8));
		svmContextWrapper.context.setAccount(address, { ...original, data });
	};
	const writeMarket = (market) =>
		writeAccount('perpMarket', market.pubkey, market);

	const errorCode = (name: string) => {
		const error = chProgram.idl.errors.find(
			(entry) => entry.name.toLowerCase() === name.toLowerCase()
		);
		assert(error, `missing error ${name}`);
		return `0x${error.code.toString(16)}`;
	};

	const expectRollback = async (fn: () => Promise<unknown>, code: string) => {
		const addresses = [
			velocityClient.getPerpMarketAccount(MARKET_INDEX).pubkey,
			velocityClient.getSpotMarketAccount(0).pubkey,
			velocityClient.getSpotMarketAccount(0).vault,
			recipientTokenAccount,
		];
		const before = await Promise.all(
			addresses.map((address) =>
				svmContextWrapper.connection.getAccountInfo(address)
			)
		);
		await expectError(fn, code);
		for (let index = 0; index < addresses.length; index++) {
			const after = await svmContextWrapper.connection.getAccountInfo(
				addresses[index]
			);
			assert(
				after.data.equals(before[index].data),
				`rollback failed for ${addresses[index]}`
			);
		}
	};

	const withdrawOne = () =>
		velocityClient.withdrawFromPerpMarketFeePool(MARKET_INDEX, new BN(10 ** 6));

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
		solUsd = await mockOracleNoProgram(
			svmContextWrapper,
			1,
			-7,
			undefined,
			10000
		);

		velocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: svmContextWrapper.provider.wallet,
			programID: chProgram.programId,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [MARKET_INDEX],
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
		await velocityClient.updatePerpAuctionDuration(new BN(0));

		await initializeQuoteSpotMarket(velocityClient, usdcMint.publicKey);

		await velocityClient.initializePerpMarket(
			MARKET_INDEX,
			solUsd,
			ammInitialBaseAssetAmount,
			ammInitialQuoteAssetAmount,
			new BN(60 * 60)
		);

		const feeStructure = velocityClient.getStateAccount().perpFeeStructure;
		feeStructure.ammFeeNumerator = 10;
		feeStructure.ifFeeNumerator = 50;
		await velocityClient.updatePerpFeeStructure(feeStructure);
		await velocityClient.updatePerpMarketFeePoolBufferTarget(
			MARKET_INDEX,
			new BN(1 * 10 ** 6)
		);
		await velocityClient.updatePerpMarketBankruptcyIfFloorPct(
			MARKET_INDEX,
			BANKRUPTCY_IF_FLOOR_DISABLED
		);

		await velocityClient.updateProtocolFeeRecipient(
			recipient.publicKey,
			MarketType.PERP
		);
		recipientTokenAccount = getAssociatedTokenAddressSync(
			usdcMint.publicKey,
			recipient.publicKey,
			true
		);

		await velocityClient.initializeUserAccountAndDepositCollateral(
			depositAmount,
			userUSDCAccount.publicKey
		);
		await velocityClient.depositIntoPerpMarketFeePool(
			MARKET_INDEX,
			feePoolSeed,
			userUSDCAccount.publicKey
		);

		// A fill books AMM provision, so the bankruptcy reserve is nonzero once the sweep
		// tokenizes it into the fee pool.
		await velocityClient.openPosition(
			PositionDirection.LONG,
			new BN(48000000000),
			MARKET_INDEX
		);
		await velocityClient.fetchAccounts();
	});

	after(async () => {
		await velocityClient.unsubscribe();
	});

	it('withdraws the exact amount and keeps it out of the drawdown window', async () => {
		const before = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		const feePoolBefore = readFeePool();
		const amount = new BN(40 * 10 ** 6);

		await velocityClient.withdrawFromPerpMarketFeePool(MARKET_INDEX, amount);
		await velocityClient.fetchAccounts();

		const after = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		assert((await readRecipient()).eq(amount), 'recipient balance');
		// the scaled debit rounds up, so the floored pool can read one raw unit lower
		const feePoolDrop = feePoolBefore.sub(readFeePool());
		assert(
			feePoolDrop.gte(amount) && feePoolDrop.lte(amount.addn(1)),
			`fee pool dropped ${feePoolDrop}`
		);
		assert(
			after.amm.totalFeeMinusDistributions.eq(
				before.amm.totalFeeMinusDistributions.sub(amount)
			),
			'tfmd must fall by exactly the amount'
		);
		assert(
			after.amm.netRevenueSinceLastFunding.eq(
				before.amm.netRevenueSinceLastFunding
			),
			'the drawdown window must not see the withdrawal'
		);
	});

	it('leaves the market trading and settling after a withdrawal', async () => {
		await velocityClient.closePosition(MARKET_INDEX);
		await velocityClient.fetchAccounts();
		await velocityClient.settlePNL(
			await velocityClient.getUserAccountPublicKey(),
			velocityClient.getUserAccount(),
			MARKET_INDEX
		);
		// the sweep drains pendings only from pnl pool surplus, so give it some
		await velocityClient.depositIntoPerpMarketPnlPool(
			MARKET_INDEX,
			new BN(50 * 10 ** 6),
			userUSDCAccount.publicKey
		);
		await velocityClient.sweepPerpMarketFees(MARKET_INDEX);
		await velocityClient.fetchAccounts();
		const ledger = velocityClient.getPerpMarketAccount(MARKET_INDEX).feeLedger;
		assert(ledger.pendingAmmProvision.eq(ZERO), 'sweep tokenized provision');
		assert(ledger.ammProtocolFeesReceived.gt(ZERO), 'reserve is nonzero');
	});

	it('rejects an amount above the fee pool', async () => {
		await expectError(
			() =>
				velocityClient.withdrawFromPerpMarketFeePool(
					MARKET_INDEX,
					readFeePool().addn(1)
				),
			INSUFFICIENT_BALANCE
		);
	});

	it('rejects stale and uncertain prices even with curve updates disabled', async () => {
		const market = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		const marketInfo = await svmContextWrapper.connection.getAccountInfo(
			market.pubkey
		);
		const oracleInfo = await svmContextWrapper.connection.getAccountInfo(
			solUsd
		);
		const clock = svmContextWrapper.context.getClock();
		try {
			market.amm.curveUpdateIntensity = 0;
			await writeMarket(market);
			svmContextWrapper.context.warpToSlot(clock.slot + BigInt(100000));
			await expectRollback(withdrawOne, errorCode('InvalidOracle'));
			await setFeedPriceNoProgram(svmContextWrapper, 1, solUsd, -30);
			await expectRollback(withdrawOne, errorCode('InvalidOracle'));
			await setFeedConfidenceNoProgram(svmContextWrapper, 1, solUsd);
			await expectRollback(withdrawOne, errorCode('InvalidOracle'));
			await setFeedPriceNoProgram(svmContextWrapper, 1, solUsd);
			await withdrawOne();
		} finally {
			svmContextWrapper.context.setAccount(solUsd, oracleInfo);
			await setFeedPriceNoProgram(svmContextWrapper, 1, solUsd, 10000);
			// Restore only the configuration so the successful withdrawal remains accounted for.
			const updated = await velocityClient.program.account.perpMarket.fetch(
				market.pubkey
			);
			const original = velocityClient.program.coder.accounts.decode(
				'perpMarket',
				marketInfo.data
			);
			updated.amm.curveUpdateIntensity = original.amm.curveUpdateIntensity;
			await writeMarket(updated);
			await velocityClient.fetchAccounts();
		}
	});

	it('rejects recorded bankruptcy debts and settlement status atomically', async () => {
		const market = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		const original = await svmContextWrapper.connection.getAccountInfo(
			market.pubkey
		);
		try {
			market.pendingBankruptcyClaims = 1;
			await writeMarket(market);
			await expectRollback(withdrawOne, NOT_ELIGIBLE);
			market.pendingBankruptcyClaims = 0;
			market.status = MarketStatus.SETTLEMENT;
			await writeMarket(market);
			await expectRollback(withdrawOne, NOT_ELIGIBLE);
		} finally {
			svmContextWrapper.context.setAccount(market.pubkey, original);
			await velocityClient.fetchAccounts();
		}
	});

	it('rolls back the transfer when the final vault invariant fails', async () => {
		const vault = velocityClient.getSpotMarketAccount(0).vault;
		const original = await svmContextWrapper.connection.getAccountInfo(vault);
		const data = Buffer.from(original.data);
		data.writeBigUInt64LE(data.readBigUInt64LE(64) - BigInt(10 ** 6), 64);
		try {
			svmContextWrapper.context.setAccount(vault, { ...original, data });
			await expectRollback(
				withdrawOne,
				errorCode('SpotMarketVaultInvariantViolated')
			);
		} finally {
			svmContextWrapper.context.setAccount(vault, original);
		}
	});

	it('rolls back pool and equity debits when the token transfer fails', async () => {
		const original = await svmContextWrapper.connection.getAccountInfo(
			recipientTokenAccount
		);
		const data = Buffer.from(original.data);
		data[108] = 2;
		try {
			svmContextWrapper.context.setAccount(recipientTokenAccount, {
				...original,
				data,
			});
			await expectRollback(withdrawOne, '0x11');
		} finally {
			svmContextWrapper.context.setAccount(recipientTokenAccount, original);
		}
	});

	it('keeps the tokenized bankruptcy provision in the fee pool', async () => {
		const ledger = velocityClient.getPerpMarketAccount(MARKET_INDEX).feeLedger;
		const reserve = ledger.ammProtocolFeesReceived.sub(
			ledger.pendingAmmProvision
		);
		assert(reserve.gt(ZERO), 'reserve is nonzero');

		// one raw unit below the reserve fails
		await expectError(
			() =>
				velocityClient.withdrawFromPerpMarketFeePool(
					MARKET_INDEX,
					readFeePool().sub(reserve).addn(1)
				),
			INSUFFICIENT_BALANCE
		);

		// one raw unit above it passes, even after the round-up takes its extra unit
		await velocityClient.withdrawFromPerpMarketFeePool(
			MARKET_INDEX,
			readFeePool().sub(reserve).subn(1)
		);
		await velocityClient.fetchAccounts();
		assert(
			readFeePool().gte(reserve) && readFeePool().lte(reserve.addn(1)),
			`fee pool ${readFeePool()} should sit at the reserve ${reserve}`
		);

		// refill for the lifecycle cases below
		await velocityClient.depositIntoPerpMarketFeePool(
			MARKET_INDEX,
			new BN(20 * 10 ** 6),
			userUSDCAccount.publicKey
		);
		await velocityClient.fetchAccounts();
	});

	it('preserves tokenized backing for bankruptcy recovery after withdrawal', async () => {
		await withdrawOne();
		const [, bankruptKey] = await velocityClient.initializeUserAccount(1);
		await velocityClient.fetchAccounts();
		const market = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		const provisionBefore = market.feeLedger.ammProtocolFeesReceived;
		const loss = new BN(1000);
		assert(market.feeLedger.pendingAmmProvision.isZero());
		assert(market.feeLedger.pendingIfFee.isZero());
		assert(provisionBefore.gt(loss));
		const feeBefore = readFeePool();
		const pnlBefore = market.pnlPool.scaledBalance;
		const socialLossBefore = market.totalSocialLoss;
		const user = await velocityClient.program.account.user.fetch(bankruptKey);
		// Seed a recorded debt with no collateral to exercise the real bankruptcy resolver.
		user.status = UserStatus.BANKRUPT;
		user.nextLiquidationId = 2;
		user.perpPositions[0].marketIndex = MARKET_INDEX;
		user.perpPositions[0].quoteAssetAmount = loss.neg();
		user.perpPositions[0].positionFlag = PositionFlag.BankruptcyClaim;
		market.quoteAssetAmount = market.quoteAssetAmount.sub(loss);
		market.numberOfUsers += 1;
		market.pendingBankruptcyClaims = 1;
		market.insuranceClaim.quoteMaxInsurance =
			market.insuranceClaim.quoteSettledInsurance;
		await writeAccount('user', bankruptKey, user);
		await writeMarket(market);
		await expectRollback(withdrawOne, NOT_ELIGIBLE);
		await velocityClient.resolvePerpBankruptcy(bankruptKey, user, MARKET_INDEX);
		await velocityClient.fetchAccounts();
		const after = velocityClient.getPerpMarketAccount(MARKET_INDEX);
		assert(
			after.feeLedger.ammProtocolFeesReceived.eq(provisionBefore.sub(loss))
		);
		assert(readFeePool().eq(feeBefore.sub(loss)));
		assert(after.pnlPool.scaledBalance.gt(pnlBefore));
		assert(after.totalSocialLoss.eq(socialLossBefore));
		assert.equal(after.pendingBankruptcyClaims, 0);
		const resolved = await velocityClient.program.account.user.fetch(
			bankruptKey
		);
		assert(resolved.perpPositions[0].quoteAssetAmount.isZero());
		assert.equal(resolved.status & UserStatus.BANKRUPT, 0);
	});

	it('rejects a signer that is not cold admin', async () => {
		const outsider = Keypair.generate();
		await svmContextWrapper.fundKeypair(outsider, 10 ** 9);
		const ix = await velocityClient.getWithdrawFromPerpMarketFeePoolIx(
			MARKET_INDEX,
			new BN(1 * 10 ** 6),
			outsider.publicKey,
			velocityClient.wallet.publicKey
		);
		// Anchor ConstraintRaw (2003), raised by the check_cold constraint.
		await expectRollback(async () => {
			const tx = await velocityClient.buildTransaction(ix);
			await velocityClient.sendTransaction(tx, [outsider], velocityClient.opts);
		}, '0x7d3');
	});

	it('allows ReduceOnly', async () => {
		await velocityClient.updatePerpMarketStatus(
			MARKET_INDEX,
			MarketStatus.REDUCE_ONLY
		);
		await velocityClient.withdrawFromPerpMarketFeePool(
			MARKET_INDEX,
			new BN(1 * 10 ** 6)
		);
		await velocityClient.updatePerpMarketStatus(
			MARKET_INDEX,
			MarketStatus.ACTIVE
		);
	});

	it('rejects a market whose expiry has arrived before the status flips', async () => {
		const clock = svmContextWrapper.context.getClock();
		const now = Number(clock.unixTimestamp);
		await velocityClient.updatePerpMarketExpiry(MARKET_INDEX, new BN(now + 10));
		// the expiry ix sets ReduceOnly, which is still an allowed status
		await velocityClient.withdrawFromPerpMarketFeePool(
			MARKET_INDEX,
			new BN(1 * 10 ** 6)
		);

		// move wall-clock time only, so the oracle slot stays fresh
		await svmContextWrapper.setTimestamp(now + 20);
		await expectError(
			() =>
				velocityClient.withdrawFromPerpMarketFeePool(
					MARKET_INDEX,
					new BN(1 * 10 ** 6)
				),
			NOT_ELIGIBLE
		);
	});
});
