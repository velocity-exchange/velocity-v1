import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { VelocityEnv } from '@velocity-exchange/sdk';

/**
 * Named connection profiles for the CLI, stored per-user (never in the repo):
 * `~/.config/velocity-admin/config.json`, overridable with
 * `VELOCITY_ADMIN_CONFIG`. A profile bundles what today is passed as
 * `-u/-k/-e/-m` on every invocation; explicit flags always win over the
 * profile, so nothing changes for flag-only users and no profile value can
 * silently redirect an explicit request.
 *
 * Multisig addresses deliberately live only in this local config, not as
 * constants in the codebase. `config init` verifies a pasted multisig against
 * the live State admins (see commands/config.ts), so the config is validated
 * at write time instead of trusted at use time.
 */

export type Profile = {
	/**
	 * Solana RPC URL. Optional: when unset, the shared `rpcs[env]` entry is
	 * used, so one URL per cluster serves every profile on it.
	 */
	url?: string;
	/** Path to the signer keypair JSON (~ expands). */
	keypair: string;
	/** Velocity env, decides program addresses and the shared RPC. */
	env: VelocityEnv;
	/** Squads V4 multisig PDA; when set, actions dispatch as proposals. */
	multisig?: string;
};

export type CliConfig = {
	version: 1;
	default?: string;
	/** Shared per-cluster RPC URLs, the fallback for profiles without `url`. */
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
 * The profile an invocation should use: `--profile` beats
 * `VELOCITY_ADMIN_PROFILE` beats the config's `default`. Returns undefined
 * when nothing selects a profile; flag-only usage stays fully supported.
 */
export function resolveProfile(flag: string | undefined): {
	name: string;
	profile: Profile;
	/** The shared RPC for the profile's env, profile.url's fallback. */
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
