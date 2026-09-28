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
 * Steps:
 *   1. resize: grow every velocity-owned zero-copy account whose struct gained fields.
 *      `extend_account` resolves the target size from the discriminator, so this step covers
 *      past and future growth the same way. Then set the transaction fee rails when they read
 *      zero. An upgraded `State` reads them from former padding, and every crank priced from
 *      zero rails pays nothing, so no turner takes it. Then create the singletons and the per-market
 *      accounts the new code loads: the relay scratch, the crank treasury, and the quoter slab
 *      of every perp market that predates it.
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
	getClobCrankConditionsPublicKey,
	getCrankTreasuryPublicKey,
	getRelayScratchPublicKey,
	getUserConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getQuoterSlabPublicKey,
	getVelocityStateAccountPublicKey,
	getSpotMarketPublicKeySync,
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

type Args = {
	url: string;
	keypair: string;
	dryRun: boolean;
	limit: number;
	syncCostUnits: number;
	fallbackSlots: bigint;
	feeRails: FeeRails;
};

type FeeRails = {
	inclusionLamports: number;
	signatureLamports: number;
	resourceFeeNumerator: number;
	resourceFeeDenominator: number;
	maxPriorityMicroLamportsPerCu: number;
};

/** `TransactionFeeRails::FLAT_PER_SIGNATURE`, which `initialize` writes on a new deployment. */
const DEFAULT_FEE_RAILS = '0,5000,0,0,0';

function parseFeeRails(raw: string): FeeRails {
	const values = raw.split(',').map((value) => Number.parseInt(value, 10));
	if (
		values.length !== 5 ||
		values.some((value) => !Number.isInteger(value) || value < 0)
	) {
		throw new Error(
			'--fee-rails takes five non-negative integers: inclusion,signature,numerator,denominator,maxPriority'
		);
	}

	const [
		inclusionLamports,
		signatureLamports,
		resourceFeeNumerator,
		resourceFeeDenominator,
		maxPriorityMicroLamportsPerCu,
	] = values;
	return {
		inclusionLamports,
		signatureLamports,
		resourceFeeNumerator,
		resourceFeeDenominator,
		maxPriorityMicroLamportsPerCu,
	};
}

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
		feeRails: parseFeeRails(get('--fee-rails', DEFAULT_FEE_RAILS)),
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

	await setFeeRailsIfUnset(program, payer, statePda, args.feeRails, act);

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
	const clobMarkets = new Set<number>();
	for (const { account } of perpMarkets) {
		// Decode through the IDL rather than by byte offset. Layouts move, and
		// a migration that reads the wrong field is worse than one that fails.
		const decoded: any = program.coder.accounts.decode('perpMarket', account.data);
		marketOracles.set(decoded.marketIndex, decoded.oracle);
		if (!decoded.clobMarket.equals(PublicKey.default)) {
			clobMarkets.add(decoded.marketIndex);
		}
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
	users: { pubkey: PublicKey; account: { data: Buffer } }[],
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

	const vaultDiscriminator = vaultsIdl.accounts.find(
		(account: any) => account.name === 'Vault'
	).discriminator;
	const vaultAccounts: { publicKey: PublicKey; account: any }[] = [];
	for (const { pubkey, account } of await provider.connection.getProgramAccounts(
		vaults.programId,
		{ filters: [{ memcmp: { offset: 0, bytes: bs58(Buffer.from(vaultDiscriminator)) } }] }
	)) {
		try {
			vaultAccounts.push({
				publicKey: pubkey,
				account: vaults.coder.accounts.decode('vault', account.data),
			});
		} catch {
			console.log(`vault ${pubkey.toBase58()}: does not decode under the current layout, skipped`);
		}
	}

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

/** Write `rails` when every rail reads zero, which is the state an upgrade
 * leaves. Rails an operator already set stay. */
async function setFeeRailsIfUnset(
	program: Program,
	payer: Keypair,
	statePda: PublicKey,
	rails: FeeRails,
	act: (label: string, ixs: TransactionInstruction[]) => Promise<void>
) {
	const info = await program.provider.connection.getAccountInfo(statePda);
	const state = program.coder.accounts.decode('state', info!.data) as {
		transactionFeeRails: FeeRails;
	};
	if (
		Object.values(state.transactionFeeRails).some((value) => Number(value) !== 0)
	) {
		console.log('\nfee rails: already set');
		return;
	}

	console.log(`\nfee rails: unset, writing ${JSON.stringify(rails)}`);
	await act('set transaction fee rails', [
		await program.methods
			.updateTransactionFeeRails(rails)
			.accounts({ admin: payer.publicKey, state: statePda })
			.instruction(),
	]);
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
