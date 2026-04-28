/**
 * Repoint the dUSDT spot market (index 0) oracle away from Pubkey::default(),
 * which causes load_maps' Oracle/Spot/Perp peek-and-break loops to all exit
 * immediately and surfaces as PerpMarketNotFound on place_perp_order.
 *
 * Two ways to point it:
 *   USDT_LAZER_FEED_ID=<u32>   initialize (if needed) a drift-owned PythLazer
 *                              PDA for that feed id, then point spot 0 at it.
 *   ORACLE_PUBKEY=<base58>     point spot 0 directly at the supplied account
 *                              (e.g. a Pyth-owned USDC/USD devnet price acct).
 *                              Pair with ORACLE_SOURCE=PYTH|PYTH_LAZER (default
 *                              PYTH).
 *
 * Required env:
 *   DEVNET_ADMIN     path to admin keypair (State.admin signer)
 *   RPC_URL          RPC endpoint (default: drift private Triton devnet)
 *   PROGRAM_ID       drift program id (default: dRiftyHA39MWEi3m9aunc5MzRF1JYuB...)
 */
import {
	Connection,
	Keypair,
	PublicKey,
} from '@solana/web3.js';
import {
	AdminClient,
	OracleSource,
	Wallet,
	getPythLazerOraclePublicKey,
	getSpotMarketPublicKey,
	loadKeypair,
} from '../sdk/src';

const DEFAULT_RPC =
	'https://drift-drift-a827.devnet.rpcpool.com/3639271b-6f0e-47c6-a643-1aaa0e498f58';
const DEFAULT_PROGRAM_ID = 'FGXfSBCXqSTkBX6zTQyPo8JbC11pn5DGKYm9MSbLC7P2';
const SPOT_MARKET_INDEX = 0;
const EXPECTED_SPOT_PDA = 'H4UqRQuYXBbfPyiADFsWwRhajS7Jpwu3zyYwFn47GXC4';

function requireEnv(name: string): string {
	const v = process.env[name];
	if (!v) throw new Error(`${name} must be set`);
	return v;
}

async function main() {
	const adminPath = requireEnv('DEVNET_ADMIN');
	const rpcUrl = process.env.RPC_URL ?? DEFAULT_RPC;
	const programId = new PublicKey(
		process.env.PROGRAM_ID ?? DEFAULT_PROGRAM_ID
	);

	const lazerFeedIdEnv = process.env.USDT_LAZER_FEED_ID;
	const oraclePubkeyEnv = process.env.ORACLE_PUBKEY;
	if (!lazerFeedIdEnv && !oraclePubkeyEnv) {
		throw new Error(
			'Set USDT_LAZER_FEED_ID=<u32> OR ORACLE_PUBKEY=<base58>'
		);
	}
	if (lazerFeedIdEnv && oraclePubkeyEnv) {
		throw new Error(
			'Set only one of USDT_LAZER_FEED_ID / ORACLE_PUBKEY'
		);
	}

	const admin = loadKeypair(adminPath) as Keypair;
	const connection = new Connection(rpcUrl, 'confirmed');
	const wallet = new Wallet(admin);

	const client = new AdminClient({
		connection,
		wallet,
		programID: programId,
		opts: { commitment: 'confirmed', skipPreflight: false },
		env: 'devnet',
	});
	await client.subscribe();

	const spotPda = await getSpotMarketPublicKey(programId, SPOT_MARKET_INDEX);
	if (spotPda.toBase58() !== EXPECTED_SPOT_PDA) {
		console.warn(
			`spot 0 PDA ${spotPda.toBase58()} != expected ${EXPECTED_SPOT_PDA}; continuing anyway`
		);
	}

	let oracle: PublicKey;
	let oracleSource: OracleSource;

	if (lazerFeedIdEnv) {
		const feedId = Number(lazerFeedIdEnv);
		if (!Number.isFinite(feedId) || feedId < 0) {
			throw new Error('USDT_LAZER_FEED_ID must be a non-negative integer');
		}
		oracle = getPythLazerOraclePublicKey(programId, feedId);
		oracleSource = OracleSource.PYTH_LAZER_STABLE_COIN;
		const info = await connection.getAccountInfo(oracle);
		if (!info) {
			console.log(
				`PythLazer PDA ${oracle.toBase58()} (feed ${feedId}) does not exist — initializing`
			);
			const sig = await client.initializePythLazerOracle(feedId);
			console.log(`  initializePythLazerOracle tx: ${sig}`);
		} else {
			console.log(
				`PythLazer PDA ${oracle.toBase58()} (feed ${feedId}) already exists — reusing`
			);
		}
	} else {
		oracle = new PublicKey(oraclePubkeyEnv!);
		const sourceName = (process.env.ORACLE_SOURCE ?? 'PYTH').toUpperCase();
		switch (sourceName) {
			case 'PYTH':
				oracleSource = OracleSource.PYTH;
				break;
			case 'PYTH_LAZER':
				oracleSource = OracleSource.PYTH_LAZER;
				break;
			default:
				throw new Error(
					`unsupported ORACLE_SOURCE=${sourceName} (PYTH or PYTH_LAZER)`
				);
		}
		const info = await connection.getAccountInfo(oracle);
		if (!info) {
			throw new Error(
				`ORACLE_PUBKEY ${oracle.toBase58()} not found on this RPC — refusing to point spot 0 at a missing account`
			);
		}
	}

	console.log('--- about to call updateSpotMarketOracle ---');
	console.log(`spot pda:      ${spotPda.toBase58()}`);
	console.log(`market index:  ${SPOT_MARKET_INDEX}`);
	console.log(`new oracle:    ${oracle.toBase58()}`);
	console.log(`oracle source: ${JSON.stringify(oracleSource)}`);

	const txSig = await client.updateSpotMarketOracle(
		SPOT_MARKET_INDEX,
		oracle,
		oracleSource,
		false
	);
	console.log(`updateSpotMarketOracle tx: ${txSig}`);

	await client.unsubscribe();
}

main().then(
	() => process.exit(0),
	(err) => {
		console.error(err);
		process.exit(1);
	}
);
