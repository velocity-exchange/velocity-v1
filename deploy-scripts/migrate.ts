/**
 * One command to run after a program upgrade: everything on-chain that has
 * to change to match the new code, in dependency order, idempotent, with a
 * dry run.
 *
 * The point is that there is exactly one thing to remember at deploy time.
 * Every migration this branch introduced is a step below; adding a new one
 * means adding a step here, not a note in a runbook.
 *
 *   bun run deploy-scripts/migrate.ts --url <rpc> --keypair <path> [--dry-run]
 *
 * Steps:
 *   1. resize — grow every velocity-owned zero-copy account whose struct
 *      gained fields (`extend_account` resolves the target size from the
 *      discriminator, so this covers past and future growth uniformly).
 *   2. liq coverage — create + sync relay liquidation conditions for every
 *      user with exposure. New users get theirs from `initialize_user`;
 *      this backfills everyone who predates that.
 *   3. watches — register the relay `WatchV0` records that make all of the
 *      above discoverable: market crank conditions, per-quoter cross
 *      conditions, and the per-user liquidation conditions from step 2.
 *
 * Safe to re-run: every step checks on-chain state first and skips what is
 * already correct, so a partial run (rate limit, laptop lid) is resumed by
 * running it again.
 */
import { createHash } from 'crypto';
import * as fs from 'fs';
import {
	AccountMeta,
	Connection,
	Keypair,
	PublicKey,
	SystemProgram,
	SYSVAR_RENT_PUBKEY,
	Transaction,
	TransactionInstruction,
} from '@solana/web3.js';
import { AnchorProvider, BN, Program } from '@coral-xyz/anchor';
import {
	getClobCrankConditionsPublicKey,
	getLiqConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getSpotMarketPublicKeySync,
	Wallet,
} from '@velocity-exchange/sdk';

const RELAY_PROGRAM = new PublicKey(
	process.env.RELAY_PROGRAM_ID ?? '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu'
);
const WATCH_V0_LEN = 112;
/** Block offset within every velocity conditions account (past anchor's disc). */
const BLOCK_OFFSET = 8;

type Args = {
	url: string;
	keypair: string;
	dryRun: boolean;
	limit: number;
	syncPaymentLamports: bigint;
	fallbackSlots: bigint;
};

function parseArgs(): Args {
	const argv = process.argv.slice(2);
	const get = (flag: string, fallback?: string) => {
		const i = argv.indexOf(flag);
		if (i >= 0 && argv[i + 1]) return argv[i + 1];
		if (fallback !== undefined) return fallback;
		throw new Error(`missing ${flag}`);
	};
	return {
		url: get('--url', process.env.RPC_URL ?? 'http://127.0.0.1:8899'),
		keypair: get('--keypair', `${process.env.HOME}/.config/solana/id.json`),
		dryRun: argv.includes('--dry-run'),
		limit: Number.parseInt(get('--limit', '0'), 10),
		syncPaymentLamports: BigInt(get('--sync-payment', '20000')),
		fallbackSlots: BigInt(get('--fallback-slots', '3000')),
	};
}

function discriminator(name: string): Buffer {
	return createHash('sha256')
		.update(`account:${name}`)
		.digest()
		.subarray(0, 8);
}

function ixDiscriminator(name: string): Buffer {
	return createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);
}

/** Zero-copy accounts `extend_account` knows how to grow, and their sizes in
 * the *current* build. Anything already at or beyond its target is skipped,
 * so this list is a superset — listing a type that never grew is harmless. */
const RESIZABLE: { name: string; size: number }[] = [
	{ name: 'User', size: 8 + 4496 },
	{ name: 'PerpMarket', size: 8 + 1328 },
	{ name: 'QuoterV0', size: 8 + 2752 },
	{ name: 'ClobCrankConditionsV0', size: 3848 },
	{ name: 'QuoterCrossConditionsV0', size: 4360 },
	{ name: 'TriggerConditionsV0', size: 6040 },
	{ name: 'LiqConditionsV0', size: 7272 },
];

