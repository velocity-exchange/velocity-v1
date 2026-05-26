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
	Account as LiteSvmAccount,
	Clock,
	FailedTransactionMetadata,
	LiteSVM,
	TransactionMetadata,
} from 'litesvm';
import bs58 from 'bs58';
import path from 'path';
import { BN, Wallet } from '../isomorphic/anchor';
import { Account, unpackAccount } from '@solana/spl-token';
import { isVersionedTransaction } from '../tx/utils';

export type ClientSubscriptionId = number;
export type Connection = SolanaConnection | LiteSvmConnection;

type LiteSvmTransactionMetaNormalized = {
	logMessages: string[];
	err: TransactionError | null;
};

type LiteSvmTransactionResponse = {
	slot: number;
	meta: LiteSvmTransactionMetaNormalized;
};

type StoredTxMeta = {
	logMessages: string[];
	err: TransactionError | null;
	computeUnitsConsumed: bigint;
};

const isFailedTxMeta = (
	x: TransactionMetadata | FailedTransactionMetadata
): x is FailedTransactionMetadata => typeof (x as any).err === 'function';

const extractInstructionError = (
	txError: any
): { index: number; code: number } | null => {
	const index = typeof txError?.index === 'number' ? txError.index : null;
	const inner = typeof txError?.err === 'function' ? txError.err() : null;
	const code = inner && typeof inner.code === 'number' ? inner.code : null;
	if (index === null || code === null) return null;
	return { index, code };
};

// Map litesvm's PascalCase variant names to legacy spaced phrases tests grep for.
const LEGACY_INSTRUCTION_ERROR_PHRASES: Record<string, string> = {
	ProgramFailedToComplete: 'Program failed to complete',
	ProgramFailedToCompile: 'Program failed to compile',
	MissingRequiredSignature: 'missing required signature for instruction',
	ComputationalBudgetExceeded: 'Computational budget exceeded',
	AccountNotExecutable: 'Account is not executable',
};

export class LiteSvmProgramTestContext {
	public readonly payer: Keypair;
	private readonly svm: LiteSVM;

	constructor(svm: LiteSVM, payer: Keypair) {
		this.svm = svm;
		this.payer = payer;
	}

	get lastBlockhash(): string {
		return this.svm.latestBlockhash();
	}

	setAccount(
		address: PublicKey,
		account: {
			executable: boolean;
			owner: PublicKey;
			lamports: number | bigint;
			data: Uint8Array | Buffer;
			rentEpoch?: number | bigint;
		}
	): void {
		const internal = (this.svm as any).inner;
		const data =
			account.data instanceof Buffer
				? Uint8Array.from(account.data)
				: account.data;
		internal.setAccount(
			address.toBytes(),
			new LiteSvmAccount(
				BigInt(account.lamports),
				data,
				account.owner.toBytes(),
				account.executable,
				BigInt(account.rentEpoch ?? 0)
			)
		);
	}

	warpToSlot(slot: bigint): void {
		this.svm.warpToSlot(slot);
	}

	setClock(clock: Clock): void {
		this.svm.setClock(clock);
	}
}

class LiteSvmProvider {
	public readonly wallet: Wallet;

	constructor(payer: Keypair) {
		this.wallet = new Wallet(payer);
	}
}

export class LiteSvmContextWrapper {
	public readonly svm: LiteSVM;
	public readonly connection: LiteSvmConnection;
	public readonly context: LiteSvmProgramTestContext;
	public readonly provider: LiteSvmProvider;
	public readonly commitment: Commitment = 'confirmed';

