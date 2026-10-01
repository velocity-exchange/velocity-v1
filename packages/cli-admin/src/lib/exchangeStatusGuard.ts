/**
 * `update_exchange_status` writes a whole mask. A status proposal records the status it was
 * built from in a memo, and `multisig execute` refuses it when the live status differs, so a
 * pause added during the signing round is not cleared.
 */
import { createHash } from 'crypto';
import { PublicKey, TransactionInstruction } from '@solana/web3.js';

export const MEMO_PROGRAM = new PublicKey(
	'MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr'
);

const GUARD_PREFIX = 'velocity exchange_status from ';

const UPDATE_EXCHANGE_STATUS = createHash('sha256')
	.update('global:update_exchange_status')
	.digest()
	.subarray(0, 8);

/** The memo that records the status a status write was built from. */
export function exchangeStatusGuardIx(
	statusAtProposal: number
): TransactionInstruction {
	return new TransactionInstruction({
		programId: MEMO_PROGRAM,
		keys: [],
		data: Buffer.from(`${GUARD_PREFIX}${statusAtProposal}`, 'utf-8'),
	});
}

export type ProposalInstruction = { program: PublicKey; data: Buffer };

/**
 * Why executing these instructions now would undo a pause, or undefined when it would not.
 * A status write without a guard memo is refused when it clears a live bit.
 */
export function exchangeStatusWriteProblem(
	instructions: readonly ProposalInstruction[],
	velocity: PublicKey,
	liveStatus: number
): string | undefined {
	const writes = instructions
		.filter(
			(ix) =>
				ix.program.equals(velocity) &&
				ix.data.subarray(0, 8).equals(UPDATE_EXCHANGE_STATUS)
		)
		.map((ix) => ix.data[8]);
	if (writes.length === 0) return undefined;

	const recorded = instructions
		.filter((ix) => ix.program.equals(MEMO_PROGRAM))
		.map((ix) => ix.data.toString('utf-8'))
		.find((text) => text.startsWith(GUARD_PREFIX));
	if (recorded !== undefined) {
		const statusAtProposal = Number.parseInt(
			recorded.slice(GUARD_PREFIX.length),
			10
		);
		if (statusAtProposal === liveStatus) return undefined;

		return (
			`the proposal was built when the exchange status was ${statusAtProposal}, and it is ` +
			`${liveStatus} now. Executing it would write ${writes.join(
				', '
			)}. Propose it again.`
		);
	}

	const cleared = writes
		.map((value) => liveStatus & ~value)
		.find((bits) => bits);
	if (cleared === undefined) return undefined;

	return (
		`the proposal writes the exchange status without recording the status it was built from, ` +
		`and it would clear bits ${cleared} of the live status ${liveStatus}. Propose it again ` +
		'with velocity-admin exchange set-status.'
	);
}
