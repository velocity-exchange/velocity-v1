/**
 * Regression suite for the NAV interest refresh (OtterSec #136/#137).
 *
 * `Vault::calculate_equity` values the vault's velocity spot deposit off the spot
 * market's stored `cumulative_deposit_interest`. Only velocity may write that
 * account, so the vaults program must CPI
 * `update_spot_market_cumulative_interest` before it snapshots NAV.
 *
 * `deposit` once refreshed the market only after it minted shares, as a side
 * effect of the deposit CPI (OtterSec #136). An entrant then priced its shares
 * against a stale index and captured a slice of the lender interest the
 * incumbents had already earned.
 *
 * `request_withdraw` and `cancel_withdraw_request` once did not refresh at all
 * (OtterSec #137). The recorded request value understated NAV, and the
 * cancellation share-forfeiture rule saw no request-window gain to forfeit.
 *
 * Every test below drives real interest accrual and then warps the clock without
 * cranking the market, so the on-chain index is provably stale when the vault
 * instruction runs. A borrower creates the utilization on the vault's
 * denomination market.
 */
import * as anchor from '@coral-xyz/anchor';
import { BN, Program } from '@coral-xyz/anchor';
import { expect } from 'chai';
import { BankrunContextWrapper } from './common/bankrunConnection';
import { startAnchor } from 'solana-bankrun';
import {
	VaultClient,
	getVaultAddressSync,
	getVaultDepositorAddressSync,
	encodeName,
	VAULT_PROGRAM_ID,
	IDL,
	WithdrawUnit,
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
} from '@velocity-exchange/sdk';
import { TestBulkAccountLoader } from './common/testBulkAccountLoader';
import {
	bootstrapSignerClientAndUserBankrun,
	initializeQuoteSpotMarket,
	initializeSolSpotMarket,
	mockUSDCMintBankrun,
} from './common/testHelpers';
import { Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';
import { mockOracleNoProgram } from './common/bankrunOracle';
import { BankrunProvider } from 'anchor-bankrun';

// ammInvariant == k == x * y
const mantissaSqrtScale = new BN(100_000);
const ammInitialQuoteAssetReserve = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);
const ammInitialBaseAssetReserve = new BN(5 * 10 ** 13).mul(mantissaSqrtScale);

const SIX_MONTHS = 180 * 24 * 60 * 60;

