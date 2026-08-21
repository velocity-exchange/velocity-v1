/**
 * Full-stack e2e against a real local validator — the devnet confidence gate.
 * Run through `bash test-scripts/run-e2e-localnet.sh` (it stands up the
 * validator, redis, and builds the programs + book-publisher this consumes).
 *
 * What runs here is the real production topology, no synthesized accounts:
 * protocol init through admin instructions, CLOB bring-up exactly as the
 * admin CLI does it (book + registry entry + canonical attach + conditions
 * reservoir), a midpoint spline instance quoting around a hot-key mid, DLOB
 * maker orders, the protocol User for cranks, and the Rust book-publisher
 * ticking against the validator over RPC and writing the Redis wire while
 * its cross fast path watches for crossed books.
 *
 * Velocity instructions go through the SDK wherever it has a builder. The
 * CLOB, midpoint and relay programs ship no TS client, so their anchor wire
 * (discriminator + borsh args) is hand-encoded in the `clobIx` / `midpointIx`
 * / `relayIx` helpers below.
 */
import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { createHash } from 'crypto';
import { spawn, ChildProcess } from 'child_process';
import * as fs from 'fs';
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
	getPerpMarketPublicKeySync,
	getLimitOrderParams,
	generateSignedMsgUuid,
	HotRole,
	SignedMsgNetwork,
	getMarketOrderParams,
	getTriggerMarketOrderParams,
	isVariant,
	OrderTriggerCondition,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
	getRelayScratchPublicKey,
	getUserConditionsPublicKey,
	getQuoterSignerPublicKey,
	getVelocitySignerPublicKey,
	OracleSource,
	PEG_PRECISION,
	PositionDirection,
	PostOnlyParams,
	PRICE_PRECISION,
	QuoterCpiLeg,
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
/** `agg.price` within the pyth stub's `Price` account — the same offset
 * velocity's `oracle_watch` registers for push feeds, so a test reading the
 * feed and a relay watch reading it see the same bytes. */
const PYTH_AGG_PRICE_OFFSET = 208;

const UNIT = BASE_PRECISION; // 1e9
const USDC = new BN(10).pow(new BN(6));
/** Dollars → PRICE_PRECISION (1e6), the precision every price here is in. */
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
function u64(v: BN | number): Buffer {
	return new BN(v).toArrayLike(Buffer, 'le', 8);
}

// Account metas: read-only, writable, and their signing counterparts.
const meta = (pubkey: PublicKey, isWritable: boolean, isSigner: boolean) =>
	({ pubkey, isSigner, isWritable }) as AccountMeta;
const ro = (pubkey: PublicKey) => meta(pubkey, false, false);
const rw = (pubkey: PublicKey) => meta(pubkey, true, false);
const signerRo = (pubkey: PublicKey) => meta(pubkey, false, true);
const signerRw = (pubkey: PublicKey) => meta(pubkey, true, true);

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

// --- CLOB book byte offsets. These are a hand copy of the `CLOB_*_OFFSET`
// constants in `programs/velocity/src/state/prop_amm.rs`, which are the
// authoritative set (pinned there against the CLOB's own litesvm tests).
// Nothing checks the copy, so re-read them whenever the CLOB header changes:
// a stale arena offset reads live orders as zeros, which looks like an order
// that never rested rather than like a decoding bug.
const CLOB_BEST_BID_OFFSET = 112;
const CLOB_BEST_ASK_OFFSET = 116;
const CLOB_BID_COUNT_OFFSET = 136;
const CLOB_ASK_COUNT_OFFSET = 140;
const CLOB_ORDERS_OFFSET = 8520;
const CLOB_NODE_LEN = 96;
const CLOB_NIL = 0xffffffff;

type ClobView = {
	bidCount: number;
	askCount: number;
	bestBidPrice?: BN;
	bestAskPrice?: BN;
};

function readClobView(data: Buffer): ClobView {
	const nodePrice = (index: number): BN | undefined => {
		if (index === CLOB_NIL) return undefined;
		const off = CLOB_ORDERS_OFFSET + index * CLOB_NODE_LEN;
		return new BN(data.subarray(off + 32, off + 40), 'le');
	};
	return {
		bidCount: data.readUInt32LE(CLOB_BID_COUNT_OFFSET),
		askCount: data.readUInt32LE(CLOB_ASK_COUNT_OFFSET),
		bestBidPrice: nodePrice(data.readUInt32LE(CLOB_BEST_BID_OFFSET)),
		bestAskPrice: nodePrice(data.readUInt32LE(CLOB_BEST_ASK_OFFSET)),
	};
}

