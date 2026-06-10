import {
	TransactionConfirmationStatus,
	AccountInfo,
	Keypair,
	PublicKey,
	Transaction,
	RpcResponseAndContext,
	Commitment,
	TransactionSignature,
	SignatureStatusConfig,
	SignatureStatus,
	GetVersionedTransactionConfig,
	GetTransactionConfig,
	VersionedTransaction,
	SimulateTransactionConfig,
	SimulatedTransactionResponse,
	TransactionReturnData,
	TransactionError,
	SignatureResultCallback,
	Connection as SolanaConnection,
	SystemProgram,
	Blockhash,
	LogsFilter,
	LogsCallback,
	AccountChangeCallback,
	LAMPORTS_PER_SOL,
	AddressLookupTableAccount,
} from '@solana/web3.js';
import {
	LiteSVM,
	Clock,
	Account as LiteSVMAccount,
	SlotHash,
	TransactionMetadata,
	FailedTransactionMetadata,
} from 'litesvm';
import bs58 from 'bs58';
import * as fs from 'fs';
import * as path from 'path';
import { BN, Wallet } from '../isomorphic/anchor';
import { Account, unpackAccount } from '@solana/spl-token';
import { isVersionedTransaction } from '../tx/utils';

export type ClientSubscriptionId = number;
export type Connection = SolanaConnection | LiteSVMConnection;

/** A program to load into the SVM at genesis, looked up in tests/fixtures or target/deploy */
export interface AddedProgram {
	name: string;
	programId: PublicKey;
}

/** An account to preload into the SVM at genesis */
export interface AddedAccount {
	address: PublicKey;
	info: AccountInfo<Buffer | Uint8Array>;
}

type SVMTransactionMeta = {
	result: string | null;
	logMessages: string[];
	computeUnitsConsumed: bigint;
	slot: number;
};

type SVMTransactionMetaNormalized = {
	logMessages: string[];
	err: TransactionError;
};

type SVMTransactionResponse = {
	slot: number;
	meta: SVMTransactionMetaNormalized;
};

/**
 * Display strings for `TransactionErrorFieldless`, matching the Display impl
 * of solana's `TransactionError` so error messages stay identical to what
 * tests asserted against under solana-bankrun (banks server formatting).
 */
const TRANSACTION_ERROR_FIELDLESS_DISPLAY: string[] = [
	'Account in use',
	'Account loaded twice',
	'Attempt to debit an account but found no record of a prior credit.',
	'Attempt to load a program that does not exist',
	'Insufficient funds for fee',
	'This account may not be used to pay transaction fees',
	'This transaction has already been processed',
	'Blockhash not found',
	'Loader call chain is too deep',
	'Transaction requires a fee but has no signature present',
	'Transaction contains an invalid account reference',
	'Transaction did not pass signature verification',
	'This program may not be used for executing instructions',
	'Transaction failed to sanitize accounts offsets correctly',
	'Transactions are currently disabled due to cluster maintenance',
	'Transaction processing left an account with an outstanding borrowed reference',
	'Transaction would exceed max Block Cost Limit',
	'Transaction version is unsupported',
	'Transaction loads a writable account that cannot be written',
	'Transaction would exceed max account limit within the block',
	'Transaction would exceed account data limit within the block',
	'Transaction locked too many accounts',
	"Transaction loads an address table account that doesn't exist",
	'Transaction loads an address table account with an invalid owner',
	'Transaction loads an address table account with invalid data',
	'Transaction address table lookup uses an invalid index',
	'Transaction leaves an account with a lower balance than rent-exempt minimum',
	'Transaction would exceed max Vote Cost Limit',
	'Transaction would exceed total account data limit',
	'Transaction exceeded max loaded accounts data size cap',
	'ResanitizationNeeded',
	'LoadedAccountsDataSizeLimit set for transaction must be greater than 0',
	'Sum of account balances before and after transaction do not match',
	'Program cache hit max limit',
	'CommitCancelled',
];

