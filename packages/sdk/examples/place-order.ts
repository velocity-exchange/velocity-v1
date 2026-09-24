/**
 * Places a 1 SOL-PERP market long on devnet. Needs an initialized Velocity account with collateral.
 * Run: KEYPAIR=~/.config/solana/id.json bunx ts-node examples/place-order.ts
 */
import { Connection } from '@solana/web3.js';
import {
	BASE_PRECISION,
	BN,
	BulkAccountLoader,
	PerpMarkets,
	PositionDirection,
	VelocityClient,
	VelocityEnv,
	Wallet,
	getMarketOrderParams,
	loadKeypair,
} from '@velocity-exchange/sdk';

const env: VelocityEnv = 'devnet';

async function main() {
	const keypairPath = process.env.KEYPAIR;
	if (!keypairPath) {
		throw new Error(
			'set KEYPAIR to a keypair file path, JSON array, or base58 key'
		);
	}

	const connection = new Connection(
		process.env.RPC_URL ?? 'https://api.devnet.solana.com',
		'confirmed'
	);

	const client = new VelocityClient({
		connection,
		wallet: new Wallet(loadKeypair(keypairPath)),
		env,
		accountSubscription: {
			type: 'polling',
			accountLoader: new BulkAccountLoader(connection, 'confirmed', 1000),
		},
	});
	// subscribe() resolves false rather than throwing when a subscription fails.
	if (!(await client.subscribe())) {
		throw new Error('failed to subscribe to Velocity accounts');
	}

	try {
		const solMarket = PerpMarkets[env].find((m) => m.baseAssetSymbol === 'SOL');
		if (!solMarket) {
			throw new Error('SOL-PERP not listed on this deployment');
		}

		const txSig = await client.placePerpOrder(
			getMarketOrderParams({
				marketIndex: solMarket.marketIndex,
				direction: PositionDirection.LONG,
				baseAssetAmount: new BN(1).mul(BASE_PRECISION),
			})
		);
		console.log(`placed 1 SOL-PERP long: ${txSig}`);
	} finally {
		await client.unsubscribe();
	}
}

main();
