/**
 * allow-verbose: the usage header of a stack service an operator drives by hand.
 *
 * Post signed Pyth Lazer updates for every devnet feed, so the local stack's oracles stay fresh
 * without a Lazer token, and a test can move a price on demand.
 *
 * The dump trusts the key in /state/snapshot/lazer-signer.json in the Lazer storage account, so
 * `post_pyth_lazer_oracle_update` accepts what this service signs. Each feed starts at the
 * price the dump holds and stays there until it is moved:
 *
 *   curl localhost:7070/prices
 *   curl -X POST localhost:7070/price -d '{"market":"SOL-PERP","price":150.25}'
 *   curl -X POST localhost:7070/price -d '{"feedId":6,"price":150.25}'
 */
import { createPrivateKey, sign } from 'crypto';
import * as fs from 'fs';
import {
	Connection,
	Keypair,
	PublicKey,
	SYSVAR_CLOCK_PUBKEY,
	Transaction,
} from '@solana/web3.js';
import {
	DevnetPerpMarkets,
	DevnetSpotMarkets,
	getPythLazerOraclePublicKey,
	PYTH_LAZER_STORAGE_ACCOUNT_KEY,
	VelocityClient,
	Wallet,
} from '@velocity-exchange/sdk';

const RPC_URL = process.env.RPC_URL ?? 'http://rpc:8899';
const CONTROL_PORT = Number(process.env.CONTROL_PORT ?? 7070);
const TICK_MS = Number(process.env.TICK_MS ?? 400);
/** One tick in this many sends with preflight, so a rejected update reaches the log. */
const PREFLIGHT_EVERY = 25;

const SOLANA_FORMAT_MAGIC = 2182742457;
const PAYLOAD_FORMAT_MAGIC = 2479346549;
const CHANNEL_ID = 3;
/** `PriceFeedProperty` discriminants, positional in `programs/pyth-lazer/src/lib.rs`. */
const PROPERTY = {
	price: 0,
	bestBid: 1,
	bestAsk: 2,
	exponent: 4,
	feedUpdateTimestamp: 12,
};
/** Half the quoted spread around the price. The spread sets the confidence velocity derives. */
const HALF_SPREAD = 0.0001;
/** Bytes ahead of `unix_timestamp` in the Clock sysvar: slot, epoch start, epoch, leader epoch. */
const CLOCK_UNIX_TIMESTAMP_OFFSET = 32;
/** Byte offsets in the Lazer `Storage` account, as `snapshot-devnet.ts` writes them. */
const LAZER_NUM_SIGNERS_OFFSET = 80;
const LAZER_SIGNERS_OFFSET = 81;
const LAZER_SIGNER_BYTES = 40;

type Feed = {
	feedId: number;
	symbols: string[];
	mantissa: bigint;
	exponent: number;
	lastTimestampUs: bigint;
};

function loadKey(file: string): Keypair {
	return Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf-8')))
	);
}

/** Ed25519 over `payload` with the seed half of a Solana secret key. */
function signPayload(signer: Keypair, payload: Buffer): Buffer {
	const pkcs8Prefix = Buffer.from('302e020100300506032b657004220420', 'hex');
	const seed = Buffer.from(signer.secretKey.subarray(0, 32));
	const key = createPrivateKey({
		key: Buffer.concat([pkcs8Prefix, seed]),
		format: 'der',
		type: 'pkcs8',
	});
	return sign(null, payload, key);
}

function u8(v: number): Buffer {
	return Buffer.from([v]);
}

function le(bytes: number, write: (b: Buffer) => void): Buffer {
	const b = Buffer.alloc(bytes);
	write(b);
	return b;
}

function feedBytes(feed: Feed, timestampUs: bigint): Buffer {
	const bid = BigInt(Math.round(Number(feed.mantissa) * (1 - HALF_SPREAD)));
	const ask = BigInt(Math.round(Number(feed.mantissa) * (1 + HALF_SPREAD)));
	return Buffer.concat([
		le(4, (b) => b.writeUInt32LE(feed.feedId)),
		u8(5),
		u8(PROPERTY.price),
		le(8, (b) => b.writeBigInt64LE(feed.mantissa)),
		u8(PROPERTY.bestBid),
		le(8, (b) => b.writeBigInt64LE(bid)),
		u8(PROPERTY.bestAsk),
		le(8, (b) => b.writeBigInt64LE(ask)),
		u8(PROPERTY.exponent),
		le(2, (b) => b.writeInt16LE(feed.exponent)),
		u8(PROPERTY.feedUpdateTimestamp),
		u8(1),
		le(8, (b) => b.writeBigUInt64LE(timestampUs)),
	]);
}

/** A signed `SolanaMessage` carrying every feed, as hex. */
function signedMessageHex(
	signer: Keypair,
	feeds: Feed[],
	timestampUs: bigint
): string {
	const payload = Buffer.concat([
		le(4, (b) => b.writeUInt32LE(PAYLOAD_FORMAT_MAGIC)),
		le(8, (b) => b.writeBigUInt64LE(timestampUs)),
		u8(CHANNEL_ID),
		u8(feeds.length),
		...feeds.map((feed) => feedBytes(feed, feed.lastTimestampUs)),
	]);
	return Buffer.concat([
		le(4, (b) => b.writeUInt32LE(SOLANA_FORMAT_MAGIC)),
		signPayload(signer, payload),
		signer.publicKey.toBuffer(),
		le(2, (b) => b.writeUInt16LE(payload.length)),
		payload,
	]).toString('hex');
}

