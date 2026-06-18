/**
 * One-shot devnet script: initialize a BTC-PERP market.
 *
 * Assumes Phases A–D of init-devnet.ts already ran (State + AmmCache exist;
 * SOL-PERP is at perp index 0). This script adds a BTC Pyth Lazer oracle PDA,
 * posts an initial signed price update to it, and initializes BTC-PERP at the
 * next perp index (read from State.numberOfMarkets).
 *
 * Idempotent: oracle init is skipped if the PDA already exists; perp init is
 * skipped if a perp market already exists at the resolved index. Re-posting
 * the price is harmless (on-chain skips messages with a stale/equal timestamp).
 *
 * Required env:
 *   DEVNET_ADMIN       path to admin keypair file (must match State.admin)
 *   PYTH_LAZER_TOKEN   auth token for the Pyth Lazer relay
 * Optional env:
 *   BTC_LAZER_FEED_ID    default 1 (Pyth Lazer feed id for BTC/USD)
 *
 * Pyth Lazer feed-id map (verified on-chain): BTC/USD=1, SOL/USD=6, USDT/USD=8.
 * The oracle PDA is derived from the feed id, so a wrong id silently misprices
 * the market — feed 1 is BTC, NOT SOL.
 *   BTC_PERP_INDEX       override the perp index to init; default = State.numberOfMarkets
 *   BTC_PEG_USD          peg in whole USD; default 100000 (i.e. ~$100k)
 *   BTC_MARGIN_INITIAL   marginRatioInitial bps-of-precision; default 2000 (20%)
 *   BTC_MARGIN_MAINT     marginRatioMaintenance; default 500 (5%)
 *   PYTH_LAZER_ENDPOINTS comma-sep WSS endpoints (default wss://pyth-lazer.dourolabs.app/v1/stream)
 *   PYTH_LAZER_WAIT_MS   ms to wait for first price message (default 30000)
 *   RPC_URL              default https://api.devnet.solana.com
 *   RECEIPT_PATH         default deploy-scripts/out/devnet-deployment.json
 */

import fs from 'fs';
import path from 'path';
import readline from 'readline';
import { Connection, PublicKey } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	AdminClient,
	AMM_RESERVE_PRECISION,
	BASE_PRECISION,
	ContractTier,
	VELOCITY_DEVNET_PROGRAM_ID,
	OracleSource,
	PEG_PRECISION,
	PRICE_PRECISION,
	PythLazerSubscriber,
	Wallet,
	ZERO,
	getAmmCachePublicKey,
	getVelocityStateAccountPublicKey,
	getPerpMarketPublicKey,
	getPythLazerOraclePublicKey,
	loadKeypair,
} from '../packages/sdk/src';

type Receipt = {
	cluster: string;
	programId: string;
	admin: string;
	usdtMint: string;
	spotMarkets: Record<number, { pubkey: string; txSig?: string }>;
	pythLazerOracles: Record<number, { pubkey: string; txSig?: string }>;
	perpMarkets: Record<number, { pubkey: string; txSig?: string }>;
	constituents: Record<number, { pubkey: string; txSig?: string }>;
	startedAt: string;
	finishedAt?: string;
	[k: string]: any;
};

function requireEnv(name: string): string {
	const v = process.env[name];
	if (!v) throw new Error(`missing env ${name}`);
	return v;
}

async function pdaExists(
	connection: Connection,
	pda: PublicKey
): Promise<boolean> {
	const info = await connection.getAccountInfo(pda, 'confirmed');
	return info !== null;
}

function logStep(title: string, note?: string) {
	const ts = new Date().toISOString();
	console.log(`\n[${ts}] ${title}${note ? ` — ${note}` : ''}`);
}

const NON_INTERACTIVE =
	process.env.NON_INTERACTIVE === '1' || process.env.YES === '1';

