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
 */
import * as anchor from '@coral-xyz/anchor';
import { Program } from '@coral-xyz/anchor';
import { assert } from 'chai';
import { createHash } from 'crypto';
import { spawn, ChildProcess } from 'child_process';
import * as fs from 'fs';
import Redis from 'ioredis';
import {
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
	BN,
	BulkAccountLoader,
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getSpotMarketPublicKeySync,
	getLimitOrderParams,
	getMarketOrderParams,
	isVariant,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
	getVelocitySignerPublicKey,
	OracleSource,
	PEG_PRECISION,
	PositionDirection,
	PostOnlyParams,
	PRICE_PRECISION,
	QuoterCpiLeg,
	QuoterType,
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

const UNIT = BASE_PRECISION; // 1e9
const PRICE = PRICE_PRECISION; // 1e6
const USDC = new BN(10).pow(new BN(6));

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

// --- CLOB book byte offsets, pinned by the program's litesvm tests
// (prop_amm.rs constants).
const CLOB_BEST_BID_OFFSET = 112;
const CLOB_BEST_ASK_OFFSET = 116;
const CLOB_BID_COUNT_OFFSET = 136;
const CLOB_ASK_COUNT_OFFSET = 140;
const CLOB_ORDERS_OFFSET = 8368;
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

describe('e2e localnet: programs + publisher + redis', function () {
	const connection = new Connection(RPC_URL, 'confirmed');
	const payer = Keypair.generate();
	const provider = new anchor.AnchorProvider(connection, new Wallet(payer), {
		commitment: 'confirmed',
		preflightCommitment: 'confirmed',
	});

	// Actors.
	const clobMakerKp = Keypair.generate();
	const midMakerKp = Keypair.generate();
	const midHotKp = Keypair.generate();
	const dlobMakerKp = Keypair.generate();
	const takerKp = Keypair.generate();
	const crosserKp = Keypair.generate();
	const publisherKp = Keypair.generate();

	let admin: TestClient;
	let clobMaker: TestClient;
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
	let protocolUser: PublicKey;
	let protocolUserStats: PublicKey;

	let publisher: ChildProcess | undefined;
	let publisherLog: number | undefined;
	let redis: Redis;
	let oracleRefresher: Promise<void> | undefined;
	let stopOracleRefresher = false;

	const airdrop = async (to: PublicKey, sol: number) => {
		const sig = await connection.requestAirdrop(to, sol * LAMPORTS_PER_SOL);
		await connection.confirmTransaction(sig, 'confirmed');
	};

	const createUsdcMint = async (): Promise<Keypair> => {
		const mint = Keypair.generate();
		const tx = new Transaction().add(
			SystemProgram.createAccount({
				fromPubkey: payer.publicKey,
				newAccountPubkey: mint.publicKey,
				lamports: await connection.getMinimumBalanceForRentExemption(
					MintLayout.span
				),
				space: MintLayout.span,
				programId: TOKEN_PROGRAM_ID,
			}),
			createInitializeMintInstruction(
				mint.publicKey,
				6,
				payer.publicKey,
				payer.publicKey
			)
		);
		await provider.sendAndConfirm(tx, [mint]);
		return mint;
	};

	const fundUsdc = async (owner: PublicKey, amount: BN): Promise<PublicKey> => {
		const account = Keypair.generate();
		const tx = new Transaction().add(
			SystemProgram.createAccount({
				fromPubkey: payer.publicKey,
				newAccountPubkey: account.publicKey,
				lamports: await connection.getMinimumBalanceForRentExemption(
					AccountLayout.span
				),
				space: AccountLayout.span,
				programId: TOKEN_PROGRAM_ID,
			}),
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
			)
		);
		await provider.sendAndConfirm(tx, [account]);
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
		clients.push(client);
		return client;
	};

	/** CLOB bring-up, exactly as `admin-cli clob-market init` does it. */
	const clobBringUp = async () => {
		const capacity = 1024;
		const space = Math.ceil((8 + 8352 + 4) / 8) * 8 + capacity * CLOB_NODE_LEN;
		clobBook = Keypair.generate();
		const config = Buffer.concat([
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
		]);
		await provider.sendAndConfirm(
			new Transaction().add(
				SystemProgram.createAccount({
					fromPubkey: payer.publicKey,
					newAccountPubkey: clobBook.publicKey,
					lamports: await connection.getMinimumBalanceForRentExemption(space),
					space,
					programId: CLOB_ID,
				}),
				new TransactionInstruction({
					programId: CLOB_ID,
					keys: [
						{ pubkey: payer.publicKey, isSigner: true, isWritable: false },
						{ pubkey: velocitySigner, isSigner: false, isWritable: false },
						{ pubkey: clobBook.publicKey, isSigner: false, isWritable: true },
					],
					data: Buffer.concat([
						ixDiscriminator('initialize_market_v0'),
						config,
					]),
				})
			),
			[clobBook]
		);

		clobEntry = PublicKey.findProgramAddressSync(
			[
				Buffer.from('quoter'),
				u16(0),
				CLOB_ID.toBuffer(),
				PublicKey.default.toBuffer(),
			],
			VELOCITY_ID
		)[0];
		const program = admin.program;
		const initQuoter = program.instruction.initializeQuoter(
			{
				marketIndex: 0,
				quoterType: QuoterType.CLOB,
				responseAccount: clobBook.publicKey,
				quoteV0Discriminator: Array.from(ixDiscriminator('quote_v0')),
				executeV0Discriminator: Array.from(ixDiscriminator('execute_v0')),
			},
			{
				accounts: {
					payer: payer.publicKey,
					authority: payer.publicKey,
					quoter: clobEntry,
					perpMarket: getPerpMarketPublicKeySync(VELOCITY_ID, 0),
					quoterProgram: CLOB_ID,
					user: PublicKey.default,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				},
			}
		);
		const legAccounts = (
			leg: QuoterCpiLeg,
			metas: { pubkey: PublicKey; isWritable: boolean }[]
		) =>
			program.instruction.updateQuoterAccounts(
				{ leg, index: 0, metas },
				{ accounts: { authority: payer.publicKey, quoter: clobEntry } }
			);
		const approve = program.instruction.updateQuoterApproved(true, {
			accounts: {
				admin: payer.publicKey,
				state: await admin.getStatePublicKey(),
				quoter: clobEntry,
			},
		});
		await provider.sendAndConfirm(
			new Transaction().add(
				initQuoter,
				legAccounts(QuoterCpiLeg.QUOTE, [
					{ pubkey: clobBook.publicKey, isWritable: true },
				]),
				legAccounts(QuoterCpiLeg.EXECUTE, [
					{ pubkey: clobBook.publicKey, isWritable: true },
					{ pubkey: velocitySigner, isWritable: false },
				]),
				approve
			)
		);

		// Attach as the market's canonical CLOB (creates conditions) + fund
		// the crank reservoir.
		conditions = getClobCrankConditionsPublicKey(VELOCITY_ID, 0);
		const attach = program.instruction.updatePerpMarketClobQuoter(
			new BN(10_000), // keeper_payment_lamports
			new BN(1500), // expire_fallback_slots
			{
				accounts: {
					admin: payer.publicKey,
					state: await admin.getStatePublicKey(),
					perpMarket: getPerpMarketPublicKeySync(VELOCITY_ID, 0),
					quoter: clobEntry,
					clobMarket: clobBook.publicKey,
					crankConditions: conditions,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				},
			}
		);
		await provider.sendAndConfirm(
			new Transaction().add(
				attach,
				SystemProgram.transfer({
					fromPubkey: payer.publicKey,
					toPubkey: conditions,
					lamports: LAMPORTS_PER_SOL,
				})
			)
		);
	};

	/** Midpoint instance + spline, then its Custom registry entry. */
	const midpointBringUp = async () => {
		midInstance = PublicKey.findProgramAddressSync(
			[
				Buffer.from('midpoint'),
				u16(0),
				midMakerKp.publicKey.toBuffer(),
				u16(0),
			],
			MIDPOINT_ID
		)[0];
		// Config borsh: market u16, sub u16, base_precision u64, staleness
		// u64, tick u64, step u64, min u64, attested bool.
		const initData = Buffer.concat([
			ixDiscriminator('initialize_quoter_v0'),
			u16(0),
			u16(0),
			u64(UNIT),
			u64(1000), // max_mid_staleness_slots — generous for a slow tick
			u64(100), // price_tick_size
			u64(100000), // size_step
			u64(100000), // min_quote_size
			Buffer.from([0]), // require_attested_flow
		]);
		const init = new TransactionInstruction({
			programId: MIDPOINT_ID,
			keys: [
				{ pubkey: payer.publicKey, isSigner: true, isWritable: true },
				{ pubkey: midMakerKp.publicKey, isSigner: true, isWritable: false },
				{ pubkey: velocitySigner, isSigner: false, isWritable: false },
				{ pubkey: midHotKp.publicKey, isSigner: false, isWritable: false },
				// Absent optional flow authority = the program id.
				{ pubkey: MIDPOINT_ID, isSigner: false, isWritable: false },
				{ pubkey: midInstance, isSigner: false, isWritable: true },
				{ pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
			],
			data: initData,
		});
		await provider.sendAndConfirm(new Transaction().add(init), [midMakerKp]);
		// Spline: 10bps / 30bps rungs, one unit each, mid $100.
		await setMidpointLevels(new BN(100).mul(PRICE), [
			{ offsetPpm: 1000, size: UNIT },
			{ offsetPpm: 3000, size: UNIT },
		]);

		const midUser = getUserAccountPublicKeySync(
			VELOCITY_ID,
			midMakerKp.publicKey,
			0
		);
		midEntry = PublicKey.findProgramAddressSync(
			[
				Buffer.from('quoter'),
				u16(0),
				MIDPOINT_ID.toBuffer(),
				midUser.toBuffer(),
			],
			VELOCITY_ID
		)[0];
		const program = admin.program;
		const initQuoter = program.instruction.initializeQuoter(
			{
				marketIndex: 0,
				quoterType: QuoterType.CUSTOM,
				responseAccount: midInstance,
				quoteV0Discriminator: Array.from(ixDiscriminator('quote_v0')),
				executeV0Discriminator: Array.from(ixDiscriminator('execute_v0')),
			},
			{
				accounts: {
					payer: payer.publicKey,
					authority: midMakerKp.publicKey,
					quoter: midEntry,
					perpMarket: getPerpMarketPublicKeySync(VELOCITY_ID, 0),
					quoterProgram: MIDPOINT_ID,
					user: midUser,
					rent: SYSVAR_RENT_PUBKEY,
					systemProgram: SystemProgram.programId,
				},
			}
		);
		const legAccounts = (
			leg: QuoterCpiLeg,
			metas: { pubkey: PublicKey; isWritable: boolean }[]
		) =>
			program.instruction.updateQuoterAccounts(
				{ leg, index: 0, metas },
				{ accounts: { authority: midMakerKp.publicKey, quoter: midEntry } }
			);
		const approve = program.instruction.updateQuoterApproved(true, {
			accounts: {
				admin: payer.publicKey,
				state: await admin.getStatePublicKey(),
				quoter: midEntry,
			},
		});
		await provider.sendAndConfirm(
			new Transaction().add(
				initQuoter,
				legAccounts(QuoterCpiLeg.QUOTE, [
					{ pubkey: midInstance, isWritable: true },
					{ pubkey: SYSVAR_INSTRUCTIONS_PUBKEY, isWritable: false },
				]),
				legAccounts(QuoterCpiLeg.EXECUTE, [
					{ pubkey: midInstance, isWritable: true },
					{ pubkey: velocitySigner, isWritable: false },
					{ pubkey: SYSVAR_INSTRUCTIONS_PUBKEY, isWritable: false },
				]),
				approve
			),
			[midMakerKp]
		);
	};

	const setMidpointMid = async (mid: BN) => {
		const data = Buffer.concat([
			ixDiscriminator('set_mid_v0'),
			u64(mid),
			u64(0),
		]);
		await provider.sendAndConfirm(
			new Transaction().add(
				new TransactionInstruction({
					programId: MIDPOINT_ID,
					keys: [
						{ pubkey: midInstance, isSigner: false, isWritable: true },
						{ pubkey: midHotKp.publicKey, isSigner: true, isWritable: false },
					],
					data,
				})
			),
			[midHotKp]
		);
	};

	const setMidpointLevels = async (
		mid: BN,
		levels: { offsetPpm: number; size: BN }[]
	) => {
		const side = Buffer.concat([
			Buffer.from([1]),
			u32(levels.length),
			...levels.flatMap((level) => [u64(level.offsetPpm), u64(level.size)]),
		]);
		const data = Buffer.concat([
			ixDiscriminator('set_levels_v0'),
			Buffer.from([1]),
			u64(mid),
			Buffer.from([0]), // sequence: None
			side, // bids
			side, // asks
		]);
		await provider.sendAndConfirm(
			new Transaction().add(
				new TransactionInstruction({
					programId: MIDPOINT_ID,
					keys: [
						{ pubkey: midInstance, isSigner: false, isWritable: true },
						{ pubkey: midHotKp.publicKey, isSigner: true, isWritable: false },
					],
					data,
				})
			),
			[midHotKp]
		);
	};

	/** The protocol-owned User (velocity signer's sub-account 0) for cranks. */
	const initProtocolUser = async () => {
		protocolUser = getUserAccountPublicKeySync(VELOCITY_ID, velocitySigner, 0);
		protocolUserStats = getUserStatsAccountPublicKey(
			VELOCITY_ID,
			velocitySigner
		);
		const program = admin.program;
		const state = await admin.getStatePublicKey();
		const name = Array.from(Buffer.alloc(32, ' '));
		Buffer.from('protocol').copy(Buffer.from(name));
		const initStats = program.instruction.initializeUserStats({
			accounts: {
				userStats: protocolUserStats,
				state,
				authority: velocitySigner,
				payer: payer.publicKey,
				rent: SYSVAR_RENT_PUBKEY,
				systemProgram: SystemProgram.programId,
			},
		});
		const initUser = program.instruction.initializeUser(0, name, {
			accounts: {
				user: protocolUser,
				userStats: protocolUserStats,
				state,
				authority: velocitySigner,
				payer: payer.publicKey,
				rent: SYSVAR_RENT_PUBKEY,
				systemProgram: SystemProgram.programId,
			},
		});
		await provider.sendAndConfirm(new Transaction().add(initStats, initUser));
	};

	const placeClobOrder = async (
		client: TestClient,
		kp: Keypair,
		direction: PositionDirection,
		price: BN,
		size: BN
	) => {
		const user = getUserAccountPublicKeySync(VELOCITY_ID, kp.publicKey, 0);
		const ix = client.program.instruction.placeClobOrder(
			{
				marketIndex: 0,
				direction,
				price,
				baseAssetAmount: size,
				maxTs: new BN(0),
				activationDelaySlots: 0,
			},
			{
				accounts: {
					state: await client.getStatePublicKey(),
					user,
					authority: kp.publicKey,
					quoter: clobEntry,
					clobMarket: clobBook.publicKey,
					clobProgram: CLOB_ID,
					velocitySigner,
					crankConditions: conditions,
				},
				remainingAccounts: [
					{ pubkey: oracle, isSigner: false, isWritable: false },
					{
						pubkey: getSpotMarketPublicKeySync(VELOCITY_ID, 0),
						isSigner: false,
						isWritable: true,
					},
					{
						pubkey: getPerpMarketPublicKeySync(VELOCITY_ID, 0),
						isSigner: false,
						isWritable: true,
					},
				],
			}
		);
		await client.sendTransaction(new Transaction().add(ix));
	};

	const readClob = async (): Promise<ClobView> => {
		const info = await connection.getAccountInfo(clobBook.publicKey);
		return readClobView(info!.data);
	};

	before(async function () {
		this.timeout(600_000);

		for (const kp of [
			payer,
			clobMakerKp,
			midMakerKp,
			midHotKp,
			dlobMakerKp,
			takerKp,
			crosserKp,
			publisherKp,
		]) {
			await airdrop(kp.publicKey, 100);
		}

		velocitySigner = getVelocitySignerPublicKey(VELOCITY_ID);
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
		const initFeed = pythProgram.instruction.initialize(
			new BN(100).mul(PRICE),
			-6,
			new BN(1).mul(PRICE).divn(100),
			{ accounts: { price: feed.publicKey } }
		);
		await provider.sendAndConfirm(
			new Transaction().add(
				SystemProgram.createAccount({
					fromPubkey: payer.publicKey,
					newAccountPubkey: feed.publicKey,
					space: 3312,
					lamports: await connection.getMinimumBalanceForRentExemption(3312),
					programId: PYTH_ID,
				}),
				initFeed
			),
			[feed]
		);
		oracle = feed.publicKey;

		// Protocol init through the real admin instructions.
		admin = newClient(payer);
		await admin.initialize(usdcMint.publicKey, true);
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
		midMaker = newClient(midMakerKp);
		dlobMaker = newClient(dlobMakerKp);
		taker = newClient(takerKp);
		crosser = newClient(crosserKp);
		for (const [client, kp] of [
			[clobMaker, clobMakerKp],
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

		// Keep the oracle fresh: on a real cluster a live feed does this; here
		// a background set_price loop re-posts $100 every second so the vAMM
		// stays inside its oracle-validity gates.
		oracleRefresher = (async () => {
			while (!stopOracleRefresher) {
				try {
					const ix = pythProgram.instruction.setPrice(new BN(100).mul(PRICE), {
						accounts: { price: oracle },
					});
					await provider.sendAndConfirm(new Transaction().add(ix));
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
			new BN(1005).mul(PRICE).divn(10),
			UNIT
		);
		await placeClobOrder(
			clobMaker,
			clobMakerKp,
			PositionDirection.LONG,
			new BN(995).mul(PRICE).divn(10),
			UNIT
		);
		await dlobMaker.placePerpOrder(
			getLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.SHORT,
				baseAssetAmount: UNIT,
				price: new BN(1006).mul(PRICE).divn(10),
				postOnly: PostOnlyParams.MUST_POST_ONLY,
			})
		);

		// The publisher, exactly as deployed: RPC transport against the
		// validator, RPC-side simulation, cross fast path armed.
		const publisherKeyPath = `${SCRATCH}/publisher-keypair.json`;
		fs.writeFileSync(
			publisherKeyPath,
			JSON.stringify(Array.from(publisherKp.secretKey))
		);
		publisherLog = fs.openSync(`${SCRATCH}/publisher.log`, 'w');
		publisher = spawn(PUBLISHER_BIN, [], {
			env: {
				...process.env,
				RPC_URL,
				TRANSPORT: 'rpc',
				VELOCITY_PROGRAM_ID: VELOCITY_ID.toBase58(),
				MARKETS: '0',
				KEYPAIR_PATH: publisherKeyPath,
				BUFFER_DIR: `${SCRATCH}/quote-buffers`,
				REDIS_URL,
				TICK_MS: '750',
				LOCAL_SIM_POOL: '0',
				CROSS_MATCH: 'true',
				RUST_LOG: 'info',
			},
			stdio: ['ignore', publisherLog, publisherLog],
		});
		publisher.on('exit', (code) => {
			if (code !== null && code !== 0) {
				console.error(
					`book-publisher exited ${code} — see ${SCRATCH}/publisher.log`
				);
			}
		});

		redis = new Redis(REDIS_URL);
		await pollUntil('first published book', 90_000, async () => {
			const doc = await redis.get('last_update_orderbook_perp_0');
			return doc ?? undefined;
		});
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
		publisher?.kill();
		if (publisherLog !== undefined) fs.closeSync(publisherLog);
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

		// L3: per-order CLOB data with derived maker User PDAs.
		const l3 = JSON.parse(
			(await redis.get('last_update_orderbook_l3_perp_0'))!
		);
		const makerPda = getUserAccountPublicKeySync(
			VELOCITY_ID,
			clobMakerKp.publicKey,
			0
		).toBase58();
		assert.equal(l3.asks[0].price, String(100.5 * 1e6));
		assert.equal(l3.asks[0].maker, makerPda);
		assert.equal(l3.bids[0].maker, makerPda);
		assert.isAbove(Number(l3.asks[0].orderId), 0);

		// Best makers key.
		const bestMakers = JSON.parse(
			(await redis.get('last_update_orderbook_best_makers_perp_0'))!
		);
		assert.deepEqual(bestMakers.bids, [makerPda]);
		assert.deepEqual(bestMakers.asks, [makerPda]);

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
		await taker.placePerpOrder(
			getMarketOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: UNIT.muln(35).divn(10),
				price: new BN(102).mul(PRICE),
			})
		);
		const takerUser = getUserAccountPublicKeySync(
			VELOCITY_ID,
			takerKp.publicKey,
			0
		);
		const order = (await taker.forceGetUserAccount())!.orders.find((o) =>
			o.baseAssetAmount.eq(UNIT.muln(35).divn(10))
		)!;

		const pair = (kp: Keypair) => [
			{
				pubkey: getUserAccountPublicKeySync(VELOCITY_ID, kp.publicKey, 0),
				isSigner: false,
				isWritable: true,
			},
			{
				pubkey: getUserStatsAccountPublicKey(VELOCITY_ID, kp.publicKey),
				isSigner: false,
				isWritable: true,
			},
		];
		const fillIx = admin.program.instruction.fillPerpOrder(
			order.orderId,
			null,
			{
				accounts: {
					state: await admin.getStatePublicKey(),
					authority: payer.publicKey,
					filler: getUserAccountPublicKeySync(VELOCITY_ID, payer.publicKey, 0),
					fillerStats: getUserStatsAccountPublicKey(
						VELOCITY_ID,
						payer.publicKey
					),
					user: takerUser,
					userStats: getUserStatsAccountPublicKey(
						VELOCITY_ID,
						takerKp.publicKey
					),
				},
				remainingAccounts: [
					{ pubkey: oracle, isSigner: false, isWritable: false },
					{
						pubkey: getSpotMarketPublicKeySync(VELOCITY_ID, 0),
						isSigner: false,
						isWritable: true,
					},
					{
						pubkey: getPerpMarketPublicKeySync(VELOCITY_ID, 0),
						isSigner: false,
						isWritable: true,
					},
					// Maker section: DLOB maker + both quoted users.
					...pair(dlobMakerKp),
					...pair(clobMakerKp),
					...pair(midMakerKp),
					// Quoter section: entries, then the union of CPI accounts.
					{ pubkey: clobEntry, isSigner: false, isWritable: false },
					{ pubkey: midEntry, isSigner: false, isWritable: false },
					{ pubkey: clobBook.publicKey, isSigner: false, isWritable: true },
					{ pubkey: velocitySigner, isSigner: false, isWritable: false },
					{ pubkey: CLOB_ID, isSigner: false, isWritable: false },
					{ pubkey: midInstance, isSigner: false, isWritable: true },
					{
						pubkey: SYSVAR_INSTRUCTIONS_PUBKEY,
						isSigner: false,
						isWritable: false,
					},
					{ pubkey: MIDPOINT_ID, isSigner: false, isWritable: false },
				],
			}
		);
		await provider.sendAndConfirm(
			new Transaction()
				.add(ComputeBudgetProgram.setComputeUnitLimit({ units: 800_000 }))
				.add(fillIx)
		);

		await taker.fetchAccounts();
		const position = taker.getUser().getPerpPosition(0)!;
		assert.equal(
			position.baseAssetAmount.toString(),
			UNIT.muln(35).divn(10).toString()
		);

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
		await setMidpointLevels(new BN(100).mul(PRICE), [
			{ offsetPpm: 1000, size: UNIT.muln(2) },
		]);
		const before = await readClob();

		const ix = await taker.getPlaceAndTakePerpOrderIx(
			getLimitOrderParams({
				marketIndex: 0,
				direction: PositionDirection.LONG,
				baseAssetAmount: UNIT,
				price: new BN(100).mul(PRICE),
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
				velocitySigner,
				crankConditions: conditions,
			}
		);
		await taker.sendTransaction(new Transaction().add(ix));

		const after = await readClob();
		assert.equal(after.bidCount, before.bidCount + 1);
		assert.equal(
			after.bestBidPrice!.toString(),
			new BN(100).mul(PRICE).toString()
		);
		// The taker's DLOB order slot is not resting open (migrated) — scope
		// to this order's price so unrelated leftovers can't bleed in.
		await taker.fetchAccounts();
		const open = taker
			.getUserAccount()!
			.orders.filter(
				(o) => isVariant(o.status, 'open') && o.price.eq(new BN(100).mul(PRICE))
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
			new BN(101).mul(PRICE),
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
					book.bestBidPrice === undefined ||
					book.bestBidPrice.lt(new BN(101).mul(PRICE));
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

	it('a mid write repositions the published spline', async function () {
		this.timeout(120_000);
		await setMidpointMid(new BN(102).mul(PRICE));
		await pollUntil('published book to track the new mid', 30_000, async () => {
			const raw = await redis.get('last_update_orderbook_perp_0');
			if (!raw) return undefined;
			const book = JSON.parse(raw);
			const propammAsk = book.asks.find((level: any) => level.sources.propamm);
			// 102 + 10bps = 102.102.
			return propammAsk?.price === String(102.102 * 1e6) ? true : undefined;
		});
	});
});
