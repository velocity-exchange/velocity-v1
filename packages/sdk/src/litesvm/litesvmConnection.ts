/**
 * web3.js-shaped adapter over LiteSVM, used by the integration tests.
 *
 * LiteSVM's public API takes `@solana/kit` types, but its native binding
 * (`svm.inner`) takes serialized transaction bytes and raw 32-byte pubkeys.
 * The SDK and Anchor 1.0 are both web3.js v1, so this adapter talks to the
 * native binding directly and never converts to kit. That is the same layer
 * the previous bankrun adapter used, so the exports here are unchanged.
 */
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
import { LiteSVM, Account, Clock } from 'litesvm';
import bs58 from 'bs58';
import { BN, Wallet } from '../isomorphic/anchor';
import { Account as TokenAccount, unpackAccount } from '@solana/spl-token';
import { isVersionedTransaction } from '../tx/utils';

export type ClientSubscriptionId = number;
export type Connection = SolanaConnection | LiteSVMConnection;

/** Programs Anchor.toml lists under [programs.localnet], loaded from target/deploy. */
const DEFAULT_PROGRAMS: { name: string; programId: PublicKey }[] = [
	{
		name: 'velocity',
		programId: new PublicKey('vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P'),
	},
	{
		name: 'vaults',
		programId: new PublicKey('vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR'),
	},
	{
		name: 'pyth',
		programId: new PublicKey('FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH'),
	},
	{
		name: 'token_faucet',
		programId: new PublicKey('V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB'),
	},
];

/**
 * Where `<name>.so` is looked up. target/deploy holds the workspace programs;
 * tests/fixtures holds the prebuilt third-party ones (serum_dex, metaplex and
 * friends), which is where solana-program-test found them.
 */
const DEFAULT_PROGRAM_DIRS = [
	process.env.SVM_DEPLOY_DIR ?? 'target/deploy',
	'tests/fixtures',
];

/**
 * Starting balance of the context payer. Tests fund individual keypairs with
 * 10,000 to 20,000 SOL at a time (liquidatePerpPnlForDeposit needs 20,000 in one
 * transfer), so this has to be generously above that or the System transfer
 * fails with `insufficient lamports`.
 */
const PAYER_LAMPORTS = 1_000_000 * LAMPORTS_PER_SOL;

/**
 * Balance of LiteSVM's internal airdrop account. It must exceed PAYER_LAMPORTS,
 * or the airdrop above silently fails and every later transaction reports
 * AccountNotFound for the payer.
 */
const AIRDROP_SOURCE_LAMPORTS = BigInt('1000000000000000000');

/** Mirrors solana-bankrun's AddedProgram. */
export type AddedProgram = { name: string; programId: PublicKey };
/** Mirrors solana-bankrun's AddedAccount. */
export type AddedAccount = { address: PublicKey; info: AccountInfo<Buffer> };

export type StartLiteSVMOptions = {
	/** Extra programs to load from target/deploy, on top of Anchor.toml's localnet set. */
	extraPrograms?: AddedProgram[];
	/** Accounts to seed before the first transaction. */
	accounts?: AddedAccount[];
	/** Directories searched for `<name>.so`, in order. */
	programDirs?: string[];
};

/**
 * Stand-in for bankrun's ProgramTestContext: an SVM plus a funded payer.
 */
export class LiteSVMContext {
	constructor(
		public readonly svm: LiteSVM,
		public readonly payer: Keypair
	) {}

	/** Matches bankrun's ProgramTestContext.lastBlockhash. */
	get lastBlockhash(): Blockhash {
		return freshBlockhash();
	}

	setAccount(address: PublicKey, info: AccountInfo<Buffer>): void {
		inner(this.svm).setAccount(
			address.toBytes(),
			new Account(
				BigInt(info.lamports),
				info.data,
				info.owner.toBytes(),
				info.executable,
				toU64(info.rentEpoch ?? 0)
			)
		);
	}

	warpToSlot(slot: bigint): void {
		inner(this.svm).warpToSlot(slot);
	}

	setClock(clock: Clock): void {
		inner(this.svm).setClock(clock);
	}

