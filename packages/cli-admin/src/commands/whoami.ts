import { PublicKey } from '@solana/web3.js';
import * as multisigSdk from '@sqds/multisig';
import { Command } from 'commander';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildProvider, loadKeypair } from '../lib/provider';
import * as ui from '../lib/ui';
import { detectCluster } from '../lib/context';
import { fetchStateAdmins } from '../lib/state';

/**
 * `whoami` reports which keys the configured signer holds on a cluster. It
 * decodes the live State and matches the signer against the cold, warm and
 * pause admins and every hot role. When a multisig is configured, it also
 * reports membership and whether that squad's vault is itself a State admin.
 */
export function registerWhoami(parent: Command): void {
	withGlobalOptions(
		parent
			.command('whoami')
			.description(
				'Report which on-chain authorities the configured signer holds: State admin tiers, hot roles, and multisig membership.'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const signer = loadKeypair(opts.keypair).publicKey;
		const cluster = await detectCluster(provider.connection);
		// The same env resolution as buildAdminClient. A declared env must match
		// the chain. The fallback default adopts the detected cluster.
		const env =
			!opts.envExplicit && (cluster === 'mainnet-beta' || cluster === 'devnet')
				? cluster
				: opts.env;

		const lamports = await provider.connection.getBalance(signer);
		ui.header('whoami', opts.profile ? pc.dim(opts.profile) : undefined);
		ui.kv('signer', pc.bold(signer.toBase58()));
		ui.kv(
			'cluster',
			`${cluster}${
				opts.envExplicit && cluster !== 'unknown' && cluster !== opts.env
					? pc.yellow(`  env says ${opts.env}, they disagree`)
					: ''
			}`
		);
		ui.kv('balance', `${(lamports / 1e9).toFixed(4)} SOL`);

		const admins = await fetchStateAdmins(provider.connection, env);
		const held: string[] = [];
		const holds = (label: string, pk: PublicKey) => {
			if (pk.equals(signer)) {
				held.push(label);
			}
		};
		holds('cold admin', admins.coldAdmin);
		holds('warm admin', admins.warmAdmin);
		holds('pause admin', admins.pauseAdmin);
		for (const [role, pk] of Object.entries(admins.hotRoles)) {
			holds(role, pk);
		}
		ui.header(
			'State roles held',
			held.length > 0 ? ui.ok(`${held.length}`) : pc.dim('none')
		);
		if (held.length > 0) {
			for (const role of held) {
				ui.line(`${pc.green('✓')} ${role}`);
			}
		} else {
			ui.note('none');
		}
		ui.kv('cold admin', pc.dim(admins.coldAdmin.toBase58()));
		ui.kv('warm admin', pc.dim(admins.warmAdmin.toBase58()));

		if (opts.multisig) {
			const multisigPda = new PublicKey(opts.multisig);
			try {
				const ms = await multisigSdk.accounts.Multisig.fromAccountAddress(
					provider.connection,
					multisigPda
				);
				const member = ms.members.find((m) => m.key.equals(signer));
				const [vault] = multisigSdk.getVaultPda({ multisigPda, index: 0 });
				const vaultRoles: string[] = [];
				if (vault.equals(admins.coldAdmin)) {
					vaultRoles.push('cold admin');
				}
				if (vault.equals(admins.warmAdmin)) {
					vaultRoles.push('warm admin');
				}
				ui.header(
					'multisig',
					member ? ui.ok('you are a member') : ui.warn('you are not a member')
				);
				ui.kv('address', pc.dim(multisigPda.toBase58()));
				ui.kv(
					'policy',
					pc.dim(
						`threshold ${ms.threshold} of ${
							ms.members.length
						}, timelock ${Number(ms.timeLock)}s`
					)
				);
				ui.kv(
					'vault 0',
					`${pc.dim(vault.toBase58())}  ${
						vaultRoles.length
							? pc.green(`State ${vaultRoles.join(' + ')}`)
							: pc.dim('not a State admin')
					}`
				);
			} catch {
				ui.header('multisig', ui.bad('not found'));
				ui.kv('address', pc.dim(opts.multisig));
				ui.note('no Squads account here');
			}
		}
		console.log('');
	});
}
