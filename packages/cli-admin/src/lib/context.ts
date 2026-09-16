import { confirm, isCancel } from '@clack/prompts';
import { Connection, PublicKey } from '@solana/web3.js';
import pc from 'picocolors';

/**
 * Per-invocation run context: which cluster the RPC actually is (verified by
 * genesis hash, not by reading the URL), who signs, and how the action
 * dispatches. Announced once before any command does work, so every
 * invocation states its blast radius up front, and a mainnet direct send
 * asks for confirmation unless `--yes`.
 *
 * Module-level singleton: the CLI is a single-command process, and threading
 * the context through every `sendOrPropose` call site would change dozens of
 * signatures for no behavioral gain.
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
 * Resolve and print the run context, and return the effective env. Called
 * once from `buildAdminClient`, so every command that talks to the chain
 * announces itself.
 *
 * Env resolution: an env declared by the user (flag or profile) that
 * contradicts the RPC's genesis hash is fatal; the one mistake this tool
 * must make impossible is "right command, wrong cluster". When env was never
 * declared (`envDeclared: false`, the legacy default), the detected cluster
 * IS the env, so flag-free invocations against devnet keep working.
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

	// One dim line on stderr: which chain, as whom, and whether anything sent
	// lands directly or as a proposal. Kept to short keys so it reads as a
	// status bar rather than competing with the command's own output.
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
 * Gate a mainnet direct send behind an interactive confirmation. No-op for
 * proposals (the multisig is the gate), for other clusters, with `--yes`, or
 * when stdin is not a TTY (scripts must not hang; `--yes` semantics are
 * implied by scripting).
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