	getClock(): Clock {
		return inner(this.svm).getClock();
	}
}

/**
 * Replaces bankrun's `startAnchor(path, extraPrograms, accounts)`. Loads the
 * Anchor.toml localnet programs, plus any extras, and funds a payer.
 */
export function startLiteSVM(
	options: StartLiteSVMOptions = {}
): LiteSVMContext {
	const {
		extraPrograms = [],
		accounts = [],
		programDirs = DEFAULT_PROGRAM_DIRS,
	} = options;

	// withNativeMints seeds the native SOL mint, which bankrun's startAnchor did
	// implicitly. Without it the vault suite fails on `Mint account So111...112
	// not found`.
	const svm = new LiteSVM()
		.withNativeMints()
		.withLamports(AIRDROP_SOURCE_LAMPORTS)
		.withSigverify(false)
		.withBlockhashCheck(false);
	const svmInner = inner(svm);
	for (const { name, programId } of [...DEFAULT_PROGRAMS, ...extraPrograms]) {
		svmInner.addProgramFromFile(
			programId.toBytes(),
			resolveProgramPath(name, programDirs)
		);
	}

	const payer = Keypair.generate();
	const airdrop = svmInner.airdrop(
		payer.publicKey.toBytes(),
		BigInt(PAYER_LAMPORTS)
	);
	if (airdrop === null || isFailed(airdrop)) {
		throw new Error(
			`startLiteSVM: failed to fund payer: ${formatTxError(airdrop)}`
		);
	}

	// LiteSVM boots with unixTimestamp 0, where bankrun used the real wall clock.
	// Suites that pin funding and oracle-derived numbers read the timestamp, so
	// leaving it at 0 shifts them (userAccount.ts is the sensitive one). Keep
	// LiteSVM's own starting slot. Its SlotHashes sysvar is populated around that
	// origin, and the address-lookup-table program rejects a "recent slot" that
	// isn't in it ("10 is not a recent slot").
	const bootClock = svmInner.getClock();
	const nowSeconds = BigInt(Math.floor(Date.now() / 1000));
	svmInner.setClock(
		new Clock(
			bootClock.slot,
			nowSeconds,
			bootClock.epoch,
			bootClock.leaderScheduleEpoch,
			nowSeconds
		)
	);

	const context = new LiteSVMContext(svm, payer);
	for (const account of accounts) {
		context.setAccount(account.address, account.info);
	}
	return context;
}

/**
 * Anchor Provider backed by LiteSVM. Replaces anchor-bankrun's BankrunProvider,
 * including its `(context, wallet?)` constructor shape.
 */
export class LiteSVMProvider {
	public readonly wallet: Wallet;
	public readonly connection: SolanaConnection;
	public readonly publicKey: PublicKey;
	public readonly context: LiteSVMContext;
	private readonly svmConnection: LiteSVMConnection;

	constructor(
		context: LiteSVMContext,
		wallet?: Wallet,
		connection?: LiteSVMConnection
	) {
		this.context = context;
		this.wallet = wallet ?? (new Wallet(context.payer) as Wallet);
		this.svmConnection = connection ?? new LiteSVMConnection(context);
		this.connection = this.svmConnection.toConnection();
		this.publicKey = this.wallet.publicKey;
	}

	async send(
		tx: Transaction | VersionedTransaction,
		signers?: Keypair[],
		_opts?: unknown
	): Promise<TransactionSignature> {
		if (isVersionedTransaction(tx)) {
			const versioned = tx as VersionedTransaction;
			signers?.forEach((signer) => versioned.sign([signer]));
			await this.wallet.signTransaction(versioned);
			await this.svmConnection.sendTransaction(versioned);
			return bs58.encode(versioned.signatures[0]);
		}
		const legacy = tx as Transaction;
		legacy.feePayer = legacy.feePayer ?? this.wallet.publicKey;
		legacy.recentBlockhash = (
			await this.svmConnection.getLatestBlockhash()
		).blockhash;
		signers?.forEach((signer) => legacy.partialSign(signer));
		await this.wallet.signTransaction(legacy);
		if (!legacy.signature) {
			throw new Error('LiteSVMProvider: missing fee payer signature');
		}
		await this.svmConnection.sendTransaction(legacy);
		return bs58.encode(legacy.signature);
	}

