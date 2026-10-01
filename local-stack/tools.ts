/**
 * Shared setup for the operator scripts that run against the stack through the bootstrap image:
 * keys, a connected client, SOL and dUSDT funding, and the parsing of market names and amounts.
 */
import * as fs from 'fs';
import { BN } from '@coral-xyz/anchor';
import {
	Connection,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	sendAndConfirmTransaction,
	SystemProgram,
	Transaction,
} from '@solana/web3.js';
import {
	BulkAccountLoader,
	DevnetPerpMarkets,
	getMarketsAndOraclesForSubscription,
	getUserAccountPublicKeySync,
	TokenFaucet,
	VelocityClient,
	Wallet,
} from '@velocity-exchange/sdk';

export const RPC_URL = process.env.RPC_URL ?? 'http://rpc:8899';
export const DLOB_URL = process.env.DLOB_URL ?? 'http://dlob-server:6969';
export const AUTHORITY_PATH = '/state/snapshot/authority.json';

const TOKEN_FAUCET = new PublicKey(
	'V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB'
);
const DUSDT_MINT = new PublicKey(
	'GqmEqYsy8EyvofDpmtFxK8zhYrgWgNokAtYoduQdL7v6'
);
const DUSDT_DECIMALS = 6;

/** Loads a keypair file, and writes a new keypair there first when the file is missing. */
export function loadKey(file: string): Keypair {
	if (!fs.existsSync(file))
		fs.writeFileSync(
			file,
			JSON.stringify(Array.from(Keypair.generate().secretKey))
		);

	return Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf-8')))
	);
}

/** Parses a decimal string such as `"12.5"` into an integer at `decimals` places. */
export function parseDecimal(value: string, decimals: number): BN {
	if (!/^\d+(\.\d+)?$/.test(value)) {
		throw new Error(`"${value}" is not a positive decimal number`);
	}

	const [whole, fraction = ''] = value.split('.');
	if (fraction.length > decimals) {
		throw new Error(`"${value}" has more than ${decimals} decimal places`);
	}

	return new BN(whole + fraction.padEnd(decimals, '0'));
}

/** Accepts a perp market symbol such as `SOL-PERP` or its index. */
export function perpMarketIndex(market: string): number {
	if (/^\d+$/.test(market)) return Number(market);

	const config = DevnetPerpMarkets.find(
		(m) => m.symbol.toLowerCase() === market.toLowerCase()
	);
	if (!config) {
		const symbols = DevnetPerpMarkets.map((m) => m.symbol).join(', ');
		throw new Error(`unknown perp market "${market}". Markets: ${symbols}`);
	}

	return config.marketIndex;
}

/**
 * Sends SOL from the stack authority and mints dUSDT from the faucet, so `wallet` can pay fees
 * and deposit. It returns the wallet's dUSDT token account.
 */
export async function fundWallet(
	connection: Connection,
	wallet: PublicKey,
	sol: number,
	dusdt: string
): Promise<PublicKey> {
	const authority = loadKey(AUTHORITY_PATH);

	if (sol > 0) {
		const transfer = SystemProgram.transfer({
			fromPubkey: authority.publicKey,
			toPubkey: wallet,
			lamports: Math.round(sol * LAMPORTS_PER_SOL),
		});
		await sendAndConfirmTransaction(
			connection,
			new Transaction().add(transfer),
			[authority]
		);
	}

	const faucet = new TokenFaucet(
		connection,
		new Wallet(authority),
		TOKEN_FAUCET,
		DUSDT_MINT
	);
	const [tokenAccount] = await faucet.createAssociatedTokenAccountAndMintTo(
		wallet,
		parseDecimal(dusdt, DUSDT_DECIMALS)
	);

	return tokenAccount;
}

export async function connectClient(
	connection: Connection,
	signer: Keypair
): Promise<VelocityClient> {
	const { perpMarketIndexes, spotMarketIndexes, oracleInfos } =
		getMarketsAndOraclesForSubscription('devnet');
	const client = new VelocityClient({
		connection,
		wallet: new Wallet(signer),
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

export function userAccountPublicKey(
	client: VelocityClient,
	authority: PublicKey
): PublicKey {
	return getUserAccountPublicKeySync(client.program.programId, authority, 0);
}
