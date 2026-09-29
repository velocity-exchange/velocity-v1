/**
 * allow-verbose: the module doc, which the budget measures against the import line.
 *
 * The relay watches that `migrate.ts` registers and `verify-upgrade.ts` requires. Anyone can
 * register a watch, so a watch serves a target only when its creator, its block offset and its
 * recorded owner program are the expected ones. The block at the offset names the resolver.
 */
import { Connection, PublicKey } from '@solana/web3.js';

export const RELAY_PROGRAM = new PublicKey(
	process.env.RELAY_PROGRAM_ID ?? '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu'
);

/** `relay_spec::WATCH_V0_LEN`: the discriminator, then `WatchV0`. */
export const WATCH_V0_LEN = 112;
const WATCH_TARGET_PROGRAM_OFFSET = 8;
const WATCH_TARGET_OFFSET = 40;
const WATCH_CREATOR_OFFSET = 72;
const WATCH_BLOCK_OFFSET_OFFSET = 104;

export type Watch = {
	address: PublicKey;
	targetProgram: PublicKey;
	target: PublicKey;
	creator: PublicKey;
	blockOffset: number;
};

export type ExpectedWatch = {
	target: PublicKey;
	/** Relay records the target's owner at registration. A watch registered
	 * before the target existed names the system program. */
	targetOwner: PublicKey;
	blockOffset: number;
	creators: PublicKey[];
};

export type WatchesOnTarget = {
	serving: Watch[];
	impostors: Watch[];
};

export function decodeWatch(address: PublicKey, data: Buffer): Watch | undefined {
	if (data.length < WATCH_V0_LEN) return undefined;
	const key = (offset: number) => new PublicKey(data.subarray(offset, offset + 32));
	return {
		address,
		targetProgram: key(WATCH_TARGET_PROGRAM_OFFSET),
		target: key(WATCH_TARGET_OFFSET),
		creator: key(WATCH_CREATOR_OFFSET),
		blockOffset: data.readUInt32LE(WATCH_BLOCK_OFFSET_OFFSET),
	};
}

export function watchServes(watch: Watch, expected: ExpectedWatch): boolean {
	return (
		watch.target.equals(expected.target) &&
		watch.targetProgram.equals(expected.targetOwner) &&
		watch.blockOffset === expected.blockOffset &&
		expected.creators.some((creator) => creator.equals(watch.creator))
	);
}

export function classifyWatches(
	watches: Watch[],
	expected: ExpectedWatch
): WatchesOnTarget {
	const serving = watches.filter((watch) => watchServes(watch, expected));
	const impostors = watches.filter((watch) => !watchServes(watch, expected));
	return { serving, impostors };
}

export async function watchesOnTarget(
	connection: Connection,
	expected: ExpectedWatch
): Promise<WatchesOnTarget> {
	const accounts = await connection.getProgramAccounts(RELAY_PROGRAM, {
		filters: [
			{ dataSize: WATCH_V0_LEN },
			{ memcmp: { offset: WATCH_TARGET_OFFSET, bytes: expected.target.toBase58() } },
		],
	});
	const watches = accounts
		.map(({ pubkey, account }) => decodeWatch(pubkey, account.data))
		.filter((watch): watch is Watch => watch !== undefined);
	return classifyWatches(watches, expected);
}

export function describeImpostor(watch: Watch): string {
	return (
		`watch ${watch.address.toBase58()} by ${watch.creator.toBase58()} ` +
		`at offset ${watch.blockOffset} for program ${watch.targetProgram.toBase58()}`
	);
}

/** Parse `--watch-creators a,b` into keys, always including `fallback`. */
export function parseWatchCreators(
	raw: string | undefined,
	fallback: PublicKey
): PublicKey[] {
	const listed = (raw ?? '')
		.split(',')
		.map((value) => value.trim())
		.filter((value) => value.length > 0)
		.map((value) => new PublicKey(value));
	return [fallback, ...listed.filter((key) => !key.equals(fallback))];
}
