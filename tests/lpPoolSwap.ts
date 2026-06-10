import * as anchor from '@coral-xyz/anchor';
import { expect, assert } from 'chai';
import { Program } from '@coral-xyz/anchor';
import {
	Account,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	Transaction,
} from '@solana/web3.js';
import {
	BN,
	TestClient,
	QUOTE_PRECISION,
	getLpPoolPublicKey,
	getConstituentTargetBasePublicKey,
	PERCENTAGE_PRECISION,
	PRICE_PRECISION,
	PEG_PRECISION,
	ConstituentTargetBaseAccount,
	OracleSource,
	SPOT_MARKET_RATE_PRECISION,
	SPOT_MARKET_WEIGHT_PRECISION,
	LPPoolAccount,
	convertToNumber,
	getConstituentVaultPublicKey,
	getConstituentPublicKey,
	ConstituentAccount,
	ZERO,
	getSerumSignerPublicKey,
	BN_MAX,
	isVariant,
	ConstituentStatus,
	getSignedTokenAmount,
	getTokenAmount,
} from '../sdk/src';
import {
	initializeQuoteSpotMarket,
	mockUSDCMint,
	mockUserUSDCAccount,
	mockOracleNoProgram,
	setFeedPriceNoProgram,
	overWriteTokenAccountBalance,
	overwriteConstituentAccount,
	mockAtaTokenAccountForMint,
	overWriteMintAccount,
	createWSolTokenAccountForUser,
	initializeSolSpotMarket,
	createUserWithUSDCAndWSOLAccount,
	sleep,
} from './testHelpers';
import { startLiteSVM } from '../sdk/src/litesvm/litesvmConnection';
import { TestBulkAccountLoader } from '../sdk/src/accounts/testBulkAccountLoader';
import { LiteSVMContextWrapper } from '../sdk/src/litesvm/litesvmConnection';
import dotenv from 'dotenv';
import { DexInstructions, Market, OpenOrders } from '@project-serum/serum';
import { listMarket, SERUM, makePlaceOrderTransaction } from './serumHelper';
import { NATIVE_MINT } from '@solana/spl-token';
import {
	CustomBorshAccountsCoder,
	CustomBorshCoder,
} from '../sdk/src/decode/customCoder';
dotenv.config();

