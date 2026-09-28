/**
 * allow-verbose: the usage header of an operator script.
 *
 * Trade against the running stack: fund a trader, rest a CLOB limit, see it in the dlob-server
 * user-orders feed, then cancel it with the handle the feed returns and see it leave.
 *
 *   docker compose -f local-stack/compose.yaml run --rm bootstrap bun run local-stack/smoke.ts
 *
 * The order goes through velocity, the book-publisher, Redis and dlob-server, so a pass covers
 * the feed path end to end.
 */
import * as fs from 'fs';
import { BN } from '@coral-xyz/anchor';
import {
	Connection,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	SystemProgram,
	Transaction,
} from '@solana/web3.js';
import {
	BASE_PRECISION,
	BulkAccountLoader,
	getMarketsAndOraclesForSubscription,
	getUserAccountPublicKeySync,
	OrderType,
	PositionDirection,
	PostOnlyParams,
	PRICE_PRECISION,
	TokenFaucet,
	UserClobOrder,
	UserClobOrdersClient,
	VelocityClient,
	Wallet,
} from '@velocity-exchange/sdk';

const RPC_URL = process.env.RPC_URL ?? 'http://rpc:8899';
const DLOB_URL = process.env.DLOB_URL ?? 'http://dlob-server:6969';
const TOKEN_FAUCET = new PublicKey(
	'V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB'
);
const DUSDT_MINT = new PublicKey(
	'GqmEqYsy8EyvofDpmtFxK8zhYrgWgNokAtYoduQdL7v6'
);
const DEPOSIT = new BN(1_000_000_000);
const MARKET_INDEX = 0;
const FEED_TIMEOUT_MS = 30_000;

function loadKey(file: string): Keypair {
	if (!fs.existsSync(file))
		fs.writeFileSync(
			file,
			JSON.stringify(Array.from(Keypair.generate().secretKey))
		);
	return Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf-8')))
	);
}

async function fundTrader(
	connection: Connection,
	authority: Keypair,
	trader: Keypair
): Promise<PublicKey> {
	if ((await connection.getBalance(trader.publicKey)) < LAMPORTS_PER_SOL) {
		const transfer = SystemProgram.transfer({
			fromPubkey: authority.publicKey,
			toPubkey: trader.publicKey,
			lamports: 10 * LAMPORTS_PER_SOL,
		});

		await connection.sendTransaction(new Transaction().add(transfer), [
			authority,
		]);
	}

	const faucet = new TokenFaucet(
		connection,
		new Wallet(trader),
		TOKEN_FAUCET,
		DUSDT_MINT
	);
	const [tokenAccount] = await faucet.createAssociatedTokenAccountAndMintTo(
		trader.publicKey,
		DEPOSIT
	);
	return tokenAccount;
}

async function connectClient(
	connection: Connection,
	trader: Keypair
): Promise<VelocityClient> {
	const { perpMarketIndexes, spotMarketIndexes, oracleInfos } =
		getMarketsAndOraclesForSubscription('devnet');
	const client = new VelocityClient({
		connection,
		wallet: new Wallet(trader),
		env: 'devnet',
		perpMarketIndexes,
		spotMarketIndexes,
		oracleInfos,
		accountSubscription: {
			type: 'polling',
			accountLoader: new BulkAccountLoader(connection, 'confirmed', 1000),
		},
	});

	await client.subscribe();
	return client;
}

async function waitForFeed(
	feed: UserClobOrdersClient,
	user: PublicKey,
	label: string,
	done: (orders: UserClobOrder[]) => boolean
): Promise<UserClobOrder[]> {
	const deadline = Date.now() + FEED_TIMEOUT_MS;
	while (Date.now() < deadline) {
		const orders = await feed.fetch(user, [MARKET_INDEX]);
		if (done(orders)) return orders;
		await new Promise((resolve) => setTimeout(resolve, 1000));
	}

	throw new Error(`the user-orders feed never showed ${label}`);
}

async function main() {
	const connection = new Connection(RPC_URL, 'confirmed');
	const authority = loadKey('/state/snapshot/authority.json');
	const trader = loadKey('/state/keys/smoke-trader.json');
	const tokenAccount = await fundTrader(connection, authority, trader);
	const client = await connectClient(connection, trader);

	const user = getUserAccountPublicKeySync(client.program.programId, trader.publicKey, 0);
	if (!(await connection.getAccountInfo(user))) {
		await client.initializeUserAccountAndDepositCollateral(DEPOSIT, tokenAccount);
	}

	await client.addUser(0);

	const oracle = client.getOracleDataForPerpMarket(MARKET_INDEX).price;
	const price = oracle
		.muln(8)
		.divn(10)
		.div(PRICE_PRECISION)
		.mul(PRICE_PRECISION);
	const placed = await client.placeAndMakePerpOrder(
		{
			orderType: OrderType.LIMIT,
			marketIndex: MARKET_INDEX,
			direction: PositionDirection.LONG,
			baseAssetAmount: BASE_PRECISION.divn(10),
			price,
			postOnly: PostOnlyParams.MUST_POST_ONLY,
		},
		undefined
	);
	console.log(`placed a bid at ${price.toString()}: ${placed}`);

	const feed = new UserClobOrdersClient(DLOB_URL);
	const [order] = await waitForFeed(
		feed,
		user,
		'the order',
		(orders) => orders.length > 0
	);
	console.log(`feed shows order ${order.orderId} at node ${order.nodeIndex}`);

	const orderRef = { nodeIndex: order.nodeIndex, orderId: order.clobOrderId };
	console.log(
		`cancelled: ${await client.cancelOrderV1({
			marketIndex: MARKET_INDEX,
			orderRef,
		})}`
	);
	await waitForFeed(
		feed,
		user,
		'the order leave',
		(orders) => orders.length === 0
	);
	console.log('smoke: the feed followed the order on and off the book');

	await client.unsubscribe();
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
