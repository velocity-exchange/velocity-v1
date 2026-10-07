/**
 * allow-verbose: the usage header of an operator script.
 *
 * A PropAMM on SOL-PERP, so a test can cross the book against a quoter that is not a CLOB:
 *
 *   bun run local:midpoint up [mid] [--name <instance>]
 *   bun run local:midpoint mid <price> [--name <instance>]
 *
 * `up` funds the maker and opens its account, creates the midpoint instance, quotes two rungs a
 * side around `mid`, registers the instance as a Custom quoter entry, and approves the entry. It
 * skips what already exists, so it runs again safely. `mid` defaults to the oracle price.
 *
 * `mid` re-quotes around a new price. The instance stops quoting once its mid is 1000 slots old,
 * about seven minutes, so a test refreshes it before it relies on the quote. `--name` picks
 * another instance with its own maker, so two PropAMMs can quote one market. The default instance
 * keeps its keys in /state/keys/midpoint-*.json, and instance `x` in /state/keys/midpoint-x-*.json.
 */
import * as fs from 'fs';
import { createHash } from 'crypto';
import { AnchorProvider, BN, Idl, Program } from '@coral-xyz/anchor';
import {
	Connection,
	PublicKey,
	sendAndConfirmTransaction,
	SystemProgram,
	SYSVAR_RENT_PUBKEY,
	Transaction,
	TransactionInstruction,
} from '@solana/web3.js';
import {
	AdminClient,
	BulkAccountLoader,
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getQuoterCrossConditionsPublicKey,
	getQuoterPublicKey,
	getQuoterSlabPublicKey,
	getRegisterWatchIxs,
	QUOTER_CROSS_BLOCK_OFFSET,
	RELAY_WATCH_V0_LEN,
	QuoterType,
	Wallet,
} from '@velocity-exchange/sdk';
import {
	AUTHORITY_PATH,
	connectClient,
	fundWallet,
	loadKey,
	parseDecimal,
	RPC_URL,
	userAccountPublicKey,
} from './tools';

const MIDPOINT_ID = new PublicKey(
	'eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D'
);
const MIDPOINT_IDL = JSON.parse(
	fs.readFileSync('tests/e2e/idl/midpoint.json', 'utf-8')
) as Idl;
const MARKET_INDEX = 0;
const BASE = new BN(1_000_000_000);
const MAKER_DEPOSIT_DUSDT = '10000';
/** 10 bps and 30 bps from the mid, one SOL each, on both sides. */
const RUNG_OFFSETS_PPM = [1000, 3000];
/**
 * The cross conditions' poll interval. The entry declares no reprice watch, so the poll is how
 * relay finds a remainder this quoter crosses. Only the maker's authority may pick it.
 */
const CROSS_FALLBACK_SLOTS = 10;

const u16 = (value: number) => {
	const buffer = Buffer.alloc(2);
	buffer.writeUInt16LE(value);
	return buffer;
};

const discriminator = (name: string) =>
	Array.from(
		createHash('sha256').update(`global:${name}`).digest().subarray(0, 8)
	);

function instance(maker: PublicKey): PublicKey {
	return PublicKey.findProgramAddressSync(
		[Buffer.from('midpoint'), u16(MARKET_INDEX), maker.toBuffer(), u16(0)],
		MIDPOINT_ID
	)[0];
}

async function send(
	connection: Connection,
	ixs: TransactionInstruction[],
	signers: ReturnType<typeof loadKey>[]
): Promise<string> {
	return sendAndConfirmTransaction(
		connection,
		new Transaction().add(...ixs),
		signers
	);
}

const DEFAULT_INSTANCE = 'a';

function keys(name: string) {
	const prefix =
		name === DEFAULT_INSTANCE
			? '/state/keys/midpoint'
			: `/state/keys/midpoint-${name}`;
	return {
		maker: loadKey(`${prefix}-maker.json`),
		config: loadKey(`${prefix}-config.json`),
		hot: loadKey(`${prefix}-hot.json`),
		authority: loadKey(AUTHORITY_PATH),
	};
}

