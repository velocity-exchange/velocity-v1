/**
 * allow-verbose: the usage header of an operator script.
 *
 * Dump the velocity and vaults accounts of a cluster, and the accounts they name one hop out,
 * into a directory that `solana-test-validator --account-dir` loads.
 * `rehearse-devnet-upgrade.sh` runs an upgrade against it.
 *
 *   bun run deploy-scripts/snapshot-devnet.ts --url <rpc> --out <dir>
 *
 * It also writes the accounts and binaries that velocity reaches without naming them: the Pyth
 * Lazer storage account, each mint's faucet config, and the vaults and faucet programs.
 *
 * Every admin and hot-role key in `State` becomes the local key in `<dir>/authority.json`, so
 * the rehearsal can sign admin instructions. `manifest.json` records the slot and the old keys.
 */
import * as fs from 'fs';
import * as path from 'path';
import { BorshAccountsCoder, Idl } from '@coral-xyz/anchor';
import { AccountInfo, Connection, Keypair, PublicKey } from '@solana/web3.js';

const VELOCITY = new PublicKey('vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P');
const VAULTS = new PublicKey('vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR');
const TOKEN_FAUCET = new PublicKey(
	'V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB'
);
/** `post_pyth_lazer_oracle_update` checks the signer against this account's trusted signers. */
const PYTH_LAZER_STORAGE = new PublicKey(
	'3rdJbqfnagQ4yx9HXJViD4zc4xpiSqmFsKpPuSCQVyQL'
);
/** Bytes ahead of the ELF in an upgradeable program's data account. */
const PROGRAM_DATA_HEADER = 45;

/** Zero bytes appended before a decode. An account the upgrade has not grown
 * yet is shorter than the new layout, and borsh fails on a short buffer. */
const DECODE_TAIL = 8192;

type Args = { url: string; out: string };

function parseArgs(): Args {
	const argv = process.argv.slice(2);
	const get = (flag: string, fallback?: string) => {
		const i = argv.indexOf(flag);
		if (i >= 0 && argv[i + 1]) return argv[i + 1];
		if (fallback !== undefined) return fallback;
		throw new Error(`missing ${flag}`);
	};

	return {
		url: get(
			'--url',
			process.env.DEVNET_RPC_URL ?? 'https://api.devnet.solana.com'
		),
		out: get('--out'),
	};
}

type OwnedAccount = { pubkey: PublicKey; account: AccountInfo<Buffer> };

class ProgramDecoder {
	private coder: BorshAccountsCoder;
	private names = new Map<string, string>();

	constructor(idl: Idl) {
		this.coder = new BorshAccountsCoder(idl);
		for (const account of idl.accounts ?? []) {
			this.names.set(
				Buffer.from(account.discriminator).toString('hex'),
				account.name
			);
		}
	}

	nameOf(data: Buffer): string | undefined {
		return this.names.get(data.subarray(0, 8).toString('hex'));
	}

	decode(data: Buffer): unknown {
		const name = this.nameOf(data);
		if (!name) return undefined;
		return this.coder.decode(
			name,
			Buffer.concat([data, Buffer.alloc(DECODE_TAIL)])
		);
	}
}

function collectPubkeys(value: unknown, into: Set<string>): void {
	if (value instanceof PublicKey) {
		if (!value.equals(PublicKey.default)) into.add(value.toBase58());
		return;
	}

	if (Array.isArray(value)) {
		value.forEach((item) => collectPubkeys(item, into));
	} else if (value && typeof value === 'object' && !Buffer.isBuffer(value)) {
		Object.values(value).forEach((item) => collectPubkeys(item, into));
	}
}

/**
 * Point every admin and hot-role key in `State` at `authority`. The keys are
 * found by field name and replaced by value, so a field that moved between
 * layouts is still found as long as its name stayed.
 */
function patchStateAuthorities(
	state: OwnedAccount,
	decoded: Record<string, unknown>,
	authority: PublicKey
): string[] {
	const replaced = Object.entries(decoded)
		.filter(([field]) =>
			/^(cold_admin|warm_admin|pause_admin|admin|hot_)/.test(field)
		)
		.map(([, value]) => value)
		.filter((value): value is PublicKey => value instanceof PublicKey)
		.filter((key) => !key.equals(PublicKey.default));

	const data = state.account.data;
	const unique = [...new Set(replaced.map((key) => key.toBase58()))];
	for (const key of unique) {
		const needle = new PublicKey(key).toBuffer();
		for (
			let at = data.indexOf(needle);
			at >= 0;
			at = data.indexOf(needle, at + 32)
		) {
			authority.toBuffer().copy(data, at);
		}
	}

	return unique;
}