/**
 * Display strings for `InstructionErrorFieldless`, matching the Display impl
 * of solana's `InstructionError`.
 */
const INSTRUCTION_ERROR_FIELDLESS_DISPLAY: string[] = [
	'generic instruction error',
	'invalid program argument',
	'invalid instruction data',
	'invalid account data for instruction',
	'account data too small for instruction',
	'insufficient funds for instruction',
	'incorrect program id for instruction',
	'missing required signature for instruction',
	'instruction requires an uninitialized account',
	'instruction requires an initialized account',
	'sum of account balances before and after instruction do not match',
	'instruction illegally modified the program id of an account',
	'instruction spent from the balance of an account it does not own',
	'instruction modified data of an account it does not own',
	'instruction changed the balance of a read-only account',
	'instruction modified data of a read-only account',
	'instruction contains duplicate accounts',
	'instruction changed executable bit of an account',
	'instruction modified rent epoch of an account',
	'insufficient account keys for instruction',
	"program other than the account's owner changed the size of the account data",
	'instruction expected an executable account',
	'instruction tries to borrow reference for an account which is already borrowed',
	'instruction left account with an outstanding borrowed reference',
	'instruction modifications of multiply-passed account differ',
	'program returned invalid error code',
	'instruction changed executable accounts data',
	'instruction changed the balance of an executable account',
	'executable accounts must be rent exempt',
	'Unsupported program id',
	'Cross-program invocation call depth too deep',
	'An account required by the instruction is missing',
	'Cross-program invocation reentrancy not allowed for this instruction',
	'Length of the seed is too long for address generation',
	'Provided seeds do not result in a valid address',
	'Failed to reallocate account data',
	'Computational budget exceeded',
	'Cross-program invocation with unauthorized signer or writable account',
	'Failed to create program execution environment',
	'Program failed to complete',
	'Program failed to compile',
	'Account is immutable',
	'Incorrect authority provided',
	'An account does not have enough lamports to be rent-exempt',
	'Invalid account owner',
	'Program arithmetic overflowed',
	'Unsupported sysvar',
	'Provided owner is not allowed',
	'Accounts data allocations exceeded the maximum allowed per transaction',
	'Max accounts exceeded',
	'Max instruction trace length exceeded',
	'Builtin programs must consume compute units',
	'Failed to serialize or deserialize account data',
];

function formatInstructionError(err: any): string {
	if (typeof err === 'number') {
		return (
			INSTRUCTION_ERROR_FIELDLESS_DISPLAY[err] ?? `instruction error: ${err}`
		);
	}
	switch (err?.constructor?.name) {
		case 'InstructionErrorCustom':
			return `custom program error: 0x${err.code.toString(16)}`;
		case 'InstructionErrorBorshIo':
			return `Failed to serialize or deserialize account data: ${err.msg}`;
		default:
			return String(err);
	}
}

/**
 * Formats litesvm's structured transaction errors into the same strings the
 * banks server produced under solana-bankrun (solana `TransactionError`
 * Display impl), e.g. `Error processing Instruction 1: custom program error: 0x1773`.
 */
function formatTransactionError(err: any): string {
	if (typeof err === 'number') {
		return (
			TRANSACTION_ERROR_FIELDLESS_DISPLAY[err] ?? `transaction error: ${err}`
		);
	}
	switch (err?.constructor?.name) {
		case 'TransactionErrorInstructionError':
			return `Error processing Instruction ${
				err.index
			}: ${formatInstructionError(err.err())}`;
		case 'TransactionErrorDuplicateInstruction':
			return `Transaction contains a duplicate instruction (${err.index}) that is not allowed`;
		case 'TransactionErrorInsufficientFundsForRent':
			return `Transaction results in an account (${err.accountIndex}) with insufficient funds for rent`;
		case 'TransactionErrorProgramExecutionTemporarilyRestricted':
			return `Execution of the program referenced by account at index ${err.accountIndex} is temporarily restricted`;
		default:
			return String(err);
	}
}

