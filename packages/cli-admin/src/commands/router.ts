import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import {
	fetchRouterConfig,
	getAssociatedTokenAddress,
	getDistributeIx,
	getInitializeIx,
	getRouterConfigPda,
	getUpdateConfigIx,
	MAINNET_USDT_MINT,
	Tier,
} from '@velocity-exchange/revenue-router-sdk';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildProvider } from '../lib/provider';
import * as ui from '../lib/ui';
import {
	reportDispatch,
	resolveAdminAuthority,
	sendOrPropose,
} from '../lib/squads';

const TIERS_HELP =
	'marginal ladder as threshold:poolBps pairs, thresholds in USDT base units, e.g. 0:6000,30000000000:7000,100000000000:9000';

/** `0:6000,30000000000:7000` -> tiers. The program validates the ladder itself
 *  (first threshold 0, strictly increasing, bps <= 10000); this only parses. */
function parseTiers(value: string): Tier[] {
	return value.split(',').map((pair) => {
		const match = /^(\d+):(\d+)$/.exec(pair.trim());
		if (!match) {
			throw new Error(
				`tier must be "<threshold>:<poolBps>", got "${pair}" (${TIERS_HELP})`
			);
		}
		return { threshold: new BN(match[1]), poolBps: Number(match[2]) };
	});
}

function usdt(baseUnits: BN | bigint | number): string {
	return `${(Number(baseUnits.toString()) / 1_000_000).toLocaleString('en-US', {
		maximumFractionDigits: 6,
	})} USDT`;
}

/**
 * Protocol revenue router: the program `protocol_fee_recipient_perp` points at.
 * The fee collector bot withdraws perp fees into its ATA and cranks
 * `distribute`, which splits them between the DFX recovery pool and the
 * treasury on a daily marginal ladder.
 */
