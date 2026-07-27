import {
	PublicKey,
	TransactionInstruction,
	TransactionMessage,
} from '@solana/web3.js';

const COMPUTE_BUDGET_PROGRAM_ID = 'ComputeBudget111111111111111111111111111111';
const TOKEN_PROGRAM_ID = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';
const SYSTEM_PROGRAM_ID = '11111111111111111111111111111111';
const ASSOCIATED_TOKEN_PROGRAM_ID =
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL';

/** Index of the mint in an associated-token-account instruction's key list. */
const ATA_IX_MINT_KEY_INDEX = 3;

/**
 * Strips the setup and teardown a provider wraps around its route, leaving the
 * instructions that go between `beginSwap` and `endSwap`.
 *
 * Velocity supplies its own compute budget, funds the input token account from
 * the spot market vault, and sweeps the output back, so the provider's versions
 * of all three are redundant. Its ATA creation for the input or output mint is
 * dropped too, since velocity creates those itself; ATAs for any other mint
 * (intermediate hops, fee accounts) are kept because nothing else creates them.
 *
 * Shared by both providers on purpose — when this lived in each client the two
 * copies drifted, and the difference only showed up as a malformed transaction.
 */
export function filterRouteInstructions({
	transactionMessage,
	inputMint,
	outputMint,
}: {
	transactionMessage: TransactionMessage;
	inputMint: PublicKey;
	outputMint: PublicKey;
}): TransactionInstruction[] {
	return transactionMessage.instructions.filter((instruction) => {
		const programId = instruction.programId.toString();

		if (
			programId === COMPUTE_BUDGET_PROGRAM_ID ||
			programId === TOKEN_PROGRAM_ID ||
			programId === SYSTEM_PROGRAM_ID
		) {
			return false;
		}

		if (programId === ASSOCIATED_TOKEN_PROGRAM_ID) {
			// Guarded: a malformed ATA instruction with a short key list would
			// otherwise throw here rather than simply being kept.
			const mint = instruction.keys[ATA_IX_MINT_KEY_INDEX]?.pubkey;

			if (mint && (mint.equals(inputMint) || mint.equals(outputMint))) {
				return false;
			}
		}

		return true;
	});
}