async function setLevels(
	program: Program,
	connection: Connection,
	quoter: PublicKey,
	hot: ReturnType<typeof loadKey>,
	mid: BN
): Promise<void> {
	const rungs = RUNG_OFFSETS_PPM.map((offsetPpm) => ({
		offsetPpm: new BN(offsetPpm),
		size: BASE,
	}));
	const ix = await program.methods
		.setLevelsV0({ mid, sequence: null, bids: rungs, asks: rungs })
		.accountsStrict({ quoter, hotAuthority: hot.publicKey })
		.instruction();
	await send(connection, [ix], [hot]);
	console.log(`midpoint quoting around ${mid.toNumber() / 1e6}`);
}

type Keys = ReturnType<typeof keys>;

function adminClient(
	connection: Connection,
	signer: Keys['maker']
): AdminClient {
	return new AdminClient({
		connection,
		wallet: new Wallet(signer),
		env: 'devnet',
		skipLoadUsers: true,
		accountSubscription: {
			type: 'polling',
			accountLoader: new BulkAccountLoader(connection, 'confirmed', 1000),
		},
	});
}

async function openMaker(
	connection: Connection,
	makerClient: Awaited<ReturnType<typeof connectClient>>,
	{ maker, config, hot }: Keys,
	makerUser: PublicKey
): Promise<void> {
	if (await connection.getAccountInfo(makerUser)) return;

	const tokenAccount = await fundWallet(
		connection,
		maker.publicKey,
		10,
		MAKER_DEPOSIT_DUSDT
	);
	await fundWallet(connection, config.publicKey, 1, '0');
	await fundWallet(connection, hot.publicKey, 1, '0');
	await makerClient.initializeUserAccountAndDepositCollateral(
		parseDecimal(MAKER_DEPOSIT_DUSDT, 6),
		tokenAccount
	);
	console.log(`maker account ${makerUser.toBase58()} opened`);
}

async function createInstance(
	connection: Connection,
	program: Program,
	{ maker, config, hot, authority }: Keys,
	slab: PublicKey
): Promise<void> {
	const quoter = instance(maker.publicKey);
	if (await connection.getAccountInfo(quoter)) return;

	const ix = await program.methods
		.initializeQuoterV0({
			marketIndex: MARKET_INDEX,
			userSubAccountId: 0,
			basePrecision: BASE,
			maxMidStalenessSlots: new BN(1000),
			priceTickSize: new BN(100),
			sizeStep: new BN(100000),
			minQuoteSize: new BN(100000),
			requireAttestedFlow: false,
			maxMidDeviationPpm: new BN(300_000),
		})
		.accountsStrict({
			// Anchor refuses a writable account that appears twice, so the payer is
			// none of the instance's own keys.
			payer: authority.publicKey,
			authority: config.publicKey,
			userAuthority: maker.publicKey,
			executeAuthority: slab,
			hotAuthority: hot.publicKey,
			quoter,
			systemProgram: SystemProgram.programId,
		})
		.instruction();
	await send(connection, [ix], [authority, config, maker]);
	console.log(`midpoint instance ${quoter.toBase58()} created`);
}

/** A Custom entry answers to the maker, so the maker registers it and the admin approves it. */
async function registerEntry(
	connection: Connection,
	{ maker, authority }: Keys,
	makerUser: PublicKey,
	slab: PublicKey
): Promise<PublicKey> {
	const quoter = instance(maker.publicKey);
	const makerAdmin = adminClient(connection, maker);
	const entry = getQuoterPublicKey(
		makerAdmin.program.programId,
		MARKET_INDEX,
		MIDPOINT_ID,
		makerUser
	);
	if (await connection.getAccountInfo(entry)) return entry;

	const register = await makerAdmin.getInitializeQuoterIx(
		MARKET_INDEX,
		{
			quoterType: QuoterType.CUSTOM,
			responseAccount: quoter,
			quoteV0Discriminator: discriminator('quote_v0'),
			quoteL3V0Discriminator: new Array(8).fill(0),
			executeV0Discriminator: discriminator('execute_v0'),
		},
		MIDPOINT_ID,
		makerUser
	);
	const metas = [
		{ pubkey: quoter, isWritable: true },
		{ pubkey: slab, isWritable: false },
	];
	const accounts = await makerAdmin.getUpdateQuoterAccountsIx(
		entry,
		metas,
		[0],
		[0, 1]
	);
	await send(connection, [register, accounts], [maker]);
	console.log(`quoter entry ${entry.toBase58()} registered`);

	const admin = adminClient(connection, authority);
	const approve = await admin.getUpdateQuoterApprovedIx(
		entry,
		true,
		MARKET_INDEX,
		MIDPOINT_ID,
		null,
		authority.publicKey,
		quoter,
		await admin.getStagedQuoterConfigHash(entry)
	);
	await send(connection, [approve], [authority]);
	console.log('quoter entry approved');
	return entry;
}

