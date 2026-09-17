import { PublicKey, TransactionInstruction } from '@solana/web3.js';

const COMPUTE_BUDGET_PROGRAM_ID = 'ComputeBudget111111111111111111111111111111';
const TOKEN_PROGRAM_ID = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';
const SYSTEM_PROGRAM_ID = '11111111111111111111111111111111';
const ASSOCIATED_TOKEN_PROGRAM_ID =
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL';

/** Index of the mint in an associated-token-account instruction's key list. */
const ATA_IX_MINT_KEY_INDEX = 3;

/**
 * Strip the setup and teardown a provider wraps around its route. What remains
 * is the instructions that go between `beginSwap` and `endSwap`.
 *
 * Velocity supplies its own compute budget, funds the input token account from
 * the spot market vault, and sweeps the output back. The provider's version of
 * all three is redundant. The provider's ATA creation for the input or output
 * mint goes too, because velocity creates those accounts itself. An ATA for any
 * other mint stays, such as an intermediate hop or a fee account, because
 * nothing else creates it.
 *
 * Both providers call this one function. Two copies drifted when each client
 * held its own, and the difference showed up only as a malformed transaction.
 */
export function filterRouteInstructions({
	instructions,
	inputMint,
	outputMint,
}: {
	instructions: TransactionInstruction[];
	inputMint: PublicKey;
	outputMint: PublicKey;
}): TransactionInstruction[] {
	return instructions.filter((instruction) => {
		const programId = instruction.programId.toString();

		if (
			programId === COMPUTE_BUDGET_PROGRAM_ID ||
			programId === TOKEN_PROGRAM_ID ||
			programId === SYSTEM_PROGRAM_ID
		) {
			return false;
		}

		if (programId === ASSOCIATED_TOKEN_PROGRAM_ID) {
			// The index is guarded because a malformed ATA instruction with a short
			// key list would throw here instead of being kept.
			const mint = instruction.keys[ATA_IX_MINT_KEY_INDEX]?.pubkey;

			if (mint && (mint.equals(inputMint) || mint.equals(outputMint))) {
				return false;
			}
		}

		return true;
	});
}
