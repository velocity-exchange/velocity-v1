/**
 * allow-verbose: the step list of the one-shot bootstrap service in compose.yaml.
 *
 * Bring the validator's copy of devnet to the state the services need. Every step reads chain
 * state first and skips what is already done, so the service runs again on every `up`.
 *
 *   1. Create and fund the service keys in /state/keys.
 *   2. Run migrate.ts, which the upgrade needs anyway.
 *   3. Give every perp market a CLOB book, then run migrate.ts again so the liquidation
 *      conditions name the books.
 *   4. Price and fund the crank treasury, and make swift's key the flow authority.
 *   5. Mint dUSDT to the keeper for the insurance-fund stake the mark TWAP crank needs.
 *   6. Write the keeper config and the UI's env file into /state.
 */
import { execFileSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import { AnchorProvider, BN, Program } from '@coral-xyz/anchor';
import {
	Connection,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	SystemProgram,
	Transaction,
} from '@solana/web3.js';
import {
	DevnetPerpMarkets,
	DevnetSpotMarkets,
	getCrankTreasuryPublicKey,
	TokenFaucet,
	Wallet,
} from '@velocity-exchange/sdk';

const RPC_URL = process.env.RPC_URL ?? 'http://rpc:8899';
const UI_RPC_URL = process.env.UI_RPC_URL ?? 'http://localhost:8899';
const STATE = '/state';
const KEYS = path.join(STATE, 'keys');
const AUTHORITY_PATH = path.join(STATE, 'snapshot', 'authority.json');
const CLOB_PROGRAM = 'BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU';
const TOKEN_FAUCET = new PublicKey(
	'V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB'
);
const DUSDT_MINT = new PublicKey(
	'GqmEqYsy8EyvofDpmtFxK8zhYrgWgNokAtYoduQdL7v6'
);
const SERVICE_KEYS = [
	'keeper',
	'publisher',
	'turner',
	'turner-payout',
	'swift-flow',
] as const;
const SERVICE_SOL = 100;
/** Crank budget the treasury refills each reservoir to, and the level that triggers a refill. */
const TREASURY_REFILL_CRANKS = { target: 1000, watermark: 100 };
const TREASURY_SOL = 500;
const BOOK_EVICT_THRESHOLD = 256;
const KEEPER_DUSDT = new BN(10_000_000_000);

type ServiceKey = (typeof SERVICE_KEYS)[number];

function loadOrCreateKey(name: string): Keypair {
	const file = path.join(KEYS, `${name}.json`);
	if (!fs.existsSync(file)) {
		fs.writeFileSync(
			file,
			JSON.stringify(Array.from(Keypair.generate().secretKey))
		);
	}

	const key = Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf-8')))
	);
	fs.writeFileSync(path.join(KEYS, `${name}.pubkey`), key.publicKey.toBase58());
	return key;
}

async function topUp(
	provider: AnchorProvider,
	to: PublicKey,
	sol: number
): Promise<void> {
	const balance = await provider.connection.getBalance(to);
	const want = sol * LAMPORTS_PER_SOL;
	if (balance >= want / 2) return;
	const transfer = SystemProgram.transfer({
		fromPubkey: provider.wallet.publicKey,
		toPubkey: to,
		lamports: want - balance,
	});

	await provider.sendAndConfirm(new Transaction().add(transfer));
}

function run(label: string, command: string, args: string[]): void {
	console.log(`\n== ${label} ==`);
	execFileSync(command, args, { stdio: 'inherit' });
}

function admin(args: string[]): void {
	const cli = 'packages/cli-admin/lib/index.js';
	run(`velocity-admin ${args.slice(0, 2).join(' ')}`, 'bun', [
		cli,
		...args,
		'--url',
		RPC_URL,
		'--keypair',
		AUTHORITY_PATH,
		'--env',
		'devnet',
		'--yes',
	]);
}

/**
 * A user exposed in three book markets fails the sync on velocity's 32 KB heap, and migrate.ts
 * stops there. Those users go without relay liquidation coverage rather than stopping the stack.
 */
function migrate(label: string): void {
	try {
		run(label, 'bun', [
			'run',
			'deploy-scripts/migrate.ts',
			'--url',
			RPC_URL,
			'--keypair',
			AUTHORITY_PATH,
		]);
	} catch {
		console.log('\nwarning: migrate.ts stopped early; see the log above');
	}
}

/** Markets whose `clob_market` is unset, with the order rules the book copies from them. */
async function marketsWithoutBook(program: Program) {
	const markets = (await (program.account as any).perpMarket.all()) as {
		account: any;
	}[];
	return markets
		.map(({ account }) => account)
		.filter((market) => market.clobMarket.equals(PublicKey.default))
		.sort((a, b) => a.marketIndex - b.marketIndex);
}

async function createBooks(program: Program): Promise<void> {
	for (const market of await marketsWithoutBook(program)) {
		const minOrderSize: BN = market.marketStats.minOrderSize;
		admin([
			'clob-market',
			'init',
			String(market.marketIndex),
			'--clob-program',
			CLOB_PROGRAM,
			'--tick-size',
			market.orderTickSize.toString(),
			'--step-size',
			market.orderStepSize.toString(),
			'--min-order-size',
			minOrderSize.toString(),
			'--blocking-min-size',
			minOrderSize.muln(10).toString(),
			// The CLI's default of 3072 is above the 512 orders a side of the default arena holds,
			// and the book refuses a threshold at or above that.
			'--evict-threshold',
			String(BOOK_EVICT_THRESHOLD),
		]);
	}
}