describe('LP Pool', () => {
	const program = anchor.workspace.Drift as Program;
	// Align account (de)serialization with on-chain zero-copy layouts
	// @ts-ignore
	program.coder.accounts = new CustomBorshAccountsCoder(program.idl);
	let contextWrapper: LiteSVMContextWrapper;
	let bulkAccountLoader: TestBulkAccountLoader;

	let adminClient: TestClient;
	let usdcMint: Keypair;
	let spotTokenMint: Keypair;
	let spotMarketOracle: PublicKey;

	let serumMarketPublicKey: PublicKey;

	let serumDriftClient: TestClient;
	let serumWSOL: PublicKey;
	let serumUSDC: PublicKey;
	let serumKeypair: Keypair;

	let adminSolAta: PublicKey;

	let openOrdersAccount: PublicKey;

	const usdcAmount = new BN(500 * 10 ** 6);
	const solAmount = new BN(2 * 10 ** 9);

	const mantissaSqrtScale = new BN(Math.sqrt(PRICE_PRECISION.toNumber()));
	const ammInitialQuoteAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
		mantissaSqrtScale
	);
	const ammInitialBaseAssetReserve = new anchor.BN(10 * 10 ** 13).mul(
		mantissaSqrtScale
	);

	const lpPoolId = 0;
	const tokenDecimals = 6;
	const lpPoolKey = getLpPoolPublicKey(program.programId, lpPoolId);

	let userUSDCAccount: Keypair;
	let serumMarket: Market;

	before(async () => {
		const context = await startLiteSVM(
			'',
			[
				{
					name: 'serum_dex',
					programId: new PublicKey(
						'srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX'
					),
				},
			],
			[]
		);

		// @ts-ignore
		contextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			contextWrapper.connection,
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(contextWrapper);
		spotTokenMint = await mockUSDCMint(contextWrapper);
		spotMarketOracle = await mockOracleNoProgram(contextWrapper, 80);

		const keypair = new Keypair();
		await contextWrapper.fundKeypair(keypair, 50 * LAMPORTS_PER_SOL);

		adminClient = new TestClient({
			connection: contextWrapper.connection.toConnection(),
			wallet: new anchor.Wallet(keypair),
			programID: program.programId,
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			subAccountIds: [],
			perpMarketIndexes: [0, 1],
			spotMarketIndexes: [0, 1, 2],
			oracleInfos: [
				{
					publicKey: spotMarketOracle,
					source: OracleSource.PYTH_LAZER,
				},
			],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader,
			},
			// Ensure the client uses the same custom coder
			coder: new CustomBorshCoder(program.idl),
		});
		await adminClient.initialize(usdcMint.publicKey, true);
		await adminClient.subscribe();
		await initializeQuoteSpotMarket(adminClient, usdcMint.publicKey);

		userUSDCAccount = await mockUserUSDCAccount(
			usdcMint,
			new BN(10).mul(QUOTE_PRECISION),
			contextWrapper,
			keypair.publicKey
		);

		await adminClient.initializeUserAccountAndDepositCollateral(
			new BN(10).mul(QUOTE_PRECISION),
			userUSDCAccount.publicKey
		);

		const periodicity = new BN(0);

		await adminClient.initializeAmmCache();

		await adminClient.initializePerpMarket(
			0,
			spotMarketOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(80 * PEG_PRECISION.toNumber())
		);
		await adminClient.addMarketToAmmCache(0);
		await adminClient.updatePerpMarketLpPoolStatus(0, 1);

		await adminClient.initializePerpMarket(
			1,
			spotMarketOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			periodicity,
			new BN(80 * PEG_PRECISION.toNumber())
		);
		await adminClient.addMarketToAmmCache(1);
		await adminClient.updatePerpMarketLpPoolStatus(1, 1);

		const optimalUtilization = SPOT_MARKET_RATE_PRECISION.div(
			new BN(2)
		).toNumber(); // 50% utilization
		const optimalRate = SPOT_MARKET_RATE_PRECISION.toNumber();
		const maxRate = SPOT_MARKET_RATE_PRECISION.toNumber();
		const initialAssetWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const maintenanceAssetWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const initialLiabilityWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const maintenanceLiabilityWeight = SPOT_MARKET_WEIGHT_PRECISION.toNumber();
		const imfFactor = 0;

		await adminClient.initializeSpotMarket(
			spotTokenMint.publicKey,
			optimalUtilization,
			optimalRate,
			maxRate,
			spotMarketOracle,
			OracleSource.PYTH_LAZER,
			initialAssetWeight,
			maintenanceAssetWeight,
			initialLiabilityWeight,
			maintenanceLiabilityWeight,
			imfFactor
		);

		adminSolAta = await createWSolTokenAccountForUser(
			contextWrapper,
			adminClient.wallet.payer,
			new BN(20 * 10 ** 9) // 10 SOL
		);

		await adminClient.initializeLpPool(
			lpPoolId,
			new BN(100), // 1 bps
			new BN(100_000_000).mul(QUOTE_PRECISION),
			new BN(1_000_000).mul(QUOTE_PRECISION),
			Keypair.generate() // dlp mint
		);
		await adminClient.initializeConstituent(lpPoolId, {
			spotMarketIndex: 0,
			decimals: 6,
			maxWeightDeviation: PERCENTAGE_PRECISION.divn(10), // 10% max dev,
			swapFeeMin: PERCENTAGE_PRECISION.divn(10000), // min fee 1 bps,
			swapFeeMax: PERCENTAGE_PRECISION.divn(100),
			maxBorrowTokenAmount: new BN(1_000_000).muln(10 ** 6),
			oracleStalenessThreshold: new BN(100),
			costToTrade: 1,
			derivativeWeight: PERCENTAGE_PRECISION,
			volatility: ZERO,
			constituentCorrelations: [],
		});
		await adminClient.updateFeatureBitFlagsMintRedeemLpPool(true);

		await adminClient.initializeConstituent(lpPoolId, {
			spotMarketIndex: 1,
			decimals: 6,
			maxWeightDeviation: PERCENTAGE_PRECISION.divn(10), // 10% max dev,
			swapFeeMin: PERCENTAGE_PRECISION.divn(10000), // min fee 1 bps,
			swapFeeMax: PERCENTAGE_PRECISION.divn(100),
			maxBorrowTokenAmount: new BN(1_000_000).muln(10 ** 6),
			oracleStalenessThreshold: new BN(100),
			costToTrade: 1,
			derivativeWeight: ZERO,
			volatility: PERCENTAGE_PRECISION.muln(4).divn(100),
			constituentCorrelations: [ZERO],
		});

		await initializeSolSpotMarket(adminClient, spotMarketOracle);
		await adminClient.updateSpotMarketStepSizeAndTickSize(
			2,
			new BN(100000000),
			new BN(100)
		);
		await adminClient.updateSpotAuctionDuration(0);

		await adminClient.deposit(
			new BN(5 * 10 ** 9), // 10 SOL
			2, // market index
			adminSolAta // user token account
		);

		await adminClient.depositIntoSpotMarketVault(
			2,
			new BN(4 * 10 ** 9), // 4 SOL
			adminSolAta
		);

		[serumDriftClient, serumWSOL, serumUSDC, serumKeypair] =
			await createUserWithUSDCAndWSOLAccount(
				contextWrapper,
				usdcMint,
				program,
				solAmount,
				usdcAmount,
				[],
				[0, 1],
				[
					{
						publicKey: spotMarketOracle,
						source: OracleSource.PYTH_LAZER,
					},
				],
				bulkAccountLoader
			);

		await contextWrapper.fundKeypair(
			serumKeypair,
			50 * LAMPORTS_PER_SOL
		);
		await serumDriftClient.deposit(usdcAmount, 0, serumUSDC);
	});

	after(async () => {
		await adminClient.unsubscribe();
		await serumDriftClient.unsubscribe();
	});

	it('LP Pool init properly', async () => {
		let lpPool: LPPoolAccount;
		try {
			lpPool = (await adminClient.program.account.lpPool.fetch(
				lpPoolKey
			)) as LPPoolAccount;
			expect(lpPool).to.not.be.null;
		} catch (e) {
			expect.fail('LP Pool should have been created');
		}

		try {
			const constituentTargetBasePublicKey = getConstituentTargetBasePublicKey(
				program.programId,
				lpPoolKey
			);
			const constituentTargetBase =
				(await adminClient.program.account.constituentTargetBase.fetch(
					constituentTargetBasePublicKey
				)) as ConstituentTargetBaseAccount;
			expect(constituentTargetBase).to.not.be.null;
			assert(constituentTargetBase.targets.length == 2);
		} catch (e) {
			expect.fail('Amm constituent map should have been created');
		}
	});

	it('lp pool swap', async () => {
		let spotOracle = adminClient.getOracleDataForSpotMarket(1);
		const price1 = convertToNumber(spotOracle.price);

		await setFeedPriceNoProgram(contextWrapper, 224.3, spotMarketOracle);
		await contextWrapper.connection.updateSlotAndClock();

		let price2 = price1;
		for (let i = 0; i < 20; i++) {
			await adminClient.fetchAccounts();
			spotOracle = adminClient.getOracleDataForSpotMarket(1);
			price2 = convertToNumber(spotOracle.price);
			if (price2 > price1) {
				break;
			}
			await sleep(250);
		}
		assert(
			price2 > price1,
			`expected oracle price to increase: ${price1} -> ${price2}`
		);

		const const0TokenAccount = getConstituentVaultPublicKey(
			program.programId,
			lpPoolKey,
			0
		);
		const const1TokenAccount = getConstituentVaultPublicKey(
			program.programId,
			lpPoolKey,
			1
		);

		const const0Key = getConstituentPublicKey(program.programId, lpPoolKey, 0);
		const const1Key = getConstituentPublicKey(program.programId, lpPoolKey, 1);

		const c0TokenBalance = new BN(224_300_000_000);
		const c1TokenBalance = new BN(1_000_000_000);

		await overWriteTokenAccountBalance(
			contextWrapper,
			const0TokenAccount,
			BigInt(c0TokenBalance.toString())
		);
		await overwriteConstituentAccount(
			contextWrapper,
			adminClient.program,
			const0Key,
			[['vaultTokenBalance', c0TokenBalance]]
		);

		await overWriteTokenAccountBalance(
			contextWrapper,
			const1TokenAccount,
			BigInt(c1TokenBalance.toString())
		);
		await overwriteConstituentAccount(
			contextWrapper,
			adminClient.program,
			const1Key,
			[['vaultTokenBalance', c1TokenBalance]]
		);

		// check fields overwritten correctly
		const c0 = (await adminClient.program.account.constituent.fetch(
			const0Key
		)) as ConstituentAccount;
		expect(c0.vaultTokenBalance.toString()).to.equal(c0TokenBalance.toString());

		const c1 = (await adminClient.program.account.constituent.fetch(
			const1Key
		)) as ConstituentAccount;
		expect(c1.vaultTokenBalance.toString()).to.equal(c1TokenBalance.toString());

		await adminClient.updateConstituentOracleInfo(c1);
		await adminClient.updateConstituentOracleInfo(c0);

		const prec = new BN(10).pow(new BN(tokenDecimals));
		console.log(
			`const0 balance: ${convertToNumber(c0.vaultTokenBalance, prec)}`
		);
		console.log(
			`const1 balance: ${convertToNumber(c1.vaultTokenBalance, prec)}`
		);

		const lpPool1 = (await adminClient.program.account.lpPool.fetch(
			lpPoolKey
		)) as LPPoolAccount;
		expect(lpPool1.lastAumSlot.toNumber()).to.be.equal(0);

		await adminClient.updateLpPoolAum(lpPool1, [1, 0]);

		const lpPool2 = (await adminClient.program.account.lpPool.fetch(
			lpPoolKey
		)) as LPPoolAccount;

		expect(lpPool2.lastAumSlot.toNumber()).to.be.greaterThan(0);
		expect(lpPool2.lastAum.gt(lpPool1.lastAum)).to.be.true;
		console.log(`AUM: ${convertToNumber(lpPool2.lastAum, QUOTE_PRECISION)}`);

		// swap c0 for c1

		const adminAuth = adminClient.wallet.publicKey;

		// mint some tokens for user
		const c0UserTokenAccount = await mockAtaTokenAccountForMint(
			contextWrapper,
			usdcMint.publicKey,
			new BN(224_300_000_000),
			adminAuth
		);
		const c1UserTokenAccount = await mockAtaTokenAccountForMint(
			contextWrapper,
			spotTokenMint.publicKey,
			new BN(1_000_000_000),
			adminAuth
		);

		const inTokenBalanceBefore =
			await contextWrapper.connection.getTokenAccount(
				c0UserTokenAccount
			);
		const outTokenBalanceBefore =
			await contextWrapper.connection.getTokenAccount(
				c1UserTokenAccount
			);

		// in = 0, out = 1
		const swapTx = new Transaction();
		swapTx.add(await adminClient.getUpdateLpPoolAumIxs(lpPool2, [0, 1]));
		swapTx.add(
			await adminClient.getLpPoolSwapIx(
				0,
				1,
				new BN(224_300_000),
				new BN(0),
				lpPoolKey,
				adminAuth
			)
		);

		// Should throw since we havnet enabled swaps yet
		try {
			await adminClient.sendTransaction(swapTx);
			assert(false, 'Should have thrown');
		} catch (error) {
			assert(error.message.includes('0x17f1'));
		}

		// Enable swaps
		await adminClient.updateFeatureBitFlagsSwapLpPool(true);

		// Send swap
		await adminClient.sendTransaction(swapTx);

		const inTokenBalanceAfter =
			await contextWrapper.connection.getTokenAccount(
				c0UserTokenAccount
			);
		const outTokenBalanceAfter =
			await contextWrapper.connection.getTokenAccount(
				c1UserTokenAccount
			);
		const diffInToken =
			inTokenBalanceAfter.amount - inTokenBalanceBefore.amount;
		const diffOutToken =
			outTokenBalanceAfter.amount - outTokenBalanceBefore.amount;

		expect(Number(diffInToken)).to.be.equal(-224_300_000);
		expect(Number(diffOutToken)).to.be.approximately(1001298, 1);

		console.log(
			`in Token:  ${inTokenBalanceBefore.amount} -> ${
				inTokenBalanceAfter.amount
			} (${Number(diffInToken) / 1e6})`
		);
		console.log(
			`out Token: ${outTokenBalanceBefore.amount} -> ${
				outTokenBalanceAfter.amount
			} (${Number(diffOutToken) / 1e6})`
		);
	});

	it('lp pool add and remove liquidity: usdc', async () => {
		// add c0 liquidity
		const adminAuth = adminClient.wallet.publicKey;
		const c0UserTokenAccount = await mockAtaTokenAccountForMint(
			contextWrapper,
			usdcMint.publicKey,
			new BN(1_000_000_000_000),
			adminAuth
		);
		let lpPool = (await adminClient.program.account.lpPool.fetch(
			lpPoolKey
		)) as LPPoolAccount;
		await adminClient.updateLpPoolAum(lpPool, [0, 1]);
		lpPool = (await adminClient.program.account.lpPool.fetch(
			lpPoolKey
		)) as LPPoolAccount;
		const lpPoolAumBefore = lpPool.lastAum;

		const userLpTokenAccount = await mockAtaTokenAccountForMint(
			contextWrapper,
			lpPool.mint,
			new BN(0),
			adminAuth
		);

		// check fields overwritten correctly
		const c0 = (await adminClient.program.account.constituent.fetch(
			getConstituentPublicKey(program.programId, lpPoolKey, 0)
		)) as ConstituentAccount;
		const c1 = (await adminClient.program.account.constituent.fetch(
			getConstituentPublicKey(program.programId, lpPoolKey, 1)
		)) as ConstituentAccount;
		await adminClient.updateConstituentOracleInfo(c1);
		await adminClient.updateConstituentOracleInfo(c0);

		const userC0TokenBalanceBefore =
			await contextWrapper.connection.getTokenAccount(
				c0UserTokenAccount
			);
		const userLpTokenBalanceBefore =
			await contextWrapper.connection.getTokenAccount(
				userLpTokenAccount
			);

		await overWriteMintAccount(
			contextWrapper,
			lpPool.mint,
			BigInt(lpPool.lastAum.toNumber())
		);

		const tokensAdded = new BN(1_000_000_000_000);
		let tx = new Transaction();
		tx.add(await adminClient.getUpdateLpPoolAumIxs(lpPool, [0, 1]));
		tx.add(
			...(await adminClient.getLpPoolAddLiquidityIx({
				inMarketIndex: 0,
				inAmount: tokensAdded,
				minMintAmount: new BN(1),
				lpPool: lpPool,
			}))
		);
		await adminClient.sendTransaction(tx);

		// Should fail to add more liquidity if it's in redulce only mode;
		await adminClient.updateConstituentStatus(
			c0.pubkey,
			ConstituentStatus.REDUCE_ONLY
		);
		tx = new Transaction();
		tx.add(await adminClient.getUpdateLpPoolAumIxs(lpPool, [0, 1]));
		tx.add(
			...(await adminClient.getLpPoolAddLiquidityIx({
				inMarketIndex: 0,
				inAmount: tokensAdded,
				minMintAmount: new BN(1),
				lpPool: lpPool,
			}))
		);
		try {
			await adminClient.sendTransaction(tx);
		} catch (e) {
			console.log(e.message);
			assert(e.message.includes('0x18c5')); // InvalidConstituentOperation
		}
		await adminClient.updateConstituentStatus(
			c0.pubkey,
			ConstituentStatus.ACTIVE
		);

		await sleep(500);

		const userC0TokenBalanceAfter =
			await contextWrapper.connection.getTokenAccount(
				c0UserTokenAccount
			);
		const userLpTokenBalanceAfter =
			await contextWrapper.connection.getTokenAccount(
				userLpTokenAccount
			);
		lpPool = (await adminClient.program.account.lpPool.fetch(
			lpPoolKey
		)) as LPPoolAccount;
		const lpPoolAumAfter = lpPool.lastAum;
		const lpPoolAumDiff = lpPoolAumAfter.sub(lpPoolAumBefore);
		expect(lpPoolAumDiff.toString()).to.be.equal(tokensAdded.toString());

		const userC0TokenBalanceDiff =
			Number(userC0TokenBalanceAfter.amount) -
			Number(userC0TokenBalanceBefore.amount);
		expect(Number(userC0TokenBalanceDiff)).to.be.equal(
			-1 * tokensAdded.toNumber()
		);

		const userLpTokenBalanceDiff =
			Number(userLpTokenBalanceAfter.amount) -
			Number(userLpTokenBalanceBefore.amount);
		expect(userLpTokenBalanceDiff).to.be.equal(
			(((tokensAdded.toNumber() * 9997) / 10000) * 9999) / 10000
		); // max weight deviation: expect min swap% fee on constituent, + 0.01% lp mint fee

		const constituentBalanceBefore = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 0)
			)
		).amount.toString();

		console.log(`constituentBalanceBefore: ${constituentBalanceBefore}`);

		// remove liquidity
		const removeTx = new Transaction();
		removeTx.add(await adminClient.getUpdateLpPoolAumIxs(lpPool, [0, 1]));
		removeTx.add(
			await adminClient.getDepositToProgramVaultIx(
				lpPoolId,
				0,
				new BN(constituentBalanceBefore)
			)
		);
		removeTx.add(
			...(await adminClient.getLpPoolRemoveLiquidityIx({
				outMarketIndex: 0,
				lpToBurn: new BN(userLpTokenBalanceAfter.amount.toString()),
				minAmountOut: new BN(1),
				lpPool: lpPool,
			}))
		);
		await adminClient.sendTransaction(removeTx);

		const constituentAfterRemoveLiquidity =
			(await adminClient.program.account.constituent.fetch(
				getConstituentPublicKey(program.programId, lpPoolKey, 0)
			)) as ConstituentAccount;

		const blTokenAmountAfterRemoveLiquidity = getSignedTokenAmount(
			getTokenAmount(
				constituentAfterRemoveLiquidity.spotBalance.scaledBalance,
				adminClient.getSpotMarketAccount(0),
				constituentAfterRemoveLiquidity.spotBalance.balanceType
			),
			constituentAfterRemoveLiquidity.spotBalance.balanceType
		);

		const withdrawFromProgramVaultTx = new Transaction();
		withdrawFromProgramVaultTx.add(
			await adminClient.getWithdrawFromProgramVaultIx(
				lpPoolId,
				0,
				blTokenAmountAfterRemoveLiquidity.abs()
			)
		);
		await adminClient.sendTransaction(withdrawFromProgramVaultTx);

		const userC0TokenBalanceAfterBurn =
			await contextWrapper.connection.getTokenAccount(
				c0UserTokenAccount
			);
		const userLpTokenBalanceAfterBurn =
			await contextWrapper.connection.getTokenAccount(
				userLpTokenAccount
			);

		const userC0TokenBalanceAfterBurnDiff =
			Number(userC0TokenBalanceAfterBurn.amount) -
			Number(userC0TokenBalanceAfter.amount);

		expect(userC0TokenBalanceAfterBurnDiff).to.be.greaterThan(0);
		expect(Number(userLpTokenBalanceAfterBurn.amount)).to.be.equal(0);

		const totalC0TokensLost = new BN(
			userC0TokenBalanceAfterBurn.amount.toString()
		).sub(tokensAdded);
		const totalC0TokensLostPercent =
			Number(totalC0TokensLost) / Number(tokensAdded);
		expect(totalC0TokensLostPercent).to.be.approximately(-0.0006, 0.0001); // lost about 7bps swapping in an out
	});

	it('Add Serum Market', async () => {
		serumMarketPublicKey = await listMarket({
			context: contextWrapper,
			wallet: contextWrapper.provider.wallet,
			baseMint: NATIVE_MINT,
			quoteMint: usdcMint.publicKey,
			baseLotSize: 100000000,
			quoteLotSize: 100,
			dexProgramId: SERUM,
			feeRateBps: 0,
		});

		serumMarket = await Market.load(
			contextWrapper.connection.toConnection(),
			serumMarketPublicKey,
			{ commitment: 'confirmed' },
			SERUM
		);

		await adminClient.initializeSerumFulfillmentConfig(
			2,
			serumMarketPublicKey,
			SERUM
		);

		serumMarket = await Market.load(
			contextWrapper.connection.toConnection(),
			serumMarketPublicKey,
			{ commitment: 'recent' },
			SERUM
		);

		const serumOpenOrdersAccount = new Account();
		const createOpenOrdersIx = await OpenOrders.makeCreateAccountTransaction(
			contextWrapper.connection.toConnection(),
			serumMarket.address,
			serumDriftClient.wallet.publicKey,
			serumOpenOrdersAccount.publicKey,
			serumMarket.programId
		);
		await serumDriftClient.sendTransaction(
			new Transaction().add(createOpenOrdersIx),
			[serumOpenOrdersAccount]
		);

		const adminOpenOrdersAccount = new Account();
		const adminCreateOpenOrdersIx =
			await OpenOrders.makeCreateAccountTransaction(
				contextWrapper.connection.toConnection(),
				serumMarket.address,
				adminClient.wallet.publicKey,
				adminOpenOrdersAccount.publicKey,
				serumMarket.programId
			);
		await adminClient.sendTransaction(
			new Transaction().add(adminCreateOpenOrdersIx),
			[adminOpenOrdersAccount]
		);

		openOrdersAccount = adminOpenOrdersAccount.publicKey;
	});

	it('swap sol for usdc', async () => {
		// Initialize new constituent for market 2
		await adminClient.initializeConstituent(lpPoolId, {
			spotMarketIndex: 2,
			decimals: 6,
			maxWeightDeviation: PERCENTAGE_PRECISION.divn(10), // 10% max dev,
			swapFeeMin: PERCENTAGE_PRECISION.divn(10000), // min fee 1 bps,
			swapFeeMax: PERCENTAGE_PRECISION.divn(100),
			maxBorrowTokenAmount: new BN(1_000_000).muln(10 ** 6),
			oracleStalenessThreshold: new BN(100),
			costToTrade: 1,
			derivativeWeight: ZERO,
			volatility: ZERO,
			constituentCorrelations: [ZERO, PERCENTAGE_PRECISION],
		});

		const beforeSOLBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 2)
			)
		).amount.toString();
		console.log(`beforeSOLBalance: ${beforeSOLBalance}`);
		const beforeUSDCBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 0)
			)
		).amount.toString();
		console.log(`beforeUSDCBalance: ${beforeUSDCBalance}`);

		const serumMarket = await Market.load(
			contextWrapper.connection.toConnection(),
			serumMarketPublicKey,
			{ commitment: 'recent' },
			SERUM
		);

		const adminSolAccount = await createWSolTokenAccountForUser(
			contextWrapper,
			adminClient.wallet.payer,
			ZERO
		);

		// place ask to sell 1 sol for 100 usdc
		const { transaction, signers } = await makePlaceOrderTransaction(
			contextWrapper.connection.toConnection(),
			serumMarket,
			{
				owner: serumDriftClient.wallet,
				payer: serumWSOL,
				side: 'sell',
				price: 100,
				size: 1,
				orderType: 'postOnly',
				clientId: undefined, // todo?
				openOrdersAddressKey: undefined,
				openOrdersAccount: undefined,
				feeDiscountPubkey: null,
				selfTradeBehavior: 'abortTransaction',
				maxTs: BN_MAX,
			}
		);

		const signerKeypairs = signers.map((signer) => {
			return Keypair.fromSecretKey(signer.secretKey);
		});

		await serumDriftClient.sendTransaction(transaction, signerKeypairs);

		const amountIn = new BN(200).muln(
			10 ** adminClient.getSpotMarketAccount(0).decimals
		);

		const { beginSwapIx, endSwapIx } = await adminClient.getSwapIx(
			{
				lpPoolId: lpPoolId,
				amountIn: amountIn,
				inMarketIndex: 0,
				outMarketIndex: 2,
				inTokenAccount: userUSDCAccount.publicKey,
				outTokenAccount: adminSolAccount,
			},
			true
		);

		const serumBidIx = serumMarket.makePlaceOrderInstruction(
			contextWrapper.connection.toConnection(),
			{
				owner: adminClient.wallet.publicKey,
				payer: userUSDCAccount.publicKey,
				side: 'buy',
				price: 100,
				size: 2, // larger than maker orders so that entire maker order is taken
				orderType: 'ioc',
				clientId: new BN(1), // todo?
				openOrdersAddressKey: openOrdersAccount,
				feeDiscountPubkey: null,
				selfTradeBehavior: 'abortTransaction',
			}
		);

		const serumConfig = await adminClient.getSerumV3FulfillmentConfig(
			serumMarket.publicKey
		);
		const settleFundsIx = DexInstructions.settleFunds({
			market: serumMarket.publicKey,
			openOrders: openOrdersAccount,
			owner: adminClient.wallet.publicKey,
			// @ts-ignore
			baseVault: serumConfig.serumBaseVault,
			// @ts-ignore
			quoteVault: serumConfig.serumQuoteVault,
			baseWallet: adminSolAccount,
			quoteWallet: userUSDCAccount.publicKey,
			vaultSigner: getSerumSignerPublicKey(
				serumMarket.programId,
				serumMarket.publicKey,
				serumConfig.serumSignerNonce
			),
			programId: serumMarket.programId,
		});

		const tx = new Transaction()
			.add(beginSwapIx)
			.add(serumBidIx)
			.add(settleFundsIx)
			.add(endSwapIx);

		// Should fail if usdc is in reduce only
		const c0pubkey = getConstituentPublicKey(program.programId, lpPoolKey, 0);
		await adminClient.updateConstituentStatus(
			c0pubkey,
			ConstituentStatus.REDUCE_ONLY
		);
		try {
			await adminClient.sendTransaction(tx);
		} catch (e) {
			assert(e.message.includes('0x18c0'));
		}
		await adminClient.updateConstituentStatus(
			c0pubkey,
			ConstituentStatus.ACTIVE
		);

		const { txSig } = await adminClient.sendTransaction(tx);

		contextWrapper.printTxLogs(txSig);

		// Balances should be accuarate after swap
		const afterSOLBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 2)
			)
		).amount.toString();
		const afterUSDCBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 0)
			)
		).amount.toString();

		const solDiff = afterSOLBalance - beforeSOLBalance;
		const usdcDiff = afterUSDCBalance - beforeUSDCBalance;

		console.log(
			`in Token:  ${beforeUSDCBalance} -> ${afterUSDCBalance} (${usdcDiff})`
		);
		console.log(
			`out Token: ${beforeSOLBalance} -> ${afterSOLBalance} (${solDiff})`
		);

		expect(usdcDiff).to.be.equal(-100040000);
		expect(solDiff).to.be.equal(1000000000);
	});

	it('deposit and withdraw atomically before swapping', async () => {
		const beforeSOLBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 2)
			)
		).amount.toString();
		const beforeUSDCBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 0)
			)
		).amount.toString();

		await adminClient.depositWithdrawToProgramVault(
			lpPoolId,
			0,
			2,
			new BN(400).mul(QUOTE_PRECISION), // 100 USDC
			new BN(2 * 10 ** 9) // 100 USDC
		);

		const afterSOLBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 2)
			)
		).amount.toString();
		const afterUSDCBalance = +(
			await contextWrapper.connection.getTokenAccount(
				getConstituentVaultPublicKey(program.programId, lpPoolKey, 0)
			)
		).amount.toString();

		const solDiff = afterSOLBalance - beforeSOLBalance;
		const usdcDiff = afterUSDCBalance - beforeUSDCBalance;

		console.log(
			`in Token:  ${beforeUSDCBalance} -> ${afterUSDCBalance} (${usdcDiff})`
		);
		console.log(
			`out Token: ${beforeSOLBalance} -> ${afterSOLBalance} (${solDiff})`
		);

		expect(usdcDiff).to.be.equal(-400000000);
		expect(solDiff).to.be.equal(2000000000);

		const constituent = (await adminClient.program.account.constituent.fetch(
			getConstituentPublicKey(program.programId, lpPoolKey, 2)
		)) as ConstituentAccount;

		assert(constituent.spotBalance.scaledBalance.eq(new BN(2000000001)));
		assert(isVariant(constituent.spotBalance.balanceType, 'borrow'));
	});
});
