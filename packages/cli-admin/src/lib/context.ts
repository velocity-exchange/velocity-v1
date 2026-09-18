import { confirm, isCancel } from '@clack/prompts';
import { Connection, PublicKey } from '@solana/web3.js';
import pc from 'picocolors';

/**
 * The run context for one invocation: cluster (verified by genesis hash), signer, and dispatch mode.
 * A module-level singleton, since the CLI is single-command and runs one context at a time.
 */

export type Cluster = 'mainnet-beta' | 'devnet' | 'testnet' | 'unknown';

const GENESIS: Record<string, Cluster> = {
	'5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d': 'mainnet-beta',
	EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG: 'devnet',
	'4uhcVJyU9pJkvQyS88uRDiswHXSCkY3zQawwpjk2NsNY': 'testnet',
};

export type RunContext = {
	cluster: Cluster;
	profile?: string;
	signer: PublicKey;
	multisig?: PublicKey;
	yes: boolean;
};

let current: RunContext | undefined;

export function getRunContext(): RunContext | undefined {
	return current;
}

export async function detectCluster(connection: Connection): Promise<Cluster> {
	try {
		return GENESIS[await connection.getGenesisHash()] ?? 'unknown';
	} catch {
		return 'unknown';
	}
}

/**
 * Resolve and print the run context, and return the effective env.
 * `buildAdminClient` calls this once, so every command that talks to the chain
 * announces itself.
 *
 * A declared env comes from a flag or a profile. A declared env that
 * contradicts the RPC's genesis hash is fatal. The one mistake this tool must
 * make impossible is the right command against the wrong cluster. When no env
 * is declared, `envDeclared` is false and the detected cluster becomes the env,
 * so an invocation with no flags against devnet still works.
 */
export async function announceContext(
	connection: Connection,
	args: {
		env: string;
		envDeclared: boolean;
		profile?: string;
		signer: PublicKey;
		multisig?: PublicKey;
		yes?: boolean;
	}
): Promise<string> {
	const cluster = await detectCluster(connection);
	let env = args.env;
	if (cluster !== 'unknown') {
		if (args.envDeclared && cluster !== args.env) {
			throw new Error(
				`cluster mismatch: env says "${args.env}" but the RPC's genesis hash is ${cluster}; ` +
					`fix the profile/flags before anything is sent`
			);
		}
		if (
			!args.envDeclared &&
			(cluster === 'mainnet-beta' || cluster === 'devnet')
		) {
			env = cluster;
		}
	}
	current = {
		cluster,
		profile: args.profile,
		signer: args.signer,
		multisig: args.multisig,
		yes: args.yes ?? false,
	};

	// One dim line on stderr. It names the chain, the signer, and whether a send
	// lands directly or as a proposal. The keys stay short so the line reads as
	// a status bar and does not compete with the command's own output.
	const clusterLabel =
		cluster === 'mainnet-beta'
			? pc.red(pc.bold(cluster))
			: cluster === 'unknown'
			? pc.yellow(`unknown chain (genesis check failed, env=${env})`)
			: pc.green(cluster);
	const parts = [clusterLabel];
	if (args.profile) {
		parts.push(pc.dim(args.profile));
	}
	parts.push(pc.dim(shorten(args.signer)));
	parts.push(
		args.multisig
			? pc.dim(`proposes to ${shorten(args.multisig)}`)
			: pc.dim('sends directly')
	);
	console.error(`  ${parts.join(pc.dim('  '))}`);
	return env;
}

/**
 * Gate a mainnet direct send behind an interactive confirmation. It does
 * nothing for a proposal, because the multisig is the gate there. It also does
 * nothing on another cluster, with `--yes`, or when stdin is not a TTY. A
 * script must not hang, so running without a TTY implies `--yes`.
 */
export async function confirmMainnetDirect(memo: string): Promise<void> {
	const ctx = current;
	if (
		!ctx ||
		ctx.cluster !== 'mainnet-beta' ||
		ctx.multisig ||
		ctx.yes ||
		!process.stdin.isTTY
	) {
		return;
	}
	const ok = await confirm({
		message: `mainnet direct send (${memo}) signed by ${shorten(
			ctx.signer
		)}, proceed?`,
	});
	if (isCancel(ok) || !ok) {
		throw new Error('aborted');
	}
}

function shorten(pk: PublicKey): string {
	const s = pk.toBase58();
	return `${s.slice(0, 4)}…${s.slice(-4)}`;
}