	constructor(svm: LiteSVM, payer?: Keypair, verifySignatures = true) {
		this.svm = svm;
		const actualPayer = payer ?? Keypair.generate();
		if (!payer) {
			// ~1M SOL — tests fund users with 10k SOL+ via SystemProgram.transfer.
			(svm as any).inner.airdrop(
				actualPayer.publicKey.toBytes(),
				BigInt(1_000_000) * BigInt(LAMPORTS_PER_SOL)
			);
		}
		this.context = new LiteSvmProgramTestContext(svm, actualPayer);
		this.provider = new LiteSvmProvider(actualPayer);
		this.connection = new LiteSvmConnection(svm, verifySignatures);
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
		const currentSlot = this.svm.getClock().slot;
		this.svm.warpToSlot(BigInt(Math.floor(Number(currentSlot) + approxSlots)));

		const currentClock = this.svm.getClock();
		const newClock = new Clock(
			currentClock.slot,
			currentClock.epochStartTimestamp,
			currentClock.epoch,
			currentClock.leaderScheduleEpoch,
			currentClock.unixTimestamp + BigInt(increment)
		);
		this.svm.setClock(newClock);
	}

	async setTimestamp(unix_timestamp: number): Promise<void> {
		const currentClock = this.svm.getClock();
		const newClock = new Clock(
			currentClock.slot,
			currentClock.epochStartTimestamp,
			currentClock.epoch,
			currentClock.leaderScheduleEpoch,
			BigInt(unix_timestamp)
		);
		this.svm.setClock(newClock);
	}
}

export class LiteSvmConnection {
	private readonly svm: LiteSVM;
	private transactionToMeta: Map<TransactionSignature, StoredTxMeta> =
		new Map();
	private clock: Clock | null = null;

	private nextClientSubscriptionId = 0;
	private onLogCallbacks = new Map<number, LogsCallback>();
	private onAccountChangeCallbacks = new Map<
		number,
		[PublicKey, AccountChangeCallback]
	>();

	private verifySignatures: boolean;

	constructor(svm: LiteSVM, verifySignatures = true) {
		this.svm = svm;
		this.verifySignatures = verifySignatures;
	}

	private get inner(): any {
		return (this.svm as any).inner;
	}