async function getMultipleChunked(connection: Connection, keys: PublicKey[]) {
	const infos: (AccountInfo<Buffer> | null)[] = [];
	for (let i = 0; i < keys.length; i += 100) {
		infos.push(
			...(await connection.getMultipleAccountsInfo(keys.slice(i, i + 100)))
		);
	}

	return infos;
}

/** The JSON shape `solana account --output json` writes and `--account-dir`
 * reads. `rentEpoch` is written as 0 because the real value, u64::MAX, does
 * not survive a JavaScript number. */
function writeAccount(
	dir: string,
	pubkey: PublicKey,
	account: AccountInfo<Buffer>
) {
	const json = {
		pubkey: pubkey.toBase58(),
		account: {
			lamports: account.lamports,
			data: [account.data.toString('base64'), 'base64'],
			owner: account.owner.toBase58(),
			executable: false,
			rentEpoch: 0,
			space: account.data.length,
		},
	};

	fs.writeFileSync(
		path.join(dir, `${pubkey.toBase58()}.json`),
		JSON.stringify(json)
	);
}

function loadAuthority(out: string): Keypair {
	const file = path.join(out, 'authority.json');
	if (fs.existsSync(file)) {
		return Keypair.fromSecretKey(
			Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf-8')))
		);
	}

	const authority = Keypair.generate();
	fs.writeFileSync(file, JSON.stringify(Array.from(authority.secretKey)));
	return authority;
}

/** What decoding the owned accounts found: the keys they name, and the keys the State patch replaced. */
type OwnedScan = {
	referenced: Set<string>;
	undecodable: string[];
	patchedAuthorities: string[];
};

async function fetchOwned(connection: Connection): Promise<OwnedAccount[]> {
	const owned: OwnedAccount[] = [];
	for (const program of [VELOCITY, VAULTS]) {
		const accounts = await connection.getProgramAccounts(program, {
			commitment: 'confirmed',
		});
		console.log(`${program.toBase58()}: ${accounts.length} accounts`);
		owned.push(...accounts);
	}

	return owned;
}

function scanOwned(
	owned: OwnedAccount[],
	decoders: Map<string, ProgramDecoder>,
	authority: PublicKey
): OwnedScan {
	const scan: OwnedScan = {
		referenced: new Set(),
		undecodable: [],
		patchedAuthorities: [],
	};
	for (const entry of owned) {
		const decoder = decoders.get(entry.account.owner.toBase58())!;
		const name = decoder.nameOf(entry.account.data);
		let decoded: unknown;
		try {
			decoded = decoder.decode(entry.account.data);
		} catch (error) {
			scan.undecodable.push(`${entry.pubkey.toBase58()} ${name}: ${error}`);
			continue;
		}

		collectPubkeys(decoded, scan.referenced);
		if (name === 'State' && entry.account.owner.equals(VELOCITY)) {
			scan.patchedAuthorities = patchStateAuthorities(
				entry,
				decoded as Record<string, unknown>,
				authority
			);
		}
	}

	return scan;
}

/** The accounts the owned accounts name, less programs and less the owned accounts themselves. */
async function fetchDependencies(
	connection: Connection,
	owned: OwnedAccount[],
	referenced: Set<string>
): Promise<OwnedAccount[]> {
	const ownedKeys = new Set(owned.map(({ pubkey }) => pubkey.toBase58()));
	const keys = [...referenced]
		.filter((key) => !ownedKeys.has(key))
		.map((key) => new PublicKey(key));
	const infos = await getMultipleChunked(connection, keys);
	return keys
		.map((pubkey, i) => ({ pubkey, account: infos[i] }))
		.filter(
			(entry): entry is OwnedAccount =>
				entry.account !== null && !entry.account.executable
		);
}