function findWorkspaceRoot(startDir: string): string {
	let dir = path.resolve(startDir);
	for (;;) {
		if (fs.existsSync(path.join(dir, 'Anchor.toml'))) {
			return dir;
		}
		const parent = path.dirname(dir);
		if (parent === dir) {
			throw new Error(
				`Could not find Anchor.toml in ${startDir} or any parent directory`
			);
		}
		dir = parent;
	}
}

function parseLocalnetPrograms(
	root: string
): { name: string; programId: PublicKey }[] {
	const toml = fs.readFileSync(path.join(root, 'Anchor.toml'), 'utf8');
	const section = toml.split('[programs.localnet]')[1];
	if (!section) {
		return [];
	}
	const programs: { name: string; programId: PublicKey }[] = [];
	for (const line of section.split('\n')) {
		const trimmed = line.trim();
		if (trimmed.startsWith('[')) {
			break;
		}
		const match = trimmed.match(/^([A-Za-z0-9_-]+)\s*=\s*"([^"]+)"/);
		if (match) {
			programs.push({ name: match[1], programId: new PublicKey(match[2]) });
		}
	}
	return programs;
}

function findProgramFile(root: string, name: string): string {
	for (const dir of ['tests/fixtures', 'target/deploy']) {
		const file = path.join(root, dir, `${name}.so`);
		if (fs.existsSync(file)) {
			return file;
		}
	}
	throw new Error(
		`Could not find ${name}.so under tests/fixtures or target/deploy`
	);
}

/**
 * Replaces solana-bankrun's `ProgramTestContext`: owns the LiteSVM instance,
 * the funded payer, and direct state-manipulation helpers (setAccount,
 * warpToSlot, setClock). Also maintains the SlotHashes sysvar so address
 * lookup table creation against a recent slot keeps working as slots warp.
 */
export class LiteSVMContext {
	public readonly svm: LiteSVM;
	public readonly payer: Keypair;
	private readonly inner: any;
	private recentSlots: bigint[] = [];
	private slotHashPool: SlotHash[] = [];
	private static readonly MAX_RECENT_SLOTS = 64;

	constructor(svm: LiteSVM, payer: Keypair) {
		this.svm = svm;
		this.payer = payer;
		// the napi inner exposes byte-oriented variants of the public API,
		// which avoids round-tripping pubkeys through base58
		// @ts-ignore
		this.inner = svm.inner;
		this.registerSlot(svm.getClock().slot);
	}

	get lastBlockhash(): string {
		return this.svm.latestBlockhash();
	}

	getClock(): Clock {
		return this.svm.getClock();
	}

	setClock(clock: Clock): void {
		this.svm.setClock(clock);
	}

	warpToSlot(slot: bigint): void {
		this.svm.warpToSlot(slot);
		this.registerSlot(slot);
		// the banks server rotated the blockhash when warping; mirror that so
		// repeated identical transactions produce distinct signatures
		this.svm.expireBlockhash();
	}

	setAccount(address: PublicKey, info: AccountInfo<Buffer | Uint8Array>): void {
		this.inner.setAccount(
			address.toBytes(),
			new LiteSVMAccount(
				BigInt(info.lamports),
				Uint8Array.from(info.data),
				info.owner.toBytes(),
				info.executable,
				BigInt(info.rentEpoch ?? 0)
			)
		);
	}

	getAccount(address: PublicKey): AccountInfo<Buffer> | null {
		const account = this.inner.getAccount(address.toBytes());
		if (account === null || account === undefined) {
			return null;
		}
		return {
			data: Buffer.from(account.data()),
			executable: account.executable(),
			lamports: Number(account.lamports()),
			owner: new PublicKey(account.owner()),
			rentEpoch: Number(account.rentEpoch()),
		};
	}

