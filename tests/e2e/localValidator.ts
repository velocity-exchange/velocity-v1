/**
 * Full-stack e2e against a real local validator. This is the devnet
 * confidence gate. Run it through `bash test-scripts/run-e2e-localnet.sh`,
 * which starts the validator and redis and builds the programs and the
 * book-publisher that this file uses.
 *
 * The topology is the production one and no account is synthesized. Protocol
 * init goes through the admin instructions. CLOB bring-up matches the admin
 * CLI: book, registry entry, canonical attach, and conditions reservoir. A
 * midpoint spline instance quotes around a hot-key mid, makers rest orders on
 * the book, and the protocol User pays for cranks. The Rust book-publisher ticks
 * against the validator over RPC and writes the Redis wire, and its cross
 * fast path watches for crossed books.
 *
 * A spec here waits for something to happen rather than making it happen, so
 * it can pass by waiting for a state it was already in. Two rules keep that
 * from turning a green run into a proof of nothing.
 *
 * Assert the precondition, not only the outcome. A poll that exits on "the
 * order is gone" also exits on an order that never rested, so a spec that
 * rests one asserts it rested before it starts waiting. `place_and_make` is
 * why the placement's own success is not enough: it lands and succeeds whether
 * the order rests or fills, so landing and resting are different events here.
 *
 * Assert who did the work. A relay-cranked outcome can arrive from the
 * publisher, from another spec, or from a crank nobody meant to fire, and the
 * account state looks the same either way. Every relay-cranked spec snapshots
 * `relayPayoutBalance()` and asserts it rose, which is the only evidence that
 * discovery reached the crank under test. A new one that leaves that out
 * still passes on a market where relay never ran.
 *
 * Read balances as deltas. An account here is shared across specs, so an
 * absolute is true for a position some earlier spec opened.
 *
 * Velocity instructions go through the SDK. The CLOB and midpoint ship no TS
 * client, so anchor-v2/scripts/gen-quoter-idls.sh generates their IDLs from
 * the programs into tests/e2e/idl. Anchor's `Program` then drives them as
 * `clobProgram` and `midpointProgram`. The relay program is on a different
 * anchor fork. Its one instruction here, `register_watch_v0`, stays
 * hand-encoded in the `relayIx` helper below.
 */
import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { createHash } from 'crypto';
import { spawn, ChildProcess } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import Redis from 'ioredis';
import {
	AccountMeta,
	AddressLookupTableProgram,
	ComputeBudgetProgram,
	Connection,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	SystemProgram,
	SYSVAR_INSTRUCTIONS_PUBKEY,
	SYSVAR_RENT_PUBKEY,
	Transaction,
	TransactionInstruction,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import {
	AccountLayout,
	createInitializeAccountInstruction,
	createInitializeMintInstruction,
	createMintToInstruction,
	MintLayout,
	TOKEN_PROGRAM_ID,
} from '@solana/spl-token';
import {
	BASE_PRECISION,
	PerpOperation,
	BN,
	BulkAccountLoader,
	getClobCrankConditionsPublicKey,
	getCrankTreasuryPublicKey,
	getPerpMarketPublicKeySync,
	getLimitOrderParams,
	getOrderParams,
	generateSignedMsgUuid,
	HotRole,
	SignedMsgNetwork,
	getMarketOrderParams,
	getTriggerMarketOrderParams,
	getTriggerLimitOrderParams,
	isVariant,
	OrderTriggerCondition,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
	getRelayScratchPublicKey,
	getUserConditionsPublicKey,
	getVelocitySignerPublicKey,
	OracleSource,
	PEG_PRECISION,
	PositionDirection,
	MarketType,
	OptionalOrderParams,
	PostOnlyParams,
	parseLogs,
	PRICE_PRECISION,
	getQuoterSlabPublicKey,
	QuoterType,
	RetryTxSender,
	TestClient,
	Wallet,
} from '../../packages/sdk/src';
import { initializeQuoteSpotMarket } from '../velocity/testHelpers';

const RPC_URL = process.env.E2E_RPC_URL ?? 'http://127.0.0.1:8899';
const REDIS_URL = process.env.E2E_REDIS_URL ?? 'redis://127.0.0.1:6399';
const SCRATCH = process.env.E2E_SCRATCH_DIR ?? '/tmp/velocity-e2e';
const PUBLISHER_BIN =
	process.env.BOOK_PUBLISHER_BIN ?? 'rust/target/debug/book-publisher';

/** `BPFLoaderUpgradeab1e11111111111111111111111`, which owns a program's data account. */
const BPF_LOADER_UPGRADEABLE_ID = new PublicKey(
	'BPFLoaderUpgradeab1e11111111111111111111111'
);
const VELOCITY_ID = new PublicKey(
	'vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P'
);
const PYTH_ID = new PublicKey('gSbePebfvPy7tRqimPoVecS2UsBvYv46ynrzWocc92s');
const CLOB_ID = new PublicKey('BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU');
const MIDPOINT_ID = new PublicKey(
	'eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D'
);
const RELAY_ID = new PublicKey(
	process.env.RELAY_PROGRAM_ID ?? '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu'
);
const TURNER_BIN = process.env.RELAY_TURNER_BIN ?? '';
const SWIFT_BIN = process.env.SWIFT_BIN ?? '';
const SWIFT_PORT = 3211;
const SWIFT_URL = `http://127.0.0.1:${SWIFT_PORT}`;
/** relay-spec's `WatchV0` account length. */
const WATCH_V0_LEN = 112;
/** `agg.price` within the pyth stub's `Price` account. Velocity's
 * `oracle_watch` registers the same offset for push feeds, so a test reading
 * the feed and a relay watch reading it see the same bytes. */
const PYTH_AGG_PRICE_OFFSET = 208;

const UNIT = BASE_PRECISION; // 1e9
const USDC = new BN(10).pow(new BN(6));
/** Converts dollars to PRICE_PRECISION (1e6). Every price here is in it. */
const usd = (dollars: number): BN =>
	new BN(Math.round(dollars * PRICE_PRECISION.toNumber()));

/** Anchor default instruction discriminator: sha256("global:<name>")[..8]. */
function ixDiscriminator(name: string): Buffer {
	return createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);
}

function u16(v: number): Buffer {
	const b = Buffer.alloc(2);
	b.writeUInt16LE(v);
	return b;
}
function u32(v: number): Buffer {
	const b = Buffer.alloc(4);
	b.writeUInt32LE(v);
	return b;
}
const meta = (pubkey: PublicKey, isWritable: boolean, isSigner: boolean) =>
	({ pubkey, isSigner, isWritable }) as AccountMeta;
const ro = (pubkey: PublicKey) => meta(pubkey, false, false);
const rw = (pubkey: PublicKey) => meta(pubkey, true, false);
const signerRo = (pubkey: PublicKey) => meta(pubkey, false, true);

async function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

async function pollUntil<T>(
	what: string,
	timeoutMs: number,
	probe: () => Promise<T | undefined>
): Promise<T> {
	const deadline = Date.now() + timeoutMs;
	for (;;) {
		const result = await probe();
		if (result !== undefined) return result;
		if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
		await sleep(500);
	}
}

// The book is read through its own instructions, never through its bytes.
// The market account's layout belongs to the CLOB. A hand copy of that layout
// here went stale as soon as the book grew a field. A stale copy reads live
// orders as zeros, which looks like an order that never rested rather than
// like a decoding bug.
//
// `quote_l3_v0` reports one row per resting order, best price first, so the
// counts and both tops of book come from it. It is simulated like any
// read-only leg. Nothing lands and the book is untouched.

const CLOB_NODE_LEN = 104;

type ClobView = {
	bidCount: number;
	askCount: number;
	bestBidPrice?: BN;
	bestAskPrice?: BN;
};

type ClobRow = { price: BN; size: BN; orderId: BN };

/** The CLOB program's anchor wire (it has no TS client). */
const clobIx = {
	/**
	 * Bytes for a market account that holds at least `capacity` orders.
	 *
	 * The size is generous rather than exact. `initialize_market_v0` derives
	 * the arena's capacity from the account's length, so a header that grows
	 * costs this book a few slots instead of sizing it wrong. Restating the
	 * header's width here is what used to go stale.
	 */
	space(capacity: number): number {
		return 32 * 1024 + capacity * CLOB_NODE_LEN;
	},
	/**
	 * A soft eviction cap for a book sized by `space(capacity)`.
	 *
	 * Both sides share one arena, so `initialize_market_v0` requires the cap to
	 * be under half the capacity it derived: two sides at the cap have to fit.
	 * A quarter leaves the suite far more headroom than it ever uses and stays
	 * valid however the header grows.
	 */
	evictThreshold(capacity: number): number {
		return Math.floor(capacity / 4);
	},
};

type SplineLevel = { offsetPpm: number; size: BN };

/** The midpoint program's anchor wire (it has no TS client either). */
const midpointIx = {
	instance(maker: PublicKey): PublicKey {
		return PublicKey.findProgramAddressSync(
			[Buffer.from('midpoint'), u16(0), maker.toBuffer(), u16(0)],
			MIDPOINT_ID
		)[0];
	},
};

/** The relay program's anchor wire (it has no TS client either). */
const relayIx = {
	/** `register_watch_v0` over a velocity condition block. The block is always
	 * the account's first field, so the offset is 8 (past anchor's
	 * discriminator). Permissionless on relay's side. */
	registerWatch(
		payer: PublicKey,
		target: PublicKey,
		watch: PublicKey,
		blockOffset: number
	): TransactionInstruction {
		return new TransactionInstruction({
			programId: RELAY_ID,
			keys: [signerRo(payer), ro(target), rw(watch)],
			data: Buffer.concat([
				ixDiscriminator('register_watch_v0'),
				u32(blockOffset),
			]),
		});
	},
};

const CLOB_IDL = JSON.parse(
	fs.readFileSync(path.join(__dirname, 'idl', 'clob.json'), 'utf8')
);
const MIDPOINT_IDL = JSON.parse(
	fs.readFileSync(path.join(__dirname, 'idl', 'midpoint.json'), 'utf8')
);

