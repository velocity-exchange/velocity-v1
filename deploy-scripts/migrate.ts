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
	getCrankTreasuryPublicKey,
	getRelayScratchPublicKey,
	getUserConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getVelocityStateAccountPublicKey,
	getSpotMarketPublicKeySync,
	Wallet,
} from '@velocity-exchange/sdk';

const RELAY_PROGRAM = new PublicKey(
	process.env.RELAY_PROGRAM_ID ?? '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu'
);
const WATCH_V0_LEN = 112;
/** Block offset within every velocity conditions account (past anchor's disc). */
const BLOCK_OFFSET = 8;
/**
 * Bytes after `UserConditionsV0.sync_payment_lamports`: `syncFallbackSlots`,
 * `positionsDigest`, and the 72-byte tail reserve. Measured from the end so a
 * field added ahead of it does not move the read.
 */
const BYTES_AFTER_SYNC_PAYMENT = 8 + 8 + 72;

type Args = {
	url: string;
	keypair: string;
	dryRun: boolean;
	limit: number;
	syncCostUnits: number;
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
		syncCostUnits: Number.parseInt(get('--sync-cost-units', '20000'), 10),
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
	{ name: 'perpMarket', size: 8 + 1328 },
	{ name: 'quoterV0', size: 8 + 784 },
	// Relay condition hosts. Sizes come from the `sizes_for_the_migration_script`
	// test in `state/relay_scratch.rs` — run it (`cargo test -p velocity --lib
	// sizes_for_the_migration_script -- --show-output`) and paste, rather than
	// working them out by hand. A type missing from (or stale in) this table is
	// the failure that has no symptom until an account is read at the wrong
	// offset.
	{ name: 'clobCrankConditionsV0', size: 808 },
	{ name: 'QuoterCrossConditionsV0', size: 2424 },
	{ name: 'UserConditionsV0', size: 6040 },
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
	const statePda = await getVelocityStateAccountPublicKey(velocity);
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

	// ---- 1b. shared resolver staging --------------------------------------
	// Every resolver names this account. Until it exists, every relay crank
	// in the program fails simulation with an owner error, so it comes
	// before anything that registers a watch.
	const scratch = getRelayScratchPublicKey(velocity);
	if (await connection.getAccountInfo(scratch)) {
		console.log(`\nscratch ${scratch.toBase58()}: already created`);
	} else {
		console.log(`\nscratch ${scratch.toBase58()}: creating`);
		await act('create relay scratch', [
			await program.methods
				.initializeRelayScratch()
				.accounts({
					scratch,
					payer: payer.publicKey,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				})
				.instruction(),
		]);
	}

	// The treasury every market's crank reservoir refills from. The CLOB crank
	// resolver and the liquidation-conditions resync both name it, so it has to
	// exist before either can run. Created inert; pricing and funding are
	// operator decisions (velocity-admin fees set-crank-treasury, then a plain
	// SOL transfer).
	const treasury = getCrankTreasuryPublicKey(velocity);
	if (await connection.getAccountInfo(treasury)) {
		console.log(`\ntreasury ${treasury.toBase58()}: already created`);
	} else {
		console.log(`\ntreasury ${treasury.toBase58()}: creating`);
		await act('create crank treasury', [
			await program.methods
				.initializeCrankTreasury()
				.accounts({
					treasury,
					admin: payer.publicKey,
					state: statePda,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				})
				.instruction(),
		]);
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
		const decoded: any = program.coder.accounts.decode('perpMarket', account.data);
		marketOracles.set(decoded.marketIndex, decoded.oracle);
	}

	let covered = 0;
	for (const { pubkey: user, account } of users) {
		if (args.limit && covered >= args.limit) break;
		const decodedUser: any = program.coder.accounts.decode('user', account.data);
		const marketIndexes = exposedPerpMarkets(decodedUser);
		if (marketIndexes.length === 0) continue;
		const userConditions = getUserConditionsPublicKey(velocity, user);
		const existing = await connection.getAccountInfo(userConditions);
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
		// `SyncLiqConditionsArgs`: cost units (u32) then the fallback interval.
		// The lamport fee is derived on chain from `State.transactionFeeRails`.
		const argsBuf = Buffer.alloc(12);
		argsBuf.writeUInt32LE(args.syncCostUnits, 0);
		argsBuf.writeBigUInt64LE(args.fallbackSlots, 4);
		await act(
			`${existing ? 'sync' : 'create+sync'} liq conditions for ${user.toBase58()}`,
			[
				new TransactionInstruction({
					programId: velocity,
					keys: [
						{ pubkey: payer.publicKey, isSigner: true, isWritable: true },
						{ pubkey: statePda, isSigner: false, isWritable: false },
						{ pubkey: user, isSigner: false, isWritable: false },
						{ pubkey: userConditions, isSigner: false, isWritable: true },
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
			const info = await connection.getAccountInfo(userConditions);
			const floor = await connection.getMinimumBalanceForRentExemption(
				info?.data.length ?? 7272
			);
			// Fifty syncs' worth. The fee is priced on chain, so read what the
			// account was actually written with rather than restating it.
			const paid = info
				? Number(
						info.data.readBigUInt64LE(
							info.data.length - BYTES_AFTER_SYNC_PAYMENT - 8
						)
				  )
				: 0;
			const want = floor + paid * 50;
			if ((info?.lamports ?? 0) < want) {
				await act(`fund sync reservoir ${userConditions.toBase58()}`, [
					SystemProgram.transfer({
						fromPubkey: payer.publicKey,
						toPubkey: userConditions,
						lamports: want - (info?.lamports ?? 0),
					}),
				]);
			}
		}
		await ensureWatch(connection, provider, payer, userConditions, act);
		covered += 1;
	}
	console.log(`liq coverage: ${covered} accounts with exposure`);

	// ---- 3. watches for market + quoter conditions ------------------------
	console.log('');
	for (const [marketIndex] of marketOracles) {
		const conditions = getClobCrankConditionsPublicKey(velocity, marketIndex);
		const info = await connection.getAccountInfo(conditions);
		if (!info) continue;
		await ensureWatch(connection, provider, payer, conditions, act);
		// The book hosts the four conditions that describe itself, so it needs
		// a watch of its own. Where its block sits and which account it is are
		// both recorded on the conditions above, by the attach that registered
		// velocity's resolvers there.
		const decoded = program.coder.accounts.decode(
			'clobCrankConditionsV0',
			info.data
		) as { clobBlockOffset: number };
		const book = await clobBookFor(connection, velocity, marketIndex, program);
		if (book && decoded.clobBlockOffset) {
			await ensureWatch(
				connection,
				provider,
				payer,
				book,
				act,
				decoded.clobBlockOffset
			);
		}
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

/** The market's book account: its perp market names the CLOB quoter entry,
 * and the entry names the book as its response account. */
async function clobBookFor(
	connection: Connection,
	velocity: PublicKey,
	marketIndex: number,
	program: { coder: { accounts: { decode(name: string, data: Buffer): unknown } } }
): Promise<PublicKey | undefined> {
	const perpMarket = getPerpMarketPublicKeySync(velocity, marketIndex);
	const marketInfo = await connection.getAccountInfo(perpMarket);
	if (!marketInfo) return undefined;
	// The market stores its book directly.
	const { clobMarket } = program.coder.accounts.decode(
		'perpMarket',
		marketInfo.data
	) as { clobMarket: PublicKey };
	if (!clobMarket || clobMarket.equals(PublicKey.default)) return undefined;
	return new PublicKey(clobMarket);
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
	) => Promise<void>,
	blockOffset: number = BLOCK_OFFSET
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
	offset.writeUInt32LE(blockOffset);
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
