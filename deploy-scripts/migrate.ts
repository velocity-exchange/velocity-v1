/**
 * allow-verbose: this is the operational runbook an operator follows after every program
 * upgrade. The step order and the report-not-cancel design in step 4 are safety decisions;
 * cutting this to a summary is how someone later "fixes" step 4 into an unsafe auto-cancel.
 *
 * Run this after a program upgrade. It applies every on-chain change the new code needs, in
 * dependency order.
 *
 *   bun run deploy-scripts/migrate.ts --url <rpc> --keypair <path> [--dry-run]
 *
 * The keypair must hold the warm or cold admin role. Before anything is sent, the script
 * refuses to run unless the CLOB program is deployed at the id velocity pins, and unless
 * `State.transactionFeeRails` prices a crank above zero. With zero rails every crank
 * payment and every sync payment is zero, and relay turners take none of that work.
 *
 * Steps:
 *   1. resize: grow every velocity-owned zero-copy account whose struct gained fields.
 *      `extend_account` resolves the target size from the discriminator, so this step covers
 *      past and future growth the same way. Then create the singletons and the per-market
 *      accounts the new code loads: the relay scratch, the crank treasury, and the quoter slab
 *      of every perp market that predates it.
 *
 *      Then give every perp market that has no book its CLOB book. Every order path requires
 *      the market's book account, so a market without one takes no order. The bring-up
 *      creates the book, registers and approves its quoter entry, and attaches it, which
 *      creates the market's crank conditions. It stops when the crank treasury is not
 *      priced, because the attach stores the treasury's refill watermark on the market.
 *      This comes before step 2, so a trigger that step 2 arms has a book to fire into.
 *   2. liq coverage: create and sync relay liquidation conditions for every user with exposure,
 *      backfilling users that predate `initialize_user` creating them automatically. The same
 *      `sync_user_conditions` call arms the user's trigger orders, because relay is the only
 *      executor that fires one.
 *   3. watches: register the relay `WatchV0` records that make the blocks from steps 1 and 2
 *      discoverable: the market crank conditions, the per-quoter cross conditions, and the
 *      per-user liquidation conditions.
 *   4. legacy orders: report every order still resting in a `User.orders` slot that is not an
 *      unfired trigger and not a book shadow. These are orders from the removed matching venue.
 *      Each still holds an `open_bids`/`open_asks` reservation against its owner's margin, so
 *      the sooner the owner cancels it the sooner that margin returns.
 *
 *      This step reports and does not cancel. Only the owner or their delegate can cancel an
 *      order, and the keeper sweep (`force_cancel_orders`) reaches an account only while it is
 *      below its initial margin requirement. A healthy owner's stale order is therefore theirs
 *      to pull.
 *   5. vault users: flag each vault's velocity User as vault-owned when the vault predates
 *      vault initialization setting that flag. Only the vault manager or the vaults admin can
 *      sign, so a vault the keypair cannot sign for is reported.
 *
 * Every step reads on-chain state first and skips what is already correct, so a run that stops
 * part way is resumed by running it again. A new migration belongs here as a step, not in a
 * runbook.
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
	BASE_PRECISION,
	decodeQuoterSlab,
	getClobCrankConditionsPublicKey,
	getCrankTreasuryPublicKey,
	getProgramDataAddress,
	getQuoterPublicKey,
	getRelayScratchPublicKey,
	getUserConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getQuoterSlabPublicKey,
	getVelocityStateAccountPublicKey,
	getSpotMarketPublicKeySync,
	quoterConfigHash,
	UserStatus,
	positionIsAvailable,
	Wallet,
} from '@velocity-exchange/sdk';

const RELAY_PROGRAM = new PublicKey(
	process.env.RELAY_PROGRAM_ID ?? '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu'
);
const VAULTS_ADMIN = new PublicKey(
	process.env.VAULTS_ADMIN ?? 'GiMXQkJXLVjScmQDkoLJShBJpTh9SDPvT2AZQq8NyEBf'
);
const WATCH_V0_LEN = 112;
/** `OrderBitFlag::PlacedOnClob`: the slot shadows an order resting on the book. */
const PLACED_ON_CLOB_BIT = 0b0100_0000;
/** Offset of the relay block in every velocity conditions account, past the
 * anchor discriminator. */
