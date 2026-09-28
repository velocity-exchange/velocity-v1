/**
 * allow-verbose: this is the operational runbook an operator follows after every program
 * upgrade. The step order and the report-not-cancel design in step 4 are safety decisions;
 * cutting this to a summary is how someone later "fixes" step 4 into an unsafe auto-cancel.
 *
 * Run this after a program upgrade. It applies every on-chain change the new code needs, in
 * dependency order.
 *
 *   bun run deploy-scripts/migrate.ts --url <rpc> --keypair <path> \
 *     [--multisig <pda> [--vault-index <n>]] [--fee-rails <i,s,num,den,maxPriority>] [--dry-run]
 *
 * The keypair is the payer and can be any funded key. The admin is the keypair, or with
 * `--multisig` the multisig's vault, and it must hold the warm or cold role. Before anything is
 * sent, the script refuses to run unless the CLOB program is deployed at the id velocity pins.
 * When `State.transactionFeeRails` prices every crank at zero, it writes `--fee-rails` first,
 * which defaults to `TransactionFeeRails::FLAT_PER_SIGNATURE`. With zero rails every crank
 * payment and every sync payment is zero, and relay turners take none of that work.
 *
 * Without `--multisig`, every instruction sends directly. With it, every warm-admin instruction
 * becomes a Squads proposal, and the vault pays that instruction's rent: the crank treasury, and
 * each book's quoter entry, slab slot and crank conditions. The payer still sends what needs no
 * admin. With the `conditionsSync` hot role it also syncs every user directly and pays for the user
 * conditions accounts. It must hold the `accountExtension` hot role when an account needs a resize. A book signs its own creation and is too large to create inside a vault
 * transaction, so the payer creates it and the multisig registers it later. A market's bring-up
 * then takes two proposals, because the approval carries the hash of the entry the first one
 * stages. Its users sync only after both execute. So run, approve and execute the proposals, and
 * run again, until a run sends and proposes nothing. A rerun does not propose a step again when a
 * pending proposal among the last `--proposal-scan` (256) already does it. It also reuses an empty
 * book from an earlier run whose registration still waits.
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
 * Each run ends with what it sent, what it proposed, and what waits on an earlier proposal.
 *
 * Every step reads on-chain state first and skips what is already correct, so a run that stops
 * part way is resumed by running it again. A new migration belongs here as a step, not in a
 * runbook.
 */
import { createHash } from 'crypto';
import * as fs from 'fs';
import {
	AccountInfo,
	AccountMeta,
	Connection,
	Keypair,
	PublicKey,
	SystemProgram,
	SYSVAR_RENT_PUBKEY,
	Transaction,
	TransactionInstruction,
	TransactionMessage,
} from '@solana/web3.js';
import { AnchorProvider, BN, Program } from '@coral-xyz/anchor';
import * as multisig from '@sqds/multisig';
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
import {
	PROPOSAL_TX_LIMIT,
	proposalTransactionSize,
	resolveAdminAuthority,
	sendOrPropose,
	setDryRun,
} from '../packages/cli-admin/src/lib/squads';

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
/** `ClobHeaderV0` offsets, past the discriminator. */
const CLOB_AUTHORITY_OFFSET = 8;
const CLOB_PLACE_AUTHORITY_OFFSET = 40;
const CLOB_BID_COUNT_OFFSET = 144;
const CLOB_ASK_COUNT_OFFSET = 148;
/** `MarketConfigV0` in wire order, as `[offset, width]` into the header. */
const CLOB_CONFIG_FIELDS: [number, number][] = [
	[168, 2], // market_index
	[104, 8], // base_precision
	[72, 8], // order_tick_size
	[80, 8], // order_step_size
	[88, 8], // min_order_size
	[96, 8], // blocking_min_size
	[152, 4], // default_activation_delay_slots
	[156, 4], // max_activation_delay_slots
	[160, 4], // unknown_user_grace_slots
	[164, 4], // evict_threshold_per_side
	[170, 2], // max_quote_levels
	[172, 2], // max_execute_fills
	[174, 2], // max_execute_users
];
const CLOB_HEADER_PREFIX_BYTES = 176;
/** Sizes of what the admin pays rent for in a book bring-up and a sync:
 * `QuoterV0::SIZE`, one `QuoterSlotV0` of slab growth,
 * `ClobCrankConditionsV0::SIZE` and `UserConditionsV0::SIZE`. */
const QUOTER_V0_BYTES = 792;
const QUOTER_SLAB_SLOT_BYTES = 776;
const CRANK_CONDITIONS_BYTES = 808;
const USER_CONDITIONS_BYTES = 6040;
/** Squads proposal statuses that can still execute. An `Active` one past the
 * multisig's stale index cannot, and `PendingProposals` drops it. */
const PENDING_PROPOSAL_STATUSES = ['Draft', 'Active', 'Approved', 'Executing'];
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
	multisig?: PublicKey;
	vaultIndex: number;
	proposalScan: number;
	feeRails: FeeRails;
};

type Act = (
	label: string,
	ixs: TransactionInstruction[],
	signers?: Keypair[]
) => Promise<void>;