	/**
	 * Records `slot` in the SlotHashes sysvar, keeping a rolling window of
	 * recent slots. The lookup table program validates `recentSlot` against
	 * this sysvar, and litesvm's `warpToSlot` does not maintain it.
	 *
	 * SlotHash instances cannot be constructed from JS, so a pool of
	 * instances harvested from `getSlotHashes()` is mutated and written back.
	 */
	registerSlot(slot: bigint): void {
		if (this.recentSlots.length === 0 || slot > this.recentSlots[0]) {
			this.recentSlots.unshift(slot);
			if (this.recentSlots.length > LiteSVMContext.MAX_RECENT_SLOTS) {
				this.recentSlots.length = LiteSVMContext.MAX_RECENT_SLOTS;
			}
		}
		while (this.slotHashPool.length < this.recentSlots.length) {
			const fetched = this.svm.getSlotHashes();
			if (fetched.length === 0) {
				break;
			}
			this.slotHashPool.push(...fetched);
		}
		const entries = this.slotHashPool.slice(0, this.recentSlots.length);
		for (let i = 0; i < entries.length; i++) {
			entries[i].slot = this.recentSlots[i];
		}
		this.svm.setSlotHashes(entries);
	}
}

/**
 * Replaces solana-bankrun's `startAnchor`: boots a LiteSVM with the anchor
 * workspace programs from Anchor.toml's [programs.localnet], plus any extra
 * programs (from tests/fixtures or target/deploy) and preloaded accounts.
 */
export async function startLiteSVM(
	workspacePath = '',
	extraPrograms: AddedProgram[] = [],
	accounts: AddedAccount[] = []
): Promise<LiteSVMContext> {
	// blockhash checking is disabled because litesvm only honors the latest
	// blockhash, while the banks server kept a queue of recent ones; tests
	// legitimately build transactions before other transactions land.
	// runtime sigverify is disabled for parity with the banks server, which
	// processed transactions at the bank level without a sigverify stage
	// (signature checking remains in the connection wrapper's web3.js
	// serialize step, as it was under bankrun).
	// native mints are preloaded to match the banks server genesis.
	const svm = new LiteSVM()
		.withBlockhashCheck(false)
		.withSigverify(false)
		.withNativeMints();

	const root = findWorkspaceRoot(
		workspacePath ? path.resolve(workspacePath) : process.cwd()
	);
	// @ts-ignore
	const inner = svm.inner;
	for (const { name, programId } of parseLocalnetPrograms(root)) {
		inner.addProgramFromFile(
			programId.toBytes(),
			path.join(root, 'target', 'deploy', `${name}.so`)
		);
	}
	for (const program of extraPrograms) {
		inner.addProgramFromFile(
			program.programId.toBytes(),
			findProgramFile(root, program.name)
		);
	}

	// litesvm boots with an all-zero clock; give it a realistic genesis like
	// the banks server did (slot 1, wall-clock timestamp)
	const now = BigInt(Math.floor(Date.now() / 1000));
	const clock = svm.getClock();
	clock.slot = BigInt(1);
	clock.unixTimestamp = now;
	clock.epochStartTimestamp = now;
	svm.setClock(clock);

	const payer = Keypair.generate();
	const context = new LiteSVMContext(svm, payer);
	context.setAccount(payer.publicKey, {
		lamports: 1_000_000 * LAMPORTS_PER_SOL,
		data: Buffer.alloc(0),
		owner: SystemProgram.programId,
		executable: false,
		rentEpoch: 0,
	});
	for (const { address, info } of accounts) {
		context.setAccount(address, {
			...info,
			data: Buffer.from(info.data),
		});
	}

	return context;
}

/**
 * Minimal stand-in for anchor-bankrun's `BankrunProvider`: tests only use it
 * as a wallet holder (plus occasional direct context access).
 */
export class LiteSVMProvider {
	public readonly wallet: Wallet;

