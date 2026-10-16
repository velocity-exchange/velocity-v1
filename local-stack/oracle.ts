/**
 * allow-verbose: the usage header of a stack service an operator drives by hand.
 *
 * Post signed Pyth Lazer updates for every devnet feed, so the local stack's oracles stay fresh
 * without a Lazer token, and a test can move a price on demand.
 *
 * The dump trusts the key in /state/snapshot/lazer-signer.json in the Lazer storage account, so
 * `post_pyth_lazer_oracle_update` accepts what this service signs. Each feed starts at the
 * price the dump holds and stays there until it is moved. With FOLLOW_RPC_URL set, each feed
 * copies the price of the same oracle account on that cluster, so the stack tracks the market.
 * With PRICE_ALERT_TOPIC_ARN set, the service also publishes each price to that SNS FIFO topic
 * every PRICE_ALERT_MS, for the candle processor. A price set by hand pins its feed until a
 * follow request releases it:
 *
 *   curl localhost:7070/prices
 *   curl -X POST localhost:7070/price -d '{"market":"SOL-PERP","price":150.25}'
 *   curl -X POST localhost:7070/price -d '{"feedId":6,"price":150.25}'
 *   curl -X POST localhost:7070/follow -d '{"market":"SOL-PERP"}'
 *   curl -X POST localhost:7070/follow -d '{}'
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
import { OraclePrice, PriceAlertPublisher } from './price-alerts';

const RPC_URL = process.env.RPC_URL ?? 'http://rpc:8899';
const CONTROL_PORT = Number(process.env.CONTROL_PORT ?? 7070);
const TICK_MS = Number(process.env.TICK_MS ?? 400);
const FOLLOW_RPC_URL = process.env.FOLLOW_RPC_URL;
const FOLLOW_MS = Number(process.env.FOLLOW_MS ?? 2000);
const PRICE_ALERT_TOPIC_ARN = process.env.PRICE_ALERT_TOPIC_ARN;
const PRICE_ALERT_MS = Number(process.env.PRICE_ALERT_MS ?? 5000);
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
	pinned: boolean;
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
			pinned: false,
		});
	}

	return feeds;
}

type ControlRequest = {
	market?: string;
	feedId?: number;
	price?: number;
};

function findFeed(
	feeds: Map<number, Feed>,
	body: ControlRequest
): Feed | undefined {
	return [...feeds.values()].find(
		(candidate) =>
			candidate.feedId === body.feedId ||
			candidate.symbols.includes(body.market ?? '')
	);
}

function setPrice(
	feeds: Map<number, Feed>,
	body: ControlRequest
): Response | null {
	const feed = findFeed(feeds, body);
	if (!feed || !(body.price! > 0)) {
		return new Response('name a known market or feedId and a positive price', {
			status: 400,
		});
	}

	feed.mantissa = BigInt(Math.round(body.price! / 10 ** feed.exponent));
	feed.pinned = true;
	console.log(
		`feed ${feed.feedId} (${feed.symbols.join(', ')}) -> ${body.price}`
	);
	return null;
}

/** Releases the named feed, or every feed when the request names none. */
function follow(
	feeds: Map<number, Feed>,
	body: ControlRequest
): Response | null {
	const named = body.market !== undefined || body.feedId !== undefined;
	const feed = named ? findFeed(feeds, body) : undefined;
	if (named && !feed) {
		return new Response('name a known market or feedId', { status: 400 });
	}

	for (const released of feed ? [feed] : feeds.values())
		released.pinned = false;
	return null;
}