/** What one run did. A proposal lands only when the multisig executes it, so
 * its follow-up steps run on the next run. */
type RunReport = {
	sent: string[];
	proposed: string[];
	awaiting: string[];
};

/** Instructions of a pending proposal that already do a step: any instruction
 * with one of these discriminators that names `account`. */
type ProposalMatch = { discriminators: Buffer[]; account: PublicKey };

type DispatchOutcome = 'sent' | 'awaiting';

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
		// The CLOB refuses more than 512 orders a side.
		bookCapacity: Number.parseInt(get('--book-capacity', '1024'), 10),
		// The admin CLI's ceiling for a crank nobody has measured yet.
		crankCostUnits: Number.parseInt(get('--crank-cu', '250000'), 10),
		expireFallbackSlots: Number.parseInt(
			get('--expire-fallback-slots', '1500'),
			10
		),
		minCrossSurplus: Number.parseInt(get('--min-cross-surplus', '10000'), 10),
		multisig: argv.includes('--multisig')
			? new PublicKey(get('--multisig'))
			: undefined,
		vaultIndex: Number.parseInt(get('--vault-index', '0'), 10),
		proposalScan: Number.parseInt(get('--proposal-scan', '256'), 10),
		feeRails: parseFeeRails(get('--fee-rails', DEFAULT_FEE_RAILS)),
	};
}

/**
 * The warm admin. Without a multisig it is the keypair, and its instructions
 * send directly. With one it is the vault, and its instructions become
 * proposals. A proposal is not made twice: a rerun finds the pending one
 * that already does the step.
 */
class AdminDispatch {
	readonly key: PublicKey;

	private constructor(
		private readonly provider: AnchorProvider,
		private readonly args: Args,
		private readonly act: Act,
		private readonly report: RunReport,
		private readonly pending: PendingProposals | undefined
	) {
		this.key = resolveAdminAuthority(provider, args.multisig, args.vaultIndex);
	}

	static async create(
		provider: AnchorProvider,
		velocity: PublicKey,
		args: Args,
		act: Act,
		report: RunReport
	): Promise<AdminDispatch> {
		const pending = args.multisig
			? await PendingProposals.load(
					provider.connection,
					args.multisig,
					velocity,
					args.proposalScan
			  )
			: undefined;
		return new AdminDispatch(provider, args, act, report, pending);
	}

	get proposes(): boolean {
		return this.args.multisig !== undefined;
	}

	/** The pending proposal that already does `match`, if there is one. */
	pendingIndex(match: ProposalMatch): bigint | undefined {
		return this.pending?.find(match);
	}

	noteAwaiting(label: string, index: bigint): void {
		console.log(`${label}: waits on proposal #${index}`);
		this.report.awaiting.push(`${label}: proposal #${index}`);
	}

	async run(
		label: string,
		ixs: TransactionInstruction[],
		match?: ProposalMatch
	): Promise<DispatchOutcome> {
		if (!this.args.multisig) {
			await this.act(label, ixs);
			return 'sent';
		}

		const existing = match ? this.pendingIndex(match) : undefined;
		if (existing !== undefined) {
			this.noteAwaiting(label, existing);
			return 'awaiting';
		}

		const result = await sendOrPropose(
			this.provider,
			ixs,
			this.args.multisig,
			label,
			this.args.vaultIndex
		);
		if (result.kind === 'proposed') {
			this.pending?.record(result.transactionIndex, ixs);
			this.report.proposed.push(`${label}: proposal #${result.transactionIndex}`);
		} else {
			this.report.proposed.push(label);
		}

		return 'awaiting';
	}

	/** Split `ixs` into proposals whose propose transaction fits the limit. */
	async pack(
		ixs: TransactionInstruction[],
		memo: string
	): Promise<TransactionInstruction[][]> {
		const multisigPda = this.args.multisig;
		if (!multisigPda) return ixs.map((ix) => [ix]);
		const { blockhash } = await this.provider.connection.getLatestBlockhash();
		const fits = (batch: TransactionInstruction[]) =>
			proposalTransactionSize(
				this.provider,
				new TransactionMessage({
					payerKey: this.key,
					recentBlockhash: blockhash,
					instructions: batch,
				}),
				multisigPda,
				BigInt(1),
				this.args.vaultIndex,
				[],
				memo,
				blockhash
			) <= PROPOSAL_TX_LIMIT;

		const batches: TransactionInstruction[][] = [];
		for (const ix of ixs) {
			const last = batches[batches.length - 1];
			if (last && fits([...last, ix])) {
				last.push(ix);
			} else {
				batches.push([ix]);
			}
		}

		return batches;
	}
}

type ProposedInstruction = {
	program: PublicKey;
	data: Buffer;
	accounts: PublicKey[];
};

/** The velocity instructions of every proposal in the scan window that can
 * still execute. */
class PendingProposals {
	private constructor(
		private readonly velocity: PublicKey,
		private readonly proposals: Map<bigint, ProposedInstruction[]>
	) {}