const BLOCK_OFFSET = 8;
/**
 * Bytes that follow `UserConditionsV0.sync_payment_lamports`: `sync_fallback_slots`,
 * `positions_digest`, `last_paid_sync_slot`, and the 64-byte tail reserve, measured from the account's end so a field added ahead of the payment does not move it.
 */
const BYTES_AFTER_SYNC_PAYMENT = 8 + 8 + 72;
/** `clob_state::ORDERS_OFFSET` and `clob_state::NODE_BYTES`: a book is its header, then one node per order. */
const CLOB_ORDERS_OFFSET = 9648;
const CLOB_NODE_BYTES = 104;
/** `BOOK_BLOCKING_FLOOR_MIN_ORDERS` in velocity. The attach refuses a lower blocking floor. */
const BOOK_BLOCKING_FLOOR_MIN_ORDERS = 10;

type Args = {
	url: string;
	keypair: string;
	dryRun: boolean;
	limit: number;
	syncCostUnits: number;
	fallbackSlots: bigint;
	bookCapacity: number;
	crankCostUnits: number;
	expireFallbackSlots: number;
	minCrossSurplus: number;
};

type Act = (
	label: string,
	ixs: TransactionInstruction[],
	signers?: Keypair[]
) => Promise<void>;

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
		// The CLOB refuses more than 512 orders a side.
		bookCapacity: Number.parseInt(get('--book-capacity', '1024'), 10),
		// The admin CLI's ceiling for a crank nobody has measured yet.
		crankCostUnits: Number.parseInt(get('--crank-cu', '250000'), 10),
		expireFallbackSlots: Number.parseInt(
			get('--expire-fallback-slots', '1500'),
			10
		),
		minCrossSurplus: Number.parseInt(get('--min-cross-surplus', '10000'), 10),
	};
}

/**
 * An account's anchor discriminator, hashed from its Rust type name. The name must match the
 * IDL exactly, or `getProgramAccounts` silently returns nothing. `assertNamesAreReal` guards against that.
 */
function discriminator(name: string): Buffer {
	return createHash('sha256')
		.update(`account:${name}`)
		.digest()
		.subarray(0, 8);
}

/**
 * Fail before sending anything when a name does not appear in the IDL. A
 * mistyped name has no symptom otherwise: the resize silently covers no
 * accounts, and the upgrade lands against accounts too small for the layout
 * that now reads them.
 */
function assertNamesAreReal(idl: any, names: string[]): void {
	const known = new Set<string>(
		(idl?.accounts ?? []).map((account: any) => account.name)
	);
	const unknown = names.filter((name) => !known.has(name));
	if (unknown.length > 0) {
		throw new Error(
			`these account names are not in the IDL: ${unknown.join(', ')}. ` +
				`The name must be the Rust type name, which is what the discriminator hashes.`
		);
	}
}

function ixDiscriminator(name: string): Buffer {
	return createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);
}

/** Zero-copy accounts `extend_account` can grow, with their `SIZE`, which
 * includes the discriminator. An account at or past its size is skipped. The
 * unit test `the_migration_script_resizes_to_the_real_sizes` parses these
 * entries, so keep each one on one line with a plain number. */