function serveControl(feeds: Map<number, Feed>): void {
	const view = () =>
		[...feeds.values()].map((feed) => ({
			feedId: feed.feedId,
			symbols: feed.symbols,
			price: Number(feed.mantissa) * 10 ** feed.exponent,
			pinned: feed.pinned,
		}));

	Bun.serve({
		port: CONTROL_PORT,
		async fetch(request) {
			const url = new URL(request.url);
			if (request.method === 'GET' && url.pathname === '/prices')
				return Response.json(view());

			const handler = { '/price': setPrice, '/follow': follow }[url.pathname];
			if (request.method !== 'POST' || !handler) {
				return new Response(
					'GET /prices, POST /price {market|feedId, price} or POST /follow {market|feedId}',
					{ status: 404 }
				);
			}

			const rejection = handler(
				feeds,
				(await request.json()) as ControlRequest
			);
			return rejection ?? Response.json(view());
		},
	});
}

/** Copies each unpinned feed's price from the oracle account of the same feed on another cluster. */
async function followCluster(
	client: VelocityClient,
	source: Connection,
	feeds: Feed[]
): Promise<void> {
	const followed = feeds.filter((feed) => !feed.pinned);
	const accounts = await source.getMultipleAccountsInfo(
		followed.map((feed) =>
			getPythLazerOraclePublicKey(client.program.programId, feed.feedId)
		)
	);

	followed.forEach((feed, index) => {
		const info = accounts[index];
		if (!info || feed.pinned) return;

		const decoded: any = client.program.coder.accounts.decode(
			'pythLazerOracle',
			info.data
		);
		const price = Number(decoded.price.toString()) * 10 ** decoded.exponent;
		feed.mantissa = BigInt(Math.round(price / 10 ** feed.exponent));
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

async function followLoop(
	client: VelocityClient,
	source: Connection,
	feeds: Map<number, Feed>
): Promise<void> {
	console.log(`following ${source.rpcEndpoint} every ${FOLLOW_MS}ms`);
	for (;;) {
		try {
			await followCluster(client, source, [...feeds.values()]);
		} catch (error) {
			console.error(`follow: ${error}`);
		}

		await new Promise((resolve) => setTimeout(resolve, FOLLOW_MS));
	}
}

/** Every market symbol of every feed, at the price the feed posts. */
function oraclePrices(client: VelocityClient, feeds: Feed[]): OraclePrice[] {
	return feeds.flatMap((feed) => {
		const price = Number(feed.mantissa) * 10 ** feed.exponent;
		const oracle = getPythLazerOraclePublicKey(
			client.program.programId,
			feed.feedId
		).toBase58();
		return feed.symbols.map((symbol) => ({
			symbol,
			oracle,
			price,
			confidence: price * HALF_SPREAD,
		}));
	});
}

async function priceAlertLoop(
	client: VelocityClient,
	publisher: PriceAlertPublisher,
	feeds: Map<number, Feed>
): Promise<void> {
	console.log(`publishing price alerts every ${PRICE_ALERT_MS}ms`);
	for (;;) {
		try {
			const slot = await client.connection.getSlot();
			await publisher.publish(oraclePrices(client, [...feeds.values()]), slot);
		} catch (error) {
			console.error(`price alert: ${error}`);
		}

		await new Promise((resolve) => setTimeout(resolve, PRICE_ALERT_MS));
	}
}

async function main() {
	const connection = new Connection(RPC_URL, 'confirmed');
	const authority = loadKey(
		process.env.AUTHORITY_KEYPAIR ?? '/state/snapshot/authority.json'
	);
	const signer = loadKey(
		process.env.LAZER_SIGNER_KEYPAIR ?? '/state/snapshot/lazer-signer.json'
	);
	await assertSignerTrusted(connection, signer.publicKey);

	const client = new VelocityClient({
		connection,
		wallet: new Wallet(authority),
		env: 'devnet',
	});
	const feeds = await loadFeeds(client);
	serveControl(feeds);
	if (FOLLOW_RPC_URL)
		void followLoop(client, new Connection(FOLLOW_RPC_URL), feeds);
	if (PRICE_ALERT_TOPIC_ARN) {
		const publisher = new PriceAlertPublisher(PRICE_ALERT_TOPIC_ARN);
		void priceAlertLoop(client, publisher, feeds);
	}
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
