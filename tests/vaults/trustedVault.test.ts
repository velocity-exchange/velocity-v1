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
	Vaults,
	VAULT_PROGRAM_ID,
	IDL,
	isNormalVaultClass,
	isTrustedVaultClass,
	WithdrawUnit,
} from '@velocity-exchange/vaults-sdk';
import {
	BulkAccountLoader,
	VELOCITY_PROGRAM_ID as VELOCITY_PROGRAM_ID,
	VelocityClient,
	OracleSource,
	PEG_PRECISION,
	PublicKey,
	QUOTE_PRECISION,
	TestClient,
	ZERO,
} from '@velocity-exchange/sdk';
import { TestBulkAccountLoader } from './common/testBulkAccountLoader';
import {
	bootstrapSignerClientAndUser,
	initializeQuoteSpotMarket,
	initializeSolSpotMarket,
	mockUSDCMint,
	printTxLogs,
} from './common/testHelpers';
import { Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';
import {
	mockOracleNoProgram,
	setFeedPriceNoProgram,
} from './common/svmOracle';
import { VaultClass } from '@velocity-exchange/vaults-sdk';

// ammInvariant == k == x * y
const mantissaSqrtScale = new BN(100_000);
const ammInitialQuoteAssetReserve = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);
const ammInitialBaseAssetReserve = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);

const SIX_MONTHS = 180 * 24 * 60 * 60;

