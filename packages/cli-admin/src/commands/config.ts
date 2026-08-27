import {
	cancel,
	confirm,
	intro,
	isCancel,
	outro,
	select,
	spinner,
	text,
} from '@clack/prompts';
import { Connection, PublicKey } from '@solana/web3.js';
import * as multisigSdk from '@sqds/multisig';
import { VelocityEnv } from '@velocity-exchange/sdk';
import { Command } from 'commander';
import * as fs from 'fs';
import * as os from 'os';
import pc from 'picocolors';
import { CliConfig, Profile, configPath, loadConfig, saveConfig } from '../lib/config';
import { detectCluster } from '../lib/context';
import { loadKeypair } from '../lib/provider';
import { fetchStateAdmins } from '../lib/state';

/**
 * Profile management: `config init` builds a profile interactively and
 * verifies every ingredient against the live cluster before writing anything
 * — the RPC by genesis hash, the keypair by loading it, the multisig by
 * deriving its vault and matching it against the on-chain State admins. A
 * profile that saves is a profile that works.
 *
 * Multisig addresses are kept in this per-user config only, on purpose —
 * they are derivable on-chain but are not written into the repo.
 */

function die(message: string): never {
	cancel(message);
	process.exit(1);
}

function unwrap<T>(value: T | symbol): T {
	if (isCancel(value)) {
		die('aborted, nothing saved');
	}
	return value as T;
}

