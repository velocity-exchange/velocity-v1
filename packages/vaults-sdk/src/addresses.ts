import { PublicKey } from '@solana/web3.js';
import * as anchor from '@coral-xyz/anchor';

export function getVaultAddressSync(
	programId: PublicKey,
	encodedName: number[]
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[
			Buffer.from(anchor.utils.bytes.utf8.encode('vault')),
			Buffer.from(encodedName),
		],
		programId
	)[0];
}

export function getVaultDepositorAddressSync(
	programId: PublicKey,
	vault: PublicKey,
	authority: PublicKey
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[
			Buffer.from(anchor.utils.bytes.utf8.encode('vault_depositor')),
			vault.toBuffer(),
			authority.toBuffer(),
		],
		programId
	)[0];
}

export function getTokenVaultAddressSync(
	programId: PublicKey,
	vault: PublicKey
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[
			Buffer.from(anchor.utils.bytes.utf8.encode('vault_token_account')),
			vault.toBuffer(),
		],
		programId
	)[0];
}

export function getInsuranceFundTokenVaultAddressSync(
	programId: PublicKey,
	vault: PublicKey,
	marketIndex: number
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[
			Buffer.from(anchor.utils.bytes.utf8.encode('vault_token_account')),
			vault.toBuffer(),
			new anchor.BN(marketIndex).toArrayLike(Buffer, 'le', 2),
		],
		programId
	)[0];
}

export function getVaultProtocolAddressSync(
	programId: PublicKey,
	vault: PublicKey
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[
			Buffer.from(anchor.utils.bytes.utf8.encode('vault_protocol')),
			vault.toBuffer(),
		],
		programId
	)[0];
}

/**
 * Highest cohort id a vault can open. Mirrors `TokenizedVaultDepositor::MAX_COHORT_ID`.
 *
 * The bound keeps the seed concatenation unambiguous. `findProgramAddress` concatenates seeds, and
 * the pool seeds hold `sharesBase` as a variable-length decimal string. A cohort id appended after
 * it as 4 little-endian bytes could only alias a longer `sharesBase` string if all 4 bytes were
 * ASCII digits. Every id at or below this bound has two zero bytes in its high half.
 */
export const MAX_TOKENIZED_COHORT_ID = 65535;

/**
 * Seeds shared by a tokenized pool's depositor account and its mint.
 *
 * Cohort 0 is the legacy pool. Its seed list omits the cohort id entirely, so every pool created
 * before cohorts keeps its address. Cohorts 1 and above append the id as 4 little-endian bytes.
 */
function tokenizedPoolSeeds(
	prefix: string,
	vault: PublicKey,
	sharesBase: number,
	cohortId: number
): Buffer[] {
	if (
		!Number.isInteger(cohortId) ||
		cohortId < 0 ||
		cohortId > MAX_TOKENIZED_COHORT_ID
	) {
		throw new Error(
			`cohortId must be an integer in 0..=${MAX_TOKENIZED_COHORT_ID}, got ${cohortId}`
		);
	}

	const seeds = [
		Buffer.from(anchor.utils.bytes.utf8.encode(prefix)),
		vault.toBuffer(),
		Buffer.from(anchor.utils.bytes.utf8.encode(sharesBase.toString())),
	];
	if (cohortId !== 0) {
		seeds.push(new anchor.BN(cohortId).toArrayLike(Buffer, 'le', 4));
	}
	return seeds;
}

/**
 * Address of a vault's tokenized depositor for one cohort.
 *
 * `cohortId` defaults to 0, the legacy pool, so existing callers keep the address they had.
 */
export function getTokenizedVaultAddressSync(
	programId: PublicKey,
	vault: PublicKey,
	sharesBase: number,
	cohortId = 0
): PublicKey {
	return PublicKey.findProgramAddressSync(
		tokenizedPoolSeeds(
			'tokenized_vault_depositor',
			vault,
			sharesBase,
			cohortId
		),
		programId
	)[0];
}

/**
 * Mint address of a vault's tokenized depositor for one cohort. Each cohort has its own SPL mint,
 * and tokens of different cohorts are not fungible with each other.
 *
 * `cohortId` defaults to 0, the legacy pool, so existing callers keep the address they had.
 */
export function getTokenizedVaultMintAddressSync(
	programId: PublicKey,
	vault: PublicKey,
	sharesBase: number,
	cohortId = 0
): PublicKey {
	return PublicKey.findProgramAddressSync(
		tokenizedPoolSeeds('mint', vault, sharesBase, cohortId),
		programId
	)[0];
}

export function getFeeUpdateAddressSync(
	programId: PublicKey,
	vault: PublicKey
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[
			Buffer.from(anchor.utils.bytes.utf8.encode('fee_update')),
			vault.toBuffer(),
		],
		programId
	)[0];
}
