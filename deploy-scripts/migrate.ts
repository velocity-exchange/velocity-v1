/**
 * Run this after a program upgrade. It applies every on-chain change the new
 * code needs, in dependency order.
 *
 *   bun run deploy-scripts/migrate.ts --url <rpc> --keypair <path> [--dry-run]
 *
 * Steps:
 *   1. resize: grow every velocity-owned zero-copy account whose struct
 *      gained fields. `extend_account` resolves the target size from the
 *      discriminator, so this step covers past and future growth the same
 *      way.
 *   2. liq coverage: create and sync relay liquidation conditions for every
 *      user with exposure. `initialize_user` creates them for a new user.
 *      This step backfills the users that predate that field.
 *   3. watches: register the relay `WatchV0` records that make the blocks
 *      above discoverable. Those are the market crank conditions, the
 *      per-quoter cross conditions, and the per-user liquidation conditions
 *      from step 2.
 *   4. legacy orders: report every order still resting in a `User.orders`
 *      slot that is not an unfired trigger and not a book shadow. Those are
 *      orders from the removed matching venue. Nothing matches them any more,
 *      but each one still holds an `open_bids`/`open_asks` reservation
 *      against its owner's margin, so the sooner the owner cancels it the
 *      sooner that margin comes back.
 *
 *      This step reports and does not cancel. Only the owner or their
 *      delegate can cancel an order, and the keeper sweep
 *      (`force_cancel_orders`) reaches an account only while it is below its
 *      initial margin requirement. A healthy owner's stale order is therefore
 *      theirs to pull.
 *
 * Every step reads on-chain state first and skips what is already correct. A
 * run that stops part way is resumed by running it again.
 *
 * A new migration belongs here as a step, not in a runbook.
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
/** `OrderBitFlag::PlacedOnClob`: the slot shadows an order resting on the book. */
const PLACED_ON_CLOB_BIT = 0b0100_0000;
/** Offset of the relay block in every velocity conditions account, past the
 * anchor discriminator. */
const BLOCK_OFFSET = 8;
/**
 * Bytes that follow `UserConditionsV0.sync_payment_lamports`. They are
 * `sync_fallback_slots`, `positions_digest`, `last_paid_sync_slot`, and the
 * 64-byte tail reserve. The read is measured from the end of the account, so a
 * field added ahead of the payment does not move it.
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

/** Zero-copy accounts `extend_account` can grow, and their sizes in the
 * current build. An account already at or beyond its target is skipped, so a
 * type that never grew is harmless to list. */
