import { Connection, PublicKey } from '@solana/web3.js';

/** `getMultipleAccountsInfo` caps at 100 keys per request. */
const MAX_KEYS_PER_CALL = 100;

/**
 * `getMultipleAccountsInfo` over any number of keys, chunked and issued
 * concurrently. Results keep the order of `keys`.
 */
export async function getAccountsBatched(
	connection: Connection,
	keys: PublicKey[]
): Promise<(Awaited<ReturnType<Connection['getAccountInfo']>> | null)[]> {
	if (keys.length === 0) {
		return [];
	}
	const chunks: PublicKey[][] = [];
	for (let i = 0; i < keys.length; i += MAX_KEYS_PER_CALL) {
		chunks.push(keys.slice(i, i + MAX_KEYS_PER_CALL));
	}
	const parts = await Promise.all(
		chunks.map((c) =>
			connection
				.getMultipleAccountsInfo(c)
				.catch(() => c.map(() => null) as any[])
		)
	);
	return parts.flat();
}

/**
 * Balances of SPL token accounts, by key.
 *
 * Reads the amount straight out of the account data rather than calling
 * `getTokenAccountBalance` per key: the SPL token account layout puts the
 * balance at a fixed offset, so one batched `getMultipleAccountsInfo` replaces
 * N round trips. A key that is missing or too short to be a token account is
 * absent from the map, which is what the caller wants to show as "n/a" --
 * distinguishable from a real zero balance.
 */
export async function getTokenBalancesBatched(
	connection: Connection,
	keys: PublicKey[]
): Promise<Map<string, bigint>> {
	/** mint(32) + owner(32), then amount as u64 LE. */
	const AMOUNT_OFFSET = 64;
	const MIN_LEN = AMOUNT_OFFSET + 8;

	const infos = await getAccountsBatched(connection, keys);
	const out = new Map<string, bigint>();
	infos.forEach((info, i) => {
		if (!info || info.data.length < MIN_LEN) {
			return;
		}
		out.set(keys[i].toBase58(), info.data.readBigUInt64LE(AMOUNT_OFFSET));
	});
	return out;
}

/** Format a raw token amount against its mint decimals. */
export function uiAmount(raw: bigint | undefined, decimals: number): string {
	if (raw === undefined) {
		return 'n/a';
	}
	const div = 10n ** BigInt(decimals);
	const whole = raw / div;
	const frac = (raw % div)
		.toString()
		.padStart(decimals, '0')
		.replace(/0+$/, '');
	return frac ? `${whole}.${frac}` : whole.toString();
}