	constructor(
		public readonly connection: LiteSVMConnection,
		public readonly context: LiteSVMContext
	) {
		this.wallet = new Wallet(context.payer);
	}

	get publicKey(): PublicKey {
		return this.wallet.publicKey;
	}

	async sendAndConfirm(
		tx: Transaction | VersionedTransaction,
		signers: Keypair[] = []
	): Promise<TransactionSignature> {
		if (isVersionedTransaction(tx)) {
			(tx as VersionedTransaction).sign([this.context.payer, ...signers]);
		} else {
			const legacyTx = tx as Transaction;
			legacyTx.feePayer = legacyTx.feePayer ?? this.wallet.publicKey;
			legacyTx.recentBlockhash = (
				await this.connection.getLatestBlockhash()
			).blockhash;
			legacyTx.sign(this.context.payer, ...signers);
		}
		return await this.connection.sendTransaction(tx);
	}
}

export class LiteSVMContextWrapper {
	public readonly connection: LiteSVMConnection;
	public readonly context: LiteSVMContext;
	public readonly provider: LiteSVMProvider;
	public readonly commitment: Commitment = 'confirmed';

	constructor(context: LiteSVMContext, verifySignatures = true) {
		this.context = context;
		this.connection = new LiteSVMConnection(context, verifySignatures);
		this.provider = new LiteSVMProvider(this.connection, context);
	}

	async sendTransaction(
		tx: Transaction | VersionedTransaction,
		additionalSigners?: Keypair[]
	): Promise<TransactionSignature> {
		const isVersioned = isVersionedTransaction(tx);
		if (!additionalSigners) {
			additionalSigners = [];
		}
		if (isVersioned) {
			tx = tx as VersionedTransaction;
			tx.message.recentBlockhash = await this.getLatestBlockhash();
			tx.sign([this.context.payer, ...additionalSigners]);
		} else {
			tx = tx as Transaction;
			tx.recentBlockhash = await this.getLatestBlockhash();
			tx.feePayer = this.context.payer.publicKey;
			tx.sign(this.context.payer, ...additionalSigners);
		}
		return await this.connection.sendTransaction(tx);
	}

	async getMinimumBalanceForRentExemption(_: number): Promise<number> {
		return 10 * LAMPORTS_PER_SOL;
	}

	async fundKeypair(
		keypair: Keypair | Wallet,
		lamports: number | bigint
	): Promise<TransactionSignature> {
		const ixs = [
			SystemProgram.transfer({
				fromPubkey: this.context.payer.publicKey,
				toPubkey: keypair.publicKey,
				lamports,
			}),
		];
		const tx = new Transaction().add(...ixs);
		return await this.sendTransaction(tx);
	}

	async getLatestBlockhash(): Promise<Blockhash> {
		const blockhash = await this.connection.getLatestBlockhash('finalized');

		return blockhash.blockhash;
	}

	printTxLogs(signature: string): void {
		this.connection.printTxLogs(signature);
	}

	async moveTimeForward(increment: number): Promise<void> {
		const approxSlots = increment / 0.4;
		const slot = await this.connection.getSlot();
		this.context.warpToSlot(BigInt(Math.floor(Number(slot) + approxSlots)));

		const currentClock = this.context.getClock();
		const newUnixTimestamp = currentClock.unixTimestamp + BigInt(increment);
		const newClock = new Clock(
			currentClock.slot,
			currentClock.epochStartTimestamp,
			currentClock.epoch,
			currentClock.leaderScheduleEpoch,
			newUnixTimestamp
		);

		this.context.setClock(newClock);
	}

	async setTimestamp(unix_timestamp: number): Promise<void> {
		const currentClock = this.context.getClock();
		const newUnixTimestamp = BigInt(unix_timestamp);
		const newClock = new Clock(
			currentClock.slot,
			currentClock.epochStartTimestamp,
			currentClock.epoch,
			currentClock.leaderScheduleEpoch,
			newUnixTimestamp
		);
		this.context.setClock(newClock);
	}
}