describe('e2e localnet: programs + publisher + redis', function () {
	const connection = new Connection(RPC_URL, 'confirmed');
	const payer = Keypair.generate();
	const provider = new anchor.AnchorProvider(connection, new Wallet(payer), {
		commitment: 'confirmed',
		preflightCommitment: 'confirmed',
	});
	// Generated clients for the CLOB and midpoint. Their wire is the fork's
	// wincode, which is byte-identical to borsh for these fixed and
	// option/vec-of-fixed args, so the upstream Program coder encodes them
	// correctly. See tests/e2e/idl and anchor-v2/scripts/gen-quoter-idls.sh.
	const clobProgram = new anchor.Program(CLOB_IDL as anchor.Idl, provider);
	const midpointProgram = new anchor.Program(
		MIDPOINT_IDL as anchor.Idl,
		provider
	);

	const clobMakerKp = Keypair.generate();
	/** A second maker on the book. A fill can then carry one maker and not the
	 * other, which is the only way to reach the withheld path. */
	const clobMaker2Kp = Keypair.generate();
	const midMakerKp = Keypair.generate();
	const midConfigKp = Keypair.generate();
	const midHotKp = Keypair.generate();
	const bookMaker2Kp = Keypair.generate();
	const takerKp = Keypair.generate();
	const crosserKp = Keypair.generate();
	const publisherKp = Keypair.generate();

	let admin: TestClient;
	let clobMaker: TestClient;
	let clobMaker2: TestClient;
	let midMaker: TestClient;
	let bookMaker2: TestClient;
	let taker: TestClient;
	let crosser: TestClient;
	const clients: TestClient[] = [];

	let usdcMint: Keypair;
	let oracle: PublicKey;
	let pythProgram: Program;

	let clobBook: Keypair;
	let clobEntry: PublicKey;
	let conditions: PublicKey;
	let midInstance: PublicKey;
	let midEntry: PublicKey;
	let velocitySigner: PublicKey;
	/** Resolved during bring-up, so that `routerTail` stays synchronous. */
	let statePdaCache: PublicKey;
	let protocolUser: PublicKey;
	let protocolUserStats: PublicKey;

	/** Every spawned service, with the log fd `after` has to close. */
	const services: { child: ChildProcess; log: number }[] = [];
	/** The retail-flow key that swift signs attestations with. It is
	 * registered on chain as `State.hot_flow_authority`. */
	const flowAuthorityKp = Keypair.generate();
	/** Where relay pays its keeper. It is a plain account that never signs,
	 * which the turner requires before it cranks an untrusted program.
	 * Velocity carries no trust flag here, so the turner treats it exactly as
	 * a third-party turner would. */
	const relayPayout = Keypair.generate();
	const turnerKeeper = Keypair.generate();
	let redis: Redis;
	let oracleRefresher: Promise<void> | undefined;
	let stopOracleRefresher = false;
	/** What the background feed posts. A live oracle is the only way to
	 * move price on a real validator, so tests set this and wait. */
	let oracleTargetPrice = 100;

	const perpMarket = getPerpMarketPublicKeySync(VELOCITY_ID, 0);
	/** The market's quoter slab: the one account fills read approved quoter
	 * configs from. The book's config lives at its slot 0. */
	const quoterSlab = getQuoterSlabPublicKey(VELOCITY_ID, 0);
	const userOf = (authority: PublicKey) =>
		getUserAccountPublicKeySync(VELOCITY_ID, authority, 0);
	const statsOf = (authority: PublicKey) =>
		getUserStatsAccountPublicKey(VELOCITY_ID, authority);
	/** A market's quoter staging entry: `["quoter", market, program, user]`.
	 * A CLOB entry is shared, so its user is `PublicKey.default`. Fills read
	 * the slab copy. The entry stays the quoter's identity, so signed routes
	 * and the canonical-CLOB attach still name it. */
	const quoterKey = (quoterProgram: PublicKey, user: PublicKey) =>
		PublicKey.findProgramAddressSync(
			[
				Buffer.from('quoter'),
				u16(0),
				quoterProgram.toBuffer(),
				user.toBuffer(),
			],
			VELOCITY_ID
		)[0];

	/** How long any send here waits for its signature.
	 *
	 * Nothing in this file confirms through `connection.confirmTransaction`.
	 * Its legacy strategy gives up after a fixed 30 seconds and reports only
	 * "unknown if it succeeded or failed", with no signature and no on-chain
	 * error. By the later scenarios this machine runs the validator alongside
	 * redis, the book-publisher, swift and a crank turner, so a transaction
	 * that did land often confirms past that mark.
	 *
	 * 60s is the validity window of the blockhash the transaction was signed
	 * with, which is 150 slots. Past it the RPC has stopped rebroadcasting, so
	 * a signature still missing is missing for good, and waiting longer only
	 * delays the report. */
	const CONFIRM_TIMEOUT_MS = 60_000;

	/** Wait for `signature` by polling its status, reporting the on-chain
	 * error if it reverted. */
	const confirmSignature = async (
		signature: string,
		what = `tx ${signature}`
	) =>
		pollUntil(`${what} to confirm`, CONFIRM_TIMEOUT_MS, async () => {
			const status = (await connection.getSignatureStatuses([signature]))
				.value[0];
			if (!status) return undefined;
			if (status.err) {
				const logs = await connection
					.getTransaction(signature, {
						commitment: 'confirmed',
						maxSupportedTransactionVersion: 0,
					})
					.catch(() => null);
				throw new Error(
					`${what} reverted: ${JSON.stringify(status.err)}\n${(
						logs?.meta?.logMessages ?? ['no logs']
					)
						.slice(-10)
						.join('\n')}`
				);
			}
			// Every account fetch after a send reads at 'confirmed', so a
			// status of 'processed' is not yet enough.
			return status.confirmationStatus === 'processed' ? undefined : status;
		});

	const send = async (
		ixs: TransactionInstruction[],
		signers: Keypair[] = []
	): Promise<string> => {
		const tx = new Transaction().add(...ixs);
		tx.feePayer = payer.publicKey;
		tx.recentBlockhash = (
			await connection.getLatestBlockhash('confirmed')
		).blockhash;
		tx.sign(payer, ...signers);
		// Preflight stays on. A reverting setup transaction returns its program
		// logs there, which is more than a status lookup can recover.
		const signature = await connection.sendRawTransaction(tx.serialize(), {
			preflightCommitment: 'confirmed',
		});
		await confirmSignature(signature);
		return signature;
	};

	/** The `User` accounts one transaction settled a maker fill for.
	 *
	 * The answer comes from the transaction's own records, which is the only
	 * way to ask what a fill did. An account balance answers what is true now,
	 * and cranks keep working while a test looks.
	 */
	const makersFilledBy = async (signature: string): Promise<string[]> => {
		const tx = await connection.getTransaction(signature, {
			commitment: 'confirmed',
			maxSupportedTransactionVersion: 0,
		});
		const logs = tx?.meta?.logMessages ?? [];
		return parseLogs(admin.program, logs)
			.filter((event) => event.name === 'orderActionRecord')
			.map((event) => (event.data as { maker?: PublicKey }).maker)
			.filter((maker): maker is PublicKey => maker != null)
			.map((maker) => maker.toBase58());
	};

	/** The maker to base fills one transaction settled, read off its
	 * `OrderActionRecord`s. This is exact and race-free, because it reads the
	 * transaction's own effect rather than post-state a later crank can move.
	 * The record's `taker` and `maker` are User PDAs. */
	const fillsBy = async (signature: string) => {
		const tx = await connection.getTransaction(signature, {
			commitment: 'confirmed',
			maxSupportedTransactionVersion: 0,
		});
		const logs = tx?.meta?.logMessages ?? [];
		return parseLogs(admin.program, logs)
			.filter((event) => event.name === 'orderActionRecord')
			.map(
				(event) =>
					event.data as {
						taker?: PublicKey | null;
						maker?: PublicKey | null;
						baseAssetAmountFilled?: BN | null;
					}
			)
			.filter((r) => r.baseAssetAmountFilled != null);
	};

	/** Base a maker settled in one transaction (exact, race-free). */
	const makerFilledBase = async (
		signature: string,
		makerUser: PublicKey
	): Promise<BN> =>
		(await fillsBy(signature))
			.filter((r) => r.maker != null && r.maker.equals(makerUser))
			.reduce((sum, r) => sum.add(r.baseAssetAmountFilled!), new BN(0));

	/** Base a taker settled in one transaction (exact, race-free). */
	const takerFilledBase = async (
		signature: string,
		takerUser: PublicKey
	): Promise<BN> =>
		(await fillsBy(signature))
			.filter(
				(r) => r.maker != null && r.taker != null && r.taker.equals(takerUser)
			)
			.reduce((sum, r) => sum.add(r.baseAssetAmountFilled!), new BN(0));

	/** Fills route through every quoter, well past the default CU budget. */
	const sendFill = (ix: TransactionInstruction, signers: Keypair[] = []) =>
		send(
			[ComputeBudgetProgram.setComputeUnitLimit({ units: 800_000 }), ix],
			signers
		);

	/** `SystemProgram.createAccount`, rent looked up for `space`. */
	const createAccount = async (
		newAccount: PublicKey,
		space: number,
		programId: PublicKey
	) =>
		SystemProgram.createAccount({
			fromPubkey: payer.publicKey,
			newAccountPubkey: newAccount,
			lamports: await connection.getMinimumBalanceForRentExemption(space),
			space,
			programId,
		});

	/** Start a service with its stdio in `$SCRATCH/<name>.log`, and register it
	 * for teardown. A non-zero exit is reported but never fails a test on its
	 * own. The scenarios' on-chain assertions are the verdict. */
	const startService = (
		name: string,
		bin: string,
		args: string[],
		env: Record<string, string>
	): ChildProcess => {
		const log = fs.openSync(`${SCRATCH}/${name}.log`, 'w');
		const child = spawn(bin, args, {
			env: { ...process.env, ...env },
			stdio: ['ignore', log, log],
		});
		child.on('exit', (code) => {
			if (code !== null && code !== 0) {
				console.error(`${name} exited ${code} — see ${SCRATCH}/${name}.log`);
			}
		});
		services.push({ child, log });
		return child;
	};

	/** A keypair on disk, the way every one of these services takes one. */
	const writeKeypair = (name: string, kp: Keypair): string => {
		const path = `${SCRATCH}/${name}.json`;
		fs.writeFileSync(path, JSON.stringify(Array.from(kp.secretKey)));
		return path;
	};

	/** Move the feed and wait until the change is on chain. */
	const setOraclePrice = async (price: number) => {
		oracleTargetPrice = price;
		await pollUntil(`oracle to reach ${price}`, 30_000, async () => {
			const info = await connection.getAccountInfo(oracle);
			if (!info) return undefined;
			const onChain =
				Number(info.data.readBigInt64LE(PYTH_AGG_PRICE_OFFSET)) / 1e6;
			return Math.abs(onChain - price) < 0.5 ? true : undefined;
		});
	};

	const airdrop = async (to: PublicKey, sol: number) => {
		const sig = await connection.requestAirdrop(to, sol * LAMPORTS_PER_SOL);
		await confirmSignature(sig, `airdrop to ${to.toBase58()}`);
	};

	const createUsdcMint = async (): Promise<Keypair> => {
		const mint = Keypair.generate();
		await send(
			[
				await createAccount(mint.publicKey, MintLayout.span, TOKEN_PROGRAM_ID),
				createInitializeMintInstruction(
					mint.publicKey,
					6,
					payer.publicKey,
					payer.publicKey
				),
			],
			[mint]
		);
		return mint;
	};

	const fundUsdc = async (owner: PublicKey, amount: BN): Promise<PublicKey> => {
		const account = Keypair.generate();
		await send(
			[
				await createAccount(
					account.publicKey,
					AccountLayout.span,
					TOKEN_PROGRAM_ID
				),
				createInitializeAccountInstruction(
					account.publicKey,
					usdcMint.publicKey,
					owner
				),
				createMintToInstruction(
					usdcMint.publicKey,
					account.publicKey,
					payer.publicKey,
					BigInt(amount.toString())
				),
			],
			[account]
		);
		return account.publicKey;
	};

	const newClient = (kp: Keypair): TestClient => {
		const client = new TestClient({
			connection,
			wallet: new Wallet(kp),
			programID: VELOCITY_ID,
			opts: { commitment: 'confirmed' },
			activeSubAccountId: 0,
			perpMarketIndexes: [0],
			spotMarketIndexes: [0],
			subAccountIds: [],
			oracleInfos: [{ publicKey: oracle, source: OracleSource.PYTH }],
			accountSubscription: {
				type: 'polling',
				accountLoader: new BulkAccountLoader(connection, 'confirmed', 500),
			},
		});
		// The SDK's default sender stops waiting after 35s. A loaded validator
		// can take longer, so hold the sender to this file's budget.
		(client.txSender as RetryTxSender).timeout = CONFIRM_TIMEOUT_MS;
		clients.push(client);
		return client;
	};

	/** The margin map every router-touching instruction opens with. It holds
	 * market 0's oracle, its quote spot market, and the perp market itself. */
	const marginMap = (): AccountMeta[] =>
		admin.getRemainingAccounts({
			userAccounts: [],
			writablePerpMarketIndexes: [0],
			writableSpotMarketIndexes: [0],
		});

	/** Register and approve a quoter, the sequence `admin-cli quoter` runs. It
	 * creates the registry entry, publishes the quoter's unified CPI account
	 * list, then has the admin approve the surface into the market's slab. */
	const registerQuoterIxs = async (args: {
		authority: PublicKey;
		quoterType: QuoterType;
		quoterProgram: PublicKey;
		responseAccount: PublicKey;
		user: PublicKey;
		/** The optional leg that reports who a ladder stands on. A book has
		 * one. A quoter that fills from its own account does not. */
		quoteL3Discriminator?: number[];
		/** The unified registered account list. Each leg names its slice by
		 * index into this list. */
		metas: { pubkey: PublicKey; isWritable: boolean }[];
		quoteIndexes: number[];
		executeIndexes: number[];
	}): Promise<{ quoter: PublicKey; ixs: TransactionInstruction[] }> => {
		const program = admin.program;
		const quoter = quoterKey(args.quoterProgram, args.user);
		return {
			quoter,
			ixs: [
				program.instruction.initializeQuoter(
					{
						marketIndex: 0,
						quoterType: args.quoterType,
						responseAccount: args.responseAccount,
						quoteV0Discriminator: Array.from(ixDiscriminator('quote_v0')),
						quoteL3V0Discriminator:
							args.quoteL3Discriminator ?? new Array(8).fill(0),
						executeV0Discriminator: Array.from(ixDiscriminator('execute_v0')),
					},
					{
						accounts: {
							state: statePdaCache,
							payer: payer.publicKey,
							authority: args.authority,
							quoter,
							perpMarket,
							// Designating the book is refused when an approved
							// quoter's account list already names it, so a book
							// registration reads the slab. No other type does.
							// Anchor's client reads a null as a missing account even
							// for an optional one, so an omitted slab is passed
							// as the program id, which the program decodes as
							// `None`.
							quoterSlab:
								args.quoterType === QuoterType.CLOB ? quoterSlab : VELOCITY_ID,
							quoterProgram: args.quoterProgram,
							user: args.user,
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
				),
				program.instruction.updateQuoterAccounts(
					{
						metas: args.metas,
						quoteIndexes: Buffer.from(args.quoteIndexes),
						executeIndexes: Buffer.from(args.executeIndexes),
					},
					{
						accounts: {
							authority: args.authority,
							quoter,
							// A book's entry answers to the State admin roles. A
							// Custom entry answers to its own stored authority and
							// ignores this account.
							state: statePdaCache,
						},
					}
				),
				program.instruction.updateQuoterApproved(
					{ approved: true },
					{
						accounts: {
							admin: payer.publicKey,
							state: await admin.getStatePublicKey(),
							quoter,
							perpMarket,
							// Approval copies the staging config into the slab slot
							// that fills read, so the slab has to exist first.
							// Approval also grows the slab to fit the slot, which
							// is why the system program is here.
							quoterSlab,
							// Approval approves a binary, so the program has to be
							// frozen. Its program-data account says whether it is.
							// The harness deploys these programs non-upgradeable.
							quoterProgram: args.quoterProgram,
							quoterProgramData: PublicKey.findProgramAddressSync(
								[args.quoterProgram.toBuffer()],
								BPF_LOADER_UPGRADEABLE_ID
							)[0],
							// A book approval asks the book for its own placement
							// rules, so a slot that would fail every fill is
							// refused here. No other type reads a book.
							// As above: an omitted optional account travels as the
							// program id, because anchor's client reads a null as
							// missing.
							clobMarket:
								args.quoterType === QuoterType.CLOB
									? args.responseAccount
									: VELOCITY_ID,
							systemProgram: SystemProgram.programId,
						},
					}
				),
			],
		};
	};

	/** CLOB bring-up, exactly as `admin-cli clob-market init` does it. */
	const clobBringUp = async () => {
		const clobCapacity = 1024;
		const space = clobIx.space(clobCapacity);
		clobBook = Keypair.generate();
		// The market is already initialized. The book must use the market's grid.
		await admin.fetchAccounts();
		const market = admin.getPerpMarketAccount(0)!;
		await send(
			[
				await createAccount(clobBook.publicKey, space, CLOB_ID),
				await clobProgram.methods
					.initializeMarketV0({
						marketIndex: 0,
						basePrecision: UNIT,
						orderTickSize: market.orderTickSize,
						orderStepSize: market.orderStepSize,
						minOrderSize: market.orderStepSize,
						blockingMinSize: new BN(0),
						defaultActivationDelaySlots: 0,
						maxActivationDelaySlots: 20,
						unknownUserGraceSlots: 2,
						evictThresholdPerSide: clobIx.evictThreshold(clobCapacity),
						maxQuoteLevels: 128,
						maxExecuteFills: 64,
						maxExecuteUsers: 32,
					})
					.accountsStrict({
						authority: payer.publicKey,
						// The market's slab is the identity velocity signs every
						// external quoter CPI as, the book's included.
						placeAuthority: quoterSlab,
						market: clobBook.publicKey,
					})
					.instruction(),
			],
			[clobBook]
		);

		const registration = await registerQuoterIxs({
			authority: payer.publicKey,
			quoterType: QuoterType.CLOB,
			quoterProgram: CLOB_ID,
			responseAccount: clobBook.publicKey,
			user: PublicKey.default,
			// The book reports who rests on it, so no reader decodes its bytes.
			quoteL3Discriminator: Array.from(ixDiscriminator('quote_l3_v0')),
			// The quote leg reads the book. The execute leg also carries the
			// quoter slab that the book checks velocity's CPI signature against.
			metas: [
				{ pubkey: clobBook.publicKey, isWritable: true },
				{ pubkey: quoterSlab, isWritable: false },
			],
			quoteIndexes: [0],
			executeIndexes: [0, 1],
		});
		clobEntry = registration.quoter;
		await send([
			// Registration reads the market's slab and approval writes it. This
			// is the market's first registration, so the slab is created here.
			// Slot 0 is the book's. The Custom quoters land on slots 1 and up.
			admin.program.instruction.initializeQuoterSlab(
				{ marketIndex: 0 },
				{
					accounts: {
						payer: payer.publicKey,
						perpMarket,
						quoterSlab,
						rent: SYSVAR_RENT_PUBKEY,
						systemProgram: SystemProgram.programId,
					},
				}
			),
			...registration.ixs,
		]);

		// Attach as the market's canonical CLOB. The conditions account is
		// created holding only its rent, the way a market is attached in
		// production. The refill crank fills the reservoir from the treasury,
		// so nothing here seeds it by hand.
		conditions = getClobCrankConditionsPublicKey(VELOCITY_ID, 0);
		await send([
			admin.program.instruction.updatePerpMarketClobQuoter(
				{
					// Cost units each crank requests. The lamport payments come
					// from these and from State.transactionFeeRails, which
					// `initialize` sets to a flat fee per signature. On this
					// harness every crank pays that flat fee whatever it asks
					// for.
					crankCostUnits: {
						removal: 30_000,
						cross: 180_000,
						takerOriginCross: 190_000,
						trigger: 40_000,
						liquidation: 120_000,
						forceCancel: 60_000,
						refill: 30_000,
					},
					expireFallbackSlots: new BN(1500), // cross fallback poll interval
					// The attach requires a floor above zero, so this is the
					// smallest floor there is. The cross-match scenario below
					// asserts only that a cross has to be profitable at all.
					minCrossSurplus: new BN(1),
				},
				{
					accounts: {
						admin: payer.publicKey,
						state: await admin.getStatePublicKey(),
						perpMarket,
						quoter: clobEntry,
						quoterSlab,
						clobMarket: clobBook.publicKey,
						clobProgram: CLOB_ID,
						crankConditions: conditions,
						treasury: getCrankTreasuryPublicKey(VELOCITY_ID),
						rent: SYSVAR_RENT_PUBKEY,
						systemProgram: SystemProgram.programId,
					},
				}
			),
		]);
	};

	/** Midpoint instance + spline, then its Custom registry entry. */
	const midpointBringUp = async () => {
		midInstance = midpointIx.instance(midMakerKp.publicKey);
		midEntry = quoterKey(MIDPOINT_ID, userOf(midMakerKp.publicKey));
		await send(
			[
				await midpointProgram.methods
					.initializeQuoterV0({
						marketIndex: 0,
						userSubAccountId: 0,
						basePrecision: UNIT,
						maxMidStalenessSlots: new BN(1000),
						priceTickSize: new BN(100),
						sizeStep: new BN(100000),
						minQuoteSize: new BN(100000),
						requireAttestedFlow: false,
						// The oracle-deviation band, which a fresh instance must
						// carry: without it the instance quotes unprotected. The
						// suite runs it wide, because some cases move the oracle
						// far from the mid on purpose and the band is not what
						// they are testing.
						maxMidDeviationPpm: new BN(300_000),
					})
					.accountsStrict({
						payer: payer.publicKey,
						authority: midConfigKp.publicKey,
						userAuthority: midMakerKp.publicKey,
						// The market's slab is the identity velocity signs every
						// external quoter CPI as.
						executeAuthority: quoterSlab,
						hotAuthority: midHotKp.publicKey,
						quoter: midInstance,
						systemProgram: SystemProgram.programId,
					})
					.instruction(),
			],
			[midConfigKp, midMakerKp]
		);
		// Spline: 10bps / 30bps rungs, one unit each, mid $100.
		await setMidpointLevels(usd(100), [
			{ offsetPpm: 1000, size: UNIT },
			{ offsetPpm: 3000, size: UNIT },
		]);

		const registration = await registerQuoterIxs({
			authority: midMakerKp.publicKey,
			quoterType: QuoterType.CUSTOM,
			quoterProgram: MIDPOINT_ID,
			responseAccount: midInstance,
			user: userOf(midMakerKp.publicKey),
			// The protected-flow fact rides the quoter wire
			// (`taker_served_window`), so the legs carry no sysvar and no
			// velocity State.
			metas: [
				{ pubkey: midInstance, isWritable: true },
				{ pubkey: quoterSlab, isWritable: false },
			],
			quoteIndexes: [0],
			executeIndexes: [0, 1],
		});
		await send(registration.ixs, [midMakerKp]);
	};

	const setMidpointMid = async (mid: BN) =>
		send(
			[
				await midpointProgram.methods
					.setMidV0({ mid, sequence: new BN(0) })
					.accountsStrict({
						quoter: midInstance,
						hotAuthority: midHotKp.publicKey,
					})
					.instruction(),
			],
			[midHotKp]
		);

	const setMidpointLevels = async (mid: BN, levels: SplineLevel[]) => {
		const rungs = levels.map((l) => ({
			offsetPpm: new BN(l.offsetPpm),
			size: l.size,
		}));
		return send(
			[
				await midpointProgram.methods
					.setLevelsV0({ mid, sequence: null, bids: rungs, asks: rungs })
					.accountsStrict({
						quoter: midInstance,
						hotAuthority: midHotKp.publicKey,
					})
					.instruction(),
			],
			[midHotKp]
		);
	};

	/** The protocol-owned User (velocity signer's sub-account 0) for cranks. */
	const initProtocolUser = async () => {
		protocolUser = userOf(velocitySigner);
		protocolUserStats = statsOf(velocitySigner);
		const program = admin.program;
		// The SDK's initializeUser builders only ever act for their own
		// wallet's authority. This User's authority is the velocity signer.
		const shared = {
			state: await admin.getStatePublicKey(),
			authority: velocitySigner,
			payer: payer.publicKey,
			rent: SYSVAR_RENT_PUBKEY,
			systemProgram: SystemProgram.programId,
		};
		await send([
			program.instruction.initializeUserStats({
				accounts: { userStats: protocolUserStats, ...shared },
			}),
			program.instruction.initializeUser(0, Array.from(Buffer.alloc(32, ' ')), {
				accounts: {
					user: protocolUser,
					userStats: protocolUserStats,
					...shared,
					// The account is optional, but anchor still wants it named.
					// The PDA gives the protocol user relay coverage like any
					// other user.
					userConditions: getUserConditionsPublicKey(VELOCITY_ID, protocolUser),
				},
			}),
		]);
	};

	const placeClobOrder = async (
		client: TestClient,
		kp: Keypair,
		direction: PositionDirection,
		price: BN,
		size: BN,
		maxTs: BN = new BN(0)
	) => {
		// Maker rests go through `place_and_make_perp_order_v1`, the general
		// maker-rest verb. A plain limit with post-only `None` rests even when
		// crossed, which is what these CLOB makers want. Every call site passes
		// `kp`'s own client, so the SDK method's `this.wallet` is `kp`.
		assert.isTrue(
			client.wallet.publicKey.equals(kp.publicKey),
			'placeClobOrder expects kp to own the client'
		);
		const ix = await client.getPlaceAndMakePerpOrderIx(
			getLimitOrderParams({
				marketIndex: 0,
				direction,
				baseAssetAmount: size,
				price,
				postOnly: PostOnlyParams.NONE,
				...(maxTs.isZero() ? {} : { maxTs }),
			}),
			{
				quoterSlab,
				clobMarket: clobBook.publicKey,
				clobProgram: CLOB_ID,
			}
			// activationDelaySlots is omitted, so the book's default applies
			// and there is no attestation.
		);
		await client.sendTransaction(new Transaction().add(ix));
	};

	/**
	 * The orders resting on one side, as the book itself reports them.
	 *
	 * The read is simulated. `quote_l3_v0` writes its rows into the market's
	 * own response tail and returns a pointer to them, so the answer comes out
	 * of the simulated post-state and nothing lands.
	 */
	const readClobSide = async (direction: 0 | 1): Promise<ClobRow[]> => {
		const message = new TransactionMessage({
			payerKey: payer.publicKey,
			recentBlockhash: (await connection.getLatestBlockhash()).blockhash,
			instructions: [
				await clobProgram.methods
					.quoteL3V0({
						direction: direction === 0 ? { long: {} } : { short: {} },
						size: new BN(0),
						maxRows: 128,
					})
					.accountsStrict({ market: clobBook.publicKey })
					.instruction(),
			],
		}).compileToV0Message();
		const sim = await connection.simulateTransaction(
			new VersionedTransaction(message),
			{
				sigVerify: false,
				accounts: {
					encoding: 'base64',
					addresses: [clobBook.publicKey.toBase58()],
				},
			}
		);
		assert.isNull(
			sim.value.err,
			`quote_l3_v0 simulation failed: ${JSON.stringify(
				sim.value.err
			)} ${JSON.stringify(sim.value.logs?.slice(-4))}`
		);
		// The return data is a `ResponsePointerV0 { offset: u32, len: u32 }`.
		// The rows themselves are in the account it points into.
		const pointer = Buffer.from(sim.value.returnData!.data[0], 'base64');
		const offset = pointer.readUInt32LE(0);
		const length = pointer.readUInt32LE(4);
		const account = Buffer.from(
			sim.value.accounts![0]!.data[0] as string,
			'base64'
		);
		const region = account.subarray(offset, offset + length);
		// `L3ResponseV0` is a row sequence, then the `more` flag. The sequence
		// prefix is `quoter_spec::LEN_BYTES`, which is eight bytes rather than
		// four. The records that follow then stay 8-byte aligned, so both
		// programs can cast them in place.
		const LEN_BYTES = 8;
		const ROW = 64; // L3RowV0: price, size, order_id, user(34), flags, pad
		const rows = region.readUInt32LE(0);
		return Array.from({ length: rows }, (_, i) => {
			const at = LEN_BYTES + i * ROW;
			return {
				price: new BN(region.subarray(at, at + 8), 'le'),
				size: new BN(region.subarray(at + 8, at + 16), 'le'),
				orderId: new BN(region.subarray(at + 16, at + 24), 'le'),
			};
		});
	};

	const readClob = async (): Promise<ClobView> => {
		// A buyer consumes the asks, a seller the bids.
		const [asks, bids] = [await readClobSide(0), await readClobSide(1)];
		return {
			bidCount: bids.length,
			askCount: asks.length,
			bestBidPrice: bids[0]?.price,
			bestAskPrice: asks[0]?.price,
		};
	};

	const registerWatch = async (target: PublicKey, blockOffset = 8) => {
		const watch = Keypair.generate();
		await send(
			[
				await createAccount(watch.publicKey, WATCH_V0_LEN, RELAY_ID),
				relayIx.registerWatch(
					payer.publicKey,
					target,
					watch.publicKey,
					blockOffset
				),
			],
			[watch]
		);
		return watch.publicKey;
	};

	const startTurner = () =>
		startService(
			'turner',
			TURNER_BIN,
			[
				'--rpc-url',
				RPC_URL,
				'--keypair',
				writeKeypair('turner-keeper', turnerKeeper),
				'--program-id',
				RELAY_ID.toBase58(),
				// Scoped as an operator would run it, and to both programs
				// that host this market's conditions. A condition's wake lives
				// on the account whose state it describes, so the book holds
				// the four that describe the book itself, and the CLOB owns
				// that account. A turner allowed only velocity drops those
				// watches at the registry query and never cranks them.
				'--target-program',
				`${VELOCITY_ID.toBase58()},${CLOB_ID.toBase58()}`,
				// Untrusted mode: velocity gets no trust flag, so relay
				// insists on a non-signing payout account and refuses any
				// executor that names a signer.
				'--payout-address',
				relayPayout.publicKey.toBase58(),
				'--tick-ms',
				'400',
				'--refresh-ticks',
				'5',
			],
			{ RUST_LOG: 'relay_crank_turner=debug,info' }
		);

	/** Create + activate an address lookup table over `addresses`. */
	const createRouterLookupTable = async (addresses: PublicKey[]) => {
		const slot = await connection.getSlot('finalized');
		const [createIx, tableAddress] =
			AddressLookupTableProgram.createLookupTable({
				authority: payer.publicKey,
				payer: payer.publicKey,
				recentSlot: slot,
			});
		const unique = [...new Set(addresses.map((a) => a.toBase58()))].map(
			(a) => new PublicKey(a)
		);
		await send([
			createIx,
			AddressLookupTableProgram.extendLookupTable({
				payer: payer.publicKey,
				authority: payer.publicKey,
				lookupTable: tableAddress,
				addresses: unique,
			}),
		]);
		// Addresses added in slot N only resolve from slot N+1. The account
		// reads back at once, but a transaction that uses it before the slot
		// turns over fails with "invalid index" at load time.
		const extendedAt = await connection.getSlot();
		await pollUntil('lookup table to activate', 30_000, async () => {
			const slotNow = await connection.getSlot();
			return slotNow > extendedAt + 1 ? slotNow : undefined;
		});
		return await pollUntil('lookup table to be readable', 30_000, async () => {
			const fetched = await connection.getAddressLookupTable(tableAddress);
			return fetched.value &&
				fetched.value.state.addresses.length === unique.length
				? fetched.value
				: undefined;
		});
	};

	const startSwift = async () => {
		// Swift's RPC simulation uses a fixed fee payer that never signs. The
		// account must still exist and hold SOL. In production a gas station
		// maintains it.
		await airdrop(
			new PublicKey('feezFJywCs7LZXXi6dyLKpr3XKgtf7KXXKZ2y6vzTSQ'),
			1
		);
		startService('swift', SWIFT_BIN, ['--server', 'swift'], {
			ENV: 'devnet',
			ENDPOINT: RPC_URL,
			WS_ENDPOINT_1: RPC_URL.replace('http', 'ws').replace('8899', '8900'),
			ELASTICACHE_HOST: '127.0.0.1',
			ELASTICACHE_PORT: new URL(REDIS_URL).port || '6379',
			PORT: String(SWIFT_PORT),
			METRICS_PORT: '9469',
			FLOW_AUTHORITY_KEYPAIR: JSON.stringify(
				Array.from(flowAuthorityKp.secretKey)
			),
			// Long enough that the scenario observes the too-early
			// response. Short enough that the signed message's slot
			// window survives the round trip.
			ATTESTATION_HOLD_MS: '600',
			// Intake's pre-flight RPC simulation is a production
			// admission guard and is outside the attestation loop. It
			// needs the client's devnet market accounts, which a
			// freshly initialized localnet does not have. The verdict
			// here is the on-chain fill at the end.
			DISABLE_RPC_SIM: 'true',
			RUST_LOG: 'info',
		});
		await pollUntil('swift to serve /health', 60_000, async () => {
			try {
				const res = await fetch(`${SWIFT_URL}/health`);
				return res.ok ? true : undefined;
			} catch {
				return undefined;
			}
		});
	};

	/** Lamports relay has paid its keeper. This is the proof that a crank came
	 * from the turner rather than from this test or the publisher, which pay
	 * their own authorities instead. */
	const relayPayoutBalance = () => connection.getBalance(relayPayout.publicKey);

	/** The account tail every router-touching instruction wants. It holds the
	 * margin maps, then the `(User, UserStats)` pairs of the makers that may
	 * fill, then the quoter section. The quoter section is the market's slab
	 * followed by the CPI accounts of the consulted quoters. A slot is
	 * consulted when its response account is in the tail. */
	const routerTail = (makerKps: Keypair[]): AccountMeta[] => [
		...marginMap(),
		...makerKps.flatMap((kp) => [
			rw(userOf(kp.publicKey)),
			rw(statsOf(kp.publicKey)),
		]),
		ro(quoterSlab),
		rw(clobBook.publicKey),
		ro(CLOB_ID),
		rw(midInstance),
		ro(SYSVAR_INSTRUCTIONS_PUBKEY),
		// Midpoint reads the live flow authority off velocity's State on both
		// legs. The instruction's own `state` account is not in the CPI account
		// map, because that map is built from the remaining accounts. So State
		// appears again here, which costs one index byte.
		ro(statePdaCache),
		ro(MIDPOINT_ID),
	];

	/** Place a taker order and route it in one instruction, with the router tail
	 * the SDK's builder cannot express: that builder has no quoter section, and
	 * it marks the quote spot market read-only.
	 *
	 * This is how a position opens on a real validator. The placement routes the
	 * order as it places it, so there is no separate fill to send. */
	const placeAndTakeIx = async (
		takerAuthority: PublicKey,
		orderParams: OptionalOrderParams,
		makerKps: Keypair[] = [clobMakerKp, bookMaker2Kp, midMakerKp]
	) =>
		admin.program.instruction.placeAndTakePerpOrderV1(
			{
				params: getOrderParams(orderParams, { marketType: MarketType.PERP }),
				successCondition: null,
			},
			{
				accounts: {
					state: await admin.getStatePublicKey(),
					user: userOf(takerAuthority),
					userStats: statsOf(takerAuthority),
					authority: takerAuthority,
					quoterSlab,
					clobMarket: clobBook.publicKey,
					clobProgram: CLOB_ID,
					// The taker signs this transaction, so no separate flow
					// attestation is needed. Anchor reads the program id as
					// `None`.
					flowAuthority: VELOCITY_ID,
				},
				remainingAccounts: routerTail(makerKps),
			}
		);

	/** Open a position by placing a taker order that routes as it places. */
	const placeAndTake = async (
		kp: Keypair,
		orderParams: OptionalOrderParams,
		makerKps: Keypair[] = [clobMakerKp, bookMaker2Kp, midMakerKp]
	) =>
		sendFill(await placeAndTakeIx(kp.publicKey, orderParams, makerKps), [kp]);

	/** Opt a user into relay coverage for liquidation thresholds and triggers.
	 * It is one instruction over one account. `extra` is appended to the
	 * condition pass's own accounts, for example the market's quoter slab for
	 * trigger routing. */
	const syncUserConditions = async (
		user: PublicKey,
		extra: AccountMeta[] = []
	) => {
		const ix = admin.program.instruction.syncUserConditions(
			{
				// Cost units the staged self-sync requests. The program derives
				// the lamport fee from State.transactionFeeRails and pays it
				// from this account's own lamports.
				syncCostUnits: 20_000,
				syncFallbackSlots: new BN(3000), // coarse fallback poll
			},
			{
				accounts: {
					payer: payer.publicKey,
					state: statePdaCache,
					user,
					userConditions: getUserConditionsPublicKey(VELOCITY_ID, user),
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				},
				// Margin maps, then the market's reservoir (keeper fee).
				remainingAccounts: [...marginMap(), ro(conditions), ...extra],
			}
		);
		return send([ix]);
	};

	before(async function () {
		this.timeout(600_000);

		for (const kp of [
			payer,
			clobMakerKp,
			clobMaker2Kp,
			midMakerKp,
			midHotKp,
			bookMaker2Kp,
			takerKp,
			crosserKp,
			publisherKp,
			turnerKeeper,
		]) {
			await airdrop(kp.publicKey, 100);
		}
		// The payout is rent-exempt but never signs. Relay credits it.
		await airdrop(relayPayout.publicKey, 1);

		velocitySigner = getVelocitySignerPublicKey(VELOCITY_ID);
		usdcMint = await createUsdcMint();

		// A $100 oracle through the pyth stub program. A test can move it on a
		// real validator, which lazer accounts do not allow because they need
		// signed posts.
		const pythIdl = JSON.parse(
			fs.readFileSync('target/idl/pyth.json', 'utf-8')
		);
		pythIdl.address = PYTH_ID.toBase58();
		anchor.setProvider(provider);
		pythProgram = new Program(pythIdl, provider);
		const feed = Keypair.generate();
		await send(
			[
				await createAccount(feed.publicKey, 3312, PYTH_ID),
				pythProgram.instruction.initialize(usd(100), -6, usd(0.01), {
					accounts: { price: feed.publicKey },
				}),
			],
			[feed]
		);
		oracle = feed.publicKey;

		// Protocol init through the real admin instructions.
		admin = newClient(payer);
		statePdaCache = await admin.getStatePublicKey();
		await admin.initialize(usdcMint.publicKey, true);

		// The program-wide resolver staging account. Every relay crank
		// simulates against it, so it exists before any watch is registered.
		await send([
			await admin.program.methods
				.initializeRelayScratch()
				.accounts({
					scratch: getRelayScratchPublicKey(VELOCITY_ID),
					payer: payer.publicKey,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				})
				.instruction(),
		]);
		await admin.subscribe();

		// The single treasury every market's crank reservoir refills from. The
		// CLOB crank resolver names it, so it exists before any condition can
		// be resolved. It is priced, then funded by a plain transfer. There is
		// no deposit instruction, because crediting lamports needs no program.
		await send([
			await admin.getInitializeCrankTreasuryIx(),
			await admin.getUpdateCrankTreasuryIx(1000, 100),
			SystemProgram.transfer({
				fromPubkey: payer.publicKey,
				toPubkey: getCrankTreasuryPublicKey(VELOCITY_ID),
				lamports: 5 * LAMPORTS_PER_SOL,
			}),
		]);

		await initializeQuoteSpotMarket(admin, usdcMint.publicKey);
		await admin.initializePerpMarket(
			0,
			oracle,
			new BN(1000).mul(new BN(10).pow(new BN(13))), // base reserve
			new BN(1000).mul(new BN(10).pow(new BN(13))), // quote reserve
			new BN(0), // periodicity
			new BN(100).mul(PEG_PRECISION),
			OracleSource.PYTH,
			undefined, // contract tier
			1000, // margin_ratio_initial
			500, // margin_ratio_maintenance
			undefined,
			undefined,
			undefined,
			true,
			20000, // base_spread (2%), which keeps the vAMM away from the touch
			50000 // max_spread
		);
		await admin.updatePerpAuctionDuration(0);

		await clobBringUp();
		await initProtocolUser();

		// Actors with deposits. The midpoint's quoted User must exist before
		// its Custom entry is created, because consent reads User.authority.
		clobMaker = newClient(clobMakerKp);
		clobMaker2 = newClient(clobMaker2Kp);
		midMaker = newClient(midMakerKp);
		bookMaker2 = newClient(bookMaker2Kp);
		taker = newClient(takerKp);
		crosser = newClient(crosserKp);
		for (const [client, kp] of [
			[clobMaker, clobMakerKp],
			[clobMaker2, clobMaker2Kp],
			[midMaker, midMakerKp],
			[bookMaker2, bookMaker2Kp],
			[taker, takerKp],
			[crosser, crosserKp],
		] as const) {
			await client.subscribe();
			const tokenAccount = await fundUsdc(
				kp.publicKey,
				new BN(100_000).mul(USDC)
			);
			await client.initializeUserAccountAndDepositCollateral(
				new BN(100_000).mul(USDC),
				tokenAccount
			);
		}

		await midpointBringUp();

		// The keeper's filler user. Every fill instruction loads it.
		const adminUsdc = await fundUsdc(payer.publicKey, new BN(1_000).mul(USDC));
		await admin.initializeUserAccountAndDepositCollateral(
			new BN(1_000).mul(USDC),
			adminUsdc
		);

		// Keep the oracle fresh. A live feed does this on a real cluster. Here
		// a background loop re-posts `oracleTargetPrice` every second, so the
		// vAMM stays inside its oracle-validity gates and a test can move the
		// price by moving the target.
		//
		// The loop must call `set_price_info` rather than `set_price`, because
		// velocity reads staleness off `valid_slot` in `get_pyth_price`, and
		// `set_price` leaves that field at whatever `initialize` wrote. A feed
		// that never advances its slot ages out of every validity gate however
		// often the price is rewritten.
		oracleRefresher = (async () => {
			// The loop sends without confirming, unlike `send`. The price only
			// has to stay where it is, and a trigger tolerates a stale oracle
			// slot. A confirming write each beat would compete with the
			// turner's cranks for the same accounts and delay them. A dropped
			// beat leaves the last price in place, which is still the target.
			let blockhash = (await connection.getLatestBlockhash()).blockhash;
			let beat = 0;
			while (!stopOracleRefresher) {
				try {
					if (beat++ % 20 === 0)
						blockhash = (await connection.getLatestBlockhash()).blockhash;
					const tx = new Transaction().add(
						pythProgram.instruction.setPriceInfo(
							usd(oracleTargetPrice),
							usd(0.01),
							new BN(await connection.getSlot()),
							{ accounts: { price: oracle } }
						)
					);
					tx.feePayer = payer.publicKey;
					tx.recentBlockhash = blockhash;
					tx.sign(payer);
					void connection
						.sendRawTransaction(tx.serialize(), { skipPreflight: true })
						.catch(() => {});
				} catch {
					// A transient failure is fine. The next beat retries.
				}
				await sleep(300);
			}
		})();

		// Standing liquidity: a CLOB bid and ask of 1.0 at 99.5 and 100.5, and a
		// second maker's ask of 1.0 at 100.6 behind it. Every quote rests on the
		// book, because that is the only place a live order rests.
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.SHORT,
			usd(100.5),
			UNIT
		);
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.LONG,
			usd(99.5),
			UNIT
		);
		await placeClobOrder(
			bookMaker2,
			bookMaker2Kp,
			PositionDirection.SHORT,
			usd(100.6),
			UNIT
		);

		// The publisher, configured exactly as it is deployed: RPC transport
		// against the validator, RPC-side simulation, cross fast path armed.
		startService('publisher', PUBLISHER_BIN, [], {
			RPC_URL,
			TRANSPORT: 'rpc',
			VELOCITY_PROGRAM_ID: VELOCITY_ID.toBase58(),
			MARKETS: '0',
			KEYPAIR_PATH: writeKeypair('publisher-keypair', publisherKp),
			BUFFER_DIR: `${SCRATCH}/quote-buffers`,
			REDIS_URL,
			TICK_MS: '750',
			LOCAL_SIM_POOL: '0',
			CROSS_MATCH: 'true',
			RUST_LOG: 'info',
		});

		redis = new Redis(REDIS_URL);
		await pollUntil('first published book', 90_000, async () => {
			const doc = await redis.get('last_update_orderbook_perp_0');
			return doc ?? undefined;
		});

		// Register both of the market's condition blocks and start a turner.
		// Relay cranks everything after this point unless a test submits.
		//
		// There are two watches, because a condition's wake lives on the
		// account whose state it describes. Velocity's conditions account
		// holds the cross fallback poll, and its block is the first field at
		// offset 8. The book holds the four conditions that describe the book:
		// an expired order, a side at its eviction threshold, a crossed book,
		// and an activation coming due. Their block sits at the offset the
		// attach recorded when it registered velocity's resolvers there. Watch
		// only the first account and none of the book's own cranks ever fire.
		const conditionsAccount = await connection.getAccountInfo(conditions);
		const bookBlockOffset = (
			admin.program.coder.accounts.decode(
				'clobCrankConditionsV0',
				conditionsAccount!.data
			) as { clobBlockOffset: number }
		).clobBlockOffset;
		assert.isAbove(bookBlockOffset, 0, 'the attach recorded the book block');
		await registerWatch(conditions);
		await registerWatch(clobBook.publicKey, bookBlockOffset);
		startTurner();

		// Register swift's attestation key as the on-chain flow authority.
		// Swift starts later with the same key.
		await admin.updateHotAdmin(
			HotRole.FlowAuthority,
			flowAuthorityKp.publicKey
		);
	});

	after(async function () {
		stopOracleRefresher = true;
		await oracleRefresher;
		// Always print the publisher's view for post-mortems.
		try {
			const book = await redis.get('last_update_orderbook_perp_0');
			console.log('last published book:', book?.slice(0, 2000));
			const log = fs.readFileSync(`${SCRATCH}/publisher.log`, 'utf-8');
			console.log(
				'publisher.log tail:\n',
				log.split('\n').slice(-25).join('\n')
			);
		} catch {
			// best effort
		}
		for (const { child, log } of services) {
			child.kill();
			fs.closeSync(log);
		}
		redis?.disconnect();
		for (const client of clients) {
			try {
				await client.unsubscribe();
			} catch {
				// already down
			}
		}
	});

	it('publishes the multi-source book on the existing wire', async function () {
		this.timeout(120_000);
		// Asks are best first: midpoint 100.1, CLOB 100.5, then the vAMM near
		// 101. Wait for a book that carries all three sources.
		let lastBook: any;
		const doc = await pollUntil('all three ask sources', 60_000, async () => {
			const raw = await redis.get('last_update_orderbook_perp_0');
			if (!raw) return undefined;
			const book = JSON.parse(raw);
			lastBook = book;
			const sources = new Set(
				book.asks.flatMap((level: any) => Object.keys(level.sources))
			);
			return sources.has('clob') &&
				sources.has('propamm') &&
				sources.has('vamm')
				? book
				: undefined;
		}).catch(async (err) => {
			console.log('last seen asks:', JSON.stringify(lastBook?.asks));
			console.log('last seen bids:', JSON.stringify(lastBook?.bids));
			// Midpoint instance internals: disc(8) + 4×32 addresses, then
			// mid_price/mid_slot/seq/staleness/tick/step/min/base, u16 sub,
			// u16 market, u8 paused, u8 attested, u8 bid_count, u8 ask_count.
			const info = await connection.getAccountInfo(midInstance);
			const d = info!.data;
			const base = 8 + 4 * 32;
			console.log('midpoint state:', {
				midPrice: d.readBigUInt64LE(base).toString(),
				midSlot: d.readBigUInt64LE(base + 8).toString(),
				staleness: d.readBigUInt64LE(base + 24).toString(),
				paused: d[base + 68],
				attested: d[base + 69],
				bidCount: d[base + 70],
				askCount: d[base + 71],
				currentSlot: await connection.getSlot(),
			});
			throw err;
		});

		assert.equal(doc.marketIndex, 0);
		assert.equal(doc.marketType, 'perp');
		assert.isAbove(Number(doc.slot), 0);
		// Best ask = the midpoint's mid + 10bps rung.
		assert.equal(doc.asks[0].price, String(100.1 * 1e6));
		assert.property(doc.asks[0].sources, 'propamm');
		// The CLOB's resting ask at 100.5 is in the ladder.
		const clobAsk = doc.asks.find((level: any) => level.sources.clob);
		assert.equal(clobAsk.price, String(100.5 * 1e6));
		// Bids mirror: best bid 99.9 (midpoint), CLOB 99.5 behind it.
		assert.equal(doc.bids[0].price, String(99.9 * 1e6));
		// Decorations: the oracle the fill would use.
		assert.equal(doc.oracleData.price, String(100 * 1e6));
		assert.equal(doc.oracle, 100 * 1e6);
		assert.isAbove(Number(doc.marketSlot), 0);

		// L3: attributed depth, every source the L2 ladder holds, with the
		// account each line settles against. Best price first, so the
		// midpoint's rung leads the same way it leads L2.
		const l3 = JSON.parse(
			(await redis.get('last_update_orderbook_l3_perp_0'))!
		);
		const makerPda = userOf(clobMakerKp.publicKey).toBase58();
		const midMakerPda = userOf(midMakerKp.publicKey).toBase58();
		assert.equal(l3.asks[0].price, String(100.1 * 1e6));
		assert.equal(l3.asks[0].source, 'propamm');
		assert.equal(l3.asks[0].maker, midMakerPda);
		assert.isNull(
			l3.asks[0].orderId,
			'a spline rung is not an order: nothing to cancel, no queue to be behind'
		);

		// The book's own orders carry an id and the maker resting them.
		const clobAskRow = l3.asks.find((row: any) => row.source === 'clob');
		assert.equal(clobAskRow.price, String(100.5 * 1e6));
		assert.equal(clobAskRow.maker, makerPda);
		assert.isAbove(Number(clobAskRow.orderId), 0);
		const clobBidRow = l3.bids.find((row: any) => row.source === 'clob');
		assert.equal(clobBidRow.maker, makerPda);

		// The vAMM is also in the ladder, settled by the market itself.
		const vammRow = l3.asks.find((row: any) => row.source === 'vamm');
		assert.equal(vammRow.maker, perpMarket.toBase58());
		assert.isNull(vammRow.orderId);

		// Best makers names the accounts a fill has to carry, so the market is
		// not one of them.
		const bestMakers = JSON.parse(
			(await redis.get('last_update_orderbook_best_makers_perp_0'))!
		);
		assert.include(bestMakers.asks, makerPda);
		assert.include(bestMakers.asks, midMakerPda);
		assert.include(bestMakers.bids, makerPda);
		assert.notInclude(bestMakers.asks, perpMarket.toBase58());

		const sub = new Redis(REDIS_URL);
		try {
			const grouped = await new Promise<any>((resolve, reject) => {
				const timer = setTimeout(
					() => reject(new Error('no grouped publish within 15s')),
					15_000
				);
				sub.subscribe('orderbook_perp_0_grouped_10');
				sub.on('message', (_channel, message) => {
					clearTimeout(timer);
					resolve(JSON.parse(message));
				});
			});
			assert.isAtLeast(grouped.asks.length, 1);
		} finally {
			sub.disconnect();
		}
	});

	it('a take splits across the midpoint and both book makers', async function () {
		this.timeout(120_000);
		// Long 3.5: midpoint 100.1 (2.0), then the book's 100.5 (1.0) and its
		// 100.6 (0.5). The vAMM ask sits about 1% out and yields to all three,
		// so it takes nothing.
		const size = UNIT.muln(35).divn(10);
		await placeAndTake(
			takerKp,
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: size,
				price: usd(102),
			}),
			[clobMakerKp, bookMaker2Kp, midMakerKp]
		);

		await taker.fetchAccounts();
		const position = taker.getUser().getPerpPosition(0)!;
		assert.equal(position.baseAssetAmount.toString(), size.toString());

		// The 100.5 ask is gone and half of the 100.6 ask remains, so the book
		// still carries one. The midpoint's first rung is consumed.
		const book = await readClob();
		assert.equal(book.askCount, 1);
		await midMaker.fetchAccounts();
		const midPosition = midMaker.getUser().getPerpPosition(0)!;
		assert.equal(
			midPosition.baseAssetAmount.toString(),
			UNIT.muln(-2).toString()
		);
		// (both rungs: 1.0 @ 100.1 + 1.0 @ 100.3)
		await clobMaker.fetchAccounts();
		assert.equal(
			clobMaker.getUser().getPerpPosition(0)!.baseAssetAmount.toString(),
			UNIT.neg().toString()
		);
		await bookMaker2.fetchAccounts();
		assert.equal(
			bookMaker2.getUser().getPerpPosition(0)!.baseAssetAmount.toString(),
			UNIT.divn(2).neg().toString()
		);
	});

	it('place-and-take rests the unfilled limit remainder on the CLOB', async function () {
		this.timeout(120_000);
		// A 100.0 limit long sits below every ask. The best ask is the
		// midpoint's 100.1, which the levels below re-arm after the fill test.
		// Nothing fills, and the remainder moves onto the CLOB as a resting
		// bid.
		await setMidpointLevels(usd(100), [
			{ offsetPpm: 1000, size: UNIT.muln(2) },
		]);
		const before = await readClob();

		const ix = await taker.getPlaceAndTakePerpOrderIx(
			getLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: UNIT,
				price: usd(100),
			}),
			{
				quoterSlab,
				clobMarket: clobBook.publicKey,
				clobProgram: CLOB_ID,
			}
		);
		await taker.sendTransaction(new Transaction().add(ix));

		const after = await readClob();
		assert.equal(after.bidCount, before.bidCount + 1);
		assert.equal(after.bestBidPrice!.toString(), usd(100).toString());
		// The taker holds no open order slot, because the order
		// moved to the book. The filter is scoped to this order's price so
		// that leftovers from other specs do not count.
		await taker.fetchAccounts();
		const open = taker
			.getUserAccount()!
			.orders.filter(
				(o) => isVariant(o.status, 'open') && o.price.eq(usd(100))
			);
		assert.equal(open.length, 0);
	});

	it('the publisher detects a crossed book and submits the cross match', async function () {
		this.timeout(120_000);
		const protocolBefore = (await connection.getAccountInfo(protocolUser))!;
		await crosser.fetchAccounts();
		// The crosser has no position in this market until this spec opens one,
		// and `getPerpPosition` returns undefined rather than a zeroed position
		// for a market a user has never traded. Read it as zero.
		const crosserBase0 =
			crosser.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);

		// Cross the book outright. A 101.0 bid against the midpoint's 100.1 ask
		// clears the two-legged taker fees with about 90bps of spread. A
		// PropAMM against CLOB cross is the one only the publisher can find.
		await placeClobOrder(
			crosser,
			crosserKp,
			PositionDirection.LONG,
			usd(101),
			UNIT
		);

		// The bid is on the book and the book is crossed. The poll below exits
		// on that bid being gone, and an order that never rested is also gone,
		// so without this the spec would pass on a placement that failed.
		const crossed = await readClob();
		assert.equal(
			crossed.bestBidPrice!.toString(),
			usd(101).toString(),
			'the crossing bid rested, so there is a cross for the publisher to find'
		);

		// The publisher's next tick sees the cross and submits
		// crank_cross_match. The crossed bid is then consumed.
		await pollUntil(
			'cross match to consume the crossed bid',
			60_000,
			async () => {
				const book = await readClob();
				const crossedBidGone =
					book.bestBidPrice === undefined || book.bestBidPrice.lt(usd(101));
				return crossedBidGone ? true : undefined;
			}
		);

		// The crossed bid filled, so the crosser is long and the midpoint maker
		// is shorter. The protocol user stayed flat and took the after-fee
		// surplus as quote balance. The check is a delta: an absolute would
		// hold on a position this spec did not open, and every later spec that
		// touches this account already reads it as one.
		await crosser.fetchAccounts();
		assert.equal(
			crosser
				.getUser()
				.getPerpPosition(0)!
				.baseAssetAmount.sub(crosserBase0)
				.toString(),
			UNIT.toString(),
			'the crosser bought the unit it bid for'
		);
		const protocolAfter = (await connection.getAccountInfo(protocolUser))!;
		assert.notDeepEqual(
			protocolAfter.data.subarray(0, 4384),
			protocolBefore.data.subarray(0, 4384),
			'protocol user settled the cross legs'
		);
	});

	// A migrated taker remainder is the one order on the book that nobody may
	// take while a counterparty crosses it. The improvement between the two
	// prices therefore cannot be won by landing a transaction at the activation
	// slot. This path hands that improvement to the taker instead. The
	// assertions below bound where the improvement lands: the taker's all-in
	// cost must beat the price it was resting at, and it must not beat the
	// counterparty's price.
	it('hands a crossed taker remainder the counterparty price, cranked by relay', async function () {
		this.timeout(120_000);

		// The curve is in every fill's baseline and is deep at the oracle near
		// 100, so a limit long at 104 crosses it and fills there. A remainder
		// only exists when the taker's bound is tighter than the curve. Pausing
		// the curve's fills is what makes a remainder reachable at a price that
		// leaves a counterparty room to improve on it. Without the pause the
		// order fills from the curve, and every assertion below passes while
		// testing nothing.
		await admin.updatePerpMarketPausedOperations(
			0,
			PerpOperation.AMM_FILL | PerpOperation.AMM_IMMEDIATE_FILL
		);

		try {
			// Park the midpoint 5% wide of a 100 mid. Every price below sits
			// inside that spread, so the midpoint does not fill the remainder at
			// placement, where its ask is 105, and does not cross the
			// counterparty ask, where its bid is 95. The only cross on the book
			// is then the pair this crank owns, which makes the poll below
			// unambiguous.
			await setMidpointLevels(usd(100), [
				{ offsetPpm: 50_000, size: UNIT.muln(2) },
			]);

			// The preconditions are asserted rather than assumed. Earlier specs
			// leave orders behind, and a stale ask under 104 would fill the order
			// instead of resting it. A plain "remainder is gone" poll would still
			// pass while testing nothing.
			const before = await readClob();
			assert.isTrue(
				before.bestAskPrice === undefined || before.bestAskPrice.gt(usd(104)),
				'a leftover ask below 104 would fill the taker instead of resting it'
			);

			await taker.fetchAccounts();
			const takerBase0 = taker.getUser().getPerpPosition(0)!.baseAssetAmount;
			const takerQuote0 = taker.getUser().getPerpPosition(0)!.quoteAssetAmount;
			const relayPaid0 = await relayPayoutBalance();
			await crosser.fetchAccounts();
			const crosserBase0 = crosser
				.getUser()
				.getPerpPosition(0)!.baseAssetAmount;

			// 104 rests above every leftover bid, so it is the book's best bid and
			// the pair the crank's scan reaches first.
			const ix = await taker.getPlaceAndTakePerpOrderIx(
				getLimitOrderParams({
					marketIndex: 0,
					direction: PositionDirection.LONG,
					baseAssetAmount: UNIT,
					price: usd(104),
				}),
				{
					quoterSlab,
					clobMarket: clobBook.publicKey,
					clobProgram: CLOB_ID,
				}
			);
			await taker.sendTransaction(new Transaction().add(ix));

			const rested = await readClob();
			assert.equal(rested.bestBidPrice!.toString(), usd(104).toString());

			// The counterparty is an ordinary maker ask 3.00 better than the price
			// the remainder is resting at.
			await placeClobOrder(
				crosser,
				crosserKp,
				PositionDirection.SHORT,
				usd(101),
				UNIT
			);

			await pollUntil(
				'the taker remainder to resolve off the book',
				60_000,
				async () => {
					const book = await readClob();
					const gone =
						book.bestBidPrice === undefined || book.bestBidPrice.lt(usd(104));
					return gone ? true : undefined;
				}
			);

			// Where the improvement landed. Fees are inside `quoteAssetAmount`, so
			// this is the all-in cost rather than the headline fill price.
			await taker.fetchAccounts();
			const dBase = taker
				.getUser()
				.getPerpPosition(0)!
				.baseAssetAmount.sub(takerBase0);
			const dQuote = taker
				.getUser()
				.getPerpPosition(0)!
				.quoteAssetAmount.sub(takerQuote0);
			assert.equal(dBase.toString(), UNIT.toString(), 'taker bought its unit');
			const allInPrice = dQuote.neg().mul(BASE_PRECISION).div(dBase);
			assert.isTrue(
				allInPrice.lt(usd(104)),
				`all-in cost ${allInPrice} must beat the 104 it rested at`
			);
			assert.isTrue(
				allInPrice.gte(usd(101)),
				`all-in cost ${allInPrice} cannot beat the counterparty's 101`
			);

			// The counterparty sold its unit. The check is a delta, because the
			// counterparty carries a long from the cross-match spec that this
			// sale flattens.
			await crosser.fetchAccounts();
			assert.equal(
				crosser
					.getUser()
					.getPerpPosition(0)!
					.baseAssetAmount.sub(crosserBase0)
					.toString(),
				UNIT.neg().toString(),
				'counterparty sold its unit'
			);

			// Relay's keeper cranked it, rather than this test or the publisher.
			// That payment is the only evidence that discovery reached this
			// crank.
			assert.isAbove(
				await relayPayoutBalance(),
				relayPaid0,
				'relay paid its keeper for the crank'
			);
		} finally {
			// Put the curve and the 10bps spline back even when an assertion above
			// throws. The specs below inherit both, and one pins a published price
			// derived from those levels. Leaving them changed turns one failure
			// here into four unrelated ones.
			await admin.updatePerpMarketPausedOperations(0, 0);
			await setMidpointLevels(usd(100), [
				{ offsetPpm: 1000, size: UNIT.muln(2) },
			]);
		}
	});

	it('a mid write repositions the published spline', async function () {
		this.timeout(120_000);
		await setMidpointMid(usd(102));
		await pollUntil('published book to track the new mid', 30_000, async () => {
			const raw = await redis.get('last_update_orderbook_perp_0');
			if (!raw) return undefined;
			const book = JSON.parse(raw);
			const propammAsk = book.asks.find((level: any) => level.sources.propamm);
			// 102 + 10bps = 102.102.
			return propammAsk?.price === String(102.102 * 1e6) ? true : undefined;
		});
	});

	// The next three specs cover a live crank-turner that finds and lands work
	// with nobody submitting it. They are relay's alone, because the publisher
	// only ever submits `crank_cross_match`. A state change here plus a credit
	// to relay's payout account therefore attributes the work to relay.

	it('reclaims an expired CLOB order without anyone submitting', async function () {
		this.timeout(180_000);
		const before = await readClob();
		const payoutBefore = await relayPayoutBalance();

		// A CLOB ask that expires in about 10s. Velocity folds the expiry into
		// the market's wake hint as it places, keeping the earliest one, so the
		// turner has a deadline to wake on.
		//
		// The ask is priced well above every bid on the book. The midpoint
		// spline quotes around 102, so an ask near the touch is crossed and
		// filled, and the order count returns to baseline for a reason that has
		// nothing to do with expiry.
		await clobMaker.fetchAccounts();
		const makerSizeBefore =
			clobMaker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);
		// The deadline is chain time rather than wall clock. `max_ts` is
		// compared against the validator's `Clock::unix_timestamp`, and a local
		// validator's clock tracks its own slot production rather than the
		// host's. Reading the wrong one arms an expiry that never comes due.
		const chainNow =
			(await connection.getBlockTime(await connection.getSlot('confirmed'))) ??
			Math.floor(Date.now() / 1000);
		const now = chainNow;
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.SHORT,
			usd(110),
			UNIT,
			new BN(now + 10)
		);
		const armed = await readClob();
		assert.equal(armed.askCount, before.askCount + 1);
		await clobMaker.fetchAccounts();
		const openAsksArmed =
			clobMaker.getUser().getPerpPosition(0)?.openAsks ?? new BN(0);

		// Nobody in this test submits anything from here on.
		//
		// The poll waits on the maker's reservation rather than on the book's
		// depth. An expired order stops being matchable as soon as it comes
		// due, so it leaves a depth reading before anything reclaims it. The
		// crank unwinds the reservation, and that only moves when the crank
		// lands.
		await pollUntil('relay to reclaim the expired order', 120_000, async () => {
			await clobMaker.fetchAccounts();
			const openAsks =
				clobMaker.getUser().getPerpPosition(0)?.openAsks ?? new BN(0);
			// An ask's reservation is held negative, so an unwind moves it
			// toward zero. Compare what it reserves, not the signed value.
			return openAsks.abs().lt(openAsksArmed.abs()) ? true : undefined;
		});
		// A fill would also unwind the reservation, so check that the maker's
		// position did not change.
		await clobMaker.fetchAccounts();
		assert.isTrue(
			(clobMaker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0)).eq(
				makerSizeBefore
			),
			'the order expired rather than trading'
		);
		assert.isAbove(
			await relayPayoutBalance(),
			payoutBefore,
			'the reclaim was paid from the market reservoir to relay keeper'
		);
	});

	it('refills a market reservoir from the treasury with nobody submitting', async function () {
		this.timeout(180_000);
		// A market is attached with an empty reservoir and a zero mirror, which
		// reads as below the watermark, so the refill is due from the moment the
		// market exists. Nobody here submits it. The condition sits on the
		// conditions account, the same account the market's watch already
		// covers, and a relay turner finds it the way it finds every other
		// crank.
		const treasuryPk = getCrankTreasuryPublicKey(VELOCITY_ID);
		const readTreasury = async () =>
			admin.program.coder.accounts.decode(
				'crankTreasuryV0',
				(await connection.getAccountInfo(treasuryPk))!.data
			);

		await pollUntil(
			'relay to refill the market reservoir',
			120_000,
			async () => {
				const treasury = await readTreasury();
				return treasury.totalRefilled.gtn(0) ? true : undefined;
			}
		);

		const treasury = await readTreasury();
		// Nothing seeded this reservoir. It was created holding rent alone, and
		// every lamport in it past that came from the treasury.
		const conditionsBalance = await connection.getBalance(conditions);
		assert.isTrue(
			treasury.totalRefilled.gtn(0) &&
				conditionsBalance > treasury.totalRefilled.toNumber(),
			'the reservoir holds what the treasury sent it'
		);
		assert.isTrue(
			treasury.totalPaid.gtn(0),
			'the keeper that refilled was paid from the treasury'
		);

		// The condition is level-triggered. Once the mirror is restated above
		// the watermark the condition stops being due, so a second refill does
		// not follow the first on an idle market.
		const refilledOnce = treasury.totalRefilled;
		await new Promise((resolve) => setTimeout(resolve, 15_000));
		assert.isTrue(
			(await readTreasury()).totalRefilled.eq(refilledOnce),
			'a full reservoir is not refilled again'
		);
	});

	it('fires a stop-market to the book, and the cross crank fills it', async function () {
		this.timeout(300_000);
		// A fresh account, so the fired order rests against clean margin. The
		// shared taker carries orders and equity floors from earlier specs that
		// would fail the placement gate for the rest.
		const stopperKp = Keypair.generate();
		await airdrop(stopperKp.publicKey, 10);
		const stopper = newClient(stopperKp);
		await stopper.subscribe();
		const stopperUsdc = await fundUsdc(
			stopperKp.publicKey,
			new BN(1_000).mul(USDC)
		);
		await stopper.initializeUserAccountAndDepositCollateral(
			new BN(1_000).mul(USDC),
			stopperUsdc
		);
		const stopperUser = userOf(stopperKp.publicKey);

		// A fired stop rests on the book taker-origin and does not fill
		// synchronously. The cross crank fills it. The taker-origin cross spec
		// isolates that path the same way: pause the curve so nothing fills the
		// rested order at placement, and park the midpoint wide so it is not
		// the counterparty either. The only cross is the maker placed below.
		await admin.updatePerpMarketPausedOperations(
			0,
			PerpOperation.AMM_FILL | PerpOperation.AMM_IMMEDIATE_FILL
		);
		try {
			await setMidpointLevels(usd(100), [
				{ offsetPpm: 200_000, size: UNIT.muln(2) },
			]);
			// A stop that sells 1.0 if the oracle rises through 104.
			await stopper.placeTriggerOrders([
				getTriggerMarketOrderParams({
					marketIndex: 0,
					direction: PositionDirection.SHORT,
					baseAssetAmount: UNIT,
					triggerPrice: usd(104),
					triggerCondition: OrderTriggerCondition.ABOVE,
				}),
			]);
			await stopper.fetchAccounts();
			const armed = stopper
				.getUserAccount()!
				.orders.find(
					(o) => isVariant(o.status, 'open') && o.triggerPrice.eq(usd(104))
				)!;
			assert.isOk(armed, 'trigger order is armed');

			// There is one conditions account per user, the same one the
			// liquidation thresholds live on, so one sync covers both halves.
			await syncUserConditions(stopperUser, [ro(quoterSlab)]);
			await registerWatch(getUserConditionsPublicKey(VELOCITY_ID, stopperUser));

			const asksBefore = (await readClob()).askCount;
			const relayPaid0 = await relayPayoutBalance();

			// Move the oracle through the trigger. Nobody submits a trigger
			// instruction. The turner fires `resolve_trigger_market_order_v1`,
			// which rests the whole fired order taker-origin on the book rather
			// than filling it.
			await setOraclePrice(106);
			await pollUntil(
				'the fired stop to rest on the book',
				200_000,
				async () => {
					await stopper.fetchAccounts();
					const stillArmed = stopper
						.getUserAccount()!
						.orders.find(
							(o) => o.orderId === armed.orderId && isVariant(o.status, 'open')
						);
					const rested = (await readClob()).askCount > asksBefore;
					return !stillArmed && rested ? true : undefined;
				}
			);
			// The slot is freed and the order rests on the book unfilled. A
			// fire to the book stops here. The v0 flip left the order live
			// instead.
			await stopper.fetchAccounts();
			assert.equal(
				(
					stopper.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0)
				).toString(),
				'0',
				'the fired stop rests; it does not fill against the book alone'
			);

			// A maker bid crosses the resting sell. The cross crank settles it.
			await placeClobOrder(
				crosser,
				crosserKp,
				PositionDirection.LONG,
				usd(106),
				UNIT
			);
			await pollUntil(
				'the cross crank to fill the fired stop',
				60_000,
				async () => {
					await stopper.fetchAccounts();
					const pos =
						stopper.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);
					return pos.ltn(0) ? true : undefined;
				}
			);
			await stopper.fetchAccounts();
			assert.isTrue(
				stopper.getUser().getPerpPosition(0)!.baseAssetAmount.ltn(0),
				'the fired stop sold into the crossing bid'
			);
			assert.isAbove(
				await relayPayoutBalance(),
				relayPaid0,
				'relay cranked both the fire and the cross'
			);
		} finally {
			await admin.updatePerpMarketPausedOperations(0, 0);
			await setMidpointLevels(usd(100), [
				{ offsetPpm: 1000, size: UNIT.muln(2) },
			]);
			await setOraclePrice(100);
		}
	});

	it('fires a trigger-limit onto the book through its own resolver', async function () {
		this.timeout(180_000);
		// A trigger-limit rests its whole order on the book when it fires. It
		// takes the `resolve_trigger_limit_order_v1` path, which is not the
		// stop-market's `resolve_trigger_market_order_v1`. This one is a buy
		// limit at 99, armed to fire when the oracle falls through 98.
		await taker.placeTriggerOrders([
			getTriggerLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: UNIT,
				price: usd(99),
				triggerPrice: usd(98),
				triggerCondition: OrderTriggerCondition.BELOW,
			}),
		]);
		await taker.fetchAccounts();
		const armed = taker
			.getUserAccount()!
			.orders.find(
				(o) =>
					isVariant(o.status, 'open') &&
					isVariant(o.orderType, 'triggerLimit') &&
					o.triggerPrice.eq(usd(98))
			)!;
		assert.isOk(armed, 'trigger-limit is armed');

		const takerUser = userOf(takerKp.publicKey);
		await syncUserConditions(takerUser, [ro(quoterSlab)]);
		await registerWatch(getUserConditionsPublicKey(VELOCITY_ID, takerUser));

		const bidsBefore = (await readClob()).bidCount;
		const relayPaid0 = await relayPayoutBalance();

		// Drop the oracle through the trigger. The turner fires
		// `resolve_trigger_limit_order_v1`, which rests the whole order on the
		// book.
		await setOraclePrice(97);
		// The slot degrades to a placed-on-clob shadow that still reads
		// status Open, so the slot never empties. Leftover bids from earlier
		// specs share this book, so the fired order is not always the best bid.
		// The book gaining a bid is therefore the signal to wait on.
		await pollUntil(
			'the trigger-limit to rest on the book',
			120_000,
			async () => {
				const rested = (await readClob()).bidCount > bidsBefore;
				return rested ? true : undefined;
			}
		);
		assert.isAbove(
			await relayPayoutBalance(),
			relayPaid0,
			'relay cranked the trigger-limit onto the book'
		);
		await setOraclePrice(100);
	});

	it('liquidates an underwater account through the router, with no inventory left behind', async function () {
		this.timeout(240_000);
		// A leveraged long that a price drop puts underwater.
		const victimKp = Keypair.generate();
		await airdrop(victimKp.publicKey, 10);
		const victim = newClient(victimKp);
		await victim.subscribe();
		const victimUsdc = await fundUsdc(victimKp.publicKey, new BN(60).mul(USDC));
		await victim.initializeUserAccountAndDepositCollateral(
			new BN(60).mul(USDC),
			victimUsdc
		);
		// A standing ask deep enough for the whole entry. Without it the vAMM's
		// slippage caps the fill near one unit and the victim reaches about
		// 1.7x, which is never liquidatable, so the scenario passes while
		// testing nothing.
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.SHORT,
			usd(102),
			UNIT.muln(5)
		);
		// About 8.5x: 5 units near 102 on 60 of collateral. A price drop to 84
		// puts equity below zero, well past maintenance.
		await placeAndTake(
			victimKp,
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: UNIT.muln(5),
				price: usd(103),
			})
		);
		await victim.fetchAccounts();
		assert.isTrue(
			victim.getUser().getPerpPosition(0)!.baseAssetAmount.eq(UNIT.muln(5)),
			'victim entered the full intended size'
		);

		// Opt the victim into relay liquidation coverage. The sync writes
		// thresholds from the live positions and a self-sync watch. The account
		// holds no reservoir. The protocol treasury pays whoever resyncs it, so
		// an underfunded user cannot leave its own thresholds stale.
		const victimUser = userOf(victimKp.publicKey);
		const userConditions = getUserConditionsPublicKey(VELOCITY_ID, victimUser);
		// The sync stores the slab, the book, and the book's program in the
		// shared account list, so the liquidation's staged executor fills
		// through the router. A market that names a book refuses a fill that
		// carries no slab. A sync without the slab therefore writes conditions
		// that detect the liquidation and can never land it.
		await syncUserConditions(victimUser, [ro(quoterSlab)]);
		await registerWatch(userConditions);

		// Standing bid for the liquidation's fill leg to route into, close
		// enough to the crashed oracle to clear the fill price bands.
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.LONG,
			usd(90),
			UNIT.muln(5)
		);

		const payoutBefore = await relayPayoutBalance();
		// The baseline is measured rather than fixed. A fixed size to compare
		// against passes as soon as the victim's position is smaller than it,
		// which reports "relay liquidated" for a relay that did nothing.
		const sizeBefore = victim.getUser().getPerpPosition(0)!.baseAssetAmount;
		// Crash the oracle. The drop is hard enough to put the victim under
		// maintenance, where equity is about $14 against about $23 required at
		// 93. It is gentle enough to stay inside the oracle price bands, which
		// a 16% single-slot move breaches with `PriceBandsBreached` on the
		// staged executor. Nobody submits a liquidation.
		await setOraclePrice(93);

		await pollUntil('relay to liquidate', 180_000, async () => {
			await victim.fetchAccounts();
			const position = victim.getUser().getPerpPosition(0);
			const reduced = !position || position.baseAssetAmount.lt(sizeBefore);
			return reduced ? true : undefined;
		});
		assert.isAbove(await relayPayoutBalance(), payoutBefore);

		// The protocol User was only the filler, so its account must still be
		// readable after the liquidation.
		const protocolUserAccount = await connection.getAccountInfo(protocolUser);
		assert.isOk(protocolUserAccount);
		await setOraclePrice(100);
		await victim.unsubscribe();
	});

	it('fills a swift order through the attested-flow loop', async function () {
		this.timeout(180_000);
		// Swift starts here, last, against a settled chain. Its client snapshots
		// the market list at startup, so a start that races the bring-up sees no
		// markets. Its intake simulation then panics on the missing market data.
		await startSwift();
		// Gate the midpoint on attestation. From here it shows its books only
		// to flow the program marked as attested. The midpoint's participation
		// below is then on-chain proof that the attestation verified, rather
		// than proof that the endpoints answered.
		await send(
			[
				await midpointProgram.methods
					.updateQuoterV0({
						maxMidStalenessSlots: null,
						priceTickSize: null,
						sizeStep: null,
						minQuoteSize: null,
						requireAttestedFlow: true,
						isPaused: null,
						maxMidDeviationPpm: null,
						midSequence: null,
					})
					.accountsStrict({
						quoter: midInstance,
						authority: midConfigKp.publicKey,
						newHotAuthority: null,
					})
					.instruction(),
			],
			[midConfigKp]
		);

		// Re-arm the midpoint. Earlier scenarios consumed its ask rungs, because
		// a filled rung is standing intent, and its mid may be stale.
		await setMidpointLevels(usd(100), [
			{ offsetPpm: 1000, size: UNIT.muln(2) },
			{ offsetPpm: 3000, size: UNIT.muln(2) },
		]);

		// The taker signs an order off chain and sends it to swift. This is the
		// real intake. It verifies the order, simulates it, publishes it to
		// keepers, and records it as attestable.
		if (
			!(await taker.isSignedMsgUserOrdersAccountInitialized(takerKp.publicKey))
		) {
			await taker.initializeSignedMsgUserOrders(takerKp.publicKey, 8);
		}
		await taker.fetchAccounts();
		const takerUser = userOf(takerKp.publicKey);
		const positionBefore =
			taker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);

		// A v0 message over a lookup table, the way a real keeper sends a swift
		// fill. The ed25519 instruction carries the whole signed message, so the
		// router tail does not fit in a legacy transaction. The table is built
		// before the order is submitted, because a table costs two slots to
		// activate, and activating it after intake would consume the hold window
		// this scenario exists to observe.
		const lookupTable = await createRouterLookupTable([
			...routerTail([clobMakerKp, midMakerKp]).map((a) => a.pubkey),
			VELOCITY_ID,
			takerUser,
			statsOf(takerKp.publicKey),
			userOf(payer.publicKey),
			statsOf(payer.publicKey),
		]);

		// Sign and submit again on each attempt. The signed message pins a slot,
		// so a retry has to re-sign rather than replay. The retries exist
		// because swift's market and oracle subscriptions load after it starts.
		// In production swift has run for hours before an order arrives. Here it
		// is seconds old.
		const submit = async () => {
			const uuid = generateSignedMsgUuid();
			const signed = taker.signSignedMsgOrderParamsMessage({
				signedMsgOrderParams: getMarketOrderParams({
					marketIndex: 0,
					direction: PositionDirection.LONG,
					baseAssetAmount: UNIT,
					price: usd(103),
					auctionDuration: 120,
					auctionStartPrice: usd(101),
					auctionEndPrice: usd(103),
				}),
				subAccountId: 0,
				slot: new BN(await connection.getSlot()),
				uuid,
				takeProfitOrderParams: null,
				stopLossOrderParams: null,
				// The message is tagged for this cluster. The program
				// refuses a message signed for the other cluster, and one
				// signed for no cluster. The route names the midpoint
				// explicitly, and the CLOB and vAMM baseline is implicit.
				// This harness builds velocity without its default features,
				// so the program names devnet. The anchor suite builds with
				// them and names mainnet, which is why the two suites tag
				// differently.
				network: SignedMsgNetwork.DEVNET,
				route: [midEntry],
			});
			const res = await fetch(`${SWIFT_URL}/orders`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					signature: signed.signature.toString('base64'),
					message: signed.orderParams.toString(),
					taker_authority: takerKp.publicKey.toBase58(),
					signing_authority: takerKp.publicKey.toBase58(),
				}),
			});
			return { uuid, signed, ok: res.ok, body: await res.text() };
		};
		let lastRejection = '';
		const accepted = await pollUntil(
			'swift to accept the signed order',
			90_000,
			async () => {
				const attempt = await submit();
				if (attempt.ok) {
					return attempt;
				}
				lastRejection = attempt.body;
				return undefined;
			}
		).catch(() => {
			throw new Error(`swift intake refused: ${lastRejection}`);
		});
		const { uuid, signed } = accepted;

		// The keeper's side, wire for wire what a filler does. It polls /attest
		// through the hold window, then carries the detached attestation as an
		// argument of the fill instruction. The flow authority signs no
		// transaction, so nothing here is co-signed.
		let sawHoldWindow = false;
		let attested:
			| { flowAuthority: string; signature: string; expiryTs: number }
			| undefined;
		for (let attempt = 0; attempt < 8 && !attested; attempt++) {
			const res = await fetch(`${SWIFT_URL}/attest`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ uuid: Buffer.from(uuid).toString() }),
			});
			const body = await res.text();
			if (res.status === 425) {
				sawHoldWindow = true;
				const { retryAfterMs } = JSON.parse(body) as {
					retryAfterMs?: number;
				};
				await new Promise((r) => setTimeout(r, retryAfterMs ?? 250));
				continue;
			}
			assert.equal(res.status, 200, `attest refused: ${body}`);
			attested = JSON.parse(body);
		}
		assert.isTrue(sawHoldWindow, 'the hold window was observed');
		assert.isOk(attested, 'attestation granted after the hold');
		// Swift signs with the key the program checks against. That key is the
		// flow-authority hot role on `State`, set during bring-up.
		assert.equal(
			attested!.flowAuthority,
			flowAuthorityKp.publicKey.toBase58(),
			'the attestation came from the on-chain flow authority'
		);
		// The blob binds to the taker's own order signature and to an
		// expiry. Velocity verifies it in-program, next to the taker
		// signature it binds to.
		const flowAttestation = {
			signature: Array.from(Buffer.from(attested!.signature, 'base64')),
			expiryTs: new BN(attested!.expiryTs),
		};

		const cuLimit = ComputeBudgetProgram.setComputeUnitLimit({
			units: 1_400_000,
		});
		// One instruction. The program verifies the signed message itself with
		// brine-ed25519, so there is no separate ed25519 precompile.
		const [placeIx] = await admin.getPlaceSignedMsgTakerPerpOrderIxs(
			signed,
			0,
			{
				taker: takerUser,
				takerStats: statsOf(takerKp.publicKey),
				takerUserAccount: taker.getUserAccount()!,
				signingAuthority: takerKp.publicKey,
			},
			[cuLimit],
			undefined,
			undefined,
			undefined,
			flowAttestation
		);
		// The v1 signed-message instruction places, fills, and rests in one
		// call, so it carries the same maker maps and quoter tail a keeper fill
		// does. The SDK builder writes only the map section, and keep-rs appends
		// the rest the same way. Both makers are carried, because the CLOB holds
		// resting orders from earlier scenarios, and a quote whose user set
		// omits them fails `StaleUserSet` once they age past the grace window.
		placeIx.keys.push(
			...[clobMakerKp, midMakerKp].flatMap((kp) => [
				rw(userOf(kp.publicKey)),
				rw(statsOf(kp.publicKey)),
			]),
			ro(quoterSlab),
			rw(clobBook.publicKey),
			ro(CLOB_ID),
			rw(midInstance),
			ro(SYSVAR_INSTRUCTIONS_PUBKEY),
			ro(statePdaCache),
			ro(MIDPOINT_ID)
		);
		const blockhash = await connection.getLatestBlockhash();
		const message = new TransactionMessage({
			payerKey: payer.publicKey,
			recentBlockhash: blockhash.blockhash,
			instructions: [cuLimit, placeIx],
		}).compileToV0Message([lookupTable]);
		const tx = new VersionedTransaction(message);
		tx.sign([payer]);

		// Preflight is skipped. On this validator a preflighted send is dropped
		// at times, while this path lands. A refused attestation reverts the
		// fill, and `confirmSignature` reports the program logs it reverted
		// with.
		const signature = await connection.sendRawTransaction(tx.serialize(), {
			skipPreflight: true,
			maxRetries: 20,
		});
		await confirmSignature(signature, 'the attested fill');

		await taker.fetchAccounts();
		const positionAfter =
			taker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);
		assert.isTrue(
			positionAfter.sub(positionBefore).eq(UNIT),
			`taker filled the full unit (got ${positionAfter
				.sub(positionBefore)
				.toString()})`
		);
		// The attestation reached the midpoint. Its books now open only to
		// attested flow, and its maker settled part of the fill. The check reads
		// the transaction rather than a position a later crank could move.
		const midFilled = await makerFilledBase(
			signature,
			userOf(midMakerKp.publicKey)
		);
		assert.isTrue(
			midFilled.gt(new BN(0)),
			`midpoint maker filled the attested flow (got ${midFilled})`
		);
	});
	it('forfeits book depth it cannot carry rather than routing it worse', async function () {
		this.timeout(120_000);
		// Two makers on the book, the second better than everything else the
		// route offers. The fill carries the first and not the second, which is
		// what happens whenever a book holds more makers than a transaction has
		// account locks for.
		//
		// The book stops at the maker it was not given and reports what it was
		// holding. The router reserves that depth instead of giving it to the
		// midpoint or the vAMM, so the taker keeps it unfilled. The remainder
		// rests where the book's own price can still reach it, rather than at a
		// price the book was beating.
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.SHORT,
			usd(100.2),
			UNIT.divn(2)
		);
		await placeClobOrder(
			clobMaker2,
			clobMaker2Kp,
			PositionDirection.SHORT,
			usd(100.4),
			UNIT.divn(2)
		);
		// The book skips an order whose owner is missing while that order is
		// younger than `unknown_user_grace_slots`, because the caller could not
		// have heard of it yet. It stops on the order once the order is older.
		// Both of this test's makers have to be past that window for the fill to
		// see the second one at all.
		await sleep(2_000);

		// The midpoint quotes worse than both makers, so it is what the reserve
		// has to keep off the withheld depth.
		await setMidpointLevels(usd(100), [
			{ offsetPpm: 8000, size: UNIT.muln(2) },
		]);

		const size = UNIT.muln(15).divn(10);
		// `clobMaker2Kp` is absent from the account list. The taker places and
		// routes its own order, so it endorses that list and the filler
		// obligation to carry every reachable maker does not apply. That
		// isolates the reserve behavior from the obligation guard. A keeper that
		// omitted a maker it had room for would be refused instead.
		const signature = await send(
			[
				ComputeBudgetProgram.setComputeUnitLimit({ units: 800_000 }),
				await placeAndTakeIx(
					takerKp.publicKey,
					getMarketOrderParams({
						marketIndex: 0,
						direction: PositionDirection.LONG,
						baseAssetAmount: size,
						price: usd(102),
					}),
					[clobMakerKp, midMakerKp]
				),
			],
			[takerKp]
		);

		// The check reads the fill itself rather than positions afterwards. The
		// claim is about this transaction, which settled for the maker it
		// carried and not for the one it did not. A position can move again as
		// soon as the fill lands. This taker's remainder rests as a taker-origin
		// bid that crosses the absent maker's ask, and a crank may match them a
		// slot later, so asserting on account state raced that crank.
		const filled = await makersFilledBy(signature);
		assert.notInclude(
			filled,
			userOf(clobMaker2Kp.publicKey).toBase58(),
			'the maker the fill could not carry is not filled'
		);
		const book = await readClob();
		assert.isAtLeast(book.askCount, 1, 'its ask is still on the book');

		// The maker the fill could carry did fill, which shows the walk reached
		// the book and passed the first order. Stopping at the second order is
		// therefore the withheld path rather than a walk that never arrived.
		assert.include(
			filled,
			userOf(clobMakerKp.publicKey).toBase58(),
			'the carried maker filled, so the walk did reach the book'
		);
	});

	it('builds a landing fill out of the accounts /route names', async function () {
		this.timeout(120_000);
		// A transaction builder needs to know which accounts a fill requires,
		// and the book's makers are the half it cannot answer itself. A book
		// order lives on the book, and the only record of who owns it is an
		// authority and a sub-account on a node. This spec asserts that the
		// endpoint answers that question, and that the answer is enough to build
		// a fill that lands.
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.SHORT,
			usd(100.5),
			UNIT
		);

		const size = UNIT.divn(2);
		const route = await pollUntil(
			'a route quoting the book',
			60_000,
			async () => {
				// No maker is named in the query. Every quote rests on the book,
				// and the endpoint reads the book itself.
				const res = await fetch(
					`${SWIFT_URL}/route?marketIndex=0&direction=long&size=${size.toString()}`
				);
				if (!res.ok) {
					return undefined;
				}
				const body: any = await res.json();
				// The publisher's buffer has to have seen the ask placed above,
				// and the route has to report this size as fillable. The suite
				// has moved the oracle by now, so this test cannot assume what
				// the book is worth.
				return body.clobMakers?.length && body.filledBase !== '0'
					? body
					: undefined;
			}
		);

		// Whichever maker is at the touch by now, the endpoint has to have read
		// it off the book rather than guessed. It must name a real actor with
		// the matching `UserStats` account. Which maker it is depends on what
		// the suite left resting, and that is not what this spec tests.
		const byUser = new Map(
			[clobMakerKp, clobMaker2Kp, bookMaker2Kp, midMakerKp, takerKp].map(
				(kp) => [userOf(kp.publicKey).toBase58(), kp]
			)
		);
		const named = route.clobMakers[0];
		const namedKp = byUser.get(named.user);
		assert.isDefined(
			namedKp,
			`/route named ${named.user}, which is not a maker in this suite`
		);
		assert.equal(
			named.userStats,
			statsOf(namedKp!.publicKey).toBase58(),
			'and pairs it with its own stats account'
		);
		const namedKps = route.clobMakers.map((m: any) => {
			const kp = byUser.get(m.user);
			assert.isDefined(kp, `/route named an unknown maker ${m.user}`);
			return kp!;
		});
		// The bound comes from the route rather than from a number this test
		// picked. It is the worst price the route quoted, with room over it, so
		// the taker crosses everything the route says it would reach.
		const worstQuoted = route.books
			.flatMap((b: any) => b.levels ?? [])
			.reduce((worst: BN, l: any) => BN.max(worst, new BN(l.price)), new BN(0));
		assert.isTrue(worstQuoted.gt(new BN(0)), 'the route quoted something');

		// The id is read before the order is placed rather than matched by size
		// afterwards. By now the taker has older orders of every common size,
		// and `find` can return a closed one. A fill against a closed order
		// lands and does nothing.
		// The taker routes its own order through the endpoint's account list,
		// which is the realistic use of /route. The call is taker-signed, so it
		// is held only to whether the endpoint's accounts let it land. The
		// filler obligation a keeper owes on withheld depth does not apply.
		const fillSig = await send(
			[
				ComputeBudgetProgram.setComputeUnitLimit({ units: 800_000 }),
				await placeAndTakeIx(
					takerKp.publicKey,
					getMarketOrderParams({
						marketIndex: 0,
						direction: PositionDirection.LONG,
						baseAssetAmount: size,
						price: worstQuoted.muln(102).divn(100),
					}),
					[...namedKps, midMakerKp]
				),
			],
			[takerKp]
		);

		// The check reads the fill off its own transaction, because a cross
		// crank on the rested remainder could move a position a slot later. The
		// taker received base, and every maker that settled the fill was one the
		// endpoint named. The account list it answered with is what filled.
		const takerGot = await takerFilledBase(fillSig, userOf(takerKp.publicKey));
		assert.isTrue(
			takerGot.gt(new BN(0)),
			"a fill built from the endpoint's account list lands and fills"
		);
		const namedUsers = new Set(
			namedKps.map((kp) => userOf(kp.publicKey).toBase58())
		);
		const settled = await makersFilledBy(fillSig);
		assert.isNotEmpty(settled, 'the endpoint-named makers settled the fill');
		for (const maker of settled) {
			assert.isTrue(
				namedUsers.has(maker),
				`maker ${maker} settled the fill but the endpoint did not name it`
			);
		}
	});
});