export function registerConfig(parent: Command): void {
	const config = parent
		.command('config')
		.description(
			`Connection profiles (stored at ${configPath()}). Select one with -p/--profile, VELOCITY_ADMIN_PROFILE, or the configured default; explicit -u/-k/-e/-m flags always override the profile.`
		);

	config
		.command('init')
		.description(
			'Interactively create (or overwrite) a profile. Verifies the RPC by genesis hash, the keypair by loading it, and an optional multisig against the live State admins.'
		)
		.action(async () => {
			intro('velocity-admin config init');
			const cfg = loadConfig();

			const name = unwrap(
				await text({
					message: 'profile name',
					placeholder: 'devnet, mainnet-cold, mainnet-crank, …',
					validate: (v) =>
						/^[a-z0-9][a-z0-9-]*$/.test(v)
							? undefined
							: 'lowercase letters, digits, dashes',
				})
			);
			if (cfg.profiles[name]) {
				const overwrite = unwrap(
					await confirm({ message: `profile "${name}" exists — overwrite?` })
				);
				if (!overwrite) {
					die('aborted, nothing saved');
				}
			}

			const url = unwrap(
				await text({
					message: 'RPC URL',
					validate: (v) =>
						v.startsWith('http') ? undefined : 'must be an http(s) URL',
				})
			);
			const s = spinner();
			s.start('checking RPC genesis hash');
			const cluster = await detectCluster(new Connection(url, 'confirmed'));
			if (cluster === 'unknown') {
				s.stop(pc.yellow('✗ could not resolve cluster from RPC'));
				die('RPC unreachable or unknown genesis — fix the URL and retry');
			}
			s.stop(`RPC is ${pc.bold(cluster)} (genesis hash verified)`);
			if (cluster !== 'mainnet-beta' && cluster !== 'devnet') {
				die(`cluster ${cluster} is not a Velocity env`);
			}
			const env = cluster as VelocityEnv;

			const keypair = unwrap(
				await text({
					message: 'signer keypair path',
					placeholder: '~/…/keypairs/<key>.json',
					validate: (v) => {
						const expanded = v.startsWith('~')
							? v.replace(/^~/, os.homedir())
							: v;
						return fs.existsSync(expanded) ? undefined : 'file not found';
					},
				})
			);
			const signerPk = loadKeypair(keypair).publicKey;
			console.log(`  signer pubkey: ${pc.bold(signerPk.toBase58())}`);

			const mode = unwrap(
				await select({
					message: 'dispatch mode',
					options: [
						{
							value: 'direct',
							label: 'direct — sign and send with the keypair',
						},
						{
							value: 'multisig',
							label:
								'multisig — wrap every action in a Squads V4 proposal',
						},
					],
				})
			);

			let multisig: string | undefined;
			if (mode === 'multisig') {
				multisig = unwrap(
					await text({
						message: 'Squads V4 multisig PDA',
						validate: (v) => {
							try {
								new PublicKey(v);
								return undefined;
							} catch {
								return 'not a valid pubkey';
							}
						},
					})
				);
				s.start('verifying multisig against on-chain State admins');
				const connection = new Connection(url, 'confirmed');
				const multisigPda = new PublicKey(multisig);
				try {
					await multisigSdk.accounts.Multisig.fromAccountAddress(
						connection,
						multisigPda
					);
				} catch {
					s.stop(pc.red('✗ no Squads multisig at that address'));
					die('not a Squads V4 multisig on this cluster');
				}
				const [vault] = multisigSdk.getVaultPda({ multisigPda, index: 0 });
				const admins = await fetchStateAdmins(connection, env);
				const matches: string[] = [];
				if (vault.equals(admins.coldAdmin)) {
					matches.push('cold admin');
				}
				if (vault.equals(admins.warmAdmin)) {
					matches.push('warm admin');
				}
				if (matches.length > 0) {
					s.stop(
						`vault 0 = ${pc.bold(vault.toBase58())} = State ${matches.join(
							' + '
						)} on ${cluster}`
					);
				} else {
					s.stop(
						pc.yellow(
							`vault 0 (${vault.toBase58()}) is NOT a State admin on ${cluster}`
						)
					);
					const anyway = unwrap(
						await confirm({
							message:
								'this squad cannot pass admin checks (fine for e.g. an upgrade-authority or treasury squad) — save anyway?',
						})
					);
					if (!anyway) {
						die('aborted, nothing saved');
					}
				}
			}

			const profile: Profile = { url, keypair, env, multisig };
			const makeDefault =
				Object.keys(cfg.profiles).length === 0 ||
				unwrap(
					await confirm({
						message: `make "${name}" the default profile?`,
						initialValue: false,
					})
				);

			const next: CliConfig = {
				...cfg,
				profiles: { ...cfg.profiles, [name]: profile },
				default: makeDefault ? name : cfg.default,
			};
			saveConfig(next);
			outro(
				`saved "${name}" to ${configPath()}${
					makeDefault ? ' (default)' : ''
				}`
			);
		});

	config
		.command('list')
		.description('List configured profiles.')
		.action(() => {
			const cfg = loadConfig();
			const names = Object.keys(cfg.profiles);
			if (names.length === 0) {
				console.log(
					`no profiles configured (${configPath()}) — run \`velocity-admin config init\``
				);
				return;
			}
			for (const name of names) {
				const p = cfg.profiles[name];
				const mark = cfg.default === name ? pc.bold('* ') : '  ';
				const mode = p.multisig ? `proposal → ${p.multisig}` : 'direct';
				console.log(
					`${mark}${pc.bold(name.padEnd(16))} ${p.env.padEnd(12)} ${mode}`
				);
				console.log(pc.dim(`    rpc ${p.url.replace(/\?.*$/, '')}`));
				console.log(pc.dim(`    key ${p.keypair}`));
			}
		});

	config
		.command('set-default <name>')
		.description('Set the default profile used when none is selected.')
		.action((name: string) => {
			const cfg = loadConfig();
			if (!cfg.profiles[name]) {
				throw new Error(`unknown profile "${name}"`);
			}
			saveConfig({ ...cfg, default: name });
			console.log(`default profile: ${name}`);
		});

	config
		.command('remove <name>')
		.description('Delete a profile from the config.')
		.action((name: string) => {
			const cfg = loadConfig();
			if (!cfg.profiles[name]) {
				throw new Error(`unknown profile "${name}"`);
			}
			const profiles = { ...cfg.profiles };
			delete profiles[name];
			saveConfig({
				...cfg,
				profiles,
				default: cfg.default === name ? undefined : cfg.default,
			});
			console.log(`removed profile "${name}"`);
		});
}