export class LiteSVMConnection {
	private readonly context: LiteSVMContext;
	private readonly inner: any;
	private transactionToMeta: Map<TransactionSignature, SVMTransactionMeta> =
		new Map();

	private nextClientSubscriptionId = 0;
	private onLogCallbacks = new Map<number, LogsCallback>();
	private onAccountChangeCallbacks = new Map<
		number,
		[PublicKey, AccountChangeCallback]
	>();

	private verifySignatures: boolean;

	constructor(context: LiteSVMContext, verifySignatures = true) {
		this.context = context;
		// @ts-ignore
		this.inner = context.svm.inner;
		this.verifySignatures = verifySignatures;
	}

	getSlot(): Promise<bigint> {
		return Promise.resolve(this.context.getClock().slot);
	}

	toConnection(): SolanaConnection {
		return this as unknown as SolanaConnection;
	}

	async getTokenAccount(publicKey: PublicKey): Promise<Account> {
		const info = await this.getAccountInfo(publicKey);
		return unpackAccount(publicKey, info, info.owner);
	}

	async getMultipleAccountsInfo(
		publicKeys: PublicKey[],
		_commitmentOrConfig?: Commitment
	): Promise<AccountInfo<Buffer>[]> {
		const accountInfos = [];

		for (const publicKey of publicKeys) {
			const accountInfo = await this.getAccountInfo(publicKey);
			accountInfos.push(accountInfo);
		}

		return accountInfos;
	}

	async getAccountInfo(
		publicKey: PublicKey
	): Promise<null | AccountInfo<Buffer>> {
		const parsedAccountInfo = await this.getParsedAccountInfo(publicKey);
		return parsedAccountInfo ? parsedAccountInfo.value : null;
	}

	async getAccountInfoAndContext(
		publicKey: PublicKey,
		_commitment?: Commitment
	): Promise<RpcResponseAndContext<null | AccountInfo<Buffer>>> {
		return await this.getParsedAccountInfo(publicKey);
	}

	async sendRawTransaction(
		rawTransaction: Buffer | Uint8Array | Array<number>,
		// eslint-disable-next-line @typescript-eslint/explicit-module-boundary-types
		_options?: any
	): Promise<TransactionSignature> {
		const tx = Transaction.from(rawTransaction);
		const signature = await this.sendTransaction(tx);
		return signature;
	}

	async sendTransaction(
		tx: Transaction | VersionedTransaction
	): Promise<TransactionSignature> {
		const isVersioned = isVersionedTransaction(tx);
		const serialized = isVersioned
			? tx.serialize()
			: tx.serialize({
					verifySignatures: this.verifySignatures,
			  });
		const result = isVersioned
			? this.inner.sendVersionedTransaction(serialized)
			: this.inner.sendLegacyTransaction(serialized);

		const signature = isVersioned
			? bs58.encode((tx as VersionedTransaction).signatures[0])
			: bs58.encode((tx as Transaction).signatures[0].signature);

		const failed = result instanceof FailedTransactionMetadata;
		const errString = failed
			? formatTransactionError((result as FailedTransactionMetadata).err())
			: null;
		const meta: TransactionMetadata = failed
			? (result as FailedTransactionMetadata).meta()
			: (result as TransactionMetadata);

		if (errString) {
			if (!errString.includes('This transaction has already been processed')) {
				throw new Error(errString);
			} else {
				console.log(`Tx already processed (sig: ${signature}): ${errString}`);
				console.log(tx);
			}
		}
		if (!this.transactionToMeta.has(signature)) {
			this.transactionToMeta.set(signature, {
				result: errString,
				logMessages: meta.logs(),
				computeUnitsConsumed: meta.computeUnitsConsumed(),
				slot: Number(this.context.getClock().slot),
			});
		}

		// update the clock slot/timestamp
		// sometimes race condition causes failures so we retry
		try {
			await this.updateSlotAndClock();
		} catch (e) {
			await this.updateSlotAndClock();
		}

		if (this.onLogCallbacks.size > 0) {
			const transaction = await this.getTransaction(signature);

			const context = { slot: transaction.slot };
			const logs = {
				logs: transaction.meta.logMessages,
				err: transaction.meta.err,
				signature,
			};
			for (const logCallback of this.onLogCallbacks.values()) {
				logCallback(logs, context);
			}
		}

		for (const [
			publicKey,
			callback,
		] of this.onAccountChangeCallbacks.values()) {
			const accountInfo = await this.getParsedAccountInfo(publicKey);
			callback(accountInfo.value, accountInfo.context);
		}

		return signature;
	}

