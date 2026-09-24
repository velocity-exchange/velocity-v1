/**
 * A read-only quickstart. Prints the SOL-PERP vAMM bid, ask and oracle price.
 * Run with: RPC_URL=<your rpc> bunx ts-node examples/read-market.ts
 */
import { Connection, Keypair } from '@solana/web3.js';
import {
	BN,
	BulkAccountLoader,
	PerpMarkets,
	PRICE_PRECISION,
	VelocityClient,
	VelocityEnv,
	Wallet,
	calculateBidAskPrice,
	convertToNumber,
} from '@velocity-exchange/sdk';

const env: VelocityEnv = 'mainnet-beta';

async function main() {
	const connection = new Connection(
		process.env.RPC_URL ?? 'https://api.mainnet-beta.solana.com',
		'confirmed'
	);

	// VelocityClient requires a wallet even on a read-only path. This keypair
	// never signs anything.
	const wallet = new Wallet(Keypair.generate());

	const client = new VelocityClient({
		connection,
		wallet,
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
		const marketIndex = solMarket.marketIndex;

		const perpMarket = client.getPerpMarketAccountOrThrow(marketIndex);
		const slot = await connection.getSlot();
		const mmOracle = client.getMMOracleDataForPerpMarket(marketIndex, slot);

		const [bid, ask] = calculateBidAskPrice(
			perpMarket.amm,
			perpMarket.marketStats,
			mmOracle,
			true,
			new BN(slot),
			client.getStateAccount()
		);

		console.log(
			`oracle:   $${convertToNumber(mmOracle.price, PRICE_PRECISION)}`
		);
		console.log(`vAMM bid: $${convertToNumber(bid, PRICE_PRECISION)}`);
		console.log(`vAMM ask: $${convertToNumber(ask, PRICE_PRECISION)}`);
	} finally {
		await client.unsubscribe();
	}
}

main();