async function confirm(prompt: string, details?: string[]): Promise<void> {
	if (details && details.length > 0) {
		console.log('');
		for (const line of details) if (line) console.log(`  ${line}`);
	}
	if (NON_INTERACTIVE) {
		console.log(`[non-interactive] ${prompt} (auto-yes)`);
		return;
	}
	const rl = readline.createInterface({
		input: process.stdin,
		output: process.stdout,
	});
	const answer: string = await new Promise((resolve) => {
		rl.question(`\n${prompt} [y/N] `, (a) => resolve(a.trim().toLowerCase()));
	});
	rl.close();
	if (answer !== 'y' && answer !== 'yes') {
		console.log('aborted by user.');
		process.exit(1);
	}
}

function tryLoadReceipt(p: string): Receipt | null {
	try {
		return JSON.parse(fs.readFileSync(p, 'utf8')) as Receipt;
	} catch {
		return null;
	}
}

async function main(): Promise<void> {
	const rpcUrl = process.env.RPC_URL ?? 'https://api.devnet.solana.com';
	const adminPath = requireEnv('DEVNET_ADMIN');
	const pythLazerToken = requireEnv('PYTH_LAZER_TOKEN');

	// Pyth Lazer feed ids: BTC/USD=1, SOL/USD=6, USDT/USD=8.
	const btcFeedId = Number(process.env.BTC_LAZER_FEED_ID ?? 1);
	if (!Number.isFinite(btcFeedId) || btcFeedId < 0) {
		throw new Error('BTC_LAZER_FEED_ID must be a non-negative integer');
	}
	const pegUsd = Number(process.env.BTC_PEG_USD ?? 100_000);
	if (!Number.isFinite(pegUsd) || pegUsd <= 0) {
		throw new Error('BTC_PEG_USD must be a positive integer');
	}
	const marginInitial = Number(process.env.BTC_MARGIN_INITIAL ?? 2000);
	const marginMaint = Number(process.env.BTC_MARGIN_MAINT ?? 500);
	const pythLazerEndpoints = (
		process.env.PYTH_LAZER_ENDPOINTS ??
		'wss://pyth-lazer.dourolabs.app/v1/stream'
	)
		.split(',')
		.map((s) => s.trim())
		.filter(Boolean);
	const pythLazerWaitMs = Number(process.env.PYTH_LAZER_WAIT_MS ?? 30_000);

	const receiptPath =
		process.env.RECEIPT_PATH ?? 'deploy-scripts/out/devnet-deployment.json';
	const absReceiptPath = path.resolve(process.cwd(), receiptPath);
	const receipt = tryLoadReceipt(absReceiptPath);

	const connection = new Connection(rpcUrl, 'confirmed');
	const keypair = loadKeypair(adminPath);
	const wallet = new Wallet(keypair);
	const programId = new PublicKey(
		receipt?.programId ?? VELOCITY_DEVNET_PROGRAM_ID
	);

	console.log(`drift program: ${programId.toBase58()}`);
	console.log(`rpc:           ${rpcUrl}`);
	console.log(`admin:         ${keypair.publicKey.toBase58()}`);
	console.log(`btc lazer fid: ${btcFeedId}`);
	console.log(`peg:           $${pegUsd}`);
	console.log(
		`receipt:       ${receiptPath}${
			receipt ? '' : ' (none found — will create)'
		}`
	);

	// === Pre-flight ===
	logStep('pre-flight checks');
	const programInfo = await connection.getAccountInfo(programId, 'confirmed');
	if (!programInfo || !programInfo.executable) {
		throw new Error(
			`drift program ${programId.toBase58()} is not deployed/executable on ${rpcUrl}.`
		);
	}
	const statePk = await getVelocityStateAccountPublicKey(programId);
	if (!(await pdaExists(connection, statePk))) {
		throw new Error(
			`State ${statePk.toBase58()} not found — run init-devnet.ts first (Phase A).`
		);
	}
	const ammCachePk = getAmmCachePublicKey(programId);
	if (!(await pdaExists(connection, ammCachePk))) {
		throw new Error(
			`AmmCache ${ammCachePk.toBase58()} not found — run init-devnet.ts first (Phase A.2).`
		);
	}

	// Subscribe so we can read State.numberOfMarkets.
	const client = new AdminClient({
		connection,
		wallet,
		programID: programId,
		env: 'devnet',
		accountSubscription: { type: 'websocket', commitment: 'confirmed' },
		perpMarketIndexes: [],
		spotMarketIndexes: [],
		oracleInfos: [],
		skipLoadUsers: true,
	});
	await client.subscribe();
	await client.fetchAccounts();

	const state = client.getStateAccount();
	const numberOfMarkets = state.numberOfMarkets;
	const overrideIndex = process.env.BTC_PERP_INDEX
		? Number(process.env.BTC_PERP_INDEX)
		: undefined;
	let perpIndex: number;
	if (overrideIndex !== undefined) {
		if (!Number.isFinite(overrideIndex) || overrideIndex < 0) {
			throw new Error('BTC_PERP_INDEX must be a non-negative integer');
		}
		if (overrideIndex > numberOfMarkets) {
			throw new Error(
				`BTC_PERP_INDEX=${overrideIndex} would leave a gap (State.numberOfMarkets=${numberOfMarkets}). Next valid index is ${numberOfMarkets}.`
			);
		}
		perpIndex = overrideIndex;
	} else {
		perpIndex = numberOfMarkets;
	}

	const btcOraclePk = getPythLazerOraclePublicKey(programId, btcFeedId);
	const btcPerpPk = await getPerpMarketPublicKey(programId, perpIndex);
	const oracleAlreadyExists = await pdaExists(connection, btcOraclePk);
	const perpAlreadyExists = await pdaExists(connection, btcPerpPk);

	const adminLamports = await connection.getBalance(
		keypair.publicKey,
		'confirmed'
	);

	await confirm('Proceed with BTC-PERP initialization?', [
		`cluster:           ${rpcUrl}`,
		`drift program:     ${programId.toBase58()}`,
		`admin:             ${keypair.publicKey.toBase58()} (${(
			adminLamports / 1e9
		).toFixed(4)} SOL)`,
		`State.coldAdmin:   ${state.coldAdmin.toBase58()}`,
		`numberOfMarkets:   ${numberOfMarkets}`,
		`btc oracle PDA:    ${btcOraclePk.toBase58()} (feed ${btcFeedId})${
			oracleAlreadyExists
				? ' [EXISTS — will skip init, will still post price]'
				: ' [will init]'
		}`,
		`btc perp PDA:      ${btcPerpPk.toBase58()} (index ${perpIndex})${
			perpAlreadyExists ? ' [EXISTS — will skip]' : ' [will init]'
		}`,
		`peg:               ${pegUsd} * PEG_PRECISION (1e6) = ${new BN(pegUsd)
			.mul(PEG_PRECISION)
			.toString()}`,
		`amm reserves:      1000 * AMM_RESERVE_PRECISION (base = quote, placeholder)`,
		`margin init/maint: ${marginInitial}bp / ${marginMaint}bp`,
		`contract tier:     SPECULATIVE (placeholder, tune pre-mainnet)`,
	]);

	// === Step 1: initialize BTC Pyth Lazer oracle PDA ===
	if (oracleAlreadyExists) {
		logStep(
			`Pyth Lazer oracle (BTC feed ${btcFeedId}) already initialized`,
			btcOraclePk.toBase58()
		);
	} else {
		logStep(`initializePythLazerOracle BTC feed=${btcFeedId}`);
		const txSig = await client.initializePythLazerOracle(btcFeedId);
		console.log(`  tx: ${txSig}`);
	}

	// === Step 2: post initial BTC price ===
	// Required before perp init: initializePerpMarket calls get_oracle_price.
	{
		logStep(
			`post initial Pyth Lazer price for BTC (feed ${btcFeedId})`,
			`endpoints=${pythLazerEndpoints.join(',')} waitMs=${pythLazerWaitMs}`
		);
		// Mirror init-devnet.ts Phase C+: include `feedUpdateTimestamp` (on-chain
		// silently skips messages without it). bestBid/Ask feed the on-chain
		// confidence calc; absent → falls back to 20bps.
		const subscriber = new PythLazerSubscriber(
			pythLazerEndpoints,
			pythLazerToken,
			[{ priceFeedIds: [btcFeedId] }],
			'devnet',
			2000,
			false,
			[
				'price',
				'bestAskPrice',
				'bestBidPrice',
				'exponent',
				'feedUpdateTimestamp',
			]
		);
		await subscriber.subscribe();
		const deadline = Date.now() + pythLazerWaitMs;
		let messageHex: string | undefined;
		while (Date.now() < deadline) {
			const messages = Array.from(
				subscriber.feedIdChunkToPriceMessage.values()
			);
			if (messages.length > 0) {
				messageHex = messages[0];
				break;
			}
			await new Promise((r) => setTimeout(r, 250));
		}
		try {
			await subscriber.unsubscribe();
		} catch {
			/* ignore */
		}
		if (!messageHex) {
			throw new Error(
				`Timed out waiting ${pythLazerWaitMs}ms for a Pyth Lazer signed message for BTC feed ${btcFeedId}. Check PYTH_LAZER_TOKEN and that ${pythLazerEndpoints.join(
					', '
				)} accepts it.`
			);
		}
		const postSig = await client.postPythLazerOracleUpdate(
			[btcFeedId],
			messageHex
		);
		console.log(`  tx: ${postSig}`);
	}

	// === Step 3: initialize BTC-PERP ===
	if (perpAlreadyExists) {
		logStep(
			`Perp market ${perpIndex} (BTC-PERP) already initialized`,
			btcPerpPk.toBase58()
		);
	} else {
		logStep(`initializePerpMarket BTC-PERP @ index ${perpIndex} (Pyth Lazer)`);
		const txSig = await client.initializePerpMarket(
			perpIndex,
			btcOraclePk,
			AMM_RESERVE_PRECISION.muln(1000), // baseAssetReserve (placeholder)
			AMM_RESERVE_PRECISION.muln(1000), // quoteAssetReserve
			new BN(60 * 60), // periodicity: 1 hour
			new BN(pegUsd).mul(PEG_PRECISION), // pegMultiplier ≈ BTC spot
			OracleSource.PYTH_LAZER,
			ContractTier.SPECULATIVE,
			marginInitial,
			marginMaint,
			0, // liquidatorFee
			10000, // ifLiquidatorFee
			0, // imfFactor
			true, // activeStatus
			0, // baseSpread
			142500, // maxSpread
			ZERO, // maxOpenInterest
			ZERO, // maxRevenueWithdrawPerPeriod
			ZERO, // quoteMaxInsurance
			BASE_PRECISION.divn(10000), // orderStepSize
			PRICE_PRECISION.divn(100000), // orderTickSize
			BASE_PRECISION.divn(10000), // minOrderSize
			undefined, // concentrationCoefScale -> default ONE
			0, // curveUpdateIntensity
			0, // ammJitIntensity
			'BTC-PERP',
			0 // lpPoolId
		);
		console.log(`  tx: ${txSig}`);
	}

	// === Update receipt ===
	const updated: Receipt = receipt ?? {
		cluster: rpcUrl,
		programId: programId.toBase58(),
		admin: keypair.publicKey.toBase58(),
		usdtMint: '',
		spotMarkets: {},
		pythLazerOracles: {},
		perpMarkets: {},
		constituents: {},
		startedAt: new Date().toISOString(),
	};
	updated.pythLazerOracles ??= {};
	updated.perpMarkets ??= {};
	updated.pythLazerOracles[btcFeedId] = { pubkey: btcOraclePk.toBase58() };
	updated.perpMarkets[perpIndex] = { pubkey: btcPerpPk.toBase58() };
	updated.finishedAt = new Date().toISOString();
	fs.mkdirSync(path.dirname(absReceiptPath), { recursive: true });
	fs.writeFileSync(absReceiptPath, JSON.stringify(updated, null, 2));
	console.log(`\nreceipt updated: ${absReceiptPath}`);

	await client.unsubscribe();
}

main().catch((e) => {
	console.error(e);
	process.exit(1);
});