describe('vault NAV interest refresh (OtterSec #136/#137)', () => {
	const initialSolPerpPrice = 100;

	let adminVelocityClient: TestClient;
	let bulkAccountLoader: TestBulkAccountLoader;
	let bankrunContextWrapper: BankrunContextWrapper;
	let usdcMint: Keypair;
	let solPerpOracle: PublicKey;
	let usdcSpotMarketKey: PublicKey;

	const vaultName = 'nav interest refresh vault';
	const commonVaultKey = getVaultAddressSync(
		VAULT_PROGRAM_ID,
		encodeName(vaultName)
	);

	// per-signer USDC mint amount
	const usdcAmount = new BN(1_000_000).mul(QUOTE_PRECISION);
	// each vault depositor puts in $1,000
	const depositAmount = new BN(1_000).mul(QUOTE_PRECISION);
	// borrow 30% of the vault's $1,000 deposit -> 30% utilization on market 0,
	// which sits below the 50% optimal utilization -> 60% APR borrow rate,
	// 18% APR deposit rate. Over six months that is ~$88 of lender interest.
	const borrowAmount = new BN(300).mul(QUOTE_PRECISION);

	const managerSigner = Keypair.generate();
	let managerClient: VaultClient;
	let managerVelocityClient: VelocityClient;

	const user1Signer = Keypair.generate();
	let user1Client: VaultClient;
	let user1VelocityClient: VelocityClient;
	let user1UserUSDCAccount: PublicKey;
	let user1VaultDepositor: PublicKey;

	const user2Signer = Keypair.generate();
	let user2Client: VaultClient;
	let user2VelocityClient: VelocityClient;
	let user2UserUSDCAccount: PublicKey;
	let user2VaultDepositor: PublicKey;

	const borrowerSigner = Keypair.generate();
	let borrowerVelocityClient: VelocityClient;
	let borrowerClient: VaultClient;
	let borrowerUSDCAccount: PublicKey;

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

	/** Reads `cumulative_deposit_interest` straight off chain (no subscription cache). */
	const fetchDepositInterestIndex = async (): Promise<BN> => {
		const spotMarket = await (
			adminVelocityClient.program as any
		).account.spotMarket.fetch(usdcSpotMarketKey);
		return spotMarket.cumulativeDepositInterest as BN;
	};

	const fetchVault = async () =>
		await managerClient.program.account.vault.fetch(commonVaultKey);

	const fetchVaultDepositor = async (vaultDepositor: PublicKey) =>
		await managerClient.program.account.vaultDepositor.fetch(vaultDepositor);

	/** Borrow USDC out of market 0 so the market starts accruing interest. */
	const openBorrow = async () => {
		await borrowerVelocityClient.fetchAccounts();
		await borrowerVelocityClient.withdraw(
			borrowAmount,
			0,
			borrowerUSDCAccount,
			false
		);
	};

	beforeEach(async () => {
		const context = await startAnchor('', [], []);
		bankrunContextWrapper = new BankrunContextWrapper(context);

		bulkAccountLoader = new TestBulkAccountLoader(
			bankrunContextWrapper.connection.toConnection(),
			'processed',
			1
		);

		usdcMint = await mockUSDCMintBankrun(bankrunContextWrapper);
		solPerpOracle = await mockOracleNoProgram(
			bankrunContextWrapper,
			initialSolPerpPrice
		);

		adminVelocityClient = new TestClient({
			connection: bankrunContextWrapper.connection.toConnection(),
			wallet: bankrunContextWrapper.provider.wallet,
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
		usdcSpotMarketKey = adminVelocityClient.getSpotMarketAccount(0)!.pubkey;

		const managerBootstrap = await bootstrapSignerClientAndUserBankrun({
			bankrunContext: bankrunContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: managerSigner,
			usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: velocityClientConfig(),
		});
		managerClient = managerBootstrap.vaultClient;
		managerVelocityClient = managerBootstrap.velocityClient;

		const user1Bootstrap = await bootstrapSignerClientAndUserBankrun({
			bankrunContext: bankrunContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: user1Signer,
			usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: velocityClientConfig(),
		});
		user1Client = user1Bootstrap.vaultClient;
		user1VelocityClient = user1Bootstrap.velocityClient;
		user1UserUSDCAccount = user1Bootstrap.userUSDCAccount.publicKey;
		user1VaultDepositor = getVaultDepositorAddressSync(
			VAULT_PROGRAM_ID,
			commonVaultKey,
			user1Signer.publicKey
		);

		const user2Bootstrap = await bootstrapSignerClientAndUserBankrun({
			bankrunContext: bankrunContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: user2Signer,
			usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: velocityClientConfig(),
		});
		user2Client = user2Bootstrap.vaultClient;
		user2VelocityClient = user2Bootstrap.velocityClient;
		user2UserUSDCAccount = user2Bootstrap.userUSDCAccount.publicKey;
		user2VaultDepositor = getVaultDepositorAddressSync(
			VAULT_PROGRAM_ID,
			commonVaultKey,
			user2Signer.publicKey
		);

		// The borrower never touches the vault. It only creates utilization on the
		// vault's denomination market, which is what pays lender interest.
		const borrowerBootstrap = await bootstrapSignerClientAndUserBankrun({
			bankrunContext: bankrunContextWrapper,
			programId: VAULT_PROGRAM_ID,
			signer: borrowerSigner,
			usdcMint,
			usdcAmount,
			vaultClientCliMode: true,
			velocityClientConfig: velocityClientConfig(),
		});
		borrowerVelocityClient = borrowerBootstrap.velocityClient;
		borrowerClient = borrowerBootstrap.vaultClient;
		borrowerUSDCAccount = borrowerBootstrap.userUSDCAccount.publicKey;

		const provider = new BankrunProvider(
			bankrunContextWrapper.context,
			adminVelocityClient.wallet as anchor.Wallet
		);
		const program = new Program(IDL, provider);
		adminClient = new VaultClient({
			// @ts-ignore
			velocityClient: adminVelocityClient,
			// @ts-ignore
			program,
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

		// Fund the borrower with SOL collateral so it can borrow USDC.
		await bankrunContextWrapper.fundKeypair(
			borrowerSigner,
			1_000 * LAMPORTS_PER_SOL
		);
		await borrowerVelocityClient.deposit(
			new BN(100 * LAMPORTS_PER_SOL),
			1,
			borrowerSigner.publicKey,
			undefined,
			undefined
		);
		await borrowerVelocityClient.updateUserMarginTradingEnabled([
			{ marginTradingEnabled: true, subAccountId: 0 },
		]);
	});

	afterEach(async () => {
		await adminVelocityClient.unsubscribe();
		await adminClient.unsubscribe();
		await managerClient.unsubscribe();
		await managerVelocityClient.unsubscribe();
		await user1Client.unsubscribe();
		await user1VelocityClient.unsubscribe();
		await user2Client.unsubscribe();
		await user2VelocityClient.unsubscribe();
		await borrowerClient.unsubscribe();
		await borrowerVelocityClient.unsubscribe();
	});

	it('#136: a later depositor cannot mint against a stale interest index', async () => {
		// user1 enters an empty vault: 1 share per token.
		await user1Client.deposit(
			user1VaultDepositor,
			depositAmount,
			{ authority: user1Signer.publicKey, vault: commonVaultKey },
			{ noLut: true },
			user1UserUSDCAccount
		);
		await openBorrow();

		const indexBeforeWarp = await fetchDepositInterestIndex();
		await bankrunContextWrapper.moveTimeForward(SIX_MONTHS);

		// Nothing cranked the market, so the stored index is provably stale. This is
		// the state in which the old `deposit` priced shares.
		expect((await fetchDepositInterestIndex()).eq(indexBeforeWarp)).to.equal(
			true
		);

		const vaultBefore = await fetchVault();
		const user1Shares = (await fetchVaultDepositor(user1VaultDepositor))
			.vaultShares as BN;
		expect(vaultBefore.totalShares.eq(user1Shares)).to.equal(true);

		await user2Client.syncVaultUsers();
		await user2Client.deposit(
			user2VaultDepositor,
			depositAmount,
			{ authority: user2Signer.publicKey, vault: commonVaultKey },
			{ noLut: true },
			user2UserUSDCAccount
		);

		// The vault instruction itself advanced the index via the refresh CPI.
		const indexAfter = await fetchDepositInterestIndex();
		expect(indexAfter.gt(indexBeforeWarp)).to.equal(true);

		// NAV just before user2 entered, priced off the FRESH index. The vault's
		// only position is the market-0 deposit it made when the index was
		// `indexBeforeWarp`, so it scales exactly with the index.
		const freshNav = depositAmount.mul(indexAfter).div(indexBeforeWarp);
		expect(freshNav.gt(depositAmount)).to.equal(true);

		const user2Shares = (await fetchVaultDepositor(user2VaultDepositor))
			.vaultShares as BN;

		// Direction 1, the old outcome. Pricing against the stale index valued the
		// vault at exactly `depositAmount`. It then minted user2 the same number of
		// shares as user1.
		expect(user2Shares.lt(user1Shares)).to.equal(true);
		expect(user2Shares.eq(user1Shares)).to.equal(false);

		// Direction 2, the current outcome: shares = amount * totalShares / freshNav.
		const expectedUser2Shares = depositAmount.mul(user1Shares).div(freshNav);
		expect(
			user2Shares.sub(expectedUser2Shares).abs().lten(2),
			`user2Shares=${user2Shares.toString()} expected=${expectedUser2Shares.toString()}`
		).to.equal(true);

		// And the value split is fair in both directions: user1 keeps all of the
		// interest it earned, user2 is worth what it just paid in.
		const vaultAfter = await fetchVault();
		const totalNav = freshNav.add(depositAmount);
		const user1Equity = totalNav
			.mul(user1Shares)
			.div(vaultAfter.totalShares as BN);
		const user2Equity = totalNav
			.mul(user2Shares)
			.div(vaultAfter.totalShares as BN);

		// tolerance: 0.01% of the deposit, i.e. $0.10 on $1,000
		const tolerance = depositAmount.divn(10_000);
		expect(
			user1Equity.sub(freshNav).abs().lte(tolerance),
			`user1Equity=${user1Equity.toString()} freshNav=${freshNav.toString()}`
		).to.equal(true);
		expect(
			user2Equity.sub(depositAmount).abs().lte(tolerance),
			`user2Equity=${user2Equity.toString()} deposit=${depositAmount.toString()}`
		).to.equal(true);

		// The stale-index outcome would have handed user2 half of the interest.
		const staleSplit = totalNav.divn(2);
		expect(user2Equity.lt(staleSplit)).to.equal(true);
	});

	it('#137: request_withdraw refreshes the index before recording the request value', async () => {
		await user1Client.deposit(
			user1VaultDepositor,
			depositAmount,
			{ authority: user1Signer.publicKey, vault: commonVaultKey },
			{ noLut: true },
			user1UserUSDCAccount
		);
		await openBorrow();

		const indexBeforeWarp = await fetchDepositInterestIndex();
		await bankrunContextWrapper.moveTimeForward(SIX_MONTHS);
		expect((await fetchDepositInterestIndex()).eq(indexBeforeWarp)).to.equal(
			true
		);

		const user1Shares = (await fetchVaultDepositor(user1VaultDepositor))
			.vaultShares as BN;

		await user1Client.syncVaultUsers();
		await user1Client.requestWithdraw(
			user1VaultDepositor,
			user1Shares,
			WithdrawUnit.SHARES,
			{ noLut: true }
		);

		// `request_withdraw` has no other velocity CPI, so the index can only have
		// moved because of the refresh CPI. The old code left this equal.
		const indexAfter = await fetchDepositInterestIndex();
		expect(
			indexAfter.gt(indexBeforeWarp),
			`index did not advance: ${indexAfter.toString()}`
		).to.equal(true);

		const freshNav = depositAmount.mul(indexAfter).div(indexBeforeWarp);
		const request = (await fetchVaultDepositor(user1VaultDepositor))
			.lastWithdrawRequest as { shares: BN; value: BN; ts: BN };

		// Direction 1: the stale index would have recorded exactly `depositAmount`.
		expect(request.value.gt(depositAmount)).to.equal(true);
		// Direction 2: it records the fresh NAV (user1 owns 100% of the vault).
		expect(
			request.value.sub(freshNav).abs().lte(depositAmount.divn(10_000)),
			`value=${request.value.toString()} freshNav=${freshNav.toString()}`
		).to.equal(true);
	});

	it('#137: cancel_withdraw_request forfeits request-window interest', async () => {
		// Two depositors: the share-forfeiture rule is skipped when the canceller
		// owns the entire vault, so user1 must not be the only shareholder.
		await user1Client.deposit(
			user1VaultDepositor,
			depositAmount,
			{ authority: user1Signer.publicKey, vault: commonVaultKey },
			{ noLut: true },
			user1UserUSDCAccount
		);
		await user2Client.syncVaultUsers();
		await user2Client.deposit(
			user2VaultDepositor,
			depositAmount,
			{ authority: user2Signer.publicKey, vault: commonVaultKey },
			{ noLut: true },
			user2UserUSDCAccount
		);
		await openBorrow();

		// Request immediately, while the index is fresh, so the recorded value is
		// user1's equity at t0 regardless of the fix.
		const sharesBeforeRequest = (await fetchVaultDepositor(user1VaultDepositor))
			.vaultShares as BN;
		await user1Client.syncVaultUsers();
		await user1Client.requestWithdraw(
			user1VaultDepositor,
			sharesBeforeRequest,
			WithdrawUnit.SHARES,
			{ noLut: true }
		);

		const vdAfterRequest = await fetchVaultDepositor(user1VaultDepositor);
		const sharesAfterRequest = vdAfterRequest.vaultShares as BN;
		const requestValue = (vdAfterRequest.lastWithdrawRequest as { value: BN })
			.value;

		const indexBeforeWarp = await fetchDepositInterestIndex();
		await bankrunContextWrapper.moveTimeForward(SIX_MONTHS);
		expect((await fetchDepositInterestIndex()).eq(indexBeforeWarp)).to.equal(
			true
		);

		await user1Client.syncVaultUsers();
		await user1Client.cancelRequestWithdraw(user1VaultDepositor, {
			noLut: true,
		});

		// Only the refresh CPI can move the index in `cancel_withdraw_request`.
		const indexAfter = await fetchDepositInterestIndex();
		expect(
			indexAfter.gt(indexBeforeWarp),
			`index did not advance: ${indexAfter.toString()}`
		).to.equal(true);

		const sharesAfterCancel = (await fetchVaultDepositor(user1VaultDepositor))
			.vaultShares as BN;

		// Direction 1: with a stale index the vault looks unchanged since the
		// request, `calculate_shares_lost` returns 0, and user1 keeps every share.
		// User1 then keeps request-window interest it had already asked to exit.
		expect(
			sharesAfterCancel.lt(sharesAfterRequest),
			`shares not forfeited: ${sharesAfterCancel.toString()} vs ${sharesAfterRequest.toString()}`
		).to.equal(true);

		// Direction 2: user1's post-cancel equity is pinned back to the value it
		// locked in at request time.
		const vaultAfter = await fetchVault();
		const user2Shares = (await fetchVaultDepositor(user2VaultDepositor))
			.vaultShares as BN;
		const totalNav = depositAmount.muln(2).mul(indexAfter).div(indexBeforeWarp);
		const user1Equity = totalNav
			.mul(sharesAfterCancel)
			.div(vaultAfter.totalShares as BN);
		expect(
			user1Equity.sub(requestValue).abs().lte(depositAmount.divn(1_000)),
			`user1Equity=${user1Equity.toString()} requestValue=${requestValue.toString()}`
		).to.equal(true);

		// The forfeited shares accrue to the remaining shareholder. User2 is worth
		// more than the $1,000 it put in.
		const user2Equity = totalNav
			.mul(user2Shares)
			.div(vaultAfter.totalShares as BN);
		expect(
			user2Equity.gt(depositAmount),
			`user2Equity=${user2Equity.toString()} deposit=${depositAmount.toString()}`
		).to.equal(true);
	});

	// The refresh CPI once carried velocity's `spot_market_valid` access control, which
	// rejects a delisted market. Every vault instruction ran that CPI first, so delisting
	// the denomination market froze all of them, including the paths that move no tokens.
	// Delisting is terminal, because `handle_update_spot_market_status` carries the same
	// guard, so there was no recovery.
	//
	// The token-moving paths stay blocked either way. Velocity's own withdraw admits only
	// Active, ReduceOnly and Settlement. This test covers what the refresh unblocks.
	it('keeps the token-less paths working when the denomination market is delisted', async () => {
		await user1Client.deposit(
			user1VaultDepositor,
			depositAmount,
			{ authority: user1Signer.publicKey, vault: commonVaultKey },
			{ noLut: true },
			user1UserUSDCAccount
		);
		await openBorrow();

		await adminVelocityClient.updateSpotMarketStatus(0, MarketStatus.DELISTED);
		await adminVelocityClient.fetchAccounts();

		// Request moves no tokens, so a wound-down market is no reason to block it.
		const shares = (await fetchVaultDepositor(user1VaultDepositor))
			.vaultShares as BN;
		await user1Client.syncVaultUsers();
		await user1Client.requestWithdraw(
			user1VaultDepositor,
			shares,
			WithdrawUnit.SHARES,
			{ noLut: true }
		);
		expect(
			(
				(await fetchVaultDepositor(user1VaultDepositor))
					.lastWithdrawRequest as { value: BN }
			).value.gt(ZERO)
		).to.equal(true);

		// Cancelling has no recovery path of its own. A depositor stuck mid-request
		// could neither finish the request nor undo it.
		await user1Client.syncVaultUsers();
		await user1Client.cancelRequestWithdraw(user1VaultDepositor, {
			noLut: true,
		});
		expect(
			(
				(await fetchVaultDepositor(user1VaultDepositor))
					.lastWithdrawRequest as { value: BN }
			).value.eq(ZERO)
		).to.equal(true);

		// The interest still accrues on a delisted market, so the refresh still books it.
		// Delisting does not stop the accrual. `deposit` and `force_delete_user` book
		// interest there too, so refusing here only ever blocked the caller.
		const indexBefore = await fetchDepositInterestIndex();
		await bankrunContextWrapper.moveTimeForward(SIX_MONTHS);
		await user1Client.syncVaultUsers();
		await user1Client.requestWithdraw(
			user1VaultDepositor,
			shares,
			WithdrawUnit.SHARES,
			{ noLut: true }
		);
		expect((await fetchDepositInterestIndex()).gt(indexBefore)).to.equal(true);
	});
});
