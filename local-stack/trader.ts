/**
 * allow-verbose: the usage header of an operator script.
 *
 * A second party on the stack. It rests, takes, cancels and crosses from the command line, so a
 * test of the webapp has a counterparty for crosses and maker fills:
 *
 *   bun run local:trader [--name <trader>] account
 *   bun run local:trader [--name <trader>] orders [market]
 *   bun run local:trader [--name <trader>] rest <market> <bid|ask> <price> <size>
 *       [--cross] [--reduce-only] [--expire-secs <s>] [--count <n> --step <price>]
 *   bun run local:trader [--name <trader>] take <market> <buy|sell> <size> [--worst <price>]
 *   bun run local:trader [--name <trader>] cancel <market> [orderId]
 *
 * A market is a symbol such as SOL-PERP or its index. Prices are in USD and sizes in base units.
 * `rest` is post-only unless `--cross` is passed, and `--cross` rests through the other side the
 * way a cross match needs. `--count` rests a ladder that steps away from the touch. `cancel`
 * without an order id cancels the trader's every order on the market. The default trader is `b`.
 * Its key lives in /state/keys/trader-<name>.json, and its first command funds it and opens its
 * account with 10000 dUSDT.
 */
import { BN } from '@coral-xyz/anchor';
import { Connection, Keypair, LAMPORTS_PER_SOL } from '@solana/web3.js';
import {
	BASE_PRECISION_EXP,
	CancelSidesV0,
	OrderType,
	PositionDirection,
	PostOnlyParams,
	PRICE_PRECISION_EXP,
	UserClobOrder,
	UserClobOrdersClient,
	VelocityClient,
} from '@velocity-exchange/sdk';
import {
	connectClient,
	DLOB_URL,
	fundWallet,
	loadKey,
	parseDecimal,
	perpMarketIndex,
	RPC_URL,
	userAccountPublicKey,
} from './tools';

const INITIAL_DEPOSIT_DUSDT = '10000';
const CANCEL_ALL_MAX_CALLS = 10;
const FEED_SETTLE_MS = 3_000;

const PRICE_DECIMALS = PRICE_PRECISION_EXP.toNumber();
const BASE_DECIMALS = BASE_PRECISION_EXP.toNumber();

type Flags = Map<string, string | true>;

type Session = {
	client: VelocityClient;
	trader: Keypair;
	feed: UserClobOrdersClient;
};

const BOOLEAN_FLAGS = new Set(['cross', 'reduce-only']);

/** Splits argv into positionals and `--flag [value]` pairs. A flag in `BOOLEAN_FLAGS` takes no value. */
function parseArgs(argv: string[]): { positional: string[]; flags: Flags } {
	const positional: string[] = [];
	const flags: Flags = new Map();

	for (let i = 0; i < argv.length; i++) {
		const arg = argv[i];
		if (!arg.startsWith('--')) {
			positional.push(arg);
			continue;
		}

		const name = arg.slice(2);
		flags.set(name, BOOLEAN_FLAGS.has(name) ? true : argv[++i]);
	}

	return { positional, flags };
}

function stringFlag(flags: Flags, name: string): string | undefined {
	const value = flags.get(name);
	if (value === true) throw new Error(`--${name} needs a value`);
	return value;
}

function formatFixed(value: BN, decimals: number): string {
	const digits = value
		.abs()
		.toString()
		.padStart(decimals + 1, '0');
	const whole = digits.slice(0, digits.length - decimals);
	const fraction = digits.slice(digits.length - decimals).replace(/0+$/, '');
	return `${value.isNeg() ? '-' : ''}${whole}${fraction ? `.${fraction}` : ''}`;
}

function describeOrder(order: UserClobOrder): string {
	const side = 'long' in order.direction ? 'bid' : 'ask';
	return [
		`order ${order.orderId}`,
		`market ${order.marketIndex}`,
		side,
		`${formatFixed(order.baseAssetAmount, BASE_DECIMALS)} @ ${formatFixed(
			order.price,
			PRICE_DECIMALS
		)}`,
		`node ${order.nodeIndex}`,
		order.takerOrigin ? 'taker-origin' : '',
	]
		.filter(Boolean)
		.join('  ');
}

/** Funds the trader and opens its account on first use. */
async function openSession(
	connection: Connection,
	name: string
): Promise<Session> {
	const trader = loadKey(`/state/keys/trader-${name}.json`);
	const client = await connectClient(connection, trader);
	const user = userAccountPublicKey(client, trader.publicKey);

	if (!(await connection.getAccountInfo(user))) {
		console.log(`opening trader ${name}: ${trader.publicKey.toBase58()}`);
		const tokenAccount = await fundWallet(
			connection,
			trader.publicKey,
			10,
			INITIAL_DEPOSIT_DUSDT
		);
		await client.initializeUserAccountAndDepositCollateral(
			parseDecimal(INITIAL_DEPOSIT_DUSDT, 6),
			tokenAccount
		);
	} else if (
		(await connection.getBalance(trader.publicKey)) < LAMPORTS_PER_SOL
	) {
		await fundWallet(connection, trader.publicKey, 10, '0');
	}

	await client.addUser(0);
	return { client, trader, feed: new UserClobOrdersClient(DLOB_URL) };
}

async function feedOrders(
	session: Session,
	marketIndexes: number[]
): Promise<UserClobOrder[]> {
	const user = userAccountPublicKey(session.client, session.trader.publicKey);
	return session.feed.fetch(user, marketIndexes);
}

