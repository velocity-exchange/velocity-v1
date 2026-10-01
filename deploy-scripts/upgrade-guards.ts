/**
 * Checks that `migrate.ts` and `verify-upgrade.ts` share: the pause bits the upgrade bracket
 * sets, the code hash of the CLOB program, and when a pending proposal does exactly a step.
 */
import { createHash } from 'crypto';
import * as fs from 'fs';
import { Connection, PublicKey, TransactionInstruction } from '@solana/web3.js';
import { ExchangeStatus, getProgramDataAddress } from '@velocity-exchange/sdk';

/** Settle and withdraw pause with liquidations, or a loser that cannot be liquidated settles its
 * loss to a winner who withdraws it. No user can act on a position, so funding pauses too. */
export const UPGRADE_PAUSE_BITS =
	ExchangeStatus.LIQ_PAUSED |
	ExchangeStatus.WITHDRAW_PAUSED |
	ExchangeStatus.SETTLE_PNL_PAUSED |
	ExchangeStatus.FUNDING_PAUSED;

/** The status with the upgrade bracket's bits cleared and every other bit kept. */
export function liftedStatus(status: number): number {
	return status & ~UPGRADE_PAUSE_BITS;
}

/** `ProgramData` header: a u32 tag, the deploy slot and an optional authority. */
const PROGRAM_DATA_METADATA_BYTES = 45;

/** `solana-verify get-executable-hash`: SHA-256 of the bytes with trailing zeros trimmed, so a
 * program account padded past its ELF hashes the same as the file. */
export function executableHash(bytes: Buffer): string {
	let end = bytes.length;
	while (end > 0 && bytes[end - 1] === 0) end -= 1;
	return createHash('sha256').update(bytes.subarray(0, end)).digest('hex');
}

/** The CLOB hash to require: `--clob-hash`, or the hash of the `--clob-so` artifact. */
export function expectedClobHash(
	clobHash: string | undefined,
	clobSo: string | undefined
): string | undefined {
	if (clobHash) return clobHash.toLowerCase();
	if (clobSo) return executableHash(fs.readFileSync(clobSo));
	return undefined;
}

/** The deployed program's executable hash, or undefined when it has no program data. */
export async function deployedProgramHash(
	connection: Connection,
	program: PublicKey
): Promise<string | undefined> {
	const programData = await connection.getAccountInfo(
		getProgramDataAddress(program)
	);
	if (!programData) return undefined;
	return executableHash(programData.data.subarray(PROGRAM_DATA_METADATA_BYTES));
}

/** One instruction of a proposal's vault message. */
export type ProposedInstruction = {
	program: PublicKey;
	data: Buffer;
	accounts: PublicKey[];
};

export function asProposed(ix: TransactionInstruction): ProposedInstruction {
	return {
		program: ix.programId,
		data: ix.data,
		accounts: ix.keys.map((key) => key.pubkey),
	};
}

/** A key equal for two instructions with the same program, data and account list. */
export function instructionKey(ix: ProposedInstruction): string {
	return [
		ix.program.toBase58(),
		ix.data.toString('hex'),
		...ix.accounts.map((account) => account.toBase58()),
	].join(':');
}

/** Whether the proposal holds exactly `expected`, in order and nothing else. */
export function proposalDoes(
	proposal: readonly ProposedInstruction[],
	expected: readonly TransactionInstruction[]
): boolean {
	return (
		proposal.length === expected.length &&
		proposal.every(
			(ix, i) => instructionKey(ix) === instructionKey(asProposed(expected[i]))
		)
	);
}

/** Whether every instruction of the proposal is one of `expectedKeys`. */
export function proposalWithin(
	proposal: readonly ProposedInstruction[],
	expectedKeys: ReadonlySet<string>
): boolean {
	return (
		proposal.length > 0 &&
		proposal.every((ix) => expectedKeys.has(instructionKey(ix)))
	);
}