describe('TestTrustedVault', () => {
	let vaultProgram: Program<Vaults>;
	const initialSolPerpPrice = 100;
	let adminVelocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let svmContextWrapper: LiteSVMContextWrapper;
	let usdcMint: Keypair;
	let solPerpOracle: PublicKey;
	const vaultName = 'fuel distribution vault';
	const commonVaultKey = getVaultAddressSync(
		VAULT_PROGRAM_ID,
		encodeName(vaultName)
	);
	const usdcAmount = new BN(1_000_000_000).mul(QUOTE_PRECISION);

	const managerSigner = Keypair.generate();
	let managerClient: VaultClient;
	let managerVelocityClient: VelocityClient;
	let managerUserUSDCAccount: PublicKey;

	let adminClient: VaultClient;

	const user1Signer = Keypair.generate();
	let user1Client: VaultClient;
	let user1VelocityClient: VelocityClient;
	let user1UserUSDCAccount: PublicKey;
	let user1VaultDepositor: PublicKey;

	// NOTE: this stays `beforeEach` (full chain rebuild per test) on purpose — do
	// not "optimize" it to a shared `before` like feeUpdate/transferVaultDepositorShares.
	// The borrow tests assert on ABSOLUTE global spot-market balances (e.g.
	// `spotMarket1.depositBalance === 100 * LAMPORTS_PER_SOL`) and each deposits SOL
	// into the same spot market, so a shared chain would accumulate balances across
	// tests and break those exact-equality assertions. A fresh chain per test is
	// required for correctness here.
	beforeEach(async () => {
		const context = startLiteSVM();

		// wrap the context to use it with the test helpers
		svmContextWrapper = new LiteSVMContextWrapper(context);

		vaultProgram = new Program<Vaults>(IDL, svmContextWrapper.provider);

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
			opts: {
				commitment: 'confirmed',
			},
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0, 1],
			subAccountIds: [],
			oracleInfos: [
				{ publicKey: solPerpOracle, source: OracleSource.PYTH_LAZER },
			],
			accountSubscription: {
				type: 'polling',
				accountLoader: bulkAccountLoader as BulkAccountLoader,
			},
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
			new BN(0), // 1 HOUR
			new BN(initialSolPerpPrice).mul(PEG_PRECISION)
		);

		await adminVelocityClient.fetchAccounts();

		const managerBootstrap = await bootstrapSignerClientAndUser({
			svmContext: svmContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: managerSigner,
			usdcMint: usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: {
				accountSubscription: {
					type: 'polling',
					accountLoader: bulkAccountLoader as BulkAccountLoader,
				},
				activeSubAccountId: 0,
				subAccountIds: [],
				perpMarketIndexes: [0],
				spotMarketIndexes: [0, 1],
				oracleInfos: [
					{ publicKey: solPerpOracle, source: OracleSource.PYTH_LAZER },
				],
			},
		});
		managerClient = managerBootstrap.vaultClient;
		managerVelocityClient = managerBootstrap.velocityClient;
		managerUserUSDCAccount = managerBootstrap.userUSDCAccount.publicKey;

		const provider = new LiteSVMProvider(
			svmContextWrapper.context,
			adminVelocityClient.wallet as anchor.Wallet
		);
		const program = new Program(IDL, provider);
		adminClient = new VaultClient({
			velocityClient: adminVelocityClient,
			// @ts-ignore
			program,
		});

		const user1Bootstrap = await bootstrapSignerClientAndUser({
			svmContext: svmContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: user1Signer,
			usdcMint: usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: {
				accountSubscription: {
					type: 'polling',
					accountLoader: bulkAccountLoader as BulkAccountLoader,
				},
				activeSubAccountId: 0,
				subAccountIds: [],
				perpMarketIndexes: [0],
				spotMarketIndexes: [0, 1],
				oracleInfos: [
					{ publicKey: solPerpOracle, source: OracleSource.PYTH_LAZER },
				],
			},
		});
		user1Client = user1Bootstrap.vaultClient;
		user1VelocityClient = user1Bootstrap.velocityClient;
		user1UserUSDCAccount = user1Bootstrap.userUSDCAccount.publicKey;
		user1VaultDepositor = getVaultDepositorAddressSync(
			vaultProgram.programId,
			commonVaultKey,
			user1Signer.publicKey
		);

		// initialize a vault and depositors
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
		await user1Client.initializeVaultDepositor(
			commonVaultKey,
			user1Signer.publicKey,
			user1Signer.publicKey,
			{ noLut: true }
		);
	});

	afterEach(async () => {
		await adminVelocityClient.unsubscribe();
		await adminClient.unsubscribe();
		await managerClient.unsubscribe();
		await managerVelocityClient.unsubscribe();
		await user1Client.unsubscribe();
		await user1VelocityClient.unsubscribe();
	});

	it('vaults initialized', async () => {
		const vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(vaultAcct.manager).to.deep.equal(managerSigner.publicKey);

		expect(isNormalVaultClass(vaultAcct.vaultClass)).to.deep.equal(true);

		const vaultDepositor = getVaultDepositorAddressSync(
			vaultProgram.programId,
			commonVaultKey,
			user1Signer.publicKey
		);
		const vdAcct = await vaultProgram.account.vaultDepositor.fetch(
			vaultDepositor
		);
		expect(vdAcct.vault).to.deep.equal(commonVaultKey);
	});

	it('admin can update vault class and borrow and repay', async () => {
		let vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(vaultAcct.manager).to.deep.equal(managerSigner.publicKey);
		expect(isNormalVaultClass(vaultAcct.vaultClass)).to.deep.equal(true);

		await adminClient.updateMarginTradingEnabled(commonVaultKey, true, {
			noLut: true,
		});

		await adminClient.adminUpdateVaultClass(
			commonVaultKey,
			VaultClass.TRUSTED,
			{ noLut: true }
		);

		vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(isTrustedVaultClass(vaultAcct.vaultClass)).to.deep.equal(true);

		// user1 deposit sol into velocity (for vault to borrow)
		await svmContextWrapper.fundKeypair(
			user1Signer,
			100 * LAMPORTS_PER_SOL
		);
		await user1VelocityClient.deposit(
			new BN(100 * LAMPORTS_PER_SOL),
			1,
			user1Signer.publicKey,
			undefined,
			undefined
		);

		// user1 deposits usdcAmount into vault
		await user1Client.deposit(
			user1VaultDepositor,
			usdcAmount,
			undefined,
			{ noLut: true },
			user1UserUSDCAccount
		);

		const vaultEquityBefore = await adminClient.calculateVaultEquity({
			address: commonVaultKey,
		});
		expect(vaultEquityBefore.toString()).to.deep.equal(usdcAmount.toString());

		await adminVelocityClient.fetchAccounts();
		const spotMarket1 = adminVelocityClient.getSpotMarketAccount(1);
		expect(spotMarket1!.depositBalance.toNumber()).to.deep.equal(
			100 * LAMPORTS_PER_SOL
		);

		const managerSOLBalance0 =
			await svmContextWrapper.connection.getBalance(
				managerSigner.publicKey
			);

		// manager performs borrow of 50 SOL
		const b = await managerClient.managerBorrow(
			commonVaultKey,
			1,
			new BN(50 * LAMPORTS_PER_SOL),
			undefined,
			{ noLut: true, cuPriceMicroLamports: 0 }
		);
		const e = await printTxLogs(
			svmContextWrapper.connection.toConnection(),
			b,
			false,
			// @ts-ignore
			adminClient.program
		);
		expect(e.length).to.deep.equal(2);
		expect((e[0].data.borrowAmount as BN).toNumber()).to.deep.equal(
			50 * LAMPORTS_PER_SOL
		);
		expect((e[0].data.borrowValue as BN).toNumber()).to.deep.equal(5000 * 1e6);
		expect(e[0].data.borrowSpotMarketIndex).to.deep.equal(1);
		expect(e[0].data.depositSpotMarketIndex).to.deep.equal(0);

		const managerSOLBalance1 =
			await svmContextWrapper.connection.getBalance(
				managerSigner.publicKey
			);

		// check spot market recognizes borrows
		const spotMarket11 = adminVelocityClient.getSpotMarketAccount(1);
		expect(spotMarket11!.borrowBalance.toNumber()).to.be.closeTo(
			50 * LAMPORTS_PER_SOL,
			5
		);

		// check manager borrowed SOL
		expect(
			(Number(managerSOLBalance1) - Number(managerSOLBalance0)) /
				LAMPORTS_PER_SOL
		).to.be.closeTo(50, 0.005);

		// check vault equity unchanged
		await adminClient.velocityClient.fetchAccounts();
		const vaultEquityAfterBorrow = await adminClient.calculateVaultEquity({
			address: commonVaultKey,
		});
		// Equity should be conserved across the borrow: the reduced net spot value
		// (the SOL borrow leg) is offset by managerBorrowedValue. The borrow leg is
		// negative and now floors toward -infinity to mirror the program's
		// get_token_value/safe_div_floor, so up to 1 unit of rounding is expected.
		expect(vaultEquityAfterBorrow.toNumber()).to.be.closeTo(
			vaultEquityBefore.toNumber(),
			1
		);

		// check vault records manager's borrow in deposit asset value
		vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(vaultAcct.managerBorrowedValue.toNumber()).to.deep.equal(5000 * 1e6);

		// manager repays in USDC
		const repayTx = await managerClient.managerRepay(
			commonVaultKey,
			0,
			new BN(4500 * 1e6), // repay 50 SOL * 100 - 10% = 4500 USDC
			new BN(5000 * 1e6), // zero out the borrow
			managerUserUSDCAccount,
			{ noLut: true, cuPriceMicroLamports: 0 }
		);
		const repayEvents = await printTxLogs(
			svmContextWrapper.connection.toConnection(),
			repayTx,
			false,
			// @ts-ignore
			adminClient.program
		);
		expect(repayEvents.length).to.deep.equal(2);
		expect(repayEvents[0].data.repayAmount.toNumber()).to.deep.equal(
			4500 * 1e6
		);
		expect(repayEvents[0].data.repayValue.toNumber()).to.deep.equal(5000 * 1e6);
		expect(repayEvents[0].data.repaySpotMarketIndex).to.deep.equal(0);
		expect(repayEvents[0].data.depositSpotMarketIndex).to.deep.equal(0);

		vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(vaultAcct.managerBorrowedValue.toNumber()).to.deep.equal(0);

		await adminClient.velocityClient.fetchAccounts();
		const vaultEquityAfterRepay = await adminClient.calculateVaultEquity({
			address: commonVaultKey,
		});
		// we repaid 10% less value
		// expect final vault equity to go down by 10% of the borrowed value.
		// Allow 1 unit: the residual borrow leg is valued with get_token_value's
		// floor-toward-negative-infinity rounding (safe_div_floor) for negatives.
		expect(vaultEquityAfterRepay.toNumber()).to.be.closeTo(
			vaultEquityBefore.toNumber() - 5000 * 1e6 * 0.1,
			1
		);
	});

	// A NAV snapshot books the interest of every market that prices it, and not only
	// of the denomination market. A manager borrow puts a liability on a second spot
	// market, and `calculate_user_equity` values that position through the second
	// market's own `cumulative_borrow_interest`. While that interest is un-booked, the
	// liability reads low and NAV reads high. The vault then overpays a withdrawer at
	// the expense of the other depositors.
	it('books a non-denomination market before pricing shares', async () => {
		await adminClient.updateMarginTradingEnabled(commonVaultKey, true, {
			noLut: true,
		});
		await adminClient.adminUpdateVaultClass(
			commonVaultKey,
			VaultClass.TRUSTED,
			{ noLut: true }
		);

		// user1 funds market 1 so there is SOL to borrow, then joins the vault.
		await svmContextWrapper.fundKeypair(
			user1Signer,
			100 * LAMPORTS_PER_SOL
		);
		await user1VelocityClient.deposit(
			new BN(100 * LAMPORTS_PER_SOL),
			1,
			user1Signer.publicKey,
			undefined,
			undefined
		);
		await user1Client.deposit(
			user1VaultDepositor,
			usdcAmount,
			undefined,
			{ noLut: true },
			user1UserUSDCAccount
		);

		// The borrow leaves the vault's velocity user holding a position in market 1.
		// 50 of 100 SOL borrowed is 50% utilization, so the market accrues at a real rate.
		await managerClient.managerBorrow(
			commonVaultKey,
			1,
			new BN(50 * LAMPORTS_PER_SOL),
			undefined,
			{ noLut: true, cuPriceMicroLamports: 0 }
		);

		await adminVelocityClient.fetchAccounts();
		const solSpotMarketKey =
			adminVelocityClient.getSpotMarketAccount(1)!.pubkey;
		/** Reads market 1 straight off chain, past the subscription cache. */
		const fetchSolBorrowIndex = async (): Promise<BN> =>
			(
				await (adminVelocityClient.program as any).account.spotMarket.fetch(
					solSpotMarketKey
				)
			).cumulativeBorrowInterest as BN;

		const borrowIndexBefore = await fetchSolBorrowIndex();
		await svmContextWrapper.moveTimeForward(SIX_MONTHS);
		// Re-post the same SOL price. The warp leaves the oracle stale, and equity is
		// gated on oracle validity, so without this the vault refuses to price at all
		// and the test could not tell a stale index from a stale oracle.
		await setFeedPriceNoProgram(
			svmContextWrapper,
			initialSolPerpPrice,
			solPerpOracle
		);

		// Nothing cranked market 1 over the warp, so its index is provably stale.
		expect(
			(await fetchSolBorrowIndex()).eq(borrowIndexBefore),
			'market 1 accrued without a crank; the fixture no longer isolates the refresh'
		).to.equal(true);

		// `request_withdraw` snapshots NAV and moves no tokens, so the refresh CPI is the
		// only thing in it that can advance market 1.
		const shares = (
			await vaultProgram.account.vaultDepositor.fetch(user1VaultDepositor)
		).vaultShares as BN;
		await user1Client.syncVaultUsers();
		await user1Client.requestWithdraw(
			user1VaultDepositor,
			shares,
			WithdrawUnit.SHARES,
			{ noLut: true }
		);

		expect(
			(await fetchSolBorrowIndex()).gt(borrowIndexBefore),
			'market 1 was not booked: NAV priced the borrow off a stale index'
		).to.equal(true);
	});

	it('admin can update vault class and update borrow', async () => {
		let vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(vaultAcct.manager).to.deep.equal(managerSigner.publicKey);
		expect(isNormalVaultClass(vaultAcct.vaultClass)).to.deep.equal(true);

		await adminClient.updateMarginTradingEnabled(commonVaultKey, true, {
			noLut: true,
		});

		await adminClient.adminUpdateVaultClass(
			commonVaultKey,
			VaultClass.TRUSTED,
			{ noLut: true }
		);

		vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(isTrustedVaultClass(vaultAcct.vaultClass)).to.deep.equal(true);

		// user1 deposit sol into velocity (for vault to borrow)
		await svmContextWrapper.fundKeypair(
			user1Signer,
			100 * LAMPORTS_PER_SOL
		);
		await user1VelocityClient.deposit(
			new BN(100 * LAMPORTS_PER_SOL),
			1,
			user1Signer.publicKey,
			undefined,
			undefined
		);

		// user1 deposits usdcAmount into vault
		await user1Client.deposit(
			user1VaultDepositor,
			usdcAmount,
			undefined,
			{ noLut: true },
			user1UserUSDCAccount
		);

		const vaultEquityBefore = await adminClient.calculateVaultEquity({
			address: commonVaultKey,
		});
		expect(vaultEquityBefore.toString()).to.deep.equal(usdcAmount.toString());

		await adminVelocityClient.fetchAccounts();
		const spotMarket1 = adminVelocityClient.getSpotMarketAccount(1);
		expect(spotMarket1!.depositBalance.toNumber()).to.deep.equal(
			100 * LAMPORTS_PER_SOL
		);

		const managerSOLBalance0 =
			await svmContextWrapper.connection.getBalance(
				managerSigner.publicKey
			);

		// manager performs borrow of 50 SOL
		const b = await managerClient.managerBorrow(
			commonVaultKey,
			1,
			new BN(50 * LAMPORTS_PER_SOL),
			undefined,
			{ noLut: true, cuPriceMicroLamports: 0 }
		);
		const e = await printTxLogs(
			svmContextWrapper.connection.toConnection(),
			b,
			false,
			// @ts-ignore
			adminClient.program
		);
		expect(e.length).to.deep.equal(2);
		expect((e[0].data.borrowAmount as BN).toNumber()).to.deep.equal(
			50 * LAMPORTS_PER_SOL
		);
		expect((e[0].data.borrowValue as BN).toNumber()).to.deep.equal(5000 * 1e6);
		expect(e[0].data.borrowSpotMarketIndex).to.deep.equal(1);
		expect(e[0].data.depositSpotMarketIndex).to.deep.equal(0);

		const managerSOLBalance1 =
			await svmContextWrapper.connection.getBalance(
				managerSigner.publicKey
			);

		// check spot market recognizes borrows
		const spotMarket11 = adminVelocityClient.getSpotMarketAccount(1);
		expect(spotMarket11!.borrowBalance.toNumber()).to.be.closeTo(
			50 * LAMPORTS_PER_SOL,
			5
		);

		// check manager borrowed SOL
		expect(
			(Number(managerSOLBalance1) - Number(managerSOLBalance0)) /
				LAMPORTS_PER_SOL
		).to.be.closeTo(50, 0.005);

		// check vault equity unchanged
		await adminClient.velocityClient.fetchAccounts();
		const vaultEquityAfterBorrow = await adminClient.calculateVaultEquity({
			address: commonVaultKey,
		});
		// Equity should be conserved across the borrow: the reduced net spot value
		// (the SOL borrow leg) is offset by managerBorrowedValue. The borrow leg is
		// negative and now floors toward -infinity to mirror the program's
		// get_token_value/safe_div_floor, so up to 1 unit of rounding is expected.
		expect(vaultEquityAfterBorrow.toNumber()).to.be.closeTo(
			vaultEquityBefore.toNumber(),
			1
		);

		// check vault records manager's borrow in deposit asset value
		vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(vaultAcct.managerBorrowedValue.toNumber()).to.deep.equal(5000 * 1e6);

		// manager repays in USDC
		await managerClient.managerUpdateBorrow(commonVaultKey, new BN(0), {
			noLut: true,
			cuPriceMicroLamports: 0,
		});

		vaultAcct = await vaultProgram.account.vault.fetch(commonVaultKey);
		expect(vaultAcct.managerBorrowedValue.toNumber()).to.deep.equal(0);

		await adminClient.velocityClient.fetchAccounts();
		const vaultEquityAfterRepay = await adminClient.calculateVaultEquity({
			address: commonVaultKey,
		});
		// we repaid 10% less value
		// expect final vault equity to go down by 10% of the borrowed value.
		// Allow 1 unit: the residual borrow leg is valued with get_token_value's
		// floor-toward-negative-infinity rounding (safe_div_floor) for negatives.
		expect(vaultEquityAfterRepay.toNumber()).to.be.closeTo(
			vaultEquityBefore.toNumber() - 5000 * 1e6,
			1
		);
	});
});