async function account(session: Session): Promise<void> {
	const user = session.client.getUser();
	console.log(`authority ${session.trader.publicKey.toBase58()}`);
	console.log(`user      ${user.getUserAccountPublicKey().toBase58()}`);
	console.log(`collateral ${formatFixed(user.getTotalCollateral(), 6)} USD`);

	for (const position of user.getActivePerpPositions()) {
		console.log(
			`position market ${position.marketIndex}: ${formatFixed(
				position.baseAssetAmount,
				BASE_DECIMALS
			)}`
		);
	}
}

async function orders(session: Session, market?: string): Promise<void> {
	const marketIndexes = market
		? [perpMarketIndex(market)]
		: session.client.getPerpMarketAccounts().map((m) => m.marketIndex);
	const rows = await feedOrders(session, marketIndexes);

	if (rows.length === 0) console.log('no resting orders');
	rows.forEach((row) => console.log(describeOrder(row)));
}

async function rest(
	session: Session,
	[market, side, price, size]: string[],
	flags: Flags
): Promise<void> {
	if (!market || !['bid', 'ask'].includes(side) || !price || !size) {
		throw new Error('rest <market> <bid|ask> <price> <size>');
	}

	const marketIndex = perpMarketIndex(market);
	const direction =
		side === 'bid' ? PositionDirection.LONG : PositionDirection.SHORT;
	const count = Number(stringFlag(flags, 'count') ?? '1');
	const step = parseDecimal(stringFlag(flags, 'step') ?? '0', PRICE_DECIMALS);
	const expireSecs = stringFlag(flags, 'expire-secs');
	const clobAccounts = await session.client.getClobAccounts(marketIndex);

	for (let rung = 0; rung < count; rung++) {
		const offset = step.muln(rung);
		const rungPrice =
			side === 'bid'
				? parseDecimal(price, PRICE_DECIMALS).sub(offset)
				: parseDecimal(price, PRICE_DECIMALS).add(offset);

		const signature = await session.client.placeAndMakePerpOrder(
			{
				orderType: OrderType.LIMIT,
				marketIndex,
				direction,
				baseAssetAmount: parseDecimal(size, BASE_DECIMALS),
				price: rungPrice,
				postOnly: flags.has('cross')
					? PostOnlyParams.NONE
					: PostOnlyParams.MUST_POST_ONLY,
				reduceOnly: flags.has('reduce-only'),
				maxTs: expireSecs
					? new BN(Math.floor(Date.now() / 1000) + Number(expireSecs))
					: null,
			},
			clobAccounts
		);

		console.log(
			`rested ${side} ${size} @ ${formatFixed(
				rungPrice,
				PRICE_DECIMALS
			)}: ${signature}`
		);
	}
}

async function take(
	session: Session,
	[market, side, size]: string[],
	flags: Flags
): Promise<void> {
	if (!market || !['buy', 'sell'].includes(side) || !size) {
		throw new Error('take <market> <buy|sell> <size> [--worst <price>]');
	}

	const worst = stringFlag(flags, 'worst');
	const signature = await session.client.placeAndTakePerpOrder({
		orderType: OrderType.MARKET,
		marketIndex: perpMarketIndex(market),
		direction:
			side === 'buy' ? PositionDirection.LONG : PositionDirection.SHORT,
		baseAssetAmount: parseDecimal(size, BASE_DECIMALS),
		...(worst ? { price: parseDecimal(worst, PRICE_DECIMALS) } : {}),
	});

	console.log(`took ${side} ${size}: ${signature}`);
}

async function cancel(
	session: Session,
	[market, orderId]: string[]
): Promise<void> {
	if (!market) throw new Error('cancel <market> [orderId]');

	const marketIndex = perpMarketIndex(market);

	if (orderId) {
		const rows = await feedOrders(session, [marketIndex]);
		const row = rows.find((r) => r.orderId === Number(orderId));
		if (!row)
			throw new Error(
				`order ${orderId} is not on the feed for market ${marketIndex}`
			);

		const signature = await session.client.cancelOrderV1({
			marketIndex,
			orderRef: { nodeIndex: row.nodeIndex, orderId: row.clobOrderId },
		});
		console.log(`cancelled order ${orderId}: ${signature}`);
		return;
	}

	// One call removes at most 128 orders, so a long book takes several.
	for (let call = 0; call < CANCEL_ALL_MAX_CALLS; call++) {
		const signature = await session.client.cancelOrdersV1({
			marketIndex,
			sides: CancelSidesV0.BOTH,
		});
		console.log(`cancel-all: ${signature}`);

		await new Promise((resolve) => setTimeout(resolve, FEED_SETTLE_MS));
		const remaining = await feedOrders(session, [marketIndex]);
		if (remaining.length === 0) return;
	}

	throw new Error(
		`orders still rest after ${CANCEL_ALL_MAX_CALLS} cancel-all calls`
	);
}

async function main() {
	const { positional, flags } = parseArgs(process.argv.slice(2));
	const [command, ...args] = positional;
	const connection = new Connection(RPC_URL, 'confirmed');
	const session = await openSession(
		connection,
		stringFlag(flags, 'name') ?? 'b'
	);

	try {
		if (command === 'account') await account(session);
		else if (command === 'orders') await orders(session, args[0]);
		else if (command === 'rest') await rest(session, args, flags);
		else if (command === 'take') await take(session, args, flags);
		else if (command === 'cancel') await cancel(session, args);
		else throw new Error('commands: account, orders, rest, take, cancel');
	} finally {
		await session.client.unsubscribe();
	}
}

main().catch((error) => {
	console.error(error instanceof Error ? error.message : error);
	process.exit(1);
});
