/**
 * Vault paths that route funds through the transit account into Velocity `deposit` must
 * revert with DepositNotFullySettled when a ReduceOnly market caps the accepted amount.
 */
import * as anchor from '@coral-xyz/anchor';
import { BN, Program, Wallet } from '@coral-xyz/anchor';
import { expect } from 'chai';
import {
	LiteSVMContextWrapper,
	TEST_ADMIN_KEYPAIR,
	startLiteSVM,
	LiteSVMProvider,
} from './common/litesvmConnection';
import {
	VaultClient,
	getVaultAddressSync,
	getVaultDepositorAddressSync,
	encodeName,
	VAULT_PROGRAM_ID,
	IDL,
	VaultClass,
} from '@velocity-exchange/vaults-sdk';
import {
	BulkAccountLoader,
	VELOCITY_PROGRAM_ID,
	VelocityClient,
	MarketStatus,
	OracleSource,
	PEG_PRECISION,
	PublicKey,
	QUOTE_PRECISION,
	TestClient,
	ZERO,
	getTokenAmount,
	isVariant,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
} from '@velocity-exchange/sdk';
import { TestBulkAccountLoader } from './common/testBulkAccountLoader';
import {
	bootstrapSignerClientAndUser,
	initializeQuoteSpotMarket,
	initializeSolSpotMarket,
	mockUSDCMint,
} from './common/testHelpers';
import { Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';
import { mockOracleNoProgram } from './common/svmOracle';

// ammInvariant == k == x * y
const mantissaSqrtScale = new BN(100_000);
const ammInitialQuoteAssetReserve = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);
const ammInitialBaseAssetReserve = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);

const usdc = (n: number) => new BN(n).mul(QUOTE_PRECISION);

async function expectRevert(fn: () => Promise<unknown>, errorName: string) {
	let err: any;
	try {
		await fn();
	} catch (e) {
		err = e;
	}
	expect(err, 'expected the transaction to revert').to.not.be.undefined;
	const text = [err.message, ...(err.logs ?? [])].join('\n');
	expect(text).to.include(errorName);
}