export function registerRouter(parent: Command): void {
	const router = parent
		.command('router')
		.description(
			'Protocol revenue router: splits withdrawn perp fees between the DFX recovery pool and the treasury.'
		);

	withGlobalOptions(
		router
			.command('show')
			.description(
				'RouterConfig: authorities, tier ladder, period and lifetime counters, router ATA balance.'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const configPda = getRouterConfigPda();
		const config = await fetchRouterConfig(provider.connection);
		if (config === null) {
			throw new Error(
				`RouterConfig ${configPda.toBase58()} is not initialized`
			);
		}

		const routerAta = getAssociatedTokenAddress(config.usdtMint, configPda);
		let ataBalance = pc.dim(
			'not created yet (Velocity creates it on first withdrawal)'
		);
		try {
			const balance = await provider.connection.getTokenAccountBalance(
				routerAta
			);
			ataBalance = usdt(BigInt(balance.value.amount));
		} catch {
			// no account yet
		}

		ui.header('router config', opts.profile ? pc.dim(opts.profile) : undefined);
		ui.kv('config', configPda.toBase58());
		ui.kv('admin', config.admin.toBase58());
		ui.kv('cranker', config.cranker.toBase58());
		ui.kv('treasury', config.treasury.toBase58());
		ui.kv('usdt mint', config.usdtMint.toBase58());
		ui.kv('router ata', `${routerAta.toBase58()} ${pc.dim('=')} ${ataBalance}`);
		ui.line();
		ui.kv(
			'period day',
			`${config.periodDay.toString()} ${pc.dim(
				`(${new Date(config.periodDay.toNumber() * 86_400_000)
					.toISOString()
					.slice(0, 10)} UTC)`
			)}`
		);
		ui.kv('period fees', usdt(config.periodFees));
		ui.kv('lifetime fees', usdt(config.lifetimeFees));
		ui.kv('lifetime to pool', usdt(config.lifetimeToPool));
		ui.kv('lifetime to treasury', usdt(config.lifetimeToTreasury));
		ui.line();
		ui.line(pc.bold('tiers'));
		ui.table(
			config.tiers
				.slice(0, config.tierCount)
				.map((tier, i) => [
					`#${i}`,
					`from ${usdt(tier.threshold)}`,
					`${tier.poolBps / 100}% pool / ${
						(10_000 - tier.poolBps) / 100
					}% treasury`,
				])
		);
	});

	withGlobalOptions(
		router
			.command('initialize <admin> <cranker> <treasury>')
			.requiredOption('--tiers <ladder>', TIERS_HELP)
			.option(
				'--usdt-mint <pubkey>',
				'USDT mint; must equal the redemption config mint',
				MAINNET_USDT_MINT.toBase58()
			)
			.description(
				'Create the RouterConfig singleton (one-time). On mainnet the signer must be the init authority baked into the program, so this is a direct send, never a multisig proposal.'
			)
	).action(
		async (
			admin: string,
			cranker: string,
			treasury: string,
			flags: { tiers: string; usdtMint: string },
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			if (opts.multisig) {
				throw new Error(
					'router initialize is signed by the init authority key directly; drop --multisig'
				);
			}
			const provider = buildProvider(opts);
			const tiers = parseTiers(flags.tiers);
			const ix = await getInitializeIx({
				connection: provider.connection,
				payer: provider.wallet.publicKey,
				admin: new PublicKey(admin),
				cranker: new PublicKey(cranker),
				treasury: new PublicKey(treasury),
				usdtMint: new PublicKey(flags.usdtMint),
				tiers,
			});
			const result = await sendOrPropose(
				provider,
				[ix],
				undefined,
				'velocity-admin router initialize'
			);
			reportDispatch(
				`RouterConfig ${getRouterConfigPda().toBase58()} created: admin ${admin}, cranker ${cranker}, treasury ${treasury}, ${
					tiers.length
				} tier(s)`,
				result
			);
		}
	);

	withGlobalOptions(
		router
			.command('update-config')
			.option('--admin <pubkey>', 'new admin (takes effect immediately)')
			.option(
				'--cranker <pubkey>',
				'new cranker (the fee collector bot wallet)'
			)
			.option(
				'--treasury <pubkey>',
				'new treasury wallet; its USDT ATA receives the treasury share'
			)
			.option('--tiers <ladder>', TIERS_HELP)
			.description(
				'Change any of admin, cranker, treasury or the tier ladder (router admin). Omitted fields keep their value. Tiers are locked for the rest of a UTC day that already distributed.'
			)
	).action(
		async (
			flags: {
				admin?: string;
				cranker?: string;
				treasury?: string;
				tiers?: string;
			},
			cmd: Command
		) => {
			if (!flags.admin && !flags.cranker && !flags.treasury && !flags.tiers) {
				throw new Error(
					'nothing to update: pass at least one of --admin, --cranker, --treasury, --tiers'
				);
			}
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const multisigPda = opts.multisig
				? new PublicKey(opts.multisig)
				: undefined;
			const ix = await getUpdateConfigIx({
				connection: provider.connection,
				admin: resolveAdminAuthority(provider, multisigPda),
				newAdmin: flags.admin ? new PublicKey(flags.admin) : undefined,
				newCranker: flags.cranker ? new PublicKey(flags.cranker) : undefined,
				newTreasury: flags.treasury ? new PublicKey(flags.treasury) : undefined,
				tiers: flags.tiers ? parseTiers(flags.tiers) : undefined,
			});
			const result = await sendOrPropose(
				provider,
				[ix],
				multisigPda,
				'velocity-admin router update-config'
			);
			const changes = Object.entries(flags)
				.filter(([, v]) => v !== undefined)
				.map(([k, v]) => `${k} = ${v}`)
				.join(', ');
			reportDispatch(`RouterConfig updated: ${changes}`, result);
		}
	);

	withGlobalOptions(
		router
			.command('distribute')
			.description(
				'Split the router ATA balance between the recovery pool and the treasury (cranker key). The fee collector bot does this daily; use it to catch up after a missed run.'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const multisigPda = opts.multisig
			? new PublicKey(opts.multisig)
			: undefined;
		// payer defaults to cranker: a Squads vault transaction can only sign as the vault.
		const ix = await getDistributeIx({
			connection: provider.connection,
			cranker: resolveAdminAuthority(provider, multisigPda),
		});
		const result = await sendOrPropose(
			provider,
			[ix],
			multisigPda,
			'velocity-admin router distribute'
		);
		reportDispatch('router distribute', result);
	});
}