const RESIZABLE: { name: string; size: number }[] = [
	{ name: 'User', size: 8 + 4496 },
	{ name: 'perpMarket', size: 8 + 1328 },
	{ name: 'quoterV0', size: 8 + 784 },
	// Relay condition hosts. The `sizes_for_the_migration_script` test in
	// `state/relay_scratch.rs` prints these sizes. Run it and paste the output
	// rather than working the sizes out by hand:
	// `cargo test -p velocity --lib sizes_for_the_migration_script -- --show-output`.
	// A type that is missing or stale here has no symptom until an account is
	// read at the wrong offset.
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

	// 1. resize
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

	// Every resolver names this account. Until it exists, every relay crank in
	// the program fails simulation with an owner error. The account therefore
	// comes before anything that registers a watch.
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
	// exist before either one can run. It is created inert. An operator decides
	// the pricing and the funding with `velocity-admin fees set-crank-treasury`
	// and a SOL transfer.
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

	// 2. liquidation coverage
	const userDisc = discriminator('User');
	const users = await connection.getProgramAccounts(velocity, {
		filters: [{ memcmp: { offset: 0, bytes: bs58(userDisc) } }],
	});
	console.log(`\nliq coverage: ${users.length} user accounts`);
	// The markets and oracles the sync needs, read once.
	const perpMarkets = await connection.getProgramAccounts(velocity, {
		filters: [
			{ memcmp: { offset: 0, bytes: bs58(discriminator('PerpMarket')) } },
		],
	});
	const marketOracles = new Map<number, PublicKey>();
	for (const { account } of perpMarkets) {
		// Decode through the IDL rather than by byte offset. Layouts move, and
		// a migration that reads the wrong field is worse than one that fails.
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
		// `SyncLiqConditionsArgs` is the cost units as a u32, then the fallback
		// interval in slots. The program derives the lamport fee from
		// `State.transactionFeeRails`.
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
			// Fund fifty syncs. The program prices the fee, so read the value
			// the account holds rather than restating it here.
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

	// 3. watches for the market and quoter conditions
	console.log('');
	for (const [marketIndex] of marketOracles) {
		const conditions = getClobCrankConditionsPublicKey(velocity, marketIndex);
		const info = await connection.getAccountInfo(conditions);
		if (!info) continue;
		await ensureWatch(connection, provider, payer, conditions, act);
		// The book hosts the four conditions that describe the book, so it
		// needs a watch of its own. The attach that registered velocity's
		// resolvers recorded where the book's block sits on the conditions
		// account above.
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

	// 4. legacy orders
	reportLegacyOrders(users, program);

	console.log(`\n${args.dryRun ? 'would run' : 'ran'} ${plan.length} steps`);
	for (const line of plan.slice(0, 40)) console.log(`  ${line}`);
	if (plan.length > 40) console.log(`  … ${plan.length - 40} more`);
}

/** The book account the perp market names. */
async function clobBookFor(
	connection: Connection,
	velocity: PublicKey,
	marketIndex: number,
	program: { coder: { accounts: { decode(name: string, data: Buffer): unknown } } }
): Promise<PublicKey | undefined> {
	const perpMarket = getPerpMarketPublicKeySync(velocity, marketIndex);
	const marketInfo = await connection.getAccountInfo(perpMarket);
	if (!marketInfo) return undefined;
	const { clobMarket } = program.coder.accounts.decode(
		'perpMarket',
		marketInfo.data
	) as { clobMarket: PublicKey };
	if (!clobMarket || clobMarket.equals(PublicKey.default)) return undefined;
	return new PublicKey(clobMarket);
}

/** Register a relay watch over a conditions block, unless the registry
 * already holds a watch on that target. */
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
	// `WatchV0` holds `target_program` and then `target`, so a memcmp finds
	// every watch on a target without decoding the account.
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
	// eslint-disable-next-line @typescript-eslint/no-var-requires
	const bs58lib = require('bs58');
	return bs58lib.default ? bs58lib.default.encode(buffer) : bs58lib.encode(buffer);
}

/**
 * Orders left in `User.orders` that the removed matching venue placed.
 *
 * An unfired trigger belongs in a slot: that is what the array is for now. A
 * book shadow belongs there too, because the slot is how a resting book order
 * is cancelled and how its reservation is released. Everything else that is
 * still open is a legacy order that nothing will fill.
 */
function reportLegacyOrders(
	users: readonly { pubkey: PublicKey; account: { data: Buffer } }[],
	program: { coder: { accounts: { decode(name: string, data: Buffer): any } } }
): void {
	const stranded: { user: PublicKey; count: number }[] = [];
	let total = 0;
	for (const { pubkey, account } of users) {
		const decoded = program.coder.accounts.decode('user', account.data);
		const count = (decoded.orders ?? []).filter((order: any) => {
			if (!order.status?.open) return false;
			const type = order.orderType ?? {};
			const isTrigger = type.triggerMarket || type.triggerLimit;
			// `triggered()` on chain: either of the two fired bits is set.
			const fired =
				order.triggerCondition?.triggeredAbove ||
				order.triggerCondition?.triggeredBelow;
			if (isTrigger && !fired) return false;
			// A book shadow keeps its slot so the owner can still cancel it.
			return !order.bitFlags || (order.bitFlags & PLACED_ON_CLOB_BIT) === 0;
		}).length;
		if (count === 0) continue;
		stranded.push({ user: pubkey, count });
		total += count;
	}

	console.log(`\nlegacy orders: ${total} across ${stranded.length} accounts`);
	if (total === 0) return;
	console.log('  each still reserves margin until its owner cancels it');
	for (const { user, count } of stranded.slice(0, 40)) {
		console.log(`  ${user.toBase58()}: ${count}`);
	}
	if (stranded.length > 40) {
		console.log(`  … ${stranded.length - 40} more accounts`);
	}
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