	static async load(
		connection: Connection,
		multisigPda: PublicKey,
		velocity: PublicKey,
		window: number
	): Promise<PendingProposals> {
		const info = await multisig.accounts.Multisig.fromAccountAddress(
			connection,
			multisigPda
		);
		const latest = BigInt(info.transactionIndex.toString());
		const stale = BigInt(info.staleTransactionIndex.toString());
		const first = latest - BigInt(window) + BigInt(1);
		const indexes: bigint[] = [];
		for (let i = first > BigInt(0) ? first : BigInt(1); i <= latest; i++) {
			indexes.push(i);
		}

		const proposalInfos = await getAccountInfosChunked(
			connection,
			indexes.map(
				(transactionIndex) =>
					multisig.getProposalPda({ multisigPda, transactionIndex })[0]
			)
		);
		const transactionInfos = await getAccountInfosChunked(
			connection,
			indexes.map(
				(index) => multisig.getTransactionPda({ multisigPda, index })[0]
			)
		);

		const proposals = new Map<bigint, ProposedInstruction[]>();
		indexes.forEach((index, i) => {
			const proposalInfo = proposalInfos[i];
			const transactionInfo = transactionInfos[i];
			if (!proposalInfo || !transactionInfo) return;
			const [proposal] = multisig.accounts.Proposal.fromAccountInfo(proposalInfo);
			const status = proposal.status.__kind;
			if (!PENDING_PROPOSAL_STATUSES.includes(status)) return;
			if (status === 'Active' && index <= stale) return;
			const message = vaultTransactionMessage(transactionInfo);
			if (message) proposals.set(index, message);
		});

		console.log(
			`multisig ${multisigPda.toBase58()}: ${proposals.size} pending proposals in the last ${indexes.length}`
		);
		return new PendingProposals(velocity, proposals);
	}

	find(match: ProposalMatch): bigint | undefined {
		for (const [index, instructions] of this.proposals) {
			const found = instructions.some(
				(ix) =>
					ix.program.equals(this.velocity) &&
					match.discriminators.some((d) => d.equals(ix.data.subarray(0, 8))) &&
					ix.accounts.some((account) => account.equals(match.account))
			);
			if (found) return index;
		}

		return undefined;
	}

	record(index: bigint, ixs: TransactionInstruction[]): void {
		this.proposals.set(
			index,
			ixs.map((ix) => ({
				program: ix.programId,
				data: ix.data,
				accounts: ix.keys.map((key) => key.pubkey),
			}))
		);
	}
}

/** The instructions a vault transaction carries, or undefined for a config
 * transaction, which decodes as something else. */
function vaultTransactionMessage(
	info: AccountInfo<Buffer>
): ProposedInstruction[] | undefined {
	let message: multisig.accounts.VaultTransaction['message'];
	try {
		message = multisig.accounts.VaultTransaction.fromAccountInfo(info)[0].message;
	} catch {
		return undefined;
	}

	return message.instructions.map((ix) => ({
		program: message.accountKeys[ix.programIdIndex],
		data: Buffer.from(ix.data),
		accounts: Array.from(ix.accountIndexes).map((i) => message.accountKeys[i]),
	}));
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
	setDryRun(args.dryRun);
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

	const report: RunReport = { sent: [], proposed: [], awaiting: [] };
	const act = async (
		label: string,
		ixs: TransactionInstruction[],
		signers: Keypair[] = []
	) => {
		report.sent.push(label);
		if (args.dryRun) return;
		await provider.sendAndConfirm(new Transaction().add(...ixs), signers);
	};

	console.log(`velocity ${velocity.toBase58()} @ ${args.url}`);
	console.log(args.dryRun ? '(dry run — nothing will be sent)\n' : '');

	const admin = await AdminDispatch.create(provider, velocity, args, act, report);
	try {
		await migrate({ connection, provider, program, payer, admin, args, act }, idl);
	} finally {
		await printReport(connection, admin, args, report);
	}
}

type Migration = {
	connection: Connection;
	provider: AnchorProvider;
	program: Program;
	payer: Keypair;
	admin: AdminDispatch;
	args: Args;
	act: Act;
};

async function migrate(ctx: Migration, idl: any) {
	const { connection, provider, program, payer, admin, args, act } = ctx;
	const velocity = program.programId;
	const statePda = await getVelocityStateAccountPublicKey(velocity);

	const clobProgram = clobProgramId(idl);
	await assertClobDeployed(connection, clobProgram);
	const stateAccount = await loadState(connection, program, statePda);
	assertAdminHoldsWarm(stateAccount, admin.key);
	if ((await ensureFeeRails(ctx, statePda, stateAccount)) === 'awaiting') {
		console.log(
			'\nfee rails: awaiting the proposal. Every later step prices a crank from them, so run again once it executes.'
		);
		return;
	}

	// 1. resize
	// `program.idl` is camelCased by the Anchor client; `idl` is the raw JSON,
	// which keeps the Rust type names the discriminator hashes.
	assertNamesAreReal(
		idl,
		RESIZABLE.map(({ name }) => name)
	);
	await resizeAccounts(ctx, statePda, stateAccount);

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

	const treasury = await ensureCrankTreasury(ctx, statePda);
	const perpMarkets = await decodeMarkets(connection, program, 'PerpMarket');
	const spotMarkets = await decodeMarkets(connection, program, 'SpotMarket');

	console.log('');
	await createMissingQuoterSlabs(
		connection,
		program,
		payer,
		[...perpMarkets.keys()],
		act
	);

	await assertTreasuryPriced(connection, program, treasury, args.dryRun);
	const books = await bringUpBooks(
		{ connection, program, payer, admin, state: statePda, clobProgram, args, act },
		perpMarkets
	);

	// 2. liquidation coverage
	const users = await connection.getProgramAccounts(velocity, {
		filters: [{ memcmp: { offset: 0, bytes: bs58(discriminator('User')) } }],
	});
	await coverUsers(
		ctx,
		statePda,
		users,
		{ perpMarkets, spotMarkets, books },
		holdsConditionsSync(stateAccount, payer.publicKey)
	);

	// 3. watches for the market and quoter conditions
	await watchMarketsAndQuoters(ctx, perpMarkets);

	// 4. legacy orders
	reportLegacyOrders(users, program);

	// 5. vault users
	await flagVaultUsers(provider, payer, users, program, act);
}

