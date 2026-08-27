import { PublicKey } from '@solana/web3.js';
import * as multisigSdk from '@sqds/multisig';
import { Command } from 'commander';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildProvider, loadKeypair } from '../lib/provider';
import { detectCluster } from '../lib/context';
import { fetchStateAdmins } from '../lib/state';

/**
 * `whoami` answers the question every authority bug starts with: which keys
 * does my signer actually hold on this cluster? It decodes the live State,
 * matches the signer against the cold/warm/pause admins and every hot role,
 * and, when a multisig is configured, reports membership and whether that
 * squad's vault is itself a State admin.
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
		// Same env resolution as buildAdminClient: a declared env must match
		// the chain; the fallback default adopts the detected cluster.
		const env =
			!opts.envExplicit && (cluster === 'mainnet-beta' || cluster === 'devnet')
				? cluster
				: opts.env;

		console.log(`signer:   ${pc.bold(signer.toBase58())}`);
		console.log(
			`cluster:  ${cluster}${
				opts.envExplicit && cluster !== 'unknown' && cluster !== opts.env
					? pc.yellow(` (WARNING: env says ${opts.env})`)
					: ''
			}`
		);
		if (opts.profile) {
			console.log(`profile:  ${opts.profile}`);
		}
		const lamports = await provider.connection.getBalance(signer);
		console.log(`balance:  ${(lamports / 1e9).toFixed(4)} SOL`);
		console.log();

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
		if (held.length > 0) {
			console.log(pc.bold('state roles held by this signer:'));
			for (const r of held) {
				console.log(`  ${pc.green('✓')} ${r}`);
			}
		} else {
			console.log('this signer holds no State roles on this cluster.');
		}
		console.log(
			pc.dim(
				`  (cold ${admins.coldAdmin.toBase58()} · warm ${admins.warmAdmin.toBase58()})`
			)
		);

		if (opts.multisig) {
			console.log();
			const multisigPda = new PublicKey(opts.multisig);
			try {
				const ms = await multisigSdk.accounts.Multisig.fromAccountAddress(
					provider.connection,
					multisigPda
				);
				const member = ms.members.find((m) => m.key.equals(signer));
				const [vault] = multisigSdk.getVaultPda({ multisigPda, index: 0 });
				console.log(pc.bold(`multisig ${multisigPda.toBase58()}:`));
				console.log(
					`  ${member ? pc.green('✓ member') : pc.yellow('✗ not a member')}` +
						` · threshold ${ms.threshold} · timelock ${Number(ms.timeLock)}s`
				);
				const vaultRoles: string[] = [];
				if (vault.equals(admins.coldAdmin)) {
					vaultRoles.push('cold admin');
				}
				if (vault.equals(admins.warmAdmin)) {
					vaultRoles.push('warm admin');
				}
				console.log(
					`  vault 0 ${vault.toBase58()}` +
						(vaultRoles.length
							? ` = State ${vaultRoles.join(' + ')}`
							: ' (not a State admin)')
				);
			} catch {
				console.log(
					pc.yellow(
						`multisig ${opts.multisig}: no Squads account on this cluster`
					)
				);
			}
		}
	});
}
