import { Command } from 'commander';
import * as os from 'os';
import { GlobalOpts } from './provider';
import { VelocityEnv } from '@velocity-exchange/sdk';
import { resolveProfile } from './config';
import { setDryRun } from './squads';

/**
 * Attach shared global options to every subcommand.
 *
 * commander v12 only walks `parent.opts()` once, so options declared on the
 * root must be redeclared on each leaf to surface in `--help` output and in
 * `cmd.opts()`. This helper keeps that consistent.
 *
 * A connection option is declared without a default. `readGlobalOpts` takes
 * the explicit flag first, then the selected profile, then the fallback
 * default. A declared default would look the same as an explicit flag, so it
 * would shadow the profile.
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
		)
		.option(
			'--dry-run',
			'build and price the instructions, send nothing: prints each instruction, the dispatch route (direct send or vault proposal) and the expected rent/fees',
			false
		);
}

export function readGlobalOpts(cmd: Command): GlobalOpts {
	const opts = cmd.optsWithGlobals();
	// Set the process-wide flag here rather than passing it through every
	// command. Every dispatching command calls this before sendOrPropose.
	setDryRun(opts.dryRun === true);
	const selected = resolveProfile(opts.profile as string | undefined);
	const profile = selected?.profile;

	// commander turns --no-multisig into `multisig: false`. That is an explicit
	// request to send directly, and it overrides a profile's multisig.
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
		dryRun: opts.dryRun === true,
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
