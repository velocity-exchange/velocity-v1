import { Command } from 'commander';
import * as os from 'os';
import { GlobalOpts } from './provider';
import { VelocityEnv } from '@velocity-exchange/sdk';
import { resolveProfile } from './config';

/**
 * Attach shared global options to every subcommand.
 *
 * commander v12 only walks `parent.opts()` once, so options declared on the
 * root must be redeclared on each leaf to surface in `--help` output and in
 * `cmd.opts()`. This helper keeps that consistent.
 *
 * Connection options are declared without defaults: `readGlobalOpts` layers
 * explicit flag > selected profile > legacy default, and a declared default
 * would be indistinguishable from an explicit flag, letting it shadow the
 * profile.
 */
export function withGlobalOptions(cmd: Command): Command {
	return cmd
		.option(
			'-p, --profile <name>',
			'connection profile from `velocity-admin config` (also VELOCITY_ADMIN_PROFILE env, or the configured default)'
		)
		.option(
			'-u, --url <url>',
			'Solana RPC URL (default: profile, else https://api.mainnet-beta.solana.com)'
		)
		.option(
			'-k, --keypair <path>',
			'path to signer keypair JSON (default: profile, else ~/.config/solana/id.json)'
		)
		.option(
			'-e, --env <env>',
			'Velocity env, mainnet-beta or devnet (default: profile, else mainnet-beta)'
		)
		.option(
			'-m, --multisig <pubkey>',
			'Squads V4 multisig PDA: wraps the action in a vault transaction proposal instead of sending directly (default: profile). Pass --no-multisig to force a direct send under a profile that proposes.'
		)
		.option(
			'--no-multisig',
			'force a direct send with the local wallet, overriding a profile multisig'
		)
		.option(
			'-y, --yes',
			'skip the interactive confirmation for mainnet direct sends'
		);
}

export function readGlobalOpts(cmd: Command): GlobalOpts {
	const opts = cmd.optsWithGlobals();
	const selected = resolveProfile(opts.profile as string | undefined);
	const profile = selected?.profile;

	// commander turns --no-multisig into `multisig: false`: an explicit
	// "send directly", overriding a profile's multisig.
	const multisig =
		opts.multisig === false
			? undefined
			: (opts.multisig as string | undefined) ?? profile?.multisig;

	const explicitEnv = (opts.env as string) ?? profile?.env;
	const env = explicitEnv ?? 'mainnet-beta';
	if (env !== 'mainnet-beta' && env !== 'devnet') {
		throw new Error(`unknown env "${env}" (expected mainnet-beta or devnet)`);
	}
	return {
		url:
			(opts.url as string) ??
			profile?.url ??
			selected?.sharedRpc ??
			'https://api.mainnet-beta.solana.com',
		keypair:
			(opts.keypair as string) ??
			profile?.keypair ??
			`${os.homedir()}/.config/solana/id.json`,
		env: env as VelocityEnv,
		envExplicit: explicitEnv !== undefined,
		multisig,
		profile: selected?.name,
		yes: Boolean(opts.yes),
	};
}

export function parseBoolean(value: string, name: string): boolean {
	if (value === 'true') {
		return true;
	}
	if (value === 'false') {
		return false;
	}
	throw new Error(`${name} must be "true" or "false", got "${value}"`);
}
