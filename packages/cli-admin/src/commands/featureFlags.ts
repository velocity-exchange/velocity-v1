import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import { parseEnable } from '../lib/args';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';

export function registerFeatureFlags(parent: Command): void {
	const ff = parent
		.command('feature-flags')
		.description(
			'Toggle protocol-wide feature bits on State.featureBitFlags. Enabling is cold-admin only; disabling accepts the FeatureFlag hot key (cold/warm/hot).'
		);

	withGlobalOptions(
		ff
			.command('median-trigger-price <enable>')
			.description(
				'Enable/disable the median trigger price for trigger-order evaluation (bit 2). Enabling requires the cold admin. <enable> = true|false|on|off|1|0.'
			)
	).action(async (enable: string, _flags, cmd: Command) => {
		const on = parseEnable(enable);
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateFeatureBitFlagsMedianTriggerPriceIx(on);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin feature-flags median-trigger-price'
			);
			reportDispatch(
				`median trigger price = ${on ? 'enabled' : 'disabled'}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

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

	withGlobalOptions(
		ff
			.command('vamm-maker-rebate <enable>')
			.description(
				'Enable/disable the vAMM earning the maker rebate on fills it makes (bit 8). Enabling requires the cold admin. <enable> = true|false|on|off|1|0.'
			)
	).action(async (enable: string, _flags, cmd: Command) => {
		const on = parseEnable(enable);
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateFeatureBitFlagsVammMakerRebateIx(on);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin feature-flags vamm-maker-rebate'
			);
			reportDispatch(
				`vamm maker rebate = ${on ? 'enabled' : 'disabled'}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});
}