function report(
	scan: OwnedScan,
	dependencies: number,
	slot: number,
	authority: PublicKey
): void {
	console.log(`${dependencies} dependency accounts, slot ${slot}`);
	console.log(
		`State authorities ${
			scan.patchedAuthorities.join(', ') || 'none'
		} -> ${authority.toBase58()}`
	);
	if (scan.undecodable.length > 0) {
		console.log(
			`${scan.undecodable.length} accounts did not decode, so their dependencies are missing:`
		);
		scan.undecodable.forEach((line) => console.log(`  ${line}`));
	}

	if (scan.patchedAuthorities.length === 0) {
		throw new Error(
			'no State authority was patched; the rehearsal could not sign admin instructions'
		);
	}
}

async function main() {
	const args = parseArgs();
	const connection = new Connection(args.url, 'confirmed');
	const accountsDir = path.join(args.out, 'accounts');
	fs.rmSync(accountsDir, { recursive: true, force: true });
	fs.mkdirSync(accountsDir, { recursive: true });
	const authority = loadAuthority(args.out);
	const velocityDecoder = new ProgramDecoder(
		readIdl('packages/sdk/src/idl/velocity.json')
	);
	const decoders = new Map<string, ProgramDecoder>([
		[VELOCITY.toBase58(), velocityDecoder],
		[
			VAULTS.toBase58(),
			new ProgramDecoder(readIdl('packages/vaults-sdk/src/idl/vaults.json')),
		],
	]);

	const slot = await connection.getSlot('confirmed');
	const owned = await fetchOwned(connection);
	const scan = scanOwned(owned, decoders, authority.publicKey);
	const dependencies = await fetchDependencies(
		connection,
		owned,
		scan.referenced
	);
	const unreferenced = await fetchUnreferenced(
		connection,
		owned,
		velocityDecoder
	);
	[...owned, ...dependencies, ...unreferenced].forEach(({ pubkey, account }) =>
		writeAccount(accountsDir, pubkey, account)
	);

	for (const [name, program] of [
		['vaults', VAULTS],
		['token_faucet', TOKEN_FAUCET],
	] as const) {
		fs.writeFileSync(
			path.join(args.out, `${name}-devnet.so`),
			await programBinary(connection, program)
		);
	}

	const manifest = {
		url: args.url,
		slot,
		createdAt: new Date().toISOString(),
		authority: authority.publicKey.toBase58(),
		patchedAuthorities: scan.patchedAuthorities,
		ownedAccounts: owned.length,
		dependencyAccounts: dependencies.length,
		unreferencedAccounts: unreferenced.map(({ pubkey }) => pubkey.toBase58()),
		undecodable: scan.undecodable,
	};

	fs.writeFileSync(
		path.join(args.out, 'manifest.json'),
		JSON.stringify(manifest, null, 2)
	);
	report(scan, dependencies.length, slot, authority.publicKey);
}

/** The Pyth Lazer storage account, and the faucet config of every spot market's mint. */
async function fetchUnreferenced(
	connection: Connection,
	owned: OwnedAccount[],
	velocityDecoder: ProgramDecoder
): Promise<OwnedAccount[]> {
	const mints = owned
		.filter(
			({ account }) => velocityDecoder.nameOf(account.data) === 'SpotMarket'
		)
		.map(
			({ account }) =>
				(velocityDecoder.decode(account.data) as { mint: PublicKey }).mint
		);
	const keys = [
		PYTH_LAZER_STORAGE,
		...mints.map(
			(mint) =>
				PublicKey.findProgramAddressSync(
					[Buffer.from('faucet_config'), mint.toBuffer()],
					TOKEN_FAUCET
				)[0]
		),
	];
	const infos = await getMultipleChunked(connection, keys);
	return keys
		.map((pubkey, i) => ({ pubkey, account: infos[i] }))
		.filter((entry): entry is OwnedAccount => entry.account !== null);
}

async function programBinary(
	connection: Connection,
	program: PublicKey
): Promise<Buffer> {
	const programAccount = await connection.getAccountInfo(program);
	if (!programAccount)
		throw new Error(`program ${program.toBase58()} is not deployed`);
	const programData = new PublicKey(programAccount.data.subarray(4, 36));
	const programDataAccount = await connection.getAccountInfo(programData);
	return programDataAccount!.data.subarray(PROGRAM_DATA_HEADER);
}

function readIdl(file: string): Idl {
	return JSON.parse(fs.readFileSync(file, 'utf-8'));
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
