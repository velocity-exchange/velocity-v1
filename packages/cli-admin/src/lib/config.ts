import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { VelocityEnv } from '@velocity-exchange/sdk';

/**
 * Named connection profiles, stored per user at `~/.config/velocity-admin/config.json`
 * (override with `VELOCITY_ADMIN_CONFIG`). An explicit flag always wins over the profile.
 */

export type Profile = {
	/** Solana RPC URL. Falls back to the shared `rpcs[env]` entry when unset. */
	url?: string;
	/** Path to the signer keypair JSON. A leading `~` expands. */
	keypair: string;
	/** Velocity env. It decides the program addresses and the shared RPC. */
	env: VelocityEnv;
	/** Squads V4 multisig PDA. When it is set, an action dispatches as a proposal. */
	multisig?: string;
};

export type CliConfig = {
	version: 1;
	default?: string;
	/** Shared per-cluster RPC URLs. A profile without `url` falls back to these. */
	rpcs?: Partial<Record<VelocityEnv, string>>;
	profiles: Record<string, Profile>;
};

const EMPTY: CliConfig = { version: 1, profiles: {} };

export function configPath(): string {
	return (
		process.env.VELOCITY_ADMIN_CONFIG ??
		path.join(os.homedir(), '.config', 'velocity-admin', 'config.json')
	);
}

export function loadConfig(): CliConfig {
	const p = configPath();
	if (!fs.existsSync(p)) {
		return { ...EMPTY, profiles: {} };
	}
	let parsed: unknown;
	try {
		parsed = JSON.parse(fs.readFileSync(p, 'utf-8'));
	} catch (e) {
		throw new Error(
			`config ${p} is not valid JSON (${
				(e as Error).message
			}); fix or delete it`
		);
	}
	const cfg = parsed as CliConfig;
	if (cfg.version !== 1 || typeof cfg.profiles !== 'object') {
		throw new Error(`config ${p} has an unknown shape; expected version 1`);
	}
	return cfg;
}

export function saveConfig(cfg: CliConfig): void {
	const p = configPath();
	fs.mkdirSync(path.dirname(p), { recursive: true });
	fs.writeFileSync(p, JSON.stringify(cfg, null, '\t') + '\n', { mode: 0o600 });
}

/**
 * The profile an invocation uses. `--profile` wins over
 * `VELOCITY_ADMIN_PROFILE`, which wins over the config's `default`. Returns
 * null when nothing selects a profile, so a flag-only invocation still works.
 */
export function resolveProfile(flag: string | undefined): {
	name: string;
	profile: Profile;
	/** The shared RPC for the profile's env. `profile.url` falls back to it. */
	sharedRpc?: string;
} | null {
	const cfg = loadConfig();
	const name = flag ?? process.env.VELOCITY_ADMIN_PROFILE ?? cfg.default;
	if (!name) {
		return null;
	}
	const profile = cfg.profiles[name];
	if (!profile) {
		const known = Object.keys(cfg.profiles);
		throw new Error(
			`unknown profile "${name}"` +
				(known.length
					? ` (configured: ${known.join(', ')})`
					: `; no profiles configured yet, run \`velocity-admin config init\``)
		);
	}
	const sharedRpc = cfg.rpcs?.[profile.env];
	if (!profile.url && !sharedRpc) {
		throw new Error(
			`profile "${name}" has no url and no shared RPC for ${profile.env}; ` +
				`set one with \`velocity-admin config set-rpc ${profile.env} <url>\``
		);
	}
	return { name, profile, sharedRpc };
}
