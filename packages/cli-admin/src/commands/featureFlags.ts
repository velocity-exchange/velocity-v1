import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';

/** Parse a CLI truthy/falsy flag argument (`true|false|on|off|1|0|enable|disable`). */
function parseEnable(value: string): boolean {
	const v = value.trim().toLowerCase();
	if (['true', 'on', '1', 'enable', 'enabled', 'yes'].includes(v)) return true;
	if (['false', 'off', '0', 'disable', 'disabled', 'no'].includes(v))
		return false;
	throw new Error(
		`expected true|false (got "${value}"). Use on/off, 1/0, enable/disable.`
	);
}

export function registerFeatureFlags(parent: Command): void {
	const ff = parent
		.command('feature-flags')
		.description(
			'Toggle protocol-wide feature bits on State.featureBitFlags. Enabling is cold-admin only; disabling accepts the FeatureFlag hot key (cold/warm/hot).'
		);

	withGlobalOptions(
		ff
			.command('builder-codes <enable>')
			.description(
				'Enable/disable builder codes (bit 4). Enabling requires the cold admin. <enable> = true|false|on|off|1|0.'
			)
	).action(async (enable: string, _flags, cmd: Command) => {
		const on = parseEnable(enable);
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateFeatureBitFlagsBuilderCodesIx(on);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin feature-flags builder-codes'
			);
			reportDispatch(`builder codes = ${on ? 'enabled' : 'disabled'}`, result);
		} finally {
			await client.unsubscribe();
		}
	});
}