async function assertSignerTrusted(
	connection: Connection,
	signer: PublicKey
): Promise<void> {
	const data = (await connection.getAccountInfo(
		PYTH_LAZER_STORAGE_ACCOUNT_KEY
	))!.data;
	const count = data.readUInt8(LAZER_NUM_SIGNERS_OFFSET);
	const trusted = Array.from({ length: count }, (_, i) => {
		const at = LAZER_SIGNERS_OFFSET + i * LAZER_SIGNER_BYTES;
		return new PublicKey(data.subarray(at, at + 32));
	}).some((key) => key.equals(signer));

	if (!trusted) {
		throw new Error(
			`the Lazer storage account does not trust ${signer.toBase58()}. The dump predates the local ` +
				'signer: run `bun run local:resnapshot`, then `bun run local:up`'
		);
	}
}

/** Every devnet feed that has an oracle account, at the price and exponent it holds. */
async function loadFeeds(client: VelocityClient): Promise<Map<number, Feed>> {
	const feeds = new Map<number, Feed>();
	for (const market of [...DevnetPerpMarkets, ...DevnetSpotMarkets]) {
		if (market.pythLazerId === undefined) continue;
		const existing = feeds.get(market.pythLazerId);
		if (existing) {
			existing.symbols.push(market.symbol);
			continue;
		}

		const oracle = getPythLazerOraclePublicKey(
			client.program.programId,
			market.pythLazerId
		);
		const info = await client.connection.getAccountInfo(oracle);
		if (!info) continue;
		const decoded: any = client.program.coder.accounts.decode(
			'pythLazerOracle',
			info.data
		);
		feeds.set(market.pythLazerId, {
			feedId: market.pythLazerId,
			symbols: [market.symbol],
			mantissa: BigInt(decoded.price.toString()),
			exponent: decoded.exponent,
			lastTimestampUs: BigInt(decoded.publishTime.toString()),
		});
	}

	return feeds;
}

function serveControl(feeds: Map<number, Feed>): void {
	const view = () =>
		[...feeds.values()].map((feed) => ({
			feedId: feed.feedId,
			symbols: feed.symbols,
			price: Number(feed.mantissa) * 10 ** feed.exponent,
		}));

	Bun.serve({
		port: CONTROL_PORT,
		async fetch(request) {
			const url = new URL(request.url);
			if (request.method === 'GET' && url.pathname === '/prices')
				return Response.json(view());
			if (request.method !== 'POST' || url.pathname !== '/price') {
				return new Response(
					'GET /prices or POST /price {market|feedId, price}',
					{ status: 404 }
				);
			}

			const body = (await request.json()) as {
				market?: string;
				feedId?: number;
				price: number;
			};
			const feed = [...feeds.values()].find(
				(candidate) =>
					candidate.feedId === body.feedId ||
					candidate.symbols.includes(body.market ?? '')
			);
			if (!feed || !(body.price > 0)) {
				return new Response(
					'name a known market or feedId and a positive price',
					{ status: 400 }
				);
			}

			feed.mantissa = BigInt(Math.round(body.price / 10 ** feed.exponent));
			console.log(
				`feed ${feed.feedId} (${feed.symbols.join(', ')}) -> ${body.price}`
			);
			return Response.json(view());
		},
	});
}

async function tick(
	client: VelocityClient,
	signer: Keypair,
	feeds: Feed[],
	preflight: boolean
) {
	const clock = (await client.connection.getAccountInfo(SYSVAR_CLOCK_PUBKEY))!
		.data;
	const nowUs = clock.readBigInt64LE(CLOCK_UNIX_TIMESTAMP_OFFSET) * 1_000_000n;
	// The program refuses a timestamp at or below the stored one, so each feed moves forward
	// by at least a microsecond even while the clock reads the same second.
	for (const feed of feeds) {
		feed.lastTimestampUs =
			nowUs > feed.lastTimestampUs ? nowUs : feed.lastTimestampUs + 1n;
	}

	const newest = feeds.reduce(
		(max, feed) => (feed.lastTimestampUs > max ? feed.lastTimestampUs : max),
		0n
	);
	const ixs = await client.getPostPythLazerOracleUpdateIxs(
		feeds.map((feed) => feed.feedId),
		signedMessageHex(signer, feeds, newest)
	);
	const payer = (client.wallet as Wallet).payer;
	await client.connection.sendTransaction(
		new Transaction().add(...ixs),
		[payer],
		{
			skipPreflight: !preflight,
		}
	);
}

async function main() {
	const connection = new Connection(RPC_URL, 'confirmed');
	const authority = loadKey('/state/snapshot/authority.json');
	const signer = loadKey('/state/snapshot/lazer-signer.json');
	await assertSignerTrusted(connection, signer.publicKey);

	const client = new VelocityClient({
		connection,
		wallet: new Wallet(authority),
		env: 'devnet',
	});
	const feeds = await loadFeeds(client);
	serveControl(feeds);
	console.log(
		`posting ${feeds.size} feeds every ${TICK_MS}ms, control on :${CONTROL_PORT}: ` +
			[...feeds.values()]
				.map((feed) => `${feed.feedId} ${feed.symbols.join('/')}`)
				.join(', ')
	);

	for (let count = 0; ; count++) {
		try {
			await tick(
				client,
				signer,
				[...feeds.values()],
				count % PREFLIGHT_EVERY === 0
			);
		} catch (error) {
			console.error(`tick ${count}: ${error}`);
		}

		await new Promise((resolve) => setTimeout(resolve, TICK_MS));
	}
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
