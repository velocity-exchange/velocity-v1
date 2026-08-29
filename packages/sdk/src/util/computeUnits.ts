import {
	ComputeBudgetProgram,
	Connection,
	Finality,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';

/**
 * Fetches a confirmed transaction and extracts, from its log messages, every "consumed compute
 * units" line attributable to `programId`. A transaction can invoke the same program more than
 * once (e.g. via CPI or multiple instructions), so this returns one entry per matching log line
 * rather than a single total.
 * @param programId - Program whose compute-unit consumption to extract.
 * @param connection - RPC connection used to fetch the transaction.
 * @param txSignature - Signature of the transaction to inspect.
 * @param commitment - Finality level to fetch the transaction at; defaults to `'confirmed'`.
 * @returns The consumed-compute-unit counts (as strings, parsed straight from the log text) for
 * each invocation of `programId` found in the transaction's logs; empty if the transaction has no
 * log messages (e.g. not yet available at the requested commitment).
 */
export async function findComputeUnitConsumption(
	programId: PublicKey,
	connection: Connection,
	txSignature: string,
	commitment: Finality = 'confirmed'
): Promise<string[]> {
	const tx = await connection.getTransaction(txSignature, { commitment });
	const computeUnits: string[] = [];
	const logMessages = tx?.meta?.logMessages;
	if (!logMessages) {
		return computeUnits;
	}
	const regex = new RegExp(
		`Program ${programId.toString()} consumed ([0-9]{0,6}) of ([0-9]{0,7}) compute units`
	);
	logMessages.forEach((logMessage) => {
		const match = logMessage.match(regex);
		if (match && match[1]) {
			computeUnits.push(match[1]);
		}
	});
	return computeUnits;
}

/**
 * Checks whether `ix` is a `ComputeBudgetProgram.setComputeUnitLimit` instruction, by matching
 * the program id and the instruction discriminator byte (`2`).
 * @param ix - Instruction to check.
 * @returns `true` if `ix` sets the transaction's compute unit limit.
 */
export function isSetComputeUnitsIx(ix: TransactionInstruction): boolean {
	// Compute budget program discriminator is first byte
	// 2: set compute unit limit
	// 3: set compute unit price
	if (
		ix.programId.equals(ComputeBudgetProgram.programId) &&
		// @ts-ignore
		ix.data.at(0) === 2
	) {
		return true;
	}
	return false;
}

/**
 * Checks whether `ix` is a `ComputeBudgetProgram.setComputeUnitPrice` instruction, by matching
 * the program id and the instruction discriminator byte (`3`).
 * @param ix - Instruction to check.
 * @returns `true` if `ix` sets the transaction's compute unit price (priority fee).
 */
export function isSetComputeUnitPriceIx(ix: TransactionInstruction): boolean {
	// Compute budget program discriminator is first byte
	// 2: set compute unit limit
	// 3: set compute unit price
	if (
		ix.programId.equals(ComputeBudgetProgram.programId) &&
		// @ts-ignore
		ix.data.at(0) === 3
	) {
		return true;
	}
	return false;
}

/**
 * Checks whether `ix` is a `SetLoadedAccountsDataSizeLimit` instruction, by matching the program
 * id and the instruction discriminator byte (`4`).
 * @param ix - Instruction to check.
 * @returns `true` if `ix` sets the transaction's loaded-accounts data size limit.
 */
export function isSetLoadedAccountsDataSizeIx(
	ix: TransactionInstruction
): boolean {
	// Compute budget program discriminator is first byte
	// 4: set loaded accounts data size limit
	return (
		ix.programId.equals(ComputeBudgetProgram.programId) &&
		// @ts-ignore
		ix.data.at(0) === 4
	);
}

/**
 * Builds a `SetLoadedAccountsDataSizeLimit` instruction: the ceiling, in bytes, on the account
 * data a transaction may load — its own accounts plus the programs it names and their program
 * data.
 *
 * Hand-rolled because `@solana/web3.js` v1 has no builder for it. The encoding is the compute
 * budget program's: a discriminator byte, then the limit as a little-endian `u32`.
 *
 * Worth setting on every transaction. The limit is priced, and it is priced on what a transaction
 * *asks for*, so a transaction that asks for nothing in particular is charged for the 64 MiB
 * default however little it loads. Asking under what the transaction really loads makes it fail
 * to load at all, so leave headroom.
 *
 * Add it at the **end** of the instruction list. The runtime finds compute-budget instructions by
 * program id wherever they sit, and an instruction added at the front shifts every index behind
 * it — which the Pyth Lazer oracle-update flow encodes (see `createMinimalEd25519VerifyIx`).
 * @param bytes - The limit, in bytes.
 * @returns The instruction.
 */
export function setLoadedAccountsDataSizeLimitIx(
	bytes: number
): TransactionInstruction {
	const data = Buffer.alloc(5);
	data.writeUInt8(4, 0);
	data.writeUInt32LE(bytes, 1);
	return new TransactionInstruction({
		programId: ComputeBudgetProgram.programId,
		keys: [],
		data,
	});
}

/**
 * Checks a list of instructions for the presence of compute-budget instructions — used by tx
 * builders to avoid appending a duplicate when the caller already supplied one.
 * @param ixs - Instructions to scan (typically an in-progress transaction's instruction list).
 * @returns Which of the three compute-budget instructions are present.
 */
export function containsComputeUnitIxs(ixs: TransactionInstruction[]): {
	hasSetComputeUnitLimitIx: boolean;
	hasSetComputeUnitPriceIx: boolean;
	hasSetLoadedAccountsDataSizeIx: boolean;
} {
	return {
		hasSetComputeUnitLimitIx: ixs.some(isSetComputeUnitsIx),
		hasSetComputeUnitPriceIx: ixs.some(isSetComputeUnitPriceIx),
		hasSetLoadedAccountsDataSizeIx: ixs.some(isSetLoadedAccountsDataSizeIx),
	};
}