async function main() {
	const args = parseArgs();
	const connection = new Connection(args.url, 'confirmed');
	const payer = Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(args.keypair, 'utf-8')))
	);
	const provider = new AnchorProvider(connection, new Wallet(payer) as any, {
		commitment: 'confirmed',
	});
	const idl = JSON.parse(
		fs.readFileSync('packages/sdk/src/idl/velocity.json', 'utf-8')
	);
	const program = new Program(idl, provider);
	const velocity = program.programId;
	const plan: string[] = [];
	const act = async (
		label: string,
		ixs: TransactionInstruction[],
		signers: Keypair[] = []
	) => {
		plan.push(label);
		if (args.dryRun) return;
		await provider.sendAndConfirm(new Transaction().add(...ixs), signers);
	};

	console.log(`velocity ${velocity.toBase58()} @ ${args.url}`);
	console.log(args.dryRun ? '(dry run — nothing will be sent)\n' : '');

	// ---- 1. resize --------------------------------------------------------
	const state = PublicKey.findProgramAddressSync(
		[Buffer.from('velocity_state')],
		velocity
	)[0];
	for (const { name, size } of RESIZABLE) {
		const accounts = await connection.getProgramAccounts(velocity, {
			filters: [
				{ memcmp: { offset: 0, bytes: bs58(discriminator(name)) } },
			],
			dataSlice: { offset: 0, length: 0 },
		});
		const keys = accounts.map((a) => a.pubkey);
		const infos = await getMultipleAccountsChunked(connection, keys);
		const stale = keys.filter((_, i) => (infos[i]?.data.length ?? 0) < size);
		if (stale.length === 0) {
			console.log(`resize ${name}: ${keys.length} accounts, all current`);
			continue;
		}
		console.log(
			`resize ${name}: ${stale.length}/${keys.length} undersized -> ${size}b`
		);
		for (const account of stale.slice(0, args.limit || stale.length)) {
			await act(`extend ${name} ${account.toBase58()}`, [
				await program.methods
					.extendAccount()
					.accounts({
						state,
						payer: payer.publicKey,
						authority: payer.publicKey,
						account,
						systemProgram: SystemProgram.programId,
					})
					.instruction(),
			]);
		}
	}

	// ---- 2. liquidation coverage -----------------------------------------
	const userDisc = discriminator('User');
	const users = await connection.getProgramAccounts(velocity, {
		filters: [{ memcmp: { offset: 0, bytes: bs58(userDisc) } }],
	});
	console.log(`\nliq coverage: ${users.length} user accounts`);
	// Markets + oracles the sync needs, gathered once.
	const perpMarkets = await connection.getProgramAccounts(velocity, {
		filters: [
			{ memcmp: { offset: 0, bytes: bs58(discriminator('PerpMarket')) } },
		],
	});
	const marketOracles = new Map<number, PublicKey>();
	for (const { account } of perpMarkets) {
		// Decoded through the IDL rather than by byte offset: layouts move,
		// and a migration reading the wrong field is worse than one that
		// fails loudly.
		const decoded: any = program.coder.accounts.decode('PerpMarket', account.data);
		marketOracles.set(decoded.marketIndex, decoded.oracle);
	}

	let covered = 0;
	for (const { pubkey: user, account } of users) {
		if (args.limit && covered >= args.limit) break;
		const decodedUser: any = program.coder.accounts.decode('User', account.data);
		const marketIndexes = exposedPerpMarkets(decodedUser);
		if (marketIndexes.length === 0) continue;
		const liqConditions = getLiqConditionsPublicKey(velocity, user);
		const existing = await connection.getAccountInfo(liqConditions);
		const syncAccounts: AccountMeta[] = [];
		for (const marketIndex of marketIndexes) {
			const oracle = marketOracles.get(marketIndex);
			if (!oracle) continue;
			syncAccounts.push({ pubkey: oracle, isSigner: false, isWritable: false });
		}
		syncAccounts.push(
			{
				pubkey: getSpotMarketPublicKeySync(velocity, 0),
				isSigner: false,
				isWritable: true,
			},
			...marketIndexes.map((marketIndex) => ({
				pubkey: getPerpMarketPublicKeySync(velocity, marketIndex),
				isSigner: false,
				isWritable: true,
			})),
			...marketIndexes.map((marketIndex) => ({
				pubkey: getClobCrankConditionsPublicKey(velocity, marketIndex),
				isSigner: false,
				isWritable: false,
			}))
		);
		const argsBuf = Buffer.alloc(16);
		argsBuf.writeBigUInt64LE(args.syncPaymentLamports, 0);
		argsBuf.writeBigUInt64LE(args.fallbackSlots, 8);
		await act(
			`${existing ? 'sync' : 'create+sync'} liq conditions for ${user.toBase58()}`,
			[
				new TransactionInstruction({
					programId: velocity,
					keys: [
						{ pubkey: payer.publicKey, isSigner: true, isWritable: true },
						{ pubkey: user, isSigner: false, isWritable: false },
						{ pubkey: liqConditions, isSigner: false, isWritable: true },
						{ pubkey: SYSVAR_RENT_PUBKEY, isSigner: false, isWritable: false },
						{
							pubkey: SystemProgram.programId,
							isSigner: false,
							isWritable: false,
						},
						...syncAccounts,
					],
					data: Buffer.concat([
						ixDiscriminator('sync_liq_conditions'),
						argsBuf,
					]),
				}),
			]
		);
		// The sync fee comes from the conditions account's own lamports.
		if (!args.dryRun) {
			const info = await connection.getAccountInfo(liqConditions);
			const floor = await connection.getMinimumBalanceForRentExemption(
				info?.data.length ?? 7272
			);
			const want = floor + Number(args.syncPaymentLamports) * 50;
			if ((info?.lamports ?? 0) < want) {
				await act(`fund sync reservoir ${liqConditions.toBase58()}`, [
					SystemProgram.transfer({
						fromPubkey: payer.publicKey,
						toPubkey: liqConditions,
						lamports: want - (info?.lamports ?? 0),
					}),
				]);
			}
		}
		await ensureWatch(connection, provider, payer, liqConditions, act);
		covered += 1;
	}
	console.log(`liq coverage: ${covered} accounts with exposure`);

	// ---- 3. watches for market + quoter conditions ------------------------
	console.log('');
	for (const [marketIndex] of marketOracles) {
		const conditions = getClobCrankConditionsPublicKey(velocity, marketIndex);
		if (!(await connection.getAccountInfo(conditions))) continue;
		await ensureWatch(connection, provider, payer, conditions, act);
	}
	const quoters = await connection.getProgramAccounts(velocity, {
		filters: [
			{ memcmp: { offset: 0, bytes: bs58(discriminator('QuoterV0')) } },
		],
		dataSlice: { offset: 0, length: 0 },
	});
	for (const { pubkey: quoter } of quoters) {
		const crossConditions = PublicKey.findProgramAddressSync(
			[Buffer.from('quoter_cross_conditions'), quoter.toBuffer()],
			velocity
		)[0];
		if (!(await connection.getAccountInfo(crossConditions))) continue;
		await ensureWatch(connection, provider, payer, crossConditions, act);
	}

	console.log(`\n${args.dryRun ? 'would run' : 'ran'} ${plan.length} steps`);
	for (const line of plan.slice(0, 40)) console.log(`  ${line}`);
	if (plan.length > 40) console.log(`  … ${plan.length - 40} more`);
}