	async updateSlotAndClock(): Promise<void> {
		const currentClock = this.context.getClock();
		const nextSlot = currentClock.slot + BigInt(1);
		const newClock = new Clock(
			nextSlot,
			currentClock.epochStartTimestamp,
			currentClock.epoch,
			currentClock.leaderScheduleEpoch,
			currentClock.unixTimestamp + BigInt(1)
		);
		this.context.setClock(newClock);
		this.context.registerSlot(nextSlot);
		// the banks server produced a fresh blockhash every slot; mirror that
		// so identical instructions re-sent later get distinct signatures
		this.context.svm.expireBlockhash();
	}

	getTime(): number {
		return Number(this.context.getClock().unixTimestamp);
	}

	async getParsedAccountInfo(
		publicKey: PublicKey
	): Promise<RpcResponseAndContext<AccountInfo<Buffer>>> {
		const accountInfo = this.context.getAccount(publicKey);
		const slot = Number(this.context.getClock().slot);
		if (accountInfo === null) {
			return {
				context: { slot },
				value: null,
			};
		}
		return {
			context: { slot },
			value: accountInfo,
		};
	}

	async getLatestBlockhash(_commitment?: Commitment): Promise<
		Readonly<{
			blockhash: string;
			lastValidBlockHeight: number;
		}>
	> {
		return {
			blockhash: this.context.svm.latestBlockhash(),
			lastValidBlockHeight: Number(this.context.getClock().slot) + 150,
		};
	}

	async getAddressLookupTable(
		accountKey: PublicKey
	): Promise<RpcResponseAndContext<null | AddressLookupTableAccount>> {
		const { context, value: accountInfo } = await this.getParsedAccountInfo(
			accountKey
		);
		let value = null;
		if (accountInfo !== null) {
			value = new AddressLookupTableAccount({
				key: accountKey,
				state: AddressLookupTableAccount.deserialize(accountInfo.data),
			});
		}

		return {
			context,
			value,
		};
	}

	async getSignatureStatus(
		signature: string,
		_config?: SignatureStatusConfig
	): Promise<RpcResponseAndContext<null | SignatureStatus>> {
		const txMeta = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		const slot = Number(this.context.getClock().slot);
		if (txMeta === undefined) {
			return {
				context: { slot },
				value: null,
			};
		}
		return {
			context: { slot },
			value: {
				slot: txMeta.slot,
				confirmations: null,
				err: txMeta.result as unknown as TransactionError,
				confirmationStatus: 'finalized' as TransactionConfirmationStatus,
			},
		};
	}

	/**
	 * There's no direct equivalent to getTransaction exposed by litesvm for
	 * arbitrary history, so transaction metadata is cached at send time -
	 * same approach the bankrun harness used.
	 */
	async getTransaction(
		signature: string,
		_rawConfig?: GetTransactionConfig | GetVersionedTransactionConfig
	): Promise<SVMTransactionResponse | null> {
		const txMeta = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		if (txMeta === undefined) {
			return null;
		}
		const meta: SVMTransactionMetaNormalized = {
			logMessages: txMeta.logMessages,
			err: txMeta.result as unknown as TransactionError,
		};
		return {
			slot: txMeta.slot,
			meta,
		};
	}