	async getSlot(): Promise<bigint> {
		return this.svm.getClock().slot;
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
		const parsed = await this.getParsedAccountInfo(publicKey);
		return parsed ? parsed.value : null;
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
		return await this.sendTransaction(tx);
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
		const result: TransactionMetadata | FailedTransactionMetadata = isVersioned
			? this.inner.sendVersionedTransaction(serialized)
			: this.inner.sendLegacyTransaction(serialized);

		const signature = isVersioned
			? bs58.encode((tx as VersionedTransaction).signatures[0])
			: bs58.encode((tx as Transaction).signatures[0].signature);

		let txError: TransactionError | null = null;
		let logMessages: string[] = [];
		let computeUnitsConsumed: bigint = BigInt(0);

		if (isFailedTxMeta(result)) {
			txError = result.err() as unknown as TransactionError;
			const meta = result.meta();
			logMessages = meta.logs();
			computeUnitsConsumed = meta.computeUnitsConsumed();

			const rawErrString = String(txError);
			// Tests do `error.message === 'Error processing Instruction N: custom program error: 0xHEX'`.
			let errString = rawErrString;
			const ixErr = extractInstructionError(txError);
			if (ixErr !== null) {
				errString = `Error processing Instruction ${
					ixErr.index
				}: custom program error: 0x${ixErr.code.toString(16)}`;
			} else {
				for (const [variant, legacy] of Object.entries(
					LEGACY_INSTRUCTION_ERROR_PHRASES
				)) {
					if (rawErrString.includes(variant)) {
						errString = `${rawErrString} (${legacy})`;
						break;
					}
				}
			}

			if (!errString.includes('This transaction has already been processed')) {
				throw new Error(errString);
			} else {
				console.log(`Tx already processed (sig: ${signature}): ${errString}`);
				console.log(tx);
			}
		} else {
			logMessages = result.logs();
			computeUnitsConsumed = result.computeUnitsConsumed();
		}

		if (!this.transactionToMeta.has(signature)) {
			this.transactionToMeta.set(signature, {
				logMessages,
				err: txError,
				computeUnitsConsumed,
			});
		}

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

	async updateSlotAndClock() {
		// Distinct signatures for back-to-back identical-content txs.
		this.svm.expireBlockhash();
		const currentClock = this.svm.getClock();
		const nextSlot = currentClock.slot + BigInt(1);
		this.svm.warpToSlot(nextSlot);
		const newClock = new Clock(
			nextSlot,
			currentClock.epochStartTimestamp,
			currentClock.epoch,
			currentClock.leaderScheduleEpoch,
			currentClock.unixTimestamp + BigInt(1)
		);
		this.svm.setClock(newClock);
		this.clock = newClock;

		// AddressLookupTable.createLookupTable validates recentSlot against SlotHashes.
		this.pushSlotHash(nextSlot);
	}

	private pushSlotHash(slot: bigint): void {
		const inner = this.inner;
		const existing = inner.getSlotHashes();
		// napi doesn't expose a SlotHash ctor; reuse an existing proxy and mutate.
		const fresh = inner.getSlotHashes()[0];
		if (!fresh) return;
		fresh.slot = slot;
		const next = [fresh, ...existing].slice(0, 512);
		inner.setSlotHashes(next);
	}

	getTime(): number {
		return Number((this.clock ?? this.svm.getClock()).unixTimestamp);
	}

	async getParsedAccountInfo(
		publicKey: PublicKey
	): Promise<RpcResponseAndContext<AccountInfo<Buffer> | null>> {
		const slot = Number(this.svm.getClock().slot);
		const acct = this.inner.getAccount(publicKey.toBytes());
		if (acct === null) {
			return { context: { slot }, value: null };
		}
		const accountInfo: AccountInfo<Buffer> = {
			data: Buffer.from(acct.data()),
			executable: acct.executable(),
			lamports: Number(acct.lamports()),
			owner: new PublicKey(acct.owner()),
			rentEpoch: Number(acct.rentEpoch()),
		};
		return { context: { slot }, value: accountInfo };
	}

	async getLatestBlockhash(_commitment?: Commitment): Promise<
		Readonly<{
			blockhash: string;
			lastValidBlockHeight: number;
		}>
	> {
		return {
			blockhash: this.svm.latestBlockhash(),
			lastValidBlockHeight: 0,
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
		const stored = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		if (!stored) {
			return { context: { slot }, value: null };
		}
		return {
			context: { slot },
			value: {
				slot,
				confirmations: 0,
				err: stored.err,
				confirmationStatus: 'finalized' as TransactionConfirmationStatus,
			},
		};
	}

	async getTransaction(
		signature: string,
		_rawConfig?: GetTransactionConfig | GetVersionedTransactionConfig
	): Promise<LiteSvmTransactionResponse | null> {
		const stored = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		if (stored === undefined) {
			return null;
		}
		const slot = Number(this.svm.getClock().slot);
		const meta: LiteSvmTransactionMetaNormalized = {
			logMessages: stored.logMessages,
			err: stored.err,
		};
		return { slot, meta };
	}

	findComputeUnitConsumption(signature: string): bigint {
		const stored = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		if (stored === undefined) {
			throw new Error('Transaction not found');
		}
		return stored.computeUnitsConsumed;
	}

	printTxLogs(signature: string): void {
		const stored = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		if (stored === undefined) {
			throw new Error('Transaction not found');
		}
		console.log(stored.logMessages);
	}

	async simulateTransaction(
		transaction: Transaction | VersionedTransaction,
		_config?: SimulateTransactionConfig
	): Promise<RpcResponseAndContext<SimulatedTransactionResponse>> {
		const isVersioned = isVersionedTransaction(transaction);
		const serialized = isVersioned
			? transaction.serialize()
			: transaction.serialize({ verifySignatures: this.verifySignatures });
		const result = isVersioned
			? this.inner.simulateVersionedTransaction(serialized)
			: this.inner.simulateLegacyTransaction(serialized);

		const slot = Number(this.svm.getClock().slot);

		let err: TransactionError | null = null;
		let meta: TransactionMetadata;
		if (isFailedTxMeta(result)) {
			err = result.err() as unknown as TransactionError;
			meta = result.meta();
		} else {
			meta = result.meta();
		}

		const returnData = meta.returnData();
		const returnDataProgramId = new PublicKey(
			returnData.programId()
		).toBase58();
		const returnDataNormalized = Buffer.from(returnData.data()).toString(
			'base64'
		);
		const txReturnData: TransactionReturnData = {
			programId: returnDataProgramId,
			data: [returnDataNormalized, 'base64'],
		};

		return {
			context: { slot },
			value: {
				err,
				logs: meta.logs(),
				accounts: undefined,
				unitsConsumed: Number(meta.computeUnitsConsumed()),
				returnData: txReturnData,
			},
		};
	}

	onSignature(
		signature: string,
		callback: SignatureResultCallback,
		_commitment?: Commitment
	): ClientSubscriptionId {
		const stored = this.transactionToMeta.get(
			signature as TransactionSignature
		);
		const slot = Number(this.svm.getClock().slot);
		if (stored) {
			callback({ err: stored.err }, { slot });
		}
		return 0;
	}

	async removeSignatureListener(_clientSubscriptionId: number): Promise<void> {
		// no-op
	}

	onLogs(
		_filter: LogsFilter,
		callback: LogsCallback,
		_commitment?: Commitment
	): ClientSubscriptionId {
		const id = this.nextClientSubscriptionId;
		this.onLogCallbacks.set(id, callback);
		this.nextClientSubscriptionId += 1;
		return id;
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
		const id = this.nextClientSubscriptionId;
		this.onAccountChangeCallbacks.set(id, [publicKey, callback]);
		this.nextClientSubscriptionId += 1;
		return id;
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

export type AddedProgram = {
	programId: PublicKey;
	/** Resolved to `tests/fixtures/<name>.so` if `path` is not given. */
	name?: string;
	/** Explicit override for the .so file location. */
	path?: string;
};

export type AddedAccount = {
	address: PublicKey;
	info: AccountInfo<Buffer>;
};

export type StartLiteSvmOpts = {
	driftSoPath?: string;
	extraPrograms?: AddedProgram[];
	extraAccounts?: AddedAccount[];
	payer?: Keypair;
	verifySignatures?: boolean;
};

export async function startLiteSvm(
	driftProgramId: PublicKey,
	opts: StartLiteSvmOpts = {}
): Promise<LiteSvmContextWrapper> {
	// Off by default: tests routinely send unsigned VersionedTransactions.
	const verifySignatures = opts.verifySignatures ?? false;

	const svm = new LiteSVM()
		.withBuiltins()
		.withSysvars()
		.withDefaultPrograms()
		.withNativeMints()
		.withLamports(BigInt('1000000000000000000'))
		.withSigverify(verifySignatures);

	// Snapshot-based oracle data has real-world timestamps; can't start at 0.
	const initialClock = svm.getClock();
	initialClock.unixTimestamp = BigInt(Math.floor(Date.now() / 1000));
	if (initialClock.slot === BigInt(0)) {
		initialClock.slot = BigInt(1);
	}
	svm.setClock(initialClock);

	const internal = (svm as any).inner;

	const driftSoPath =
		opts.driftSoPath ??
		path.join(process.cwd(), 'target', 'deploy', 'drift.so');

	internal.addProgramFromFile(driftProgramId.toBytes(), driftSoPath);

	for (const program of opts.extraPrograms ?? []) {
		const soPath =
			program.path ??
			path.join(process.cwd(), 'tests', 'fixtures', `${program.name}.so`);
		internal.addProgramFromFile(program.programId.toBytes(), soPath);
	}

	const wrapper = new LiteSvmContextWrapper(svm, opts.payer, verifySignatures);

	for (const acct of opts.extraAccounts ?? []) {
		wrapper.context.setAccount(acct.address, {
			executable: acct.info.executable,
			owner: acct.info.owner,
			lamports: acct.info.lamports,
			data: acct.info.data,
			rentEpoch: acct.info.rentEpoch ?? 0,
		});
	}

	return wrapper;
}