	async sendAndConfirm(
		tx: Transaction | VersionedTransaction,
		signers?: Keypair[],
		opts?: unknown
	): Promise<TransactionSignature> {
		return this.send(tx, signers, opts);
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
		this.provider = new LiteSVMProvider(context, undefined, this.connection);
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
		this.context.setClock(
			new Clock(
				currentClock.slot,
				currentClock.epochStartTimestamp,
				currentClock.epoch,
				currentClock.leaderScheduleEpoch,
				currentClock.unixTimestamp + BigInt(increment)
			)
		);
	}

	async setTimestamp(unix_timestamp: number): Promise<void> {
		const currentClock = this.context.getClock();
		this.context.setClock(
			new Clock(
				currentClock.slot,
				currentClock.epochStartTimestamp,
				currentClock.epoch,
				currentClock.leaderScheduleEpoch,
				BigInt(unix_timestamp)
			)
		);
	}
}

type TransactionMetaNormalized = {
	logMessages: string[];
	err: TransactionError | null;
};

type LiteSVMTransactionResponse = {
	slot: number;
	meta: TransactionMetaNormalized;
};

/** Result of a send, normalized across success and failure. */
type SendResult = {
	logMessages: string[];
	computeUnitsConsumed: bigint;
	err: TransactionError | null;
	returnData: { programId: PublicKey; data: Uint8Array } | null;
	slot: number;
};

export class LiteSVMConnection {
	private readonly context: LiteSVMContext;
	private transactionToMeta: Map<TransactionSignature, SendResult> = new Map();
	private nextClientSubscriptionId = 0;
	private onLogCallbacks = new Map<number, LogsCallback>();
	private onAccountChangeCallbacks = new Map<
		number,
		[PublicKey, AccountChangeCallback]
	>();
	private verifySignatures: boolean;
	/**
	 * Mirror of the SlotHashes sysvar. LiteSVM's warpToSlot does not add entries,
	 * where bankrun's bank did, and the address-lookup-table program rejects a
	 * `recent_slot` that is absent from it ("<slot> is not a recent slot"). We
	 * keep the list here rather than re-reading it each time, because it is
	 * rewritten on every transaction.
	 */
	private slotHashes: { slot: bigint; hash: string }[] | null = null;

	constructor(context: LiteSVMContext, verifySignatures = true) {
		this.context = context;
		this.verifySignatures = verifySignatures;
	}

	private get svm() {
		return inner(this.context.svm);
	}

	async getSlot(): Promise<bigint> {
		return this.svm.getClock().slot;
	}

	toConnection(): SolanaConnection {
		return this as unknown as SolanaConnection;
	}

	async getTokenAccount(publicKey: PublicKey): Promise<TokenAccount> {
		const info = await this.getAccountInfo(publicKey);
		if (info === null) {
			throw new Error(`Account not found: ${publicKey.toBase58()}`);
		}
		return unpackAccount(publicKey, info, info.owner);
	}

	// Returns the decoded SPL token Account (with .amount), not web3.js's
	// { value: { amount } } shape. Callers read `.amount` directly.
	async getTokenAccountBalance(
		publicKey: PublicKey,
		_commitment?: Commitment
	): Promise<TokenAccount> {
		return this.getTokenAccount(publicKey);
	}

	// Lamport balance as a bigint; callers wrap with Number().
	async getBalance(publicKey: PublicKey): Promise<bigint> {
		return this.svm.getBalance(publicKey.toBytes()) ?? BigInt(0);
	}

	async getMultipleAccountsInfo(
		publicKeys: PublicKey[],
		_commitmentOrConfig?: Commitment
	): Promise<(AccountInfo<Buffer> | null)[]> {
		const accountInfos = [];
		for (const publicKey of publicKeys) {
			accountInfos.push(await this.getAccountInfo(publicKey));
		}
		return accountInfos;
	}

