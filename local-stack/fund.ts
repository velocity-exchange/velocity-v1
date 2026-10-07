/**
 * allow-verbose: the usage header of an operator script.
 *
 * Fund a wallet on the stack with SOL from the stack authority and dUSDT from the faucet:
 *
 *   bun run local:fund <pubkey> [--sol <amount>] [--dusdt <amount>]
 *
 * The defaults are 10 SOL and 10000 dUSDT. A test wallet needs both: SOL for fees and rent,
 * dUSDT to deposit as collateral.
 */
import { Connection, PublicKey } from '@solana/web3.js';
import { fundWallet, RPC_URL } from './tools';

const USAGE =
	'usage: bun run local:fund <pubkey> [--sol <amount>] [--dusdt <amount>]';

function parseArgs(argv: string[]): {
	wallet: PublicKey;
	sol: number;
	dusdt: string;
} {
	const positional: string[] = [];
	let sol = 10;
	let dusdt = '10000';

	for (let i = 0; i < argv.length; i++) {
		if (argv[i] === '--sol') sol = Number(argv[++i]);
		else if (argv[i] === '--dusdt') dusdt = argv[++i];
		else positional.push(argv[i]);
	}

	if (positional.length !== 1 || !Number.isFinite(sol) || sol < 0) {
		throw new Error(USAGE);
	}

	return { wallet: new PublicKey(positional[0]), sol, dusdt };
}

async function main() {
	const { wallet, sol, dusdt } = parseArgs(process.argv.slice(2));
	const connection = new Connection(RPC_URL, 'confirmed');

	const tokenAccount = await fundWallet(connection, wallet, sol, dusdt);
	const balance = await connection.getTokenAccountBalance(tokenAccount);
	const lamports = await connection.getBalance(wallet);

	console.log(`wallet ${wallet.toBase58()}`);
	console.log(`  SOL:   ${lamports / 1e9}`);
	console.log(
		`  dUSDT: ${balance.value.uiAmountString} in ${tokenAccount.toBase58()}`
	);
}

main().catch((error) => {
	console.error(error instanceof Error ? error.message : error);
	process.exit(1);
});
