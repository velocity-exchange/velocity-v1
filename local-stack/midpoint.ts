/**
 * allow-verbose: the usage header of an operator script.
 *
 * A PropAMM on SOL-PERP, so a test can cross the book against a quoter that is not a CLOB:
 *
 *   bun run local:midpoint up [mid]
 *   bun run local:midpoint mid <price>
 *
 * `up` funds the maker and opens its account, creates the midpoint instance, quotes two rungs a
 * side around `mid`, registers the instance as a Custom quoter entry, and approves the entry. It
 * skips what already exists, so it runs again safely. `mid` defaults to the oracle price.
 *
 * `mid` re-quotes around a new price. The instance stops quoting once its mid is 1000 slots old,
 * about seven minutes, so a test refreshes it before it relies on the quote. The keys live in
 * /state/keys/midpoint-*.json.
 */
import * as fs from 'fs';
import { createHash } from 'crypto';
import { AnchorProvider, BN, Idl, Program } from '@coral-xyz/anchor';
import {
	Connection,
	PublicKey,
	sendAndConfirmTransaction,
	SystemProgram,
	Transaction,
	TransactionInstruction,
} from '@solana/web3.js';
import {
	AdminClient,
	BulkAccountLoader,
	getQuoterPublicKey,
	getQuoterSlabPublicKey,
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

function keys() {
	return {
		maker: loadKey('/state/keys/midpoint-maker.json'),
		config: loadKey('/state/keys/midpoint-config.json'),
		hot: loadKey('/state/keys/midpoint-hot.json'),
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
): Promise<void> {
	const quoter = instance(maker.publicKey);
	const makerAdmin = adminClient(connection, maker);
	const entry = getQuoterPublicKey(
		makerAdmin.program.programId,
		MARKET_INDEX,
		MIDPOINT_ID,
		makerUser
	);
	if (await connection.getAccountInfo(entry)) return;

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
}

async function up(connection: Connection, program: Program, midArg?: string) {
	const stackKeys = keys();
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
		await registerEntry(connection, stackKeys, makerUser, slab);
	} finally {
		await makerClient.unsubscribe();
	}
}

async function main() {
	const [command, price] = process.argv.slice(2);
	const connection = new Connection(RPC_URL, 'confirmed');
	const { maker, hot } = keys();
	const program = new Program(
		MIDPOINT_IDL,
		new AnchorProvider(connection, new Wallet(maker) as never, {})
	);

	if (command === 'up') await up(connection, program, price);
	else if (command === 'mid' && price)
		await setLevels(
			program,
			connection,
			instance(maker.publicKey),
			hot,
			parseDecimal(price, 6)
		);
	else throw new Error('commands: up [mid], mid <price>');
}

main().catch((error) => {
	console.error(error instanceof Error ? error.message : error);
	process.exit(1);
});