/**
 * Attaches the entry's relay cross conditions, or re-prices them, and registers the relay watch
 * that turners find them by. They let relay find a taker remainder or a book order that this
 * quoter's quote crosses.
 */
async function attachCrossConditions(
	connection: Connection,
	{ maker }: Keys,
	entry: PublicKey
): Promise<void> {
	const makerAdmin = adminClient(connection, maker);
	const programId = makerAdmin.program.programId;
	const ix = makerAdmin.program.instruction.initializeQuoterCrossConditions(
		{ expireFallbackSlots: new BN(CROSS_FALLBACK_SLOTS) },
		{
			accounts: {
				payer: maker.publicKey,
				state: await makerAdmin.getStatePublicKey(),
				quoter: entry,
				perpMarket: getPerpMarketPublicKeySync(programId, MARKET_INDEX),
				quoterSlab: getQuoterSlabPublicKey(programId, MARKET_INDEX),
				marketConditions: getClobCrankConditionsPublicKey(
					programId,
					MARKET_INDEX
				),
				crossConditions: getQuoterCrossConditionsPublicKey(programId, entry),
				rent: SYSVAR_RENT_PUBKEY,
				systemProgram: SystemProgram.programId,
			},
		}
	);
	await send(connection, [ix], [maker]);

	const { watch, ixs: register } = await getRegisterWatchIxs({
		payer: maker.publicKey,
		target: getQuoterCrossConditionsPublicKey(programId, entry),
		blockOffset: QUOTER_CROSS_BLOCK_OFFSET,
		seed: `cross-${entry.toBase58().slice(0, 26)}`,
		rentLamports: await connection.getMinimumBalanceForRentExemption(
			RELAY_WATCH_V0_LEN
		),
	});
	if (await connection.getAccountInfo(watch)) return;

	await send(connection, register, [maker]);
	console.log(
		`relay watch ${watch.toBase58()} registered for ${entry.toBase58()}`
	);
}

async function up(
	connection: Connection,
	program: Program,
	name: string,
	midArg?: string
) {
	const stackKeys = keys(name);
	const makerClient = await connectClient(connection, stackKeys.maker);
	const makerUser = userAccountPublicKey(
		makerClient,
		stackKeys.maker.publicKey
	);
	const slab = getQuoterSlabPublicKey(
		makerClient.program.programId,
		MARKET_INDEX
	);

	try {
		await openMaker(connection, makerClient, stackKeys, makerUser);
		await createInstance(connection, program, stackKeys, slab);

		const mid = midArg
			? parseDecimal(midArg, 6)
			: new BN(
					makerClient.getOracleDataForPerpMarket(MARKET_INDEX).price.toString()
			  );
		await setLevels(
			program,
			connection,
			instance(stackKeys.maker.publicKey),
			stackKeys.hot,
			mid
		);
		const entry = await registerEntry(connection, stackKeys, makerUser, slab);
		await attachCrossConditions(connection, stackKeys, entry);
	} finally {
		await makerClient.unsubscribe();
	}
}

async function main() {
	const argv = process.argv.slice(2);
	const nameAt = argv.indexOf('--name');
	const name = nameAt === -1 ? DEFAULT_INSTANCE : argv.splice(nameAt, 2)[1];
	if (!name) throw new Error('--name needs a value');

	const [command, price] = argv;
	const connection = new Connection(RPC_URL, 'confirmed');
	const { maker, hot } = keys(name);
	const program = new Program(
		MIDPOINT_IDL,
		new AnchorProvider(connection, new Wallet(maker) as never, {})
	);

	if (command === 'up') await up(connection, program, name, price);
	else if (command === 'mid' && price)
		await setLevels(
			program,
			connection,
			instance(maker.publicKey),
			hot,
			parseDecimal(price, 6)
		);
	else
		throw new Error(
			'commands: up [mid], mid <price>, with [--name <instance>]'
		);
}

main()
	.then(() => process.exit(0))
	.catch((error) => {
		console.error(error instanceof Error ? error.message : error);
		process.exit(1);
	});