/** Register a relay watch over a conditions block, unless one already
 * exists for that (target, offset). */
async function ensureWatch(
	connection: Connection,
	provider: AnchorProvider,
	payer: Keypair,
	target: PublicKey,
	act: (
		label: string,
		ixs: TransactionInstruction[],
		signers?: Keypair[]
	) => Promise<void>
) {
	// WatchV0 leads with target_program then target, so the registry is
	// queryable by target without decoding.
	const existing = await connection.getProgramAccounts(RELAY_PROGRAM, {
		filters: [{ memcmp: { offset: 40, bytes: target.toBase58() } }],
		dataSlice: { offset: 0, length: 0 },
	});
	if (existing.length > 0) return;
	const watch = Keypair.generate();
	const offset = Buffer.alloc(4);
	offset.writeUInt32LE(BLOCK_OFFSET);
	await act(
		`register watch -> ${target.toBase58()}`,
		[
			SystemProgram.createAccount({
				fromPubkey: payer.publicKey,
				newAccountPubkey: watch.publicKey,
				lamports:
					await connection.getMinimumBalanceForRentExemption(WATCH_V0_LEN),
				space: WATCH_V0_LEN,
				programId: RELAY_PROGRAM,
			}),
			new TransactionInstruction({
				programId: RELAY_PROGRAM,
				keys: [
					{ pubkey: payer.publicKey, isSigner: true, isWritable: false },
					{ pubkey: target, isSigner: false, isWritable: false },
					{ pubkey: watch.publicKey, isSigner: false, isWritable: true },
				],
				data: Buffer.concat([ixDiscriminator('register_watch_v0'), offset]),
			}),
		],
		[watch]
	);
}

async function getMultipleAccountsChunked(
	connection: Connection,
	keys: PublicKey[]
) {
	const out: (null | { data: Buffer })[] = [];
	for (let i = 0; i < keys.length; i += 100) {
		const chunk = await connection.getMultipleAccountsInfo(
			keys.slice(i, i + 100)
		);
		out.push(...chunk.map((a) => (a ? { data: a.data } : null)));
	}
	return out;
}

function bs58(buffer: Buffer): string {
	// web3.js accepts base58 for memcmp; use its own encoder via PublicKey
	// when the payload is 32 bytes, otherwise fall back to bs58 of 8 bytes.
	// eslint-disable-next-line @typescript-eslint/no-var-requires
	const bs58lib = require('bs58');
	return bs58lib.default ? bs58lib.default.encode(buffer) : bs58lib.encode(buffer);
}

/** Perp markets the user has a live position in. */
function exposedPerpMarkets(user: any): number[] {
	const markets: number[] = [];
	for (const position of user.perpPositions ?? []) {
		const base = new BN(position.baseAssetAmount ?? 0);
		const quote = new BN(position.quoteAssetAmount ?? 0);
		if (!base.isZero() || !quote.isZero()) markets.push(position.marketIndex);
	}
	return [...new Set(markets)];
}

main().catch((err) => {
	console.error(err);
	process.exit(1);
});
