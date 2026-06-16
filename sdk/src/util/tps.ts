import { Connection, PublicKey } from '@solana/web3.js';

export async function estimateTps(
	programId: PublicKey,
	connection: Connection,
	failed: boolean
): Promise<number> {
	let signatures = await connection.getSignaturesForAddress(
		programId,
		undefined,
		'finalized'
	);
	if (failed) {
		signatures = signatures.filter((signature) => signature.err);
	}

	const numberOfSignatures = signatures.length;

	if (numberOfSignatures === 0) {
		return 0;
	}

	const newest = signatures[0].blockTime;
	const oldest = signatures[numberOfSignatures - 1].blockTime;
	if (newest == null || oldest == null) {
		return 0;
	}
	const windowSecs = newest - oldest;
	if (windowSecs <= 0) {
		return 0;
	}
	return numberOfSignatures / windowSecs;
}