const RESIZABLE: { name: string; size: number }[] = [
	{ name: 'User', size: 4496 },
	{ name: 'PerpMarket', size: 1560 },
	{ name: 'QuoterV0', size: 792 },
	// Relay condition hosts. Sizes come from `cargo test -p velocity --lib
	// sizes_for_the_migration_script -- --show-output` in `state/relay_scratch.rs`. A stale or missing entry here has no symptom until read at the wrong offset.
	{ name: 'ClobCrankConditionsV0', size: 808 },
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

	const clobProgram = clobProgramId(idl);
	await assertClobDeployed(connection, clobProgram);
	await assertCranksPriced(connection, program, statePda);

	// 1. resize
	// `program.idl` is camelCased by the Anchor client; `idl` is the raw JSON,
	// which keeps the Rust type names the discriminator hashes.
	assertNamesAreReal(
		idl,
		RESIZABLE.map(({ name }) => name)
	);

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

	const perpMarkets = await connection.getProgramAccounts(velocity, {
		filters: [
			{ memcmp: { offset: 0, bytes: bs58(discriminator('PerpMarket')) } },
		],
	});
	const marketOracles = new Map<number, PublicKey>();
	const decodedPerpMarkets = new Map<number, any>();
	for (const { account } of perpMarkets) {
		// Decode through the IDL rather than by byte offset. Layouts move, and
		// a migration that reads the wrong field is worse than one that fails.
		const decoded: any = program.coder.accounts.decode('perpMarket', account.data);
		marketOracles.set(decoded.marketIndex, decoded.oracle);
		decodedPerpMarkets.set(decoded.marketIndex, decoded);
	}

	const spotMarkets = await connection.getProgramAccounts(velocity, {
		filters: [
			{ memcmp: { offset: 0, bytes: bs58(discriminator('SpotMarket')) } },
		],
	});
	const spotMarketOracles = new Map<number, PublicKey>();
	for (const { account } of spotMarkets) {
		const decoded: any = program.coder.accounts.decode('spotMarket', account.data);
		spotMarketOracles.set(decoded.marketIndex, decoded.oracle);
	}

	console.log('');
	await createMissingQuoterSlabs(
		connection,
		program,
		payer,
		[...marketOracles.keys()],
		act
	);

	await assertTreasuryPriced(connection, program, treasury, args.dryRun);
	const clobMarkets = await bringUpBooks(
		{ connection, program, payer, state: statePda, clobProgram, args, act },
		decodedPerpMarkets
	);

	// 2. liquidation coverage
	const userDisc = discriminator('User');
	const users = await connection.getProgramAccounts(velocity, {
		filters: [{ memcmp: { offset: 0, bytes: bs58(userDisc) } }],
	});

	console.log(`\nliq coverage: ${users.length} user accounts`);

	let covered = 0;
	for (const { pubkey: user, account } of users) {
		if (args.limit && covered >= args.limit) break;
		const decodedUser: any = program.coder.accounts.decode('user', account.data);
		const marketIndexes = exposedPerpMarkets(decodedUser);
		if (marketIndexes.length === 0) continue;
		const userConditions = getUserConditionsPublicKey(velocity, user);
		const existing = await connection.getAccountInfo(userConditions);
		// Market 0 rides along unconditionally: liquidation settles quote PnL
		// against it. `validate_market_coverage` requires every other spot
		// market the user holds a position in.
		const spotMarketIndexes = new Set<number>([
			0,
			...exposedSpotMarkets(decodedUser),
		]);
		// The sync refuses an account passed twice, so markets that share an
		// oracle pass it once.
		const oracles = new Map<string, PublicKey>();
		for (const marketIndex of marketIndexes) {
			const oracle = marketOracles.get(marketIndex);
			if (oracle) oracles.set(oracle.toBase58(), oracle);
		}
		for (const marketIndex of spotMarketIndexes) {
			const oracle = spotMarketOracles.get(marketIndex);
			if (oracle && !oracle.equals(PublicKey.default)) {
				oracles.set(oracle.toBase58(), oracle);
			}
		}

		const syncAccounts: AccountMeta[] = [...oracles.values()].map((oracle) => ({
			pubkey: oracle,
			isSigner: false,
			isWritable: false,
		}));
		// The sync refuses an account it cannot classify, and a market with no
		// CLOB has no crank conditions account. A market with a CLOB must bring
		// its quoter slab.
		const bookMarkets = marketIndexes.filter((marketIndex) => clobMarkets.has(marketIndex));

		syncAccounts.push(
			...[...spotMarketIndexes].map((marketIndex) => ({
				pubkey: getSpotMarketPublicKeySync(velocity, marketIndex),
				isSigner: false,
				isWritable: true,
			})),
			...marketIndexes.map((marketIndex) => ({
				pubkey: getPerpMarketPublicKeySync(velocity, marketIndex),
				isSigner: false,
				isWritable: true,
			})),
			...bookMarkets.map((marketIndex) => ({
				pubkey: getClobCrankConditionsPublicKey(velocity, marketIndex),
				isSigner: false,
				isWritable: false,
			})),
			...bookMarkets.map((marketIndex) => ({
				pubkey: getQuoterSlabPublicKey(velocity, marketIndex),
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
			`${existing ? 'sync' : 'create+sync'} user conditions for ${user.toBase58()}`,
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
						ixDiscriminator('sync_user_conditions'),
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

	// 5. vault users
	await flagVaultUsers(provider, payer, users, program, act);

	console.log(`\n${args.dryRun ? 'would run' : 'ran'} ${plan.length} steps`);
	for (const line of plan.slice(0, 40)) console.log(`  ${line}`);
	if (plan.length > 40) console.log(`  … ${plan.length - 40} more`);
}

/**
 * Without the vault-owned flag, the revenue-share sweep can credit a vault's User. That
 * dilutes the vault's depositors.
 */
async function flagVaultUsers(
	provider: AnchorProvider,
	payer: Keypair,
	users: readonly { pubkey: PublicKey; account: { data: Buffer } }[],
	program: Program,
	act: (label: string, ixs: TransactionInstruction[]) => Promise<void>
) {
	const vaultsIdl = JSON.parse(
		fs.readFileSync('packages/vaults-sdk/src/idl/vaults.json', 'utf-8')
	);
	const vaults = new Program(vaultsIdl, provider);
	const statusByUser = new Map<string, number>(
		users.map(({ pubkey, account }) => [
			pubkey.toBase58(),
			(program.coder.accounts.decode('user', account.data) as any).status,
		])
	);

	const vaultAccounts = await (vaults.account as any).vault.all();
	let unflagged = 0;
	for (const { publicKey: vault, account } of vaultAccounts) {
		const status = statusByUser.get(account.user.toBase58());
		if (status === undefined || (status & UserStatus.VAULT_OWNED) !== 0) {
			continue;
		}

		unflagged += 1;
		const canSign =
			payer.publicKey.equals(account.manager) ||
			payer.publicKey.equals(VAULTS_ADMIN);
		if (!canSign) {
			console.log(
				`vault ${vault.toBase58()}: unflagged, needs manager ${account.manager.toBase58()} or the vaults admin`
			);
			continue;
		}

		await act(`flag vault user ${account.user.toBase58()}`, [
			await vaults.methods
				.markUserVaultOwned()
				.accounts({
					vault,
					authority: payer.publicKey,
					velocityUser: account.user,
				})
				.instruction(),
		]);
	}

	console.log(
		`\nvault users: ${vaultAccounts.length} vaults, ${unflagged} unflagged`
	);
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

/**
 * Every order path loads the market's quoter slab. A market created before the
 * slab existed has none, and its `quoterSlab` field reads as the default key.
 * `initialize_quoter_slab` creates the slab and stores it on the market.
 */
async function createMissingQuoterSlabs(
	connection: Connection,
	program: Program,
	payer: Keypair,
	marketIndexes: number[],
	act: (label: string, ixs: TransactionInstruction[]) => Promise<void>
) {
	const velocity = program.programId;
	for (const marketIndex of marketIndexes) {
		const quoterSlab = getQuoterSlabPublicKey(velocity, marketIndex);
		if (await connection.getAccountInfo(quoterSlab)) {
			console.log(`quoter slab market ${marketIndex}: already created`);
			continue;
		}

		console.log(`quoter slab market ${marketIndex}: creating`);
		await act(`create quoter slab for market ${marketIndex}`, [
			await program.methods
				.initializeQuoterSlab({ marketIndex })
				.accounts({
					payer: payer.publicKey,
					perpMarket: getPerpMarketPublicKeySync(velocity, marketIndex),
					quoterSlab,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				})
				.instruction(),
		]);
	}
}

/** The CLOB program id velocity pins, as the IDL records it on the attach. */
function clobProgramId(idl: any): PublicKey {
	const attach = (idl.instructions ?? []).find(
		(ix: any) => ix.name === 'update_perp_market_clob_quoter'
	);
	const address = attach?.accounts?.find(
		(account: any) => account.name === 'clob_program'
	)?.address;
	if (!address) {
		throw new Error('the IDL does not pin a clob_program on the attach');
	}

	return new PublicKey(address);
}

async function assertClobDeployed(
	connection: Connection,
	clobProgram: PublicKey
): Promise<void> {
	const info = await connection.getAccountInfo(clobProgram);
	if (!info?.executable) {
		throw new Error(
			`the CLOB program ${clobProgram.toBase58()} is not deployed. Every order path ` +
				`requires a book, so deploy the CLOB before the velocity upgrade and this migration.`
		);
	}

	console.log(`clob ${clobProgram.toBase58()}: deployed`);
}

/** Mirrors `TransactionFeeRails::transaction_cost` for one signature: zero only
 * when no rail charges anything. */
async function assertCranksPriced(
	connection: Connection,
	program: Program,
	statePda: PublicKey
): Promise<void> {
	const info = await connection.getAccountInfo(statePda);
	if (!info) throw new Error(`state ${statePda.toBase58()} not found`);
	const rails = (program.coder.accounts.decode('state', info.data) as any)
		.transactionFeeRails;
	const chargesCostUnits =
		rails.resourceFeeNumerator > 0 && rails.resourceFeeDenominator > 0;
	if (
		rails.inclusionLamports === 0 &&
		rails.signatureLamports === 0 &&
		!chargesCostUnits
	) {
		throw new Error(
			'State.transactionFeeRails prices every crank at zero, and relay turners take no unpaid ' +
				'work. Set them first: velocity-admin fees set-transaction-rails <inclusionLamports> ' +
				'<signatureLamports> <resourceFeeNumerator> <resourceFeeDenominator> <maxPriorityMicroLamportsPerCu>'
		);
	}

	console.log(`fee rails: ${JSON.stringify(rails)}`);
}

/** The attach stores the treasury's refill watermark on the market, so an
 * inert treasury leaves the market's reservoir with no refill. */
async function assertTreasuryPriced(
	connection: Connection,
	program: Program,
	treasury: PublicKey,
	dryRun: boolean
): Promise<void> {
	const info = await connection.getAccountInfo(treasury);
	if (!info && dryRun) {
		console.log('\ntreasury: not created yet; the book step needs it priced');
		return;
	}

	const decoded: any = info
		? program.coder.accounts.decode('crankTreasuryV0', info.data)
		: undefined;
	if (!decoded?.refillTargetCranks || !decoded?.refillWatermarkCranks) {
		throw new Error(
			`the crank treasury ${treasury.toBase58()} is not priced. Run velocity-admin fees ` +
				`set-crank-treasury <refillTargetCranks> <refillWatermarkCranks>, fund it with SOL, ` +
				`and run this migration again.`
		);
	}

	console.log(
		`\ntreasury: refills to ${decoded.refillTargetCranks} cranks at ${decoded.refillWatermarkCranks}, holds ${info?.lamports} lamports`
	);
}

type BookBringUp = {
	connection: Connection;
	program: Program;
	payer: Keypair;
	state: PublicKey;
	clobProgram: PublicKey;
	args: Args;
	act: Act;
};

/**
 * Give every perp market its book. Returns the markets that have one. A dry run
 * sends nothing, so it returns only the books that already exist.
 */
async function bringUpBooks(
	ctx: BookBringUp,
	perpMarkets: Map<number, any>
): Promise<Set<number>> {
	console.log('');
	for (const [marketIndex, market] of perpMarkets) {
		await bringUpBook(ctx, marketIndex, market);
	}

	return new Set<number>(
		[...perpMarkets].flatMap(([marketIndex, market]) =>
			ctx.args.dryRun && market.clobMarket.equals(PublicKey.default)
				? []
				: [marketIndex]
		)
	);
}

/**
 * Give a perp market its CLOB book, as `velocity-admin clob-market init`
 * does. Each stage reads chain first, so a run that stops part way resumes.
 * The book account and its quoter registration land in one transaction, because
 * the book is a fresh keypair and the market names it once.
 */
async function bringUpBook(
	ctx: BookBringUp,
	marketIndex: number,
	market: any
): Promise<void> {
	const { connection, program, clobProgram, args, act } = ctx;
	const velocity = program.programId;
	const quoter = getQuoterPublicKey(
		velocity,
		marketIndex,
		clobProgram,
		PublicKey.default
	);

	let book: PublicKey = market.clobMarket;
	if (book.equals(PublicKey.default)) {
		const bookKeypair = Keypair.generate();
		book = bookKeypair.publicKey;
		console.log(`book market ${marketIndex}: creating ${book.toBase58()}`);
		await act(
			`create and register book ${book.toBase58()} for market ${marketIndex}`,
			await designateBookIxs(ctx, marketIndex, market, book, quoter),
			[bookKeypair]
		);
	} else if (!(await connection.getAccountInfo(quoter))) {
		throw new Error(
			`perp market ${marketIndex} names book ${book.toBase58()} but quoter entry ` +
				`${quoter.toBase58()} does not exist. Finish that bring-up by hand.`
		);
	}

	const slabInfo = await connection.getAccountInfo(
		getQuoterSlabPublicKey(velocity, marketIndex)
	);
	const approved =
		slabInfo !== null &&
		decodeQuoterSlab(slabInfo.data).slots.some((slot) =>
			slot.entry.equals(quoter)
		);
	if (!approved) {
		// The approval carries the hash of the staged entry, so it is read after
		// the registration lands. A dry run has no entry to read.
		const quoterInfo = await connection.getAccountInfo(quoter);
		if (!quoterInfo && !args.dryRun) {
			throw new Error(`quoter entry ${quoter.toBase58()} did not land`);
		}

		console.log(`book market ${marketIndex}: approving ${quoter.toBase58()}`);
		await act(
			`approve book quoter ${quoter.toBase58()} for market ${marketIndex}`,
			quoterInfo
				? [await approveBookIx(ctx, marketIndex, book, quoter, quoterInfo.data)]
				: []
		);
	}

	const conditions = getClobCrankConditionsPublicKey(velocity, marketIndex);
	if (await connection.getAccountInfo(conditions)) {
		console.log(`book market ${marketIndex}: attached`);
		return;
	}

	console.log(`book market ${marketIndex}: attaching`);
	await act(`attach book for market ${marketIndex}`, [
		await attachBookIx(ctx, marketIndex, book, quoter),
	]);
}

/** Create the book, initialize it on the CLOB with the market's slab as both
 * authorities, register its quoter entry, and set the entry's account list. */
async function designateBookIxs(
	ctx: BookBringUp,
	marketIndex: number,
	market: any,
	book: PublicKey,
	quoter: PublicKey
): Promise<TransactionInstruction[]> {
	const { connection, program, payer, clobProgram, args } = ctx;
	const velocity = program.programId;
	const quoterSlab = getQuoterSlabPublicKey(velocity, marketIndex);
	const space = CLOB_ORDERS_OFFSET + args.bookCapacity * CLOB_NODE_BYTES;

	const createBook = SystemProgram.createAccount({
		fromPubkey: payer.publicKey,
		newAccountPubkey: book,
		lamports: await connection.getMinimumBalanceForRentExemption(space),
		space,
		programId: clobProgram,
	});

	const initBook = new TransactionInstruction({
		programId: clobProgram,
		keys: [
			{ pubkey: quoterSlab, isSigner: false, isWritable: false },
			{ pubkey: quoterSlab, isSigner: false, isWritable: false },
			{ pubkey: book, isSigner: true, isWritable: true },
		],

		data: Buffer.concat([
			ixDiscriminator('initialize_market_v0'),
			bookConfig(marketIndex, market, args.bookCapacity),
		]),
	});

	const registerQuoter = await program.methods
		.initializeQuoter({
			marketIndex,
			quoterType: { clob: {} },
			responseAccount: book,
			quoteV0Discriminator: Array.from(ixDiscriminator('quote_v0')),
			quoteL3V0Discriminator: Array.from(ixDiscriminator('quote_l3_v0')),
			executeV0Discriminator: Array.from(ixDiscriminator('execute_v0')),
		})
		.accountsStrict({
			payer: payer.publicKey,
			authority: payer.publicKey,
			quoter,
			perpMarket: getPerpMarketPublicKeySync(velocity, marketIndex),
			state: ctx.state,
			quoterSlab,
			clobMarket: book,
			quoterProgram: clobProgram,
			user: PublicKey.default,
			rent: SYSVAR_RENT_PUBKEY,
			systemProgram: SystemProgram.programId,
		})
		.instruction();

	// The quote legs read the book. The execute leg also carries the slab,
	// whose signature the book checks.
	const registerAccounts = await program.methods
		.updateQuoterAccounts({
			metas: [
				{ pubkey: book, isWritable: true },
				{ pubkey: quoterSlab, isWritable: false },
			],
			quoteIndexes: Buffer.from([0]),
			executeIndexes: Buffer.from([0, 1]),
		})
		.accountsStrict({
			authority: payer.publicKey,
			quoter,
			state: ctx.state,
		})
		.instruction();

	return [createBook, initBook, registerQuoter, registerAccounts];
}

/**
 * Borsh `MarketConfigV0`. The book takes the market's grid, because the attach
 * requires the book's tick and step to equal the market's and its minimum to
 * sit at or under the market's. The remaining settings are the admin CLI's
 * defaults.
 */
function bookConfig(marketIndex: number, market: any, capacity: number): Buffer {
	const step: BN = market.orderStepSize;
	const marketMinimum: BN = market.marketStats.minOrderSize;
	const bookMinimum = marketMinimum.isZero()
		? step
		: marketMinimum.div(step).mul(step);
	if (bookMinimum.isZero()) {
		throw new Error(
			`perp market ${marketIndex}: minimum order size ${marketMinimum} is under its step ${step}`
		);
	}

	const blockingFloor = BN.max(bookMinimum, marketMinimum).muln(
		BOOK_BLOCKING_FLOOR_MIN_ORDERS
	);
	const u16 = (v: number) => new BN(v).toArrayLike(Buffer, 'le', 2);
	const u32 = (v: number) => new BN(v).toArrayLike(Buffer, 'le', 4);
	const u64 = (v: BN) => v.toArrayLike(Buffer, 'le', 8);
	return Buffer.concat([
		u16(marketIndex),
		u64(BASE_PRECISION),
		u64(market.orderTickSize),
		u64(step),
		u64(bookMinimum),
		u64(blockingFloor),
		u32(1), // default_activation_delay_slots
		u32(20), // max_activation_delay_slots
		u32(2), // unknown_user_grace_slots
		// The CLOB requires the eviction cap under half the arena.
		u32(Math.floor(capacity / 4)),
		u16(128), // max_quote_levels
		u16(64), // max_execute_fills
		u16(32), // max_execute_users
	]);
}

async function approveBookIx(
	ctx: BookBringUp,
	marketIndex: number,
	book: PublicKey,
	quoter: PublicKey,
	quoterData: Buffer
): Promise<TransactionInstruction> {
	const { program, payer, clobProgram } = ctx;
	const velocity = program.programId;
	return await program.methods
		.updateQuoterApproved({
			approved: true,
			stagedConfigHash: quoterConfigHash(quoterData),
		})
		.accountsStrict({
			admin: payer.publicKey,
			state: ctx.state,
			quoter,
			perpMarket: getPerpMarketPublicKeySync(velocity, marketIndex),
			quoterSlab: getQuoterSlabPublicKey(velocity, marketIndex),
			quoterProgram: clobProgram,
			quoterProgramData: getProgramDataAddress(clobProgram),
			clobMarket: book,
			responseAccount: book,
			systemProgram: SystemProgram.programId,
		})
		.instruction();
}

/** The attach prices every crank from the rails and the one cost-unit figure
 * the run was given, and creates the market's crank conditions. */
async function attachBookIx(
	ctx: BookBringUp,
	marketIndex: number,
	book: PublicKey,
	quoter: PublicKey
): Promise<TransactionInstruction> {
	const { program, payer, clobProgram, args } = ctx;
	const velocity = program.programId;
	const units = args.crankCostUnits;
	return await program.methods
		.updatePerpMarketClobQuoter({
			crankCostUnits: {
				removal: units,
				cross: units,
				takerOriginCross: units,
				trigger: units,
				liquidation: units,
				forceCancel: units,
				refill: units,
			},
			expireFallbackSlots: new BN(args.expireFallbackSlots),
			minCrossSurplus: new BN(args.minCrossSurplus),
		})
		.accountsStrict({
			admin: payer.publicKey,
			state: ctx.state,
			perpMarket: getPerpMarketPublicKeySync(velocity, marketIndex),
			quoter,
			quoterSlab: getQuoterSlabPublicKey(velocity, marketIndex),
			clobMarket: book,
			clobProgram,
			crankConditions: getClobCrankConditionsPublicKey(velocity, marketIndex),
			treasury: getCrankTreasuryPublicKey(velocity),
			rent: SYSVAR_RENT_PUBKEY,
			systemProgram: SystemProgram.programId,
		})
		.instruction();
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

/** The perp markets the sync requires. That is every position the program
 * does not read as available, which includes one that holds only trigger
 * orders. */
function exposedPerpMarkets(user: any): number[] {
	const markets: number[] = (user.perpPositions ?? [])
		.filter((position: any) => !positionIsAvailable(position))
		.map((position: any) => position.marketIndex);

	return [...new Set(markets)];
}

/** Spot markets the user holds a balance or an open order in, matching the
 * program's own `SpotPosition::is_available`. */
function exposedSpotMarkets(user: any): number[] {
	const markets: number[] = [];
	for (const position of user.spotPositions ?? []) {
		const balance = new BN(position.scaledBalance ?? 0);
		if (!balance.isZero() || (position.openOrders ?? 0) !== 0) {
			markets.push(position.marketIndex);
		}
	}

	return [...new Set(markets)];
}

main().catch((err) => {
	console.error(err);
	process.exit(1);
});