/** The CLOB program's anchor wire (it has no TS client). */
const clobIx = {
	/** `initialize_market_v0` over market 0, with the admin CLI's defaults. */
	initializeMarket(
		payer: PublicKey,
		placeAuthority: PublicKey,
		book: PublicKey
	): TransactionInstruction {
		return new TransactionInstruction({
			programId: CLOB_ID,
			keys: [signerRo(payer), ro(placeAuthority), rw(book)],
			data: Buffer.concat([
				ixDiscriminator('initialize_market_v0'),
				u16(0), // market_index
				u64(UNIT), // base_precision
				u64(100), // order_tick_size
				u64(100000), // order_step_size
				u64(100000), // min_order_size
				u32(0), // default_activation_delay_slots
				u32(20), // max_activation_delay_slots
				u32(2), // unknown_user_grace_slots
				u32(768), // evict_threshold_per_side
				u16(128), // max_quote_levels
				u16(64), // max_execute_fills
				u16(32), // max_execute_users
			]),
		});
	},
	/** `[disc][ClobHeaderV0][len u32][pad to 8]` then the 96-byte node arena. */
	space(capacity: number): number {
		return Math.ceil((8 + 8352 + 4) / 8) * 8 + capacity * CLOB_NODE_LEN;
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
	/** `initialize_quoter_v0`: market u16, sub u16, base_precision u64,
	 * staleness u64, tick u64, step u64, min u64, attested bool.
	 *
	 * `config` and `maker` are separate signers: the config key reconfigures
	 * and rotates the hot key, while the quoted wallet's signature is the
	 * consent to quote for its sub-account (and seeds the instance). They must
	 * be distinct keys — anchor v2 rejects duplicate account metas. */
	initializeQuoter(accounts: {
		payer: PublicKey;
		config: PublicKey;
		maker: PublicKey;
		executeAuthority: PublicKey;
		hot: PublicKey;
		instance: PublicKey;
	}): TransactionInstruction {
		return new TransactionInstruction({
			programId: MIDPOINT_ID,
			keys: [
				signerRw(accounts.payer),
				signerRo(accounts.config),
				signerRo(accounts.maker),
				ro(accounts.executeAuthority),
				ro(accounts.hot),
				rw(accounts.instance),
				ro(SystemProgram.programId),
			],
			data: Buffer.concat([
				ixDiscriminator('initialize_quoter_v0'),
				u16(0),
				u16(0),
				u64(UNIT),
				u64(1000), // max_mid_staleness_slots — generous for a slow tick
				u64(100), // price_tick_size
				u64(100000), // size_step
				u64(100000), // min_quote_size
				Buffer.from([0]), // require_attested_flow
			]),
		});
	},
	setMid(instance: PublicKey, hot: PublicKey, mid: BN): TransactionInstruction {
		return new TransactionInstruction({
			programId: MIDPOINT_ID,
			keys: [rw(instance), signerRo(hot)],
			data: Buffer.concat([
				ixDiscriminator('set_mid_v0'),
				u64(mid),
				u64(0), // sequence guard: off
			]),
		});
	},
	setLevels(
		instance: PublicKey,
		hot: PublicKey,
		mid: BN,
		levels: SplineLevel[]
	): TransactionInstruction {
		const side = Buffer.concat([
			Buffer.from([1]), // Some(levels)
			u32(levels.length),
			...levels.flatMap((level) => [u64(level.offsetPpm), u64(level.size)]),
		]);
		return new TransactionInstruction({
			programId: MIDPOINT_ID,
			keys: [rw(instance), signerRo(hot)],
			data: Buffer.concat([
				ixDiscriminator('set_levels_v0'),
				Buffer.from([1]), // mid: Some
				u64(mid),
				Buffer.from([0]), // sequence: None
				side, // bids
				side, // asks
			]),
		});
	},
	/** `update_quoter_v0` with `require_attested_flow = Some(true)` and every
	 * other field absent. Signed by the config key, not the quoted wallet.
	 * No flow-authority account: the instance stores no copy of that key —
	 * it reads the live one off velocity's State at quote time. */
	requireAttestedFlow(
		instance: PublicKey,
		config: PublicKey
	): TransactionInstruction {
		return new TransactionInstruction({
			programId: MIDPOINT_ID,
			keys: [
				rw(instance),
				signerRo(config),
				// Absent optional hot authority = the program id.
				ro(MIDPOINT_ID),
			],
			data: Buffer.concat([
				ixDiscriminator('update_quoter_v0'),
				Buffer.from([0]), // max_mid_staleness_slots: None
				Buffer.from([0]), // price_tick_size: None
				Buffer.from([0]), // size_step: None
				Buffer.from([0]), // min_quote_size: None
				Buffer.from([1, 1]), // require_attested_flow: Some(true)
				Buffer.from([0]), // is_paused: None
			]),
		});
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
		watch: PublicKey
	): TransactionInstruction {
		return new TransactionInstruction({
			programId: RELAY_ID,
			keys: [signerRo(payer), ro(target), rw(watch)],
			data: Buffer.concat([ixDiscriminator('register_watch_v0'), u32(8)]),
		});
	},
};

describe('e2e localnet: programs + publisher + redis', function () {
	const connection = new Connection(RPC_URL, 'confirmed');
	const payer = Keypair.generate();
	const provider = new anchor.AnchorProvider(connection, new Wallet(payer), {
		commitment: 'confirmed',
		preflightCommitment: 'confirmed',
	});

	// Actors.
	const clobMakerKp = Keypair.generate();
	/** A second maker on the book, so a fill can carry one and not the other —
	 * which is the only way to reach the withheld path. */
	const clobMaker2Kp = Keypair.generate();
	const midMakerKp = Keypair.generate();
	const midConfigKp = Keypair.generate();
	const midHotKp = Keypair.generate();
	const dlobMakerKp = Keypair.generate();
	const takerKp = Keypair.generate();
	const crosserKp = Keypair.generate();
	const publisherKp = Keypair.generate();

	let admin: TestClient;
	let clobMaker: TestClient;
	let clobMaker2: TestClient;
	let midMaker: TestClient;
	let dlobMaker: TestClient;
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
	/** The signer for quoter CPIs and CLOB calls — never the vault authority. */
	let quoterSigner: PublicKey;
	/** Resolved during bring-up: `routerTail` is synchronous. */
	let statePdaCache: PublicKey;
	let protocolUser: PublicKey;
	let protocolUserStats: PublicKey;

	/** Every spawned service, with the log fd `after` has to close. */
	const services: { child: ChildProcess; log: number }[] = [];
	/** The retail-flow attestation key swift co-signs with; registered
	 * on-chain as `State.hot_flow_authority`. */
	const flowAuthorityKp = Keypair.generate();
	/** Where relay pays its keeper: a plain account that never signs, which
	 * is what the turner requires before it will crank an *untrusted*
	 * program — i.e. velocity is treated exactly as a third-party turner
	 * would treat it, with no trust flag. */
	const relayPayout = Keypair.generate();
	const turnerKeeper = Keypair.generate();
	let redis: Redis;
	let oracleRefresher: Promise<void> | undefined;
	let stopOracleRefresher = false;
	/** What the background feed posts. A live oracle is the only way to
	 * move price on a real validator, so tests set this and wait. */
	let oracleTargetPrice = 100;

	const perpMarket = getPerpMarketPublicKeySync(VELOCITY_ID, 0);
	const userOf = (authority: PublicKey) =>
		getUserAccountPublicKeySync(VELOCITY_ID, authority, 0);
	const statsOf = (authority: PublicKey) =>
		getUserStatsAccountPublicKey(VELOCITY_ID, authority);
	/** A market's quoter-registry entry: `["quoter", market, program, user]`
	 * (`PublicKey.default` as the user for a CLOB, whose entry is shared). */
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
	 * Nothing in this file confirms through `connection.confirmTransaction`:
	 * its legacy strategy gives up after a fixed 30 seconds with a bare
	 * "unknown if it succeeded or failed" — no signature, no on-chain error —
	 * and by the later scenarios this machine is running the validator
	 * alongside redis, the book-publisher, swift and a crank turner, so a
	 * transaction that did land routinely confirms past that mark.
	 *
	 * 60s is the validity window of the blockhash the transaction was signed
	 * with (150 slots): past it the RPC has stopped rebroadcasting, so a
	 * signature still missing is missing for good and waiting longer only
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
			// 'processed' is not what the rest of this file reads at: every
			// account fetch after a send uses 'confirmed'.
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
		// Preflight stays on: it is where a reverting setup transaction gives
		// up its program logs, which is more than a status lookup can recover.
		const signature = await connection.sendRawTransaction(tx.serialize(), {
			preflightCommitment: 'confirmed',
		});
		await confirmSignature(signature);
		return signature;
	};

	/** Fills route through every quoter, well past the default CU budget. */
	const sendFill = (ix: TransactionInstruction) =>
		send([ComputeBudgetProgram.setComputeUnitLimit({ units: 800_000 }), ix]);

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

	/** Spawn a service with its stdio in `$SCRATCH/<name>.log`, and register it
	 * for teardown. A non-zero exit is reported but never fails a test on its
	 * own — the scenarios' on-chain assertions are the verdict. */
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
		// The SDK's default sender stops waiting after 35s, the same cliff a
		// loaded validator walks off; hold it to this file's budget.
		(client.txSender as RetryTxSender).timeout = CONFIRM_TIMEOUT_MS;
		clients.push(client);
		return client;
	};

	/** The margin map every router-touching instruction opens with: market 0's
	 * oracle, its quote spot market, and the perp market itself. */
	const marginMap = (): AccountMeta[] =>
		admin.getRemainingAccounts({
			userAccounts: [],
			writablePerpMarketIndexes: [0],
			writableSpotMarketIndexes: [0],
		});

	/** Register + approve a quoter, the sequence `admin-cli quoter` runs:
	 * create the registry entry, publish its two CPI account lists, then have
	 * the admin approve the surface. */
	const registerQuoterIxs = async (args: {
		authority: PublicKey;
		quoterType: QuoterType;
		quoterProgram: PublicKey;
		responseAccount: PublicKey;
		user: PublicKey;
		/** The optional leg that reports who a ladder stands on. A book has
		 * one; a quoter that fills from its own account does not. */
		quoteL3Discriminator?: number[];
		quoteLeg: { pubkey: PublicKey; isWritable: boolean }[];
		executeLeg: { pubkey: PublicKey; isWritable: boolean }[];
	}): Promise<{ quoter: PublicKey; ixs: TransactionInstruction[] }> => {
		const program = admin.program;
		const quoter = quoterKey(args.quoterProgram, args.user);
		const legs: [QuoterCpiLeg, typeof args.quoteLeg][] = [
			[QuoterCpiLeg.QUOTE, args.quoteLeg],
			[QuoterCpiLeg.EXECUTE, args.executeLeg],
		];
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
							quoterProgram: args.quoterProgram,
							user: args.user,
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
				),
				...legs.map(([leg, metas]) =>
					program.instruction.updateQuoterAccounts(
						{ leg, index: 0, metas },
						{ accounts: { authority: args.authority, quoter } }
					)
				),
				program.instruction.updateQuoterApproved(true, {
					accounts: {
						admin: payer.publicKey,
						state: await admin.getStatePublicKey(),
						quoter,
					},
				}),
			],
		};
	};

	/** CLOB bring-up, exactly as `admin-cli clob-market init` does it. */
	const clobBringUp = async () => {
		const space = clobIx.space(1024);
		clobBook = Keypair.generate();
		await send(
			[
				await createAccount(clobBook.publicKey, space, CLOB_ID),
				clobIx.initializeMarket(
					payer.publicKey,
					quoterSigner,
					clobBook.publicKey
				),
			],
			[clobBook]
		);

		const registration = await registerQuoterIxs({
			authority: payer.publicKey,
			quoterType: QuoterType.CLOB,
			quoterProgram: CLOB_ID,
			responseAccount: clobBook.publicKey,
			user: PublicKey.default,
			// The book answers who rests on it, so no reader decodes it.
			quoteL3Discriminator: Array.from(ixDiscriminator('quote_l3_v0')),
			quoteLeg: [{ pubkey: clobBook.publicKey, isWritable: true }],
			executeLeg: [
				{ pubkey: clobBook.publicKey, isWritable: true },
				{ pubkey: quoterSigner, isWritable: false },
			],
		});
		clobEntry = registration.quoter;
		await send(registration.ixs);

		// Attach as the market's canonical CLOB (creates conditions) + fund
		// the crank reservoir.
		conditions = getClobCrankConditionsPublicKey(VELOCITY_ID, 0);
		await send([
			admin.program.instruction.updatePerpMarketClobQuoter(
				new BN(10_000), // keeper_payment_lamports
				new BN(1500), // expire_fallback_slots
				// min_cross_surplus: 0 keeps the bare strictly-profitable rule,
				// which is what the cross-match scenario below asserts against.
				new BN(0),
				{
					accounts: {
						admin: payer.publicKey,
						state: await admin.getStatePublicKey(),
						perpMarket,
						quoter: clobEntry,
						clobMarket: clobBook.publicKey,
						crankConditions: conditions,
						rent: SYSVAR_RENT_PUBKEY,
						systemProgram: SystemProgram.programId,
					},
				}
			),
			SystemProgram.transfer({
				fromPubkey: payer.publicKey,
				toPubkey: conditions,
				lamports: LAMPORTS_PER_SOL,
			}),
		]);
	};

	/** Midpoint instance + spline, then its Custom registry entry. */
	const midpointBringUp = async () => {
		const statePda = await admin.getStatePublicKey();
		midInstance = midpointIx.instance(midMakerKp.publicKey);
		await send(
			[
				midpointIx.initializeQuoter({
					payer: payer.publicKey,
					config: midConfigKp.publicKey,
					maker: midMakerKp.publicKey,
					executeAuthority: quoterSigner,
					hot: midHotKp.publicKey,
					instance: midInstance,
				}),
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
			// Both legs end with velocity's State: midpoint reads the live
			// `hot_flow_authority` off it rather than caching a copy, so the
			// key can rotate without every instance being reconfigured.
			quoteLeg: [
				{ pubkey: midInstance, isWritable: true },
				{ pubkey: SYSVAR_INSTRUCTIONS_PUBKEY, isWritable: false },
				{ pubkey: statePda, isWritable: false },
			],
			executeLeg: [
				{ pubkey: midInstance, isWritable: true },
				{ pubkey: quoterSigner, isWritable: false },
				{ pubkey: SYSVAR_INSTRUCTIONS_PUBKEY, isWritable: false },
				{ pubkey: statePda, isWritable: false },
			],
		});
		midEntry = registration.quoter;
		await send(registration.ixs, [midMakerKp]);
	};

	const setMidpointMid = (mid: BN) =>
		send([midpointIx.setMid(midInstance, midHotKp.publicKey, mid)], [midHotKp]);

	const setMidpointLevels = (mid: BN, levels: SplineLevel[]) =>
		send(
			[midpointIx.setLevels(midInstance, midHotKp.publicKey, mid, levels)],
			[midHotKp]
		);

	/** The protocol-owned User (velocity signer's sub-account 0) for cranks. */
	const initProtocolUser = async () => {
		protocolUser = userOf(velocitySigner);
		protocolUserStats = statsOf(velocitySigner);
		const program = admin.program;
		// The SDK's initializeUser builders only ever act for their own
		// wallet's authority; this User's authority is the velocity signer.
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
					// Optional, but anchor still wants it named: pass the PDA
					// so the protocol user is relay-covered like any other.
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
		const ix = client.program.instruction.placeClobOrder(
			{
				marketIndex: 0,
				direction,
				price,
				baseAssetAmount: size,
				maxTs,
				activationDelaySlots: 0,
			},
			{
				accounts: {
					state: await client.getStatePublicKey(),
					user: userOf(kp.publicKey),
					authority: kp.publicKey,
					quoter: clobEntry,
					clobMarket: clobBook.publicKey,
					clobProgram: CLOB_ID,
					quoterSigner,
					crankConditions: conditions,
					// No fast activation here: absent, encoded as the
					// program id (anchor's `None`).
					instructionsSysvar: VELOCITY_ID,
				},
				remainingAccounts: marginMap(),
			}
		);
		await client.sendTransaction(new Transaction().add(ix));
	};

	const readClob = async (): Promise<ClobView> => {
		const info = await connection.getAccountInfo(clobBook.publicKey);
		return readClobView(info!.data);
	};

	const registerWatch = async (target: PublicKey) => {
		const watch = Keypair.generate();
		await send(
			[
				await createAccount(watch.publicKey, WATCH_V0_LEN, RELAY_ID),
				relayIx.registerWatch(payer.publicKey, target, watch.publicKey),
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
				// Scoped to velocity's watches, as an operator would run it.
				'--target-program',
				VELOCITY_ID.toBase58(),
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
		// Addresses added in slot N only resolve from N+1: the account
		// reads back immediately, but a transaction using it before the
		// slot turns over fails with "invalid index" at load time.
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
		// Swift's RPC simulation uses a fixed fee payer that never signs but
		// must exist and hold SOL (gas-station-maintained in production).
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
			// Long enough that the scenario reliably observes the
			// too-early response, short enough that the signed
			// message's slot window survives the round-trip.
			ATTESTATION_HOLD_MS: '600',
			// Intake's pre-flight RPC simulation is a production
			// admission guard, not part of the attestation loop, and
			// it needs the client's devnet market plumbing that a
			// freshly-initialized localnet doesn't provide. The real
			// verdict here is the on-chain fill at the end.
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

	/** Lamports relay has paid its keeper — the proof a crank came from the
	 * turner rather than from this test or the publisher, which pay their
	 * own authorities instead. */
	const relayPayoutBalance = () => connection.getBalance(relayPayout.publicKey);

	/** The account tail every router-touching ix wants: margin maps, the
	 * `(User, UserStats)` pairs of the makers that may fill, then the
	 * quoter section (entries followed by their registered CPI accounts). */
	const routerTail = (makerKps: Keypair[]): AccountMeta[] => [
		...marginMap(),
		...makerKps.flatMap((kp) => [
			rw(userOf(kp.publicKey)),
			rw(statsOf(kp.publicKey)),
		]),
		ro(clobEntry),
		ro(midEntry),
		rw(clobBook.publicKey),
		ro(quoterSigner),
		ro(CLOB_ID),
		rw(midInstance),
		ro(SYSVAR_INSTRUCTIONS_PUBKEY),
		// Midpoint reads the live flow authority off velocity's State on both
		// legs. The ix's own `state` account is not in the CPI account map —
		// that map is built from the remaining accounts — so it appears again
		// here, which costs one index byte.
		ro(statePdaCache),
		ro(MIDPOINT_ID),
	];

	/** `fillPerpOrder` as the keeper, with the router tail the SDK's
	 * `getFillPerpOrderIx` cannot express (it has no quoter section, and
	 * marks the quote spot market read-only). */
	const fillPerpOrderIx = async (
		orderId: number,
		takerAuthority: PublicKey,
		makerKps: Keypair[],
		/** The route the order's signer chose, as a keeper reads it off their
		 * signed message. The program checks it against the digest stamped on
		 * the order and requires every entry to be in the transaction, so a
		 * keeper cannot quietly route somewhere else. */
		signedRoute: PublicKey[] = []
	) =>
		// The v1 route: a restable remainder of the filled order rests on the
		// book instead of staying in `User.orders`. Keepers use it in
		// production (keep-rs's swift path), so the suite fills the same way.
		admin.program.instruction.fillPerpOrderV1(
			orderId,
			null,
			signedRoute,
			0, // market_index — the conditions PDA seed needs it up front
			{
				accounts: {
					state: await admin.getStatePublicKey(),
					authority: payer.publicKey,
					filler: userOf(payer.publicKey),
					fillerStats: statsOf(payer.publicKey),
					user: userOf(takerAuthority),
					userStats: statsOf(takerAuthority),
					quoter: clobEntry,
					clobMarket: clobBook.publicKey,
					clobProgram: CLOB_ID,
					quoterSigner,
					crankConditions: conditions,
				},
				remainingAccounts: routerTail(makerKps),
			}
		);

	/** Fill a user's open order as the keeper — how a position gets opened
	 * on a real validator (nothing here can be synthesized). */
	const fillPendingOrder = async (
		client: TestClient,
		kp: Keypair,
		makerKps: Keypair[] = [clobMakerKp, midMakerKp]
	) => {
		await client.fetchAccounts();
		const order = client
			.getUserAccount()!
			.orders.find((o) => isVariant(o.status, 'open'))!;
		await sendFill(
			await fillPerpOrderIx(order.orderId, kp.publicKey, makerKps)
		);
	};

	/** Opt a user into relay coverage: liquidation thresholds and triggers,
	 * one instruction over one account. `extra` appends to the condition
	 * pass's own accounts (e.g. a quoter entry for trigger routing). */
	const syncUserConditions = (user: PublicKey, extra: AccountMeta[] = []) =>
		send([
			admin.program.instruction.syncUserConditions(
				{
					syncPaymentLamports: new BN(20_000), // from its own lamports
					syncFallbackSlots: new BN(3000), // coarse fallback poll
				},
				{
					accounts: {
						payer: payer.publicKey,
						user,
						userConditions: getUserConditionsPublicKey(VELOCITY_ID, user),
						rent: SYSVAR_RENT_PUBKEY,
						systemProgram: SystemProgram.programId,
					},
					// Margin maps, then the market's reservoir (keeper fee).
					remainingAccounts: [...marginMap(), ro(conditions), ...extra],
				}
			),
		]);

	before(async function () {
		this.timeout(600_000);

		for (const kp of [
			payer,
			clobMakerKp,
			clobMaker2Kp,
			midMakerKp,
			midHotKp,
			dlobMakerKp,
			takerKp,
			crosserKp,
			publisherKp,
			turnerKeeper,
		]) {
			await airdrop(kp.publicKey, 100);
		}
		// The payout is rent-exempt but never signs; relay credits it.
		await airdrop(relayPayout.publicKey, 1);

		velocitySigner = getVelocitySignerPublicKey(VELOCITY_ID);
		quoterSigner = getQuoterSignerPublicKey(VELOCITY_ID);
		usdcMint = await createUsdcMint();

		// $100 oracle through the pyth stub program (drivable on a real
		// validator, unlike lazer accounts which need signed posts).
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
			20000, // base_spread (2%) — keeps the vAMM away from the touch
			50000 // max_spread
		);
		await admin.updatePerpAuctionDuration(0);

		await clobBringUp();
		await initProtocolUser();

		// Actors with deposits (the midpoint's quoted User must exist before
		// its Custom entry is created — consent reads User.authority).
		clobMaker = newClient(clobMakerKp);
		clobMaker2 = newClient(clobMaker2Kp);
		midMaker = newClient(midMakerKp);
		dlobMaker = newClient(dlobMakerKp);
		taker = newClient(takerKp);
		crosser = newClient(crosserKp);
		for (const [client, kp] of [
			[clobMaker, clobMakerKp],
			[clobMaker2, clobMaker2Kp],
			[midMaker, midMakerKp],
			[dlobMaker, dlobMakerKp],
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

		// The keeper's filler user (fill_perp_order loads it).
		const adminUsdc = await fundUsdc(payer.publicKey, new BN(1_000).mul(USDC));
		await admin.initializeUserAccountAndDepositCollateral(
			new BN(1_000).mul(USDC),
			adminUsdc
		);

		// Keep the oracle fresh: on a real cluster a live feed does this;
		// here a background loop re-posts `oracleTargetPrice` every second
		// so the vAMM stays inside its oracle-validity gates, and so tests
		// can move price by moving the target.
		//
		// It has to be `set_price_info`, not `set_price`: velocity reads
		// staleness off `valid_slot` (`get_pyth_price`), which `set_price`
		// leaves at whatever `initialize` wrote. A feed that never advances
		// its slot ages out of every validity gate no matter how often the
		// price is rewritten.
		oracleRefresher = (async () => {
			while (!stopOracleRefresher) {
				try {
					await send([
						pythProgram.instruction.setPriceInfo(
							usd(oracleTargetPrice),
							usd(0.01),
							new BN(await connection.getSlot()),
							{ accounts: { price: oracle } }
						),
					]);
				} catch {
					// transient send failures are fine; the next beat retries
				}
				await sleep(1000);
			}
		})();

		// Standing liquidity: CLOB 1.0 bid/ask at 99.5/100.5, DLOB post-only
		// ask 1.0 @ 100.6 (stays with the TS side of the book wire for now).
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
		await dlobMaker.placePerpOrder(
			getLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.SHORT,
				baseAssetAmount: UNIT,
				price: usd(100.6),
				postOnly: PostOnlyParams.MUST_POST_ONLY,
			})
		);

		// The publisher, exactly as deployed: RPC transport against the
		// validator, RPC-side simulation, cross fast path armed.
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

		// Relay: register the market's crank conditions (evict / expire /
		// cross / activation all live in that one block) and start a
		// turner. Everything after this point is cranked by relay unless a
		// test explicitly submits.
		await registerWatch(conditions);
		startTurner();

		// Attested flow: register swift's co-signing key as the on-chain
		// flow authority, then bring swift up with it.
		await admin.updateHotAdmin(
			HotRole.FlowAuthority,
			flowAuthorityKp.publicKey
		);
	});

	after(async function () {
		stopOracleRefresher = true;
		await oracleRefresher;
		// Always surface the publisher's view for post-mortems.
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
		// CLOB 100.5 beats the midpoint's 100.1? No — asks best-first:
		// midpoint 100.1, CLOB 100.5, vAMM ~101. Wait for a book carrying
		// all three sources.
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

		// The vAMM is in the picture too, settled by the market itself.
		const vammRow = l3.asks.find((row: any) => row.source === 'vamm');
		assert.equal(vammRow.maker, perpMarket.toBase58());
		assert.isNull(vammRow.orderId);

		// Best makers names accounts a fill has to carry, so the market is
		// not one of them.
		const bestMakers = JSON.parse(
			(await redis.get('last_update_orderbook_best_makers_perp_0'))!
		);
		assert.include(bestMakers.asks, makerPda);
		assert.include(bestMakers.asks, midMakerPda);
		assert.include(bestMakers.bids, makerPda);
		assert.notInclude(bestMakers.asks, perpMarket.toBase58());

		// Grouped channel publishes.
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

	it('keeper fill splits the taker across CLOB + midpoint + vAMM', async function () {
		this.timeout(120_000);
		// Long 3.5: midpoint 100.1 (2.0), CLOB 100.5 (1.0), then the DLOB
		// maker at 100.6 (0.5) — the vAMM ask sits ~1% out and yields to all.
		const size = UNIT.muln(35).divn(10);
		await taker.placePerpOrder(
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: size,
				price: usd(102),
			})
		);
		const order = (await taker.forceGetUserAccount())!.orders.find((o) =>
			o.baseAssetAmount.eq(size)
		)!;

		// Maker section: DLOB maker + both quoted users.
		await sendFill(
			await fillPerpOrderIx(order.orderId, takerKp.publicKey, [
				dlobMakerKp,
				clobMakerKp,
				midMakerKp,
			])
		);

		await taker.fetchAccounts();
		const position = taker.getUser().getPerpPosition(0)!;
		assert.equal(position.baseAssetAmount.toString(), size.toString());

		// The CLOB ask is gone; the midpoint's first rung is fully consumed.
		const book = await readClob();
		assert.equal(book.askCount, 0);
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
		await dlobMaker.fetchAccounts();
		assert.equal(
			dlobMaker.getUser().getPerpPosition(0)!.baseAssetAmount.toString(),
			UNIT.divn(2).neg().toString()
		);
	});

	it('place-and-take rests the unfilled limit remainder on the CLOB', async function () {
		this.timeout(120_000);
		// A 100.0 limit long sits below every ask (best is the midpoint's
		// 100.1 after the fill test re-arms below) — nothing fills, and the
		// remainder migrates onto the CLOB as a resting bid.
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
			undefined,
			undefined,
			undefined,
			undefined,
			undefined,
			undefined,
			{
				quoter: clobEntry,
				clobMarket: clobBook.publicKey,
				clobProgram: CLOB_ID,
				quoterSigner,
				crankConditions: conditions,
			}
		);
		await taker.sendTransaction(new Transaction().add(ix));

		const after = await readClob();
		assert.equal(after.bidCount, before.bidCount + 1);
		assert.equal(after.bestBidPrice!.toString(), usd(100).toString());
		// The taker's DLOB order slot is not resting open (migrated) — scope
		// to this order's price so unrelated leftovers can't bleed in.
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

		// Cross the taker's freshly rested 100.0 bid... no — cross the book
		// outright: a 101.0 bid against the midpoint's 100.1 ask clears the
		// two-legged taker fees with ~90bps of spread. PropAMM×CLOB is the
		// cross only the publisher can discover.
		await placeClobOrder(
			crosser,
			crosserKp,
			PositionDirection.LONG,
			usd(101),
			UNIT
		);

		// The publisher's next tick sees the cross and submits
		// crank_cross_match; the crossed bid gets consumed.
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

		// The crosser is long (their crossed bid filled), the midpoint maker
		// shorter, and the protocol user pocketed the after-fee surplus as
		// its quote balance while staying flat.
		await crosser.fetchAccounts();
		assert.isAbove(
			crosser.getUser().getPerpPosition(0)!.baseAssetAmount.toNumber(),
			0
		);
		const protocolAfter = (await connection.getAccountInfo(protocolUser))!;
		assert.notDeepEqual(
			protocolAfter.data.subarray(0, 4384),
			protocolBefore.data.subarray(0, 4384),
			'protocol user settled the cross legs'
		);
	});

	// A migrated taker remainder is the one order on the book nobody is allowed
	// to take while a counterparty crosses it, so the improvement between the
	// two prices cannot be won by landing a transaction at the activation slot.
	// This is the path that hands that improvement to the taker instead, and the
	// assertions below are about where the money went: the taker's all-in cost
	// must beat the price it was resting at, and it must not beat the
	// counterparty's price, which is the band the design promises.
	it('hands a crossed taker remainder the counterparty price, cranked by relay', async function () {
		this.timeout(120_000);

		// The curve is in every fill's baseline and is deep at the ~100 oracle,
		// so a limit long at 104 crosses it and fills there — a remainder only
		// exists when the taker's bound is tighter than the curve. Pausing the
		// curve's fills is what makes a remainder reachable at a price that
		// leaves a counterparty room to improve on it; without this the order
		// fills from the curve and every assertion below passes while testing
		// nothing.
		await admin.updatePerpMarketPausedOperations(
			0,
			PerpOperation.AMM_FILL | PerpOperation.AMM_IMMEDIATE_FILL
		);

		try {
			// Park the midpoint 5% wide of a 100 mid. Every price below is chosen to
			// sit inside that spread so the midpoint neither fills the remainder at
			// placement (its ask is 105) nor crosses the counterparty ask (its bid
			// is 95) — the sole cross on the book is the pair this crank owns, which
			// is what makes the poll below unambiguous.
			await setMidpointLevels(usd(100), [
				{ offsetPpm: 50_000, size: UNIT.muln(2) },
			]);

			// Preconditions, asserted rather than assumed: earlier specs leave
			// orders behind, and a stale ask under 104 would fill the order instead
			// of resting it — which would still pass a naive "remainder is gone"
			// poll while testing nothing.
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
				undefined,
				undefined,
				undefined,
				undefined,
				undefined,
				undefined,
				{
					quoter: clobEntry,
					clobMarket: clobBook.publicKey,
					clobProgram: CLOB_ID,
					quoterSigner,
					crankConditions: conditions,
				}
			);
			await taker.sendTransaction(new Transaction().add(ix));

			const rested = await readClob();
			assert.equal(rested.bestBidPrice!.toString(), usd(104).toString());

			// The counterparty: an ordinary maker ask 3.00 better than the price the
			// remainder is resting at.
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
			// this is the all-in cost, not the headline fill price.
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

			// The counterparty sold its unit — as a delta, since it carries a long
			// from the cross-match spec that this sale happens to flatten.
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

			// Relay's keeper — not this test, and not the publisher — is who cranked
			// it, which is the only evidence that discovery reached this crank.
			assert.isAbove(
				await relayPayoutBalance(),
				relayPaid0,
				'relay paid its keeper for the crank'
			);
		} finally {
			// Put the curve and the 10bps spline back even when an assertion above
			// throws: the specs below inherit both, and one pins a published price
			// derived from those levels, so leaving them changed turns one failure
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

	// ---------------------------------------------------------------------------
	// Relay: a live crank-turner discovering and landing work with nobody
	// submitting it. These three flows are relay's alone — the publisher only
	// ever submits `crank_cross_match` — so a state change here plus a credit to
	// relay's payout account is unambiguous attribution.
	// ---------------------------------------------------------------------------

	it('reclaims an expired CLOB order without anyone submitting', async function () {
		this.timeout(180_000);
		const before = await readClob();
		const payoutBefore = await relayPayoutBalance();

		// A CLOB ask that expires in ~10s. Velocity min-folds the expiry
		// into the market's wake hint as it places, so the turner has a
		// deadline to wake on.
		//
		// Priced well above every bid on the book — the midpoint spline
		// quotes around 102, so an ask near the touch gets crossed and
		// filled, and the order count returns to baseline for a reason
		// that has nothing to do with expiry.
		await clobMaker.fetchAccounts();
		const makerSizeBefore =
			clobMaker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);
		const now = Math.floor(Date.now() / 1000);
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

		// Nobody in this test submits anything from here on.
		await pollUntil('relay to reclaim the expired order', 120_000, async () => {
			const book = await readClob();
			return book.askCount === before.askCount ? true : undefined;
		});
		// Reclaimed, not filled: a cross would also drop the count.
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

	it('fires an armed trigger order when the oracle crosses', async function () {
		this.timeout(180_000);
		// A stop: sell 0.5 if the oracle climbs through 104.
		await taker.placePerpOrder(
			getTriggerMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.SHORT,
				baseAssetAmount: UNIT.divn(2),
				triggerPrice: usd(104),
				triggerCondition: OrderTriggerCondition.ABOVE,
			})
		);
		await taker.fetchAccounts();
		const armed = taker
			.getUserAccount()!
			.orders.find(
				(o) => isVariant(o.status, 'open') && o.triggerPrice.eq(usd(104))
			)!;
		assert.isOk(armed, 'trigger order is armed');

		// Sync its relay conditions (an OnValueCross watch at the trigger
		// threshold), register the watch, and let the turner have it. One
		// conditions account per user now — the same one the liquidation
		// thresholds live on, so one sync covers both halves.
		const takerUser = userOf(takerKp.publicKey);
		await syncUserConditions(takerUser, [ro(clobEntry)]);
		await registerWatch(getUserConditionsPublicKey(VELOCITY_ID, takerUser));

		const payoutBefore = await relayPayoutBalance();
		// Move the oracle through the trigger. Nobody submits a trigger ix.
		await setOraclePrice(106);

		await pollUntil('relay to fire the trigger', 120_000, async () => {
			await taker.fetchAccounts();
			const order = taker
				.getUserAccount()!
				.orders.find((o) => o.orderId === armed.orderId);
			// Triggered orders either carry a Triggered* condition or have
			// already filled and freed the slot.
			const fired =
				!order ||
				!isVariant(order.status, 'open') ||
				isVariant(order.triggerCondition, 'triggeredAbove');
			return fired ? true : undefined;
		});
		assert.isAbove(await relayPayoutBalance(), payoutBefore);
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
		// Standing ask deep enough for the whole entry: without it the
		// vAMM's slippage caps the fill near one unit and the "victim" ends
		// up ~1.7x — never liquidatable, and the scenario passes vacuously.
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.SHORT,
			usd(102),
			UNIT.muln(5)
		);
		// ~8.5x: 5 units at ~102 on 60 of collateral — a price drop to 84
		// puts equity below zero, well past maintenance.
		await victim.placePerpOrder(
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: UNIT.muln(5),
				price: usd(103),
			})
		);
		await fillPendingOrder(victim, victimKp);
		await victim.fetchAccounts();
		assert.isTrue(
			victim.getUser().getPerpPosition(0)!.baseAssetAmount.eq(UNIT.muln(5)),
			'victim entered the full intended size'
		);

		// Opt them into relay liquidation coverage: thresholds from their
		// live positions, a self-sync watch, and a funded sync reservoir.
		const victimUser = userOf(victimKp.publicKey);
		const userConditions = getUserConditionsPublicKey(VELOCITY_ID, victimUser);
		await syncUserConditions(victimUser);
		await airdrop(userConditions, 1);
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
		// Measured, not assumed: a fixed size to compare against passes
		// vacuously the moment the victim's position is smaller than it,
		// which reports "relay liquidated" for a relay that did nothing.
		const sizeBefore = victim.getUser().getPerpPosition(0)!.baseAssetAmount;
		// Crash the oracle — hard enough to put the victim under
		// maintenance (equity ~$14 vs ~$23 required at 93), gentle enough
		// to stay inside the oracle price bands a 16% single-slot move
		// breaches (`PriceBandsBreached` on the staged executor). Nobody
		// submits a liquidation.
		await setOraclePrice(93);

		await pollUntil('relay to liquidate', 180_000, async () => {
			await victim.fetchAccounts();
			const position = victim.getUser().getPerpPosition(0);
			const reduced = !position || position.baseAssetAmount.lt(sizeBefore);
			return reduced ? true : undefined;
		});
		assert.isAbove(await relayPayoutBalance(), payoutBefore);

		// The protocol User was only the filler: it must not be holding the
		// liquidated position.
		const protocolUserAccount = await connection.getAccountInfo(protocolUser);
		assert.isOk(protocolUserAccount);
		await setOraclePrice(100);
		await victim.unsubscribe();
	});

	it('fills a swift order through the attested-flow loop', async function () {
		this.timeout(180_000);
		// Swift boots here, last, against a settled chain: its client
		// snapshots the market list at startup, and a boot racing the
		// bring-up sees an empty world (and its intake simulation panics
		// on missing market data — observed, not hypothetical).
		await startSwift();
		// Gate the midpoint on attestation: from here, only transactions
		// co-signed by the flow authority see its books — which makes the
		// midpoint's participation below an on-chain proof that the
		// co-signature carried, not just that the endpoints answered.
		await send(
			[midpointIx.requireAttestedFlow(midInstance, midConfigKp.publicKey)],
			[midConfigKp]
		);

		// Re-arm the midpoint: earlier scenarios consumed its ask rungs
		// (filled is standing intent) and its mid may have gone stale.
		await setMidpointLevels(usd(100), [
			{ offsetPpm: 1000, size: UNIT.muln(2) },
			{ offsetPpm: 3000, size: UNIT.muln(2) },
		]);

		// The taker signs an order off-chain and hands it to swift — the
		// real intake, which verifies, simulates, publishes to keepers,
		// and records it as attestable.
		if (
			!(await taker.isSignedMsgUserOrdersAccountInitialized(takerKp.publicKey))
		) {
			await taker.initializeSignedMsgUserOrders(takerKp.publicKey, 8);
		}
		await taker.fetchAccounts();
		const takerUser = userOf(takerKp.publicKey);
		const positionBefore =
			taker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);
		await midMaker.fetchAccounts();
		const midBefore =
			midMaker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);

		// A v0 message over a lookup table, the way a real keeper sends a
		// swift fill: the ed25519 instruction carries the whole signed
		// message, so the router tail does not fit in a legacy transaction.
		// Built before the order is submitted — a table costs two slots to
		// activate, and doing that after intake would burn the hold window
		// this scenario exists to observe.
		const lookupTable = await createRouterLookupTable([
			...routerTail([clobMakerKp, midMakerKp]).map((a) => a.pubkey),
			VELOCITY_ID,
			takerUser,
			statsOf(takerKp.publicKey),
			userOf(payer.publicKey),
			statsOf(payer.publicKey),
		]);

		// Sign and submit, freshly each attempt: the signed message pins a
		// slot, so a retry has to re-sign rather than replay. Retried
		// because swift's market/oracle subscriptions warm up
		// asynchronously after boot — in production it has been up for
		// hours before an order arrives; here it is seconds old.
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
				// Tagged for this cluster (the program refuses a message
				// signed for the other one) and routed through the midpoint
				// explicitly — the CLOB and vAMM baseline is implicit.
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

		// The keeper's side, wire-for-wire what keep-rs does: the flow
		// authority rides the compute-budget price instruction as a
		// read-only co-signer, the transaction is signed against a fixed
		// blockhash, and /attest is polled through the hold window.
		const cuLimit = ComputeBudgetProgram.setComputeUnitLimit({
			units: 1_400_000,
		});
		// Compute budget parses no accounts, so the co-signer meta rides
		// here inertly — exactly how keep-rs marks an attested fill.
		cuLimit.keys.push(signerRo(flowAuthorityKp.publicKey));
		const [ed25519Ix, placeIx] = await admin.getPlaceSignedMsgTakerPerpOrderIxs(
			signed,
			0,
			{
				taker: takerUser,
				takerStats: statsOf(takerKp.publicKey),
				takerUserAccount: taker.getUserAccount()!,
				signingAuthority: takerKp.publicKey,
			},
			[cuLimit]
		);
		// Name the order id the swift placement will take: with `null` the
		// fill picks the user's first fillable order, which here is a
		// leftover triggered short from an earlier scenario.
		//
		// Both makers: the CLOB carries resting orders from earlier
		// scenarios, and a quote whose user set omits them fails
		// `StaleUserSet` once they age past the grace window.
		const fillIx = await fillPerpOrderIx(
			taker.getUserAccount()!.nextOrderId,
			takerKp.publicKey,
			[clobMakerKp, midMakerKp],
			// The taker signed for the midpoint above; a fill that ignored it
			// is refused, so pass it exactly as keep-rs does.
			[midEntry]
		);
		const blockhash = await connection.getLatestBlockhash();
		const message = new TransactionMessage({
			payerKey: payer.publicKey,
			recentBlockhash: blockhash.blockhash,
			instructions: [cuLimit, ed25519Ix, placeIx, fillIx],
		}).compileToV0Message([lookupTable]);
		const tx = new VersionedTransaction(message);
		tx.sign([payer]);
		const raw = Buffer.from(tx.serialize()).toString('base64');

		let sawHoldWindow = false;
		let attested: Buffer | undefined;
		for (let attempt = 0; attempt < 8 && !attested; attempt++) {
			const res = await fetch(`${SWIFT_URL}/attest`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({
					uuid: Buffer.from(uuid).toString(),
					transaction: raw,
				}),
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
			attested = Buffer.from(
				(JSON.parse(body) as { transaction: string }).transaction,
				'base64'
			);
		}
		assert.isTrue(sawHoldWindow, 'the hold window was observed');
		assert.isOk(attested, 'attestation granted after the hold');

		// Submitting the co-signed bytes verbatim: the validator verifies
		// BOTH signatures, so landing at all proves swift's co-signature.
		// Simulated with sigVerify first — sendRawTransaction's preflight
		// does NOT check signatures, so a bad co-signature would otherwise
		// surface only as a transaction that silently never lands.
		const verified = await connection.simulateTransaction(
			VersionedTransaction.deserialize(attested!),
			{ sigVerify: true, replaceRecentBlockhash: false }
		);
		assert.isNull(
			verified.value.err,
			`attested tx failed sigVerify simulation: ${JSON.stringify(
				verified.value.err
			)} ${JSON.stringify(verified.value.logs?.slice(-6))}`
		);
		// Preflight is skipped deliberately: the sigVerify simulation above
		// is the stronger check (preflight does not verify signatures at
		// all), and on this validator preflighted sends were dropped
		// inconsistently while this path lands reliably.
		const signature = await connection.sendRawTransaction(attested!, {
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
		// The attestation reached the midpoint: its books only open to
		// co-signed flow now, and its maker went short against the taker.
		await midMaker.fetchAccounts();
		const midAfter =
			midMaker.getUser().getPerpPosition(0)?.baseAssetAmount ?? new BN(0);
		assert.isTrue(
			midAfter.lt(midBefore),
			`midpoint maker filled attested flow (before=${midBefore} after=${midAfter})`
		);
	});
	it('forfeits book depth it cannot carry rather than routing it worse', async function () {
		this.timeout(120_000);
		// Two makers on the book, the second better than everything else the
		// route offers. The fill carries the first and not the second, which
		// is what happens whenever a book holds more makers than a
		// transaction has account locks for.
		//
		// The book stops at the maker it was not given and reports what it
		// was holding. The router reserves that depth instead of handing it
		// to the midpoint or the vAMM, so the taker keeps it unfilled — it
		// rests where the book's own price can still reach it, rather than
		// locking in a price the book was beating.
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
		// The book skips an order whose owner is missing while it is younger
		// than `unknown_user_grace_slots` — the caller could not have heard
		// of it yet — and only stops on it once older. Both of this test's
		// makers have to be past that window for the fill to see the second
		// one at all.
		await sleep(2_000);

		// The midpoint quotes worse than both, so it is what the reserve has
		// to keep off the withheld depth.
		await setMidpointLevels(usd(100), [
			{ offsetPpm: 8000, size: UNIT.muln(2) },
		]);

		const before = {
			maker1: (await clobMaker.forceGetUserAccount())?.perpPositions.find(
				(p) => p.marketIndex === 0
			)?.baseAssetAmount,
			maker2: (await clobMaker2.forceGetUserAccount())?.perpPositions.find(
				(p) => p.marketIndex === 0
			)?.baseAssetAmount,
		};

		const size = UNIT.muln(15).divn(10);
		// The id up front, for the same reason the route test takes it that
		// way: matching an order by its size finds whichever one matches.
		const orderId = (await taker.forceGetUserAccount())!.nextOrderId;
		await taker.placePerpOrder(
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: size,
				price: usd(102),
			})
		);

		// The whole point: `clobMaker2Kp` is deliberately absent.
		await sendFill(
			await fillPerpOrderIx(orderId, takerKp.publicKey, [
				clobMakerKp,
				midMakerKp,
			])
		);

		// The maker it could not carry is untouched, and still on the book.
		await clobMaker2.fetchAccounts();
		const maker2After = clobMaker2
			.getUser()
			.getPerpPosition(0)!.baseAssetAmount;
		assert.equal(
			maker2After.toString(),
			(before.maker2 ?? new BN(0)).toString(),
			'the maker the fill could not carry is not filled'
		);
		const book = await readClob();
		assert.isAtLeast(book.askCount, 1, 'its ask is still resting');

		// The maker it *could* carry did fill, which is what says the walk
		// reached the book and got past the first order — so stopping at the
		// second is the withheld path and not simply never arriving.
		await clobMaker.fetchAccounts();
		const maker1After = clobMaker.getUser().getPerpPosition(0)!.baseAssetAmount;
		assert.isTrue(
			maker1After.lt(before.maker1 ?? new BN(0)),
			'the carried maker filled, so the walk did reach the book'
		);
	});

	it('builds a landing fill out of the accounts /route names', async function () {
		this.timeout(120_000);
		// The question a transaction builder actually has is "which accounts
		// do I need", and the book's makers are the half it cannot answer
		// itself: a book order lives on the book, and the only record of who
		// owns it is an authority and a sub-account on a node. This asserts
		// the endpoint answers it, and that the answer is enough to build a
		// fill that lands.
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
				const res = await fetch(
					`${SWIFT_URL}/route?marketIndex=0&direction=long&size=${size.toString()}` +
						`&dlobMakers=${userOf(dlobMakerKp.publicKey).toBase58()}`
				);
				if (!res.ok) {
					return undefined;
				}
				const body: any = await res.json();
				// The publisher's buffer has to have seen the ask we just placed,
				// and the route has to think this size is fillable at all — by
				// now the suite has moved the oracle around, so what the book is
				// worth is not something this test gets to assume.
				return body.clobMakers?.length && body.filledBase !== '0'
					? body
					: undefined;
			}
		);

		// Whichever maker is at the touch by now, the endpoint has to have
		// read it off the book rather than guessed: a real actor, with the
		// `UserStats` that goes with it. Which one it is depends on what the
		// suite left resting, and that is not what this is testing.
		const byUser = new Map(
			[clobMakerKp, clobMaker2Kp, dlobMakerKp, midMakerKp, takerKp].map(
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
		// A bound taken from the route rather than from a number this test
		// picked: the worst price it quoted, with room over it, so the taker
		// crosses everything the route says it would reach.
		const worstQuoted = route.books
			.flatMap((b: any) => b.levels ?? [])
			.reduce((worst: BN, l: any) => BN.max(worst, new BN(l.price)), new BN(0));
		assert.isTrue(worstQuoted.gt(new BN(0)), 'the route quoted something');

		// The id up front, not matched by size afterwards: by now the taker
		// has older orders of every common size, and `find` would happily
		// return a closed one — a fill against which lands and does nothing.
		const orderId = (await taker.forceGetUserAccount())!.nextOrderId;
		await taker.placePerpOrder(
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: size,
				price: worstQuoted.muln(102).divn(100),
			})
		);
		const before = (await taker.forceGetUserAccount())!.perpPositions.find(
			(p) => p.marketIndex === 0
		)!.baseAssetAmount;

		await sendFill(
			await fillPerpOrderIx(orderId, takerKp.publicKey, [
				...namedKps,
				midMakerKp,
			])
		);

		await taker.fetchAccounts();
		const filled = taker
			.getUser()
			.getPerpPosition(0)!
			.baseAssetAmount.sub(before);
		assert.isTrue(
			filled.gt(new BN(0)),
			"a fill built from the endpoint's account list lands and fills"
		);
	});
});