describe('vault deposits into a ReduceOnly market', () => {
	const initialSolPerpPrice = 100;

	let adminVelocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let svmContextWrapper: LiteSVMContextWrapper;
	let usdcMint: Keypair;
	let solPerpOracle: PublicKey;

	const vaultName = 'reduce only deposit vault';
	const commonVaultKey = getVaultAddressSync(
		VAULT_PROGRAM_ID,
		encodeName(vaultName)
	);
	const vaultUserKey = getUserAccountPublicKeySync(
		new PublicKey(VELOCITY_PROGRAM_ID),
		commonVaultKey
	);
	let vaultTokenAccount: PublicKey;

	const usdcAmount = usdc(100_000);
	const vaultDeposit = usdc(1_000);
	// borrow 100 USDC more than the vault holds, so its velocity user ends up with
	// a 100 USDC borrow on the denomination market
	const managerBorrow = usdc(1_100);
	const outstandingBorrow = usdc(100);

	const managerSigner = Keypair.generate();
	let managerClient: VaultClient;
	let managerVelocityClient: VelocityClient;
	let managerUSDCAccount: PublicKey;

	const depositorSigner = Keypair.generate();
	let depositorClient: VaultClient;
	let depositorVelocityClient: VelocityClient;
	let depositorUSDCAccount: PublicKey;
	let depositorVaultDepositor: PublicKey;

	// supplies USDC liquidity to market 0 and SOL collateral to the vault user
	const lpSigner = Keypair.generate();
	let lpVelocityClient: TestClient;
	let lpVaultClient: VaultClient;
	let lpWSOLAccount: PublicKey;

	let adminClient: VaultClient;

	const velocityClientConfig = () => ({
		accountSubscription: {
			type: 'polling' as const,
			accountLoader: bulkAccountLoader as BulkAccountLoader,
		},
		activeSubAccountId: 0,
		subAccountIds: [],
		perpMarketIndexes: [0],
		spotMarketIndexes: [0, 1],
		oracleInfos: [
			{ publicKey: solPerpOracle, source: OracleSource.PYTH_LAZER },
		],
	});

	const transitBalance = async () =>
		BigInt(
			(
				await svmContextWrapper.connection.getTokenAccountBalance(
					vaultTokenAccount
				)
			).amount.toString()
		);

	const vaultUsdcPosition = async () => {
		await adminVelocityClient.fetchAccounts();
		const user = await adminVelocityClient.program.account.user.fetch(
			vaultUserKey
		);
		return (user.spotPositions as any[]).find((p) => p.marketIndex === 0);
	};

	const vaultUsdcBorrow = async () => {
		const position = await vaultUsdcPosition();
		expect(isVariant(position.balanceType, 'borrow')).to.equal(true);
		return getTokenAmount(
			position.scaledBalance,
			adminVelocityClient.getSpotMarketAccount(0)!,
			position.balanceType
		);
	};

	// borrow balances round up, so allow 1 unit
	const expectUsdcBorrow = async (expected: BN) =>
		expect((await vaultUsdcBorrow()).sub(expected).abs().toNumber()).to.be.lte(
			1
		);

	before(async () => {
		const context = startLiteSVM();
		svmContextWrapper = new LiteSVMContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			svmContextWrapper.connection.toConnection(),
			'processed',
			1
		);

		usdcMint = await mockUSDCMint(svmContextWrapper);
		solPerpOracle = await mockOracleNoProgram(
			svmContextWrapper,
			initialSolPerpPrice
		);

		const adminWallet = new Wallet(
			Keypair.fromSecretKey(Buffer.from(TEST_ADMIN_KEYPAIR))
		);
		await svmContextWrapper.fundKeypair(
			adminWallet.payer,
			100 * LAMPORTS_PER_SOL
		);

		adminVelocityClient = new TestClient({
			connection: svmContextWrapper.connection.toConnection(),
			wallet: adminWallet,
			programID: new PublicKey(VELOCITY_PROGRAM_ID),
			opts: { commitment: 'confirmed' },
			...velocityClientConfig(),
		});
		await adminVelocityClient.initialize(usdcMint.publicKey, true);
		await adminVelocityClient.subscribe();

		await initializeQuoteSpotMarket(adminVelocityClient, usdcMint.publicKey);
		await initializeSolSpotMarket(adminVelocityClient, solPerpOracle);
		await adminVelocityClient.initializePerpMarket(
			0,
			solPerpOracle,
			ammInitialBaseAssetReserve,
			ammInitialQuoteAssetReserve,
			new BN(0),
			new BN(initialSolPerpPrice).mul(PEG_PRECISION)
		);
		await adminVelocityClient.fetchAccounts();

		const managerBootstrap = await bootstrapSignerClientAndUser({
			svmContext: svmContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: managerSigner,
			usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: velocityClientConfig(),
		});
		managerClient = managerBootstrap.vaultClient;
		managerVelocityClient = managerBootstrap.velocityClient;
		managerUSDCAccount = managerBootstrap.userUSDCAccount.publicKey;

		const depositorBootstrap = await bootstrapSignerClientAndUser({
			svmContext: svmContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: depositorSigner,
			usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: velocityClientConfig(),
		});
		depositorClient = depositorBootstrap.vaultClient;
		depositorVelocityClient = depositorBootstrap.velocityClient;
		depositorUSDCAccount = depositorBootstrap.userUSDCAccount.publicKey;
		depositorVaultDepositor = getVaultDepositorAddressSync(
			VAULT_PROGRAM_ID,
			commonVaultKey,
			depositorSigner.publicKey
		);

		await svmContextWrapper.fundKeypair(lpSigner, 20 * LAMPORTS_PER_SOL);
		const lpBootstrap = await bootstrapSignerClientAndUser({
			svmContext: svmContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: lpSigner,
			usdcMint,
			usdcAmount,
			depositCollateral: true,
			solAmount: new BN(10 * LAMPORTS_PER_SOL),
			vaultClientCliMode: true,
			velocityClientConfig: velocityClientConfig(),
		});
		lpVelocityClient = lpBootstrap.velocityClient;
		lpVaultClient = lpBootstrap.vaultClient;
		lpWSOLAccount = lpBootstrap.userWSOLAccount!;

		const provider = new LiteSVMProvider(
			svmContextWrapper.context,
			adminVelocityClient.wallet as anchor.Wallet
		);
		adminClient = new VaultClient({
			velocityClient: adminVelocityClient,
			// @ts-ignore
			program: new Program(IDL, provider),
		});

		await managerClient.initializeVault(
			{
				name: encodeName(vaultName),
				spotMarketIndex: 0,
				redeemPeriod: ZERO,
				maxTokens: ZERO,
				managementFee: ZERO,
				profitShare: 0,
				hurdleRate: 0,
				permissioned: false,
				minDepositAmount: ZERO,
			},
			{ noLut: true }
		);
		vaultTokenAccount = (
			await managerClient.program.account.vault.fetch(commonVaultKey)
		).tokenAccount;

		await depositorClient.initializeVaultDepositor(
			commonVaultKey,
			depositorSigner.publicKey,
			depositorSigner.publicKey,
			{ noLut: true }
		);
		await depositorClient.deposit(
			depositorVaultDepositor,
			vaultDeposit,
			undefined,
			{ noLut: true },
			depositorUSDCAccount
		);

		// Give the vault user SOL collateral so it can carry a USDC borrow. The
		// test build of velocity accepts deposits from an external authority.
		const lpDepositIxs = await lpVelocityClient.getDepositTxnIx(
			new BN(10 * LAMPORTS_PER_SOL),
			1,
			lpWSOLAccount
		);
		const lpUserKey = await lpVelocityClient.getUserAccountPublicKey();
		const lpUserStatsKey = lpVelocityClient.getUserStatsAccountPublicKey();
		const vaultUserStatsKey = getUserStatsAccountPublicKey(
			new PublicKey(VELOCITY_PROGRAM_ID),
			commonVaultKey
		);
		for (const ix of lpDepositIxs) {
			for (const key of ix.keys) {
				if (key.pubkey.equals(lpUserKey)) key.pubkey = vaultUserKey;
				else if (key.pubkey.equals(lpUserStatsKey))
					key.pubkey = vaultUserStatsKey;
			}
		}
		await lpVelocityClient.sendTransaction(
			await lpVelocityClient.buildTransaction(lpDepositIxs),
			[],
			lpVelocityClient.opts
		);

		await adminClient.updateMarginTradingEnabled(commonVaultKey, true, {
			noLut: true,
		});
		await adminClient.adminUpdateVaultClass(
			commonVaultKey,
			VaultClass.TRUSTED,
			{ noLut: true }
		);
		await managerClient.managerBorrow(
			commonVaultKey,
			0,
			managerBorrow,
			managerUSDCAccount,
			{ noLut: true }
		);

		await adminVelocityClient.updateSpotMarketStatus(
			0,
			MarketStatus.REDUCE_ONLY
		);

		await expectUsdcBorrow(outstandingBorrow);
		expect(await transitBalance()).to.equal(0n);
	});

	after(async () => {
		await adminVelocityClient.unsubscribe();
		await adminClient.unsubscribe();
		await managerClient.unsubscribe();
		await managerVelocityClient.unsubscribe();
		await depositorClient.unsubscribe();
		await depositorVelocityClient.unsubscribe();
		await lpVaultClient.unsubscribe();
		await lpVelocityClient.unsubscribe();
	});

	it('rejects a depositor deposit larger than the outstanding borrow', async () => {
		await expectRevert(
			() =>
				depositorClient.deposit(
					depositorVaultDepositor,
					usdc(500),
					undefined,
					{ noLut: true },
					depositorUSDCAccount
				),
			'DepositNotFullySettled'
		);
	});

	it('rejects a manager deposit larger than the outstanding borrow', async () => {
		await expectRevert(
			() =>
				managerClient.managerDeposit(
					commonVaultKey,
					usdc(500),
					{ noLut: true },
					managerUSDCAccount
				),
			'DepositNotFullySettled'
		);
	});

	it('rejects a manager repay larger than the outstanding borrow', async () => {
		await expectRevert(
			() =>
				managerClient.managerRepay(
					commonVaultKey,
					0,
					usdc(500),
					usdc(500),
					managerUSDCAccount,
					{ noLut: true }
				),
			'DepositNotFullySettled'
		);
	});

	it('still accepts a deposit that only repays the borrow', async () => {
		await depositorClient.deposit(
			depositorVaultDepositor,
			usdc(40),
			undefined,
			{ noLut: true },
			depositorUSDCAccount
		);

		expect(await transitBalance()).to.equal(0n);
		await expectUsdcBorrow(usdc(60));

		// exactly the remaining borrow, rounding included, settles in full
		await depositorClient.deposit(
			depositorVaultDepositor,
			await vaultUsdcBorrow(),
			undefined,
			{ noLut: true },
			depositorUSDCAccount
		);

		expect(await transitBalance()).to.equal(0n);
		const position = await vaultUsdcPosition();
		expect(position === undefined || position.scaledBalance.isZero()).to.equal(
			true
		);
	});
});