async function fundTreasury(
	provider: AnchorProvider,
	program: Program
): Promise<void> {
	const treasury = getCrankTreasuryPublicKey(program.programId);
	admin([
		'fees',
		'set-crank-treasury',
		String(TREASURY_REFILL_CRANKS.target),
		String(TREASURY_REFILL_CRANKS.watermark),
	]);
	await topUp(provider, treasury, TREASURY_SOL);
}

async function mintKeeperDusdt(
	provider: AnchorProvider,
	keeper: PublicKey
): Promise<void> {
	const faucet = new TokenFaucet(
		provider.connection,
		provider.wallet,
		TOKEN_FAUCET,
		DUSDT_MINT
	);
	const [tokenAccount] = await faucet.createAssociatedTokenAccountAndMintTo(
		keeper,
		new BN(0)
	);
	const balance = await provider.connection.getTokenAccountBalance(
		tokenAccount
	);
	if (new BN(balance.value.amount).lt(KEEPER_DUSDT)) {
		await faucet.mintToUser(tokenAccount, KEEPER_DUSDT);
	}
}

function writeKeeperConfig(): void {
	const lazerIds = [...DevnetPerpMarkets, ...DevnetSpotMarkets]
		.map((market) => market.pythLazerId)
		.filter((id): id is number => id !== undefined);
	const config = {
		global: {
			velocityEnv: 'devnet',
			endpoint: RPC_URL,
			wsEndpoint: RPC_URL.replace('http', 'ws'),
			keeperPrivateKey: path.join(KEYS, 'keeper.json'),
			priorityFeeMethod: 'solana',
			initUser: true,
			lazerEndpoints: ['wss://pyth-lazer.dourolabs.app/v1/stream'],
		},
		enabledBots: [
			'fundingRateUpdater',
			'userPnlSettler',
			// Both read Pyth Lazer and refuse to start without a token.
			...(process.env.PYTH_LAZER_TOKEN
				? ['markTwapCrank', 'pythLazerCranker']
				: []),
		],
		botConfigs: {
			fundingRateUpdater: { botId: 'funding', dryRun: false },
			userPnlSettler: { botId: 'pnl-settler', dryRun: false },
			markTwapCrank: {
				botId: 'mark-twap',
				dryRun: false,
				autoStakeIfBelowMin: true,
				ifStakeTargetQuote: 1500,
				crankIntervalToMarketIndicies: {
					15000: DevnetPerpMarkets.map((m) => m.marketIndex),
				},
			},
			pythLazerCranker: {
				botId: 'pyth-lazer',
				dryRun: false,
				intervalMs: 400,
				skipSimulation: true,
				pythLazerIdsByChannel: { 'fixed_rate@200ms': [...new Set(lazerIds)] },
			},
		},
	};

	// YAML is a superset of JSON, so the keeper's YAML loader reads this as it is.
	fs.writeFileSync(
		path.join(STATE, 'keeper.yaml'),
		JSON.stringify(config, null, 2)
	);
}

function writeUiEnv(): void {
	const lines = [
		'NEXT_PUBLIC_DEFAULT_TO_DEVNET=true',
		`NEXT_PUBLIC_RPC_OVERRIDE=${UI_RPC_URL}`,
		`NEXT_PUBLIC_RPC_WS_OVERRIDE=${UI_RPC_URL.replace('http', 'ws').replace(
			'8899',
			'8900'
		)}`,
		'NEXT_PUBLIC_DLOB_SERVER_OVERRIDE=http://localhost:6969',
		'NEXT_PUBLIC_OVERRIDE_WS_URL=ws://localhost:3000/ws',
		'NEXT_PUBLIC_SWIFT_SERVER_URL_OVERRIDE=http://localhost:3003',
		'NEXT_PUBLIC_IGNORE_GEOBLOCK=true',
		`NEXT_PUBLIC_FEE_PAYER_DEVNET_PUBLIC_KEY=${fs.readFileSync(
			path.join(KEYS, 'keeper.pubkey'),
			'utf-8'
		)}`,
	];

	fs.writeFileSync(path.join(STATE, 'ui.env.local'), lines.join('\n') + '\n');
}

async function main() {
	fs.mkdirSync(KEYS, { recursive: true });
	const authority = Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(AUTHORITY_PATH, 'utf-8')))
	);
	const provider = new AnchorProvider(
		new Connection(RPC_URL, 'confirmed'),
		new Wallet(authority) as any,
		{
			commitment: 'confirmed',
		}
	);
	const program = new Program(
		JSON.parse(fs.readFileSync('packages/sdk/src/idl/velocity.json', 'utf-8')),
		provider
	);

	const keys = Object.fromEntries(
		SERVICE_KEYS.map((name) => [name, loadOrCreateKey(name)])
	) as Record<ServiceKey, Keypair>;
	for (const name of SERVICE_KEYS) {
		await topUp(
			provider,
			keys[name].publicKey,
			name === 'turner-payout' ? 1 : SERVICE_SOL
		);
	}

	migrate('migrate');
	await createBooks(program);
	migrate('migrate, after the books exist');

	await fundTreasury(provider, program);
	admin([
		'auth',
		'set-hot-admin',
		'flowAuthority',
		keys['swift-flow'].publicKey.toBase58(),
	]);
	await mintKeeperDusdt(provider, keys.keeper.publicKey);

	writeKeeperConfig();
	writeUiEnv();
	console.log('\n== bootstrap done ==');
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