	async getAccountInfo(
		publicKey: PublicKey
	): Promise<null | AccountInfo<Buffer>> {
		return (await this.getParsedAccountInfo(publicKey)).value;
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
		// Raw bytes may encode either a legacy or a versioned (v0) transaction.
		// They must be deserialized with the matching type: legacy parses with
		// Transaction.from, versioned needs VersionedTransaction.deserialize.
		let tx: Transaction | VersionedTransaction;
		try {
			tx = Transaction.from(rawTransaction);
		} catch (e) {
			tx = VersionedTransaction.deserialize(rawTransaction as Uint8Array);
		}
		return await this.sendTransaction(tx);
	}

	async sendTransaction(
		tx: Transaction | VersionedTransaction
	): Promise<TransactionSignature> {
		const isVersioned = isVersionedTransaction(tx);
		const serialized = isVersioned
			? (tx as VersionedTransaction).serialize()
			: (tx as Transaction).serialize({
					verifySignatures: this.verifySignatures,
			  });

		const result = isVersioned
			? this.svm.sendVersionedTransaction(serialized)
			: this.svm.sendLegacyTransaction(serialized);

		let signature: string;
		if (isVersioned) {
			signature = bs58.encode((tx as VersionedTransaction).signatures[0]);
		} else {
			const legacySignature = (tx as Transaction).signatures[0].signature;
			if (legacySignature === null) {
				throw new Error(
					'LiteSVMConnection: transaction is missing its first signature'
				);
			}
			signature = bs58.encode(legacySignature);
		}

		const err = isFailed(result) ? (result.err() as TransactionError) : null;
		const meta = isFailed(result) ? result.meta() : result;
		const returnDataRaw = meta.returnData();
		const sendResult: SendResult = {
			logMessages: meta.logs(),
			computeUnitsConsumed: meta.computeUnitsConsumed(),
			err,
			returnData:
				returnDataRaw && returnDataRaw.data().length > 0
					? {
							programId: new PublicKey(returnDataRaw.programId()),
							data: returnDataRaw.data(),
					  }
					: null,
			slot: Number(this.svm.getClock().slot),
		};

		if (err !== null) {
			// The message must be exactly Solana's Display text and nothing else:
			// tests compare it with strict equality (liquidatePerpWithFill.ts).
			// Program logs ride along as a property, the way web3.js's
			// SendTransactionError carries them.
			const error = new Error(describeTransactionError(err));
			(error as Error & { logs?: string[] }).logs = sendResult.logMessages;
			throw error;
		}

		if (!this.transactionToMeta.has(signature)) {
			this.transactionToMeta.set(signature, sendResult);
		}

		// Advance the clock one slot per transaction, matching the previous
		// adapter. Tests rely on the slot and timestamp moving between sends.
		this.advanceSlot();

		if (this.onLogCallbacks.size > 0) {
			const context = { slot: sendResult.slot };
			const logs = {
				logs: sendResult.logMessages,
				err: sendResult.err,
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
			if (accountInfo.value === null) {
				continue;
			}
			callback(accountInfo.value, accountInfo.context);
		}

		// Hand the event loop a turn. LiteSVM applies a transaction synchronously,
		// so without this an `await sendTransaction(...)` never yields, timer-driven
		// subscribers never run, and the polling account loader serves stale data
		// to callers that derive arguments from cached state.
		await new Promise((resolve) => setTimeout(resolve, 0));

		return signature;
	}

	private advanceSlot(): void {
		const currentClock = this.svm.getClock();
		const nextSlot = currentClock.slot + BigInt(1);
		this.svm.warpToSlot(nextSlot);
		this.svm.setClock(
			new Clock(
				nextSlot,
				currentClock.epochStartTimestamp,
				currentClock.epoch,
				currentClock.leaderScheduleEpoch,
				currentClock.unixTimestamp + BigInt(1)
			)
		);
		this.recordSlotHash(nextSlot);
	}

	/** Solana keeps the most recent 512 slots in the SlotHashes sysvar. */
	private recordSlotHash(slot: bigint): void {
		if (this.slotHashes === null) {
			this.slotHashes = this.svm
				.getSlotHashes()
				.map((h) => ({ slot: h.slot, hash: h.hash }));
		}
		this.slotHashes.unshift({ slot, hash: this.svm.latestBlockhash() });
		if (this.slotHashes.length > 512) {
			this.slotHashes.length = 512;
		}
		this.svm.setSlotHashes(this.slotHashes);
	}

	/** Retained for call-site compatibility with the previous adapter. */
	async updateSlotAndClock(): Promise<void> {
		this.advanceSlot();
	}

	getTime(): number {
		return Number(this.svm.getClock().unixTimestamp);
	}

	async getParsedAccountInfo(
		publicKey: PublicKey
	): Promise<RpcResponseAndContext<null | AccountInfo<Buffer>>> {
		const slot = Number(this.svm.getClock().slot);
		const account = this.svm.getAccount(publicKey.toBytes());
		if (account === null) {
			return { context: { slot }, value: null };
		}
		return {
			context: { slot },
			value: {
				data: Buffer.from(account.data()),
				executable: account.executable(),
				lamports: Number(account.lamports()),
				owner: new PublicKey(account.owner()),
				rentEpoch: Number(account.rentEpoch()),
			},
		};
	}

	async getLatestBlockhash(_commitment?: Commitment): Promise<
		Readonly<{
			blockhash: string;
			lastValidBlockHeight: number;
		}>
	> {
		return {
			blockhash: freshBlockhash(),
			lastValidBlockHeight: Number(this.svm.getClock().slot) + 300,
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
		return { context, value };
	}

	async getSignatureStatus(
		signature: string,
		_config?: SignatureStatusConfig
	): Promise<RpcResponseAndContext<null | SignatureStatus>> {
		const slot = Number(this.svm.getClock().slot);
		const meta = this.transactionToMeta.get(signature as TransactionSignature);
		if (meta === undefined) {
			return { context: { slot }, value: null };
		}
		// LiteSVM applies a transaction synchronously, so anything we have a
		// record of is already final.
		return {
			context: { slot },
			value: {
				slot: meta.slot,
				confirmations: 0,
				err: meta.err,
				confirmationStatus: 'finalized' as TransactionConfirmationStatus,
			},
		};
	}

	async getTransaction(
		signature: string,
		_rawConfig?: GetTransactionConfig | GetVersionedTransactionConfig
	): Promise<LiteSVMTransactionResponse | null> {
		const meta = this.transactionToMeta.get(signature as TransactionSignature);
		if (meta === undefined) {
			return null;
		}
		return {
			slot: meta.slot,
			meta: { logMessages: meta.logMessages, err: meta.err },
		};
	}

	private getTransactionMetaOrThrow(signature: string): SendResult {
		const meta = this.transactionToMeta.get(signature as TransactionSignature);
		if (meta === undefined) {
			throw new Error('Transaction not found');
		}
		return meta;
	}

	findComputeUnitConsumption(signature: string): bigint {
		return this.getTransactionMetaOrThrow(signature).computeUnitsConsumed;
	}

	printTxLogs(signature: string): void {
		console.log(this.getTransactionMetaOrThrow(signature).logMessages);
	}

	async simulateTransaction(
		transaction: Transaction | VersionedTransaction,
		_config?: SimulateTransactionConfig
	): Promise<RpcResponseAndContext<SimulatedTransactionResponse>> {
		const isVersioned = isVersionedTransaction(transaction);
		const serialized = isVersioned
			? (transaction as VersionedTransaction).serialize()
			: (transaction as Transaction).serialize({
					verifySignatures: this.verifySignatures,
			  });
		const result = isVersioned
			? this.svm.simulateVersionedTransaction(serialized)
			: this.svm.simulateLegacyTransaction(serialized);

		const failed = isFailed(result);
		const meta = result.meta();
		let returnData: TransactionReturnData | undefined;
		const raw = meta.returnData();
		if (raw && raw.data().length > 0) {
			returnData = {
				programId: new PublicKey(raw.programId()).toBase58(),
				data: [Buffer.from(raw.data()).toString('base64'), 'base64'],
			};
		}
		return {
			context: { slot: Number(this.svm.getClock().slot) },
			value: {
				err: failed ? (result.err() as TransactionError) : null,
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
		const meta = this.transactionToMeta.get(signature as TransactionSignature);
		if (meta) {
			callback({ err: meta.err }, { slot: meta.slot });
		}
		return 0;
	}

	async removeSignatureListener(_clientSubscriptionId: number): Promise<void> {
		// Exists only to match the web3.js interface.
	}

	onLogs(
		_filter: LogsFilter,
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

/**
 * A distinct blockhash per fetch. `withBlockhashCheck` is off because LiteSVM
 * would reject one it never issued.
 *
 * Two constraints have to hold at once. The SDK's retry sender resends identical
 * signed bytes while awaiting confirmation, and those must deduplicate or the
 * transaction executes twice. Separately-built transactions must differ, or the
 * runtime answers AlreadyProcessed and a test expecting a program error never
 * reaches the program. A unique value per fetch satisfies both: resent bytes stay
 * identical and the runtime's transaction history catches them, while two builds
 * never collide. LiteSVM's `expireBlockhash` cannot be used here, as it
 * invalidates the previous blockhash rather than queueing it.
 */
function freshBlockhash(): Blockhash {
	const bytes = Buffer.alloc(32);
	for (let i = 0; i < 32; i += 1) {
		bytes[i] = Math.floor(Math.random() * 256);
	}
	return bs58.encode(bytes);
}

const U64_MAX = BigInt('18446744073709551615');

/**
 * web3.js represents a rent-exempt account's `rentEpoch` as a JS number, and
 * `u64::MAX` does not fit a double: it rounds up to 2^64, which napi then
 * rejects with "Bigint too large for u64". Clamp instead.
 */
function toU64(value: number | bigint): bigint {
	const asBigInt =
		typeof value === 'bigint' ? value : BigInt(Math.trunc(Number(value)));
	if (asBigInt < BigInt(0)) {
		return BigInt(0);
	}
	return asBigInt > U64_MAX ? U64_MAX : asBigInt;
}

function resolveProgramPath(name: string, dirs: string[]): string {
	// eslint-disable-next-line @typescript-eslint/no-var-requires
	const fs = require('fs');
	for (const dir of dirs) {
		const candidate = `${dir}/${name}.so`;
		if (fs.existsSync(candidate)) {
			return candidate;
		}
	}
	throw new Error(
		`startLiteSVM: no ${name}.so in any of: ${dirs.join(', ')}. ` +
			'Build the programs first, or add the fixture.'
	);
}

export function asBN(value: number | bigint): BN {
	return new BN(Number(value));
}

/**
 * LiteSVM's native binding. Its public wrapper only accepts `@solana/kit`
 * types; the binding underneath takes raw bytes, which is what web3.js gives us.
 */
type LiteSVMInner = {
	getAccount(pubkey: Uint8Array): Account | null;
	setAccount(pubkey: Uint8Array, data: Account): void;
	getBalance(pubkey: Uint8Array): bigint | null;
	latestBlockhash(): string;
	airdrop(pubkey: Uint8Array, lamports: bigint): unknown;
	addProgramFromFile(programId: Uint8Array, path: string): void;
	sendLegacyTransaction(txBytes: Uint8Array): any;
	sendVersionedTransaction(txBytes: Uint8Array): any;
	simulateLegacyTransaction(txBytes: Uint8Array): any;
	simulateVersionedTransaction(txBytes: Uint8Array): any;
	warpToSlot(slot: bigint): void;
	getClock(): Clock;
	setClock(clock: Clock): void;
	getSlotHashes(): { slot: bigint; hash: string }[];
	setSlotHashes(hashes: { slot: bigint; hash: string }[]): void;
	minimumBalanceForRentExemption(dataLen: bigint): bigint;
};

function inner(svm: LiteSVM): LiteSVMInner {
	return (svm as unknown as { inner: LiteSVMInner }).inner;
}

function isFailed(result: any): boolean {
	return typeof result?.err === 'function';
}

/**
 * LiteSVM returns transaction errors as napi enum members, which stringify to
 * bare integers. Map the fieldless ones back to their names so a failure says
 * what happened.
 */
const TRANSACTION_ERROR_NAMES = [
	'AccountInUse',
	'AccountLoadedTwice',
	'AccountNotFound',
	'ProgramAccountNotFound',
	'InsufficientFundsForFee',
	'InvalidAccountForFee',
	'AlreadyProcessed',
	'BlockhashNotFound',
	'CallChainTooDeep',
	'MissingSignatureForFee',
	'InvalidAccountIndex',
	'SignatureFailure',
	'InvalidProgramForExecution',
	'SanitizeFailure',
	'ClusterMaintenance',
	'AccountBorrowOutstanding',
	'WouldExceedMaxBlockCostLimit',
	'UnsupportedVersion',
	'InvalidWritableAccount',
	'WouldExceedMaxAccountCostLimit',
	'WouldExceedAccountDataBlockLimit',
	'TooManyAccountLocks',
	'AddressLookupTableNotFound',
	'InvalidAddressLookupTableOwner',
	'InvalidAddressLookupTableData',
];

/** Read a napi field that may be exposed as a value or a getter method. */
function napiField(obj: unknown, key: string): unknown {
	if (!obj || typeof obj !== 'object') {
		return undefined;
	}
	try {
		const raw = (obj as Record<string, unknown>)[key];
		return typeof raw === 'function' ? (raw as () => unknown).call(obj) : raw;
	} catch (e) {
		return undefined;
	}
}

/** Names for InstructionErrorFieldless, indexed by its discriminant. */
const INSTRUCTION_ERROR_NAMES = [
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
	'instruction illegally modified program id of an account',
	'instruction spent from the balance of an account it does not own',
	'instruction modified data of an account it does not own',
	'instruction changed the balance of a read-only account',
	'instruction modified data of a read-only account',
	'instruction contains duplicate accounts',
	'instruction changed executable bit of an account',
	'instruction modified rent epoch of an account',
	'insufficient account keys for instruction',
];

/**
 * Renders an instruction error the way Solana's Display impl does, because tests
 * match on the message text. `liquidatePerpWithFill.ts` asserts exactly
 * "Error processing Instruction 1: custom program error: 0x1773".
 */
function describeInstructionError(inner: unknown): string {
	if (typeof inner === 'number') {
		return INSTRUCTION_ERROR_NAMES[inner] ?? `instruction error ${inner}`;
	}
	const code = napiField(inner, 'code');
	if (typeof code === 'number') {
		return `custom program error: 0x${code.toString(16)}`;
	}
	const name = (inner as { constructor?: { name?: string } })?.constructor
		?.name;
	return name ?? String(inner);
}

/**
 * LiteSVM returns transaction errors as napi values: a bare integer for the
 * fieldless variants, or a class instance for the rest. Render them the way
 * Solana's Display impl does, which is the text bankrun surfaced.
 */
function describeTransactionError(err: unknown): string {
	if (typeof err === 'number') {
		return TRANSACTION_ERROR_NAMES[err] ?? `TransactionError(${err})`;
	}
	const index = napiField(err, 'index');
	const inner = napiField(err, 'err') ?? napiField(err, 'error');
	if (typeof index === 'number' && inner !== undefined) {
		return `Error processing Instruction ${index}: ${describeInstructionError(
			inner
		)}`;
	}
	if (err && typeof err === 'object') {
		const name = (err as { constructor?: { name?: string } }).constructor?.name;
		return name ?? String(err);
	}
	return String(err);
}

function formatTxError(result: any): string {
	if (result === null || result === undefined) {
		return 'no result';
	}
	if (!isFailed(result)) {
		return 'succeeded';
	}
	const logs = result.meta?.().logs?.() ?? [];
	return `${describeTransactionError(result.err())}${
		logs.length ? `\n${logs.join('\n')}` : ''
	}`;
}