/** Register the relay watches over each market's crank conditions, its book,
 * and each quoter's cross conditions. */
async function watchMarketsAndQuoters(
	ctx: Migration,
	perpMarkets: Map<number, any>
): Promise<void> {
	const { connection, provider, program, payer, act } = ctx;
	const velocity = program.programId;
	console.log('');
	for (const [marketIndex] of perpMarkets) {
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
}

async function loadState(
	connection: Connection,
	program: Program,
	statePda: PublicKey
): Promise<any> {
	const info = await connection.getAccountInfo(statePda);
	if (!info) throw new Error(`state ${statePda.toBase58()} not found`);
	return program.coder.accounts.decode('state', info.data);
}

function assertAdminHoldsWarm(stateAccount: any, admin: PublicKey): void {
	const holds = [stateAccount.coldAdmin, stateAccount.warmAdmin].some(
		(key: PublicKey) => !key.equals(PublicKey.default) && key.equals(admin)
	);
	if (!holds) {
		throw new Error(
			`${admin.toBase58()} holds neither the warm nor the cold admin role. Pass the ` +
				`admin keypair, or --multisig with the multisig whose vault holds the role.`
		);
	}

	console.log(`admin ${admin.toBase58()}: holds the warm or cold role`);
}

/** Every market of one kind, keyed by market index and decoded through the
 * IDL rather than by byte offset. Layouts move, and a migration that reads
 * the wrong field is worse than one that fails. */
async function decodeMarkets(
	connection: Connection,
	program: Program,
	name: 'PerpMarket' | 'SpotMarket'
): Promise<Map<number, any>> {
	const accounts = await connection.getProgramAccounts(program.programId, {
		filters: [{ memcmp: { offset: 0, bytes: bs58(discriminator(name)) } }],
	});
	const coderName = name === 'PerpMarket' ? 'perpMarket' : 'spotMarket';
	return new Map(
		accounts.map(({ account }) => {
			const decoded: any = program.coder.accounts.decode(coderName, account.data);
			return [decoded.marketIndex, decoded];
		})
	);
}

/**
 * Grow every account whose struct gained fields. `extend_account` takes the
 * payer as its authority, so under a multisig the keypair needs the
 * `AccountExtension` hot role. A proposal per stale account would put each
 * one behind an approval.
 */
async function resizeAccounts(
	ctx: Migration,
	statePda: PublicKey,
	stateAccount: any
): Promise<void> {
	const { connection, program, payer, args, act } = ctx;
	for (const { name, size } of RESIZABLE) {
		const accounts = await connection.getProgramAccounts(program.programId, {
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
		assertCanExtend(stateAccount, payer.publicKey);

		for (const account of stale.slice(0, args.limit || stale.length)) {
			await act(`extend ${name} ${account.toBase58()}`, [
				await program.methods
					.extendAccount()
					.accounts({
						state: statePda,
						payer: payer.publicKey,
						authority: payer.publicKey,
						account,
						systemProgram: SystemProgram.programId,
					})
					.instruction(),
			]);
		}
	}
}

/** Mirrors `State::is_hot` for `HotRole::AccountExtension`. */
function assertCanExtend(stateAccount: any, payer: PublicKey): void {
	const holds = [
		stateAccount.coldAdmin,
		stateAccount.warmAdmin,
		stateAccount.hotAccountExtension,
	].some((key: PublicKey) => !key.equals(PublicKey.default) && key.equals(payer));
	if (!holds) {
		throw new Error(
			`the keypair ${payer.toBase58()} cannot sign extend_account. Give it the hot role: ` +
				`velocity-admin --multisig <pda> auth set-hot-admin accountExtension ${payer.toBase58()}`
		);
	}
}

/** Mirrors `State::is_hot` for `HotRole::ConditionsSync`. The holder may set
 * paid terms on another user's conditions, so it syncs without a proposal. */
function holdsConditionsSync(stateAccount: any, payer: PublicKey): boolean {
	return [
		stateAccount.coldAdmin,
		stateAccount.warmAdmin,
		stateAccount.hotConditionsSync,
	].some((key: PublicKey) => !key.equals(PublicKey.default) && key.equals(payer));
}

/**
 * The treasury every market's crank reservoir refills from. The CLOB crank
 * resolver and the liquidation-conditions resync both name it, so it has to
 * exist before either one can run. It is created inert. An operator decides
 * the pricing and the funding with `velocity-admin fees set-crank-treasury`
 * and a SOL transfer.
 */
async function ensureCrankTreasury(
	ctx: Migration,
	statePda: PublicKey
): Promise<PublicKey> {
	const { connection, program, admin } = ctx;
	const treasury = getCrankTreasuryPublicKey(program.programId);
	if (await connection.getAccountInfo(treasury)) {
		console.log(`\ntreasury ${treasury.toBase58()}: already created`);
		return treasury;
	}

	console.log(`\ntreasury ${treasury.toBase58()}: creating`);
	await admin.run(
		'migrate: create crank treasury',
		[
			await program.methods
				.initializeCrankTreasury()
				.accounts({
					treasury,
					admin: admin.key,
					state: statePda,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				})
				.instruction(),
		],
		{
			discriminators: [ixDiscriminator('initialize_crank_treasury')],
			account: treasury,
		}
	);
	return treasury;
}

type MarketsAndBooks = {
	perpMarkets: Map<number, any>;
	spotMarkets: Map<number, any>;
	books: BookStatus;
};

/**
 * Create and sync relay liquidation conditions for every user with exposure.
 * A sync that writes paid terms needs the user's authority, the warm admin or
 * the `conditionsSync` hot role. A payer with that role sends every sync
 * directly. Otherwise, under a multisig, the syncs are proposed, packed several
 * to a proposal, and a user whose conditions already hold paid terms gets no
 * new sync. Its reservoir and watch are still topped up.
 */
async function coverUsers(
	ctx: Migration,
	statePda: PublicKey,
	users: readonly { pubkey: PublicKey; account: { data: Buffer } }[],
	markets: MarketsAndBooks,
	payerSyncs: boolean
): Promise<void> {
	const { connection, provider, program, payer, admin, args, act } = ctx;
	console.log(`\nliq coverage: ${users.length} user accounts`);
	const proposesSyncs = admin.proposes && !payerSyncs;
	const syncer = payerSyncs ? payer.publicKey : admin.key;
	if (admin.proposes && payerSyncs) {
		console.log(
			`liq coverage: ${syncer.toBase58()} holds conditionsSync and syncs directly`
		);
	}

	let covered = 0;
	let deferred = 0;
	const toPropose: TransactionInstruction[] = [];
	for (const { pubkey: user, account } of users) {
		if (args.limit && covered >= args.limit) break;
		const decodedUser: any = program.coder.accounts.decode('user', account.data);
		const marketIndexes = exposedPerpMarkets(decodedUser);
		if (marketIndexes.length === 0) continue;

		// A sync arms the user's triggers, and an armed trigger needs a book.
		if (marketIndexes.some((index) => markets.books.awaiting.has(index))) {
			deferred += 1;
			continue;
		}

		const userConditions = getUserConditionsPublicKey(program.programId, user);
		const existing = await connection.getAccountInfo(userConditions);
		const ix = syncUserConditionsIx(
			ctx,
			statePda,
			user,
			decodedUser,
			marketIndexes,
			markets,
			syncer
		);
		covered += 1;

		if (proposesSyncs && !(existing && syncPayment(existing.data) > 0)) {
			const pending = admin.pendingIndex({
				discriminators: [ixDiscriminator('sync_user_conditions')],
				account: userConditions,
			});
			if (pending === undefined) toPropose.push(ix);
			else admin.noteAwaiting(`sync ${user.toBase58()}`, pending);
			continue;
		}

		if (!proposesSyncs) {
			await act(
				`${existing ? 'sync' : 'create+sync'} user conditions for ${user.toBase58()}`,
				[ix]
			);
		}

		if (!args.dryRun) await fundSyncReservoir(ctx, userConditions);
		await ensureWatch(connection, provider, payer, userConditions, act);
	}

	for (const batch of await admin.pack(toPropose, 'migrate: sync user conditions')) {
		await admin.run(`migrate: sync ${batch.length} user conditions`, batch);
	}

	console.log(`liq coverage: ${covered} accounts with exposure`);
	if (deferred > 0) {
		console.log(
			`liq coverage: ${deferred} accounts wait on a book that is not attached yet`
		);
	}
}

/** The markets and oracles a sync must carry for one user, in the order the
 * program classifies them. */
function syncCoverageAccounts(
	velocity: PublicKey,
	decodedUser: any,
	marketIndexes: number[],
	markets: MarketsAndBooks
): AccountMeta[] {
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
		const oracle = markets.perpMarkets.get(marketIndex)?.oracle;
		if (oracle) oracles.set(oracle.toBase58(), oracle);
	}

	for (const marketIndex of spotMarketIndexes) {
		const oracle = markets.spotMarkets.get(marketIndex)?.oracle;
		if (oracle && !oracle.equals(PublicKey.default)) {
			oracles.set(oracle.toBase58(), oracle);
		}
	}

	// The sync refuses an account it cannot classify, and a market with no
	// CLOB has no crank conditions account. A market with a CLOB must bring
	// its quoter slab.
	const bookMarkets = marketIndexes.filter((marketIndex) =>
		markets.books.attached.has(marketIndex)
	);
	return [
		...[...oracles.values()].map((oracle) => accountMeta(oracle, false)),
		...[...spotMarketIndexes].map((index) =>
			accountMeta(getSpotMarketPublicKeySync(velocity, index), true)
		),
		...marketIndexes.map((index) =>
			accountMeta(getPerpMarketPublicKeySync(velocity, index), true)
		),
		...bookMarkets.map((index) =>
			accountMeta(getClobCrankConditionsPublicKey(velocity, index), false)
		),
		...bookMarkets.map((index) =>
			accountMeta(getQuoterSlabPublicKey(velocity, index), false)
		),
	];
}

/** `sync_user_conditions` for one user. `syncer` signs and pays, because a
 * sync that writes paid terms needs the user's authority, the warm admin or the
 * `conditionsSync` hot role. */
function syncUserConditionsIx(
	ctx: Migration,
	statePda: PublicKey,
	user: PublicKey,
	decodedUser: any,
	marketIndexes: number[],
	markets: MarketsAndBooks,
	syncer: PublicKey
): TransactionInstruction {
	const velocity = ctx.program.programId;
	// `SyncLiqConditionsArgs` is the cost units as a u32, then the fallback
	// interval in slots. The program derives the lamport fee from
	// `State.transactionFeeRails`.
	const argsBuf = Buffer.alloc(12);
	argsBuf.writeUInt32LE(ctx.args.syncCostUnits, 0);
	argsBuf.writeBigUInt64LE(ctx.args.fallbackSlots, 4);
	return new TransactionInstruction({
		programId: velocity,
		keys: [
			{ pubkey: syncer, isSigner: true, isWritable: true },
			accountMeta(statePda, false),
			accountMeta(user, false),
			accountMeta(getUserConditionsPublicKey(velocity, user), true),
			accountMeta(SYSVAR_RENT_PUBKEY, false),
			accountMeta(SystemProgram.programId, false),
			...syncCoverageAccounts(velocity, decodedUser, marketIndexes, markets),
		],

		data: Buffer.concat([ixDiscriminator('sync_user_conditions'), argsBuf]),
	});
}

function accountMeta(pubkey: PublicKey, isWritable: boolean): AccountMeta {
	return { pubkey, isSigner: false, isWritable };
}

/** `UserConditionsV0.sync_payment_lamports`: the fee one sync pays, which the
 * program prices. Zero means the block holds no paid terms. */
function syncPayment(data: Buffer): number {
	return Number(data.readBigUInt64LE(data.length - BYTES_AFTER_SYNC_PAYMENT - 8));
}

/** Fund fifty syncs. The sync fee comes from the conditions account's own
 * lamports. */
async function fundSyncReservoir(
	ctx: Migration,
	userConditions: PublicKey
): Promise<void> {
	const info = await ctx.connection.getAccountInfo(userConditions);
	const floor = await ctx.connection.getMinimumBalanceForRentExemption(
		info?.data.length ?? 7272
	);
	const want = floor + (info ? syncPayment(info.data) : 0) * 50;
	if ((info?.lamports ?? 0) >= want) return;

	await ctx.act(`fund sync reservoir ${userConditions.toBase58()}`, [
		SystemProgram.transfer({
			fromPubkey: ctx.payer.publicKey,
			toPubkey: userConditions,
			lamports: want - (info?.lamports ?? 0),
		}),
	]);
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

/** Write the fee rails when they price every crank at zero, which is what an
 * upgrade leaves: `State` reads them from former padding. The priced test
 * mirrors `TransactionFeeRails::transaction_cost` for one signature. */
async function ensureFeeRails(
	ctx: Migration,
	statePda: PublicKey,
	stateAccount: any
): Promise<DispatchOutcome> {
	const rails = stateAccount.transactionFeeRails;
	const chargesCostUnits =
		rails.resourceFeeNumerator > 0 && rails.resourceFeeDenominator > 0;
	if (rails.inclusionLamports > 0 || rails.signatureLamports > 0 || chargesCostUnits) {
		console.log(`fee rails: ${JSON.stringify(rails)}`);
		return 'sent';
	}

	console.log(`fee rails: unpriced, writing ${JSON.stringify(ctx.args.feeRails)}`);
	return await ctx.admin.run(
		'migrate: set transaction fee rails',
		[
			await ctx.program.methods
				.updateTransactionFeeRails(ctx.args.feeRails)
				.accounts({ admin: ctx.admin.key, state: statePda })
				.instruction(),
		],
		{
			discriminators: [ixDiscriminator('update_transaction_fee_rails')],
			account: statePda,
		}
	);
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
		const create = info
			? ''
			: 'Create it first. Under a multisig, execute the proposal that creates it. ';
		throw new Error(
			`the crank treasury ${treasury.toBase58()} is ${info ? 'not priced' : 'not created'}. ` +
				`${create}Run velocity-admin fees set-crank-treasury <refillTargetCranks> <refillWatermarkCranks>, fund it with SOL, ` +
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
	admin: AdminDispatch;
	state: PublicKey;
	clobProgram: PublicKey;
	args: Args;
	act: Act;
};

/** Markets whose book is attached, and markets whose bring-up waits on a
 * multisig proposal. */
type BookStatus = { attached: Set<number>; awaiting: Set<number> };

/** Give every perp market its book. A direct dry run counts each market as
 * attached, because the direct path would attach it. */
async function bringUpBooks(
	ctx: BookBringUp,
	perpMarkets: Map<number, any>
): Promise<BookStatus> {
	console.log('');
	const status: BookStatus = { attached: new Set(), awaiting: new Set() };
	for (const [marketIndex, market] of perpMarkets) {
		const outcome = await bringUpBook(ctx, marketIndex, market);
		status[outcome === 'sent' ? 'attached' : 'awaiting'].add(marketIndex);
	}

	return status;
}

/**
 * Give a perp market its CLOB book, as `velocity-admin clob-market init`
 * does. The payer creates the book directly, because the book signs its own
 * creation and is too large to create inside a vault transaction. The warm
 * admin then registers the entry, and in a second round approves and
 * attaches it. The approval carries the hash of the staged entry, so it is
 * built after the registration lands. Each stage reads chain first, so a run
 * that stops part way resumes.
 */
async function bringUpBook(
	ctx: BookBringUp,
	marketIndex: number,
	market: any
): Promise<DispatchOutcome> {
	const { connection, program, clobProgram, args } = ctx;
	const velocity = program.programId;
	const quoter = getQuoterPublicKey(
		velocity,
		marketIndex,
		clobProgram,
		PublicKey.default
	);

	let book: PublicKey = market.clobMarket;
	if (book.equals(PublicKey.default)) {
		book =
			(await findUnnamedBook(ctx, marketIndex, market)) ??
			(await createBook(ctx, marketIndex, market));
		const registration = await ctx.admin.run(
			`migrate: book A market ${marketIndex}`,
			await registerBookIxs(ctx, marketIndex, book, quoter),
			{ discriminators: [ixDiscriminator('initialize_quoter')], account: quoter }
		);
		if (registration === 'awaiting') return 'awaiting';
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
	const attached = await connection.getAccountInfo(
		getClobCrankConditionsPublicKey(velocity, marketIndex)
	);
	if (approved && attached) {
		console.log(`book market ${marketIndex}: attached`);
		return 'sent';
	}

	// A direct dry run sent no registration, so it has no entry to hash.
	const quoterInfo = await connection.getAccountInfo(quoter);
	if (!quoterInfo) {
		if (args.dryRun) return 'sent';
		throw new Error(`quoter entry ${quoter.toBase58()} did not land`);
	}

	console.log(`book market ${marketIndex}: approving and attaching`);
	const ixs = [
		...(approved
			? []
			: [await approveBookIx(ctx, marketIndex, book, quoter, quoterInfo.data)]),
		...(attached ? [] : [await attachBookIx(ctx, marketIndex, book, quoter)]),
	];
	return await ctx.admin.run(`migrate: book B market ${marketIndex}`, ixs, {
		discriminators: [
			ixDiscriminator('update_quoter_approved'),
			ixDiscriminator('update_perp_market_clob_quoter'),
		],
		account: quoter,
	});
}

/**
 * A book from an earlier run that the market does not name yet, because its
 * registration waits on the multisig. Any such book is as good as a new one,
 * whoever created it. It holds no order and the config this script writes,
 * and only the slab, as both of its authorities, can change either. Several
 * matches resolve to the lowest address, so every rerun picks the same one.
 */
async function findUnnamedBook(
	ctx: BookBringUp,
	marketIndex: number,
	market: any
): Promise<PublicKey | undefined> {
	const quoterSlab = getQuoterSlabPublicKey(ctx.program.programId, marketIndex);
	const candidates = await ctx.connection.getProgramAccounts(ctx.clobProgram, {
		filters: [
			{ memcmp: { offset: CLOB_AUTHORITY_OFFSET, bytes: quoterSlab.toBase58() } },
			{
				memcmp: {
					offset: CLOB_PLACE_AUTHORITY_OFFSET,
					bytes: quoterSlab.toBase58(),
				},
			},
		],

		dataSlice: { offset: 0, length: CLOB_HEADER_PREFIX_BYTES },
	});

	const config = bookConfig(marketIndex, market, ctx.args.bookCapacity);
	const matches = candidates
		.filter(({ account }) => isEmptyBookWithConfig(account.data, config))
		.map(({ pubkey }) => pubkey)
		.sort((a, b) => Buffer.compare(a.toBuffer(), b.toBuffer()));
	if (matches.length === 0) return undefined;

	console.log(
		`book market ${marketIndex}: reusing ${matches[0].toBase58()}` +
			(matches.length > 1 ? `, the lowest of ${matches.length} matches` : '')
	);
	return matches[0];
}

function isEmptyBookWithConfig(header: Buffer, config: Buffer): boolean {
	if (header.length < CLOB_HEADER_PREFIX_BYTES) return false;
	const stored = Buffer.concat(
		CLOB_CONFIG_FIELDS.map(([offset, width]) =>
			header.subarray(offset, offset + width)
		)
	);

	return (
		stored.equals(config) &&
		header.readUInt32LE(CLOB_BID_COUNT_OFFSET) === 0 &&
		header.readUInt32LE(CLOB_ASK_COUNT_OFFSET) === 0
	);
}

/** Create the book and initialize it on the CLOB with the market's slab as
 * both authorities. It needs no admin, only the book's own signature. */
async function createBook(
	ctx: BookBringUp,
	marketIndex: number,
	market: any
): Promise<PublicKey> {
	const { connection, program, payer, clobProgram, args } = ctx;
	const quoterSlab = getQuoterSlabPublicKey(program.programId, marketIndex);
	const space = CLOB_ORDERS_OFFSET + args.bookCapacity * CLOB_NODE_BYTES;
	const book = Keypair.generate();

	const createAccount = SystemProgram.createAccount({
		fromPubkey: payer.publicKey,
		newAccountPubkey: book.publicKey,
		lamports: await connection.getMinimumBalanceForRentExemption(space),
		space,
		programId: clobProgram,
	});

	const initBook = new TransactionInstruction({
		programId: clobProgram,
		keys: [
			{ pubkey: quoterSlab, isSigner: false, isWritable: false },
			{ pubkey: quoterSlab, isSigner: false, isWritable: false },
			{ pubkey: book.publicKey, isSigner: true, isWritable: true },
		],

		data: Buffer.concat([
			ixDiscriminator('initialize_market_v0'),
			bookConfig(marketIndex, market, args.bookCapacity),
		]),
	});

	console.log(`book market ${marketIndex}: creating ${book.publicKey.toBase58()}`);
	await ctx.act(
		`create book ${book.publicKey.toBase58()} for market ${marketIndex}`,
		[createAccount, initBook],
		[book]
	);
	return book.publicKey;
}

/** Register the book's quoter entry, which names the book on the market, and
 * set the entry's account list. The admin pays for the entry. */
async function registerBookIxs(
	ctx: BookBringUp,
	marketIndex: number,
	book: PublicKey,
	quoter: PublicKey
): Promise<TransactionInstruction[]> {
	const { program, admin, clobProgram } = ctx;
	const velocity = program.programId;
	const quoterSlab = getQuoterSlabPublicKey(velocity, marketIndex);

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
			payer: admin.key,
			authority: admin.key,
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
			authority: admin.key,
			quoter,
			state: ctx.state,
		})
		.instruction();

	return [registerQuoter, registerAccounts];
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
	const { program, admin, clobProgram } = ctx;
	const velocity = program.programId;
	return await program.methods
		.updateQuoterApproved({
			approved: true,
			stagedConfigHash: quoterConfigHash(quoterData),
		})
		.accountsStrict({
			admin: admin.key,
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
	const { program, admin, clobProgram, args } = ctx;
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
			admin: admin.key,
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

/** What the run sent and proposed, what waits on approval, and what the
 * vault pays for the proposals it executes. */
async function printReport(
	connection: Connection,
	admin: AdminDispatch,
	args: Args,
	report: RunReport
): Promise<void> {
	const list = (lines: string[]) => {
		for (const line of lines.slice(0, 40)) console.log(`  ${line}`);
		if (lines.length > 40) console.log(`  … ${lines.length - 40} more`);
	};

	console.log(`\n${args.dryRun ? 'would send' : 'sent'} ${report.sent.length} transactions`);
	list(report.sent);
	if (!args.multisig) return;

	console.log(
		`${args.dryRun ? 'would propose' : 'proposed'} ${report.proposed.length} to multisig ${args.multisig.toBase58()}`
	);
	list(report.proposed);
	console.log(`waiting on ${report.awaiting.length} earlier proposals`);
	list(report.awaiting);

	const rent = (bytes: number) => connection.getMinimumBalanceForRentExemption(bytes);
	const slabSlotRent = (await rent(QUOTER_SLAB_SLOT_BYTES)) - (await rent(0));
	const perBook =
		(await rent(QUOTER_V0_BYTES)) + slabSlotRent + (await rent(CRANK_CONDITIONS_BYTES));
	const perUser = await rent(USER_CONDITIONS_BYTES);
	const vaultLamports = await connection.getBalance(admin.key);
	console.log(
		`\nvault ${admin.key.toBase58()} holds ${vaultLamports / 1e9} SOL. It pays about ` +
			`${perBook / 1e9} SOL per book, and ${perUser / 1e9} SOL per user conditions ` +
			'account it proposes. A payer with the conditionsSync hot role pays for those instead.'
	);

	if (report.proposed.length + report.awaiting.length > 0) {
		console.log(
			'Approve and execute the proposals, then run this migration again. ' +
				'It is done when a run sends and proposes nothing.'
		);
	}
}

async function getAccountInfosChunked(connection: Connection, keys: PublicKey[]) {
	const out: Awaited<ReturnType<Connection['getMultipleAccountsInfo']>> = [];
	for (let i = 0; i < keys.length; i += 100) {
		out.push(...(await connection.getMultipleAccountsInfo(keys.slice(i, i + 100))));
	}

	return out;
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