	findComputeUnitConsumption(signature: string): bigint {
		const txMeta = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		if (txMeta === undefined) {
			throw new Error('Transaction not found');
		}
		return txMeta.computeUnitsConsumed;
	}

	printTxLogs(signature: string): void {
		const txMeta = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		if (txMeta === undefined) {
			throw new Error('Transaction not found');
		}
		console.log(txMeta.logMessages);
	}

	async simulateTransaction(
		transaction: Transaction | VersionedTransaction,
		_config?: SimulateTransactionConfig
	): Promise<RpcResponseAndContext<SimulatedTransactionResponse>> {
		const isVersioned = isVersionedTransaction(transaction);
		const serialized = isVersioned
			? transaction.serialize()
			: transaction.serialize({
					requireAllSignatures: false,
					verifySignatures: false,
			  });
		const result = isVersioned
			? this.inner.simulateVersionedTransaction(serialized)
			: this.inner.simulateLegacyTransaction(serialized);

		const failed = result instanceof FailedTransactionMetadata;
		const meta: TransactionMetadata = failed
			? (result as FailedTransactionMetadata).meta()
			: result.meta();
		const returnDataRaw = meta.returnData();
		const returnData: TransactionReturnData = {
			programId: new PublicKey(returnDataRaw.programId()).toBase58(),
			data: [Buffer.from(returnDataRaw.data()).toString('base64'), 'base64'],
		};
		return {
			context: { slot: Number(this.context.getClock().slot) },
			value: {
				err: failed
					? (formatTransactionError(
							(result as FailedTransactionMetadata).err()
					  ) as unknown as TransactionError)
					: null,
				logs: meta.logs(),
				accounts: undefined,
				unitsConsumed: Number(meta.computeUnitsConsumed()),
				returnData,
			},
		};
	}

	onSignature(
		signature: string,
		callback: SignatureResultCallback,
		_commitment?: Commitment
	): ClientSubscriptionId {
		const txMeta = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		this.getSlot().then((slot) => {
			if (txMeta) {
				callback(
					{ err: txMeta.result as unknown as TransactionError },
					{ slot: Number(slot) }
				);
			}
		});
		return 0;
	}

	async removeSignatureListener(_clientSubscriptionId: number): Promise<void> {
		// Nothing actually has to happen here! Pretty cool, huh?
		// This function signature only exists to match the web3js interface
	}

	onLogs(
		filter: LogsFilter,
		callback: LogsCallback,
		_commitment?: Commitment
	): ClientSubscriptionId {
		const subscriptId = this.nextClientSubscriptionId;

		this.onLogCallbacks.set(subscriptId, callback);

		this.nextClientSubscriptionId += 1;

		return subscriptId;
	}

	async removeOnLogsListener(
		clientSubscriptionId: ClientSubscriptionId
	): Promise<void> {
		this.onLogCallbacks.delete(clientSubscriptionId);
	}

	onAccountChange(
		publicKey: PublicKey,
		callback: AccountChangeCallback,
		// @ts-ignore
		_commitment?: Commitment
	): ClientSubscriptionId {
		const subscriptId = this.nextClientSubscriptionId;

		this.onAccountChangeCallbacks.set(subscriptId, [publicKey, callback]);

		this.nextClientSubscriptionId += 1;

		return subscriptId;
	}

	async removeAccountChangeListener(
		clientSubscriptionId: ClientSubscriptionId
	): Promise<void> {
		this.onAccountChangeCallbacks.delete(clientSubscriptionId);
	}

	async getMinimumBalanceForRentExemption(_: number): Promise<number> {
		return 10 * LAMPORTS_PER_SOL;
	}
}

export function asBN(value: number | bigint): BN {
	return new BN(Number(value));
}
