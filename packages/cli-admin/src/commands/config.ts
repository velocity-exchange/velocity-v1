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
import {
	CliConfig,
	Profile,
	configPath,
	loadConfig,
	saveConfig,
} from '../lib/config';
import { detectCluster } from '../lib/context';
import { loadKeypair } from '../lib/provider';
import { fetchStateAdmins } from '../lib/state';

/**
 * Profile management. `config init` verifies the RPC (genesis hash), keypair, and multisig
 * (vault vs onchain State admins) before writing. Multisig addresses live only in this per-user config.
 */

function die(message: string): never {
	cancel(message);
	process.exit(1);
}

function unwrap<T>(value: T | symbol): T {
	if (isCancel(value)) {
		die('aborted, nothing saved');
	}

	// Pasted prompt input often carries stray whitespace. A trailing space in an
	// RPC URL makes the connection fail with an unclear error.
	return (typeof value === 'string' ? value.trim() : value) as T;
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
					await confirm({ message: `profile "${name}" exists, overwrite?` })
				);
				if (!overwrite) {
					die('aborted, nothing saved');
				}
			}

			const env = unwrap(
				await select({
					message: 'cluster',
					options: [
						{ value: 'devnet' as VelocityEnv, label: 'devnet' },
						{ value: 'mainnet-beta' as VelocityEnv, label: 'mainnet-beta' },
					],
				})
			);

			// One RPC per cluster serves every profile on that cluster. A profile
			// carries its own url only when it must differ.
			const s = spinner();
			const shared = cfg.rpcs?.[env];
			let url: string;
			let profileUrl: string | undefined;
			let saveShared = false;
			const useShared =
				shared !== undefined &&
				unwrap(
					await confirm({
						message: `use the shared ${env} RPC (${shared.replace(
							/\?.*$/,
							''
						)})?`,
					})
				);
			if (useShared && shared) {
				url = shared;
			} else {
				url = unwrap(
					await text({
						message: `RPC URL for ${env}`,
						validate: (v) =>
							v.startsWith('http') ? undefined : 'must be an http(s) URL',
					})
				);
				saveShared = unwrap(
					await confirm({
						message: `save as the shared ${env} RPC (used by every profile without its own url)?`,
					})
				);
				if (!saveShared) {
					profileUrl = url;
				}
			}
			s.start('checking RPC genesis hash');
			const cluster = await detectCluster(new Connection(url, 'confirmed'));
			if (cluster === 'unknown') {
				// Re-fetch without the silent catch, so the command can report the
				// failure.
				let reason = 'unrecognized genesis hash';
				try {
					reason = `unrecognized genesis hash ${await new Connection(
						url,
						'confirmed'
					).getGenesisHash()}`;
				} catch (e) {
					reason = (e as Error).message;
				}
				s.stop(pc.yellow('✗ could not resolve cluster from RPC'));
				die(`RPC check failed: ${reason}`);
			}
			if (cluster !== env) {
				s.stop(pc.red(`✗ RPC is ${cluster}, not ${env}`));
				die('RPC and cluster disagree, nothing saved');
			}
			s.stop(`RPC is ${pc.bold(cluster)} (genesis hash verified)`);

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
							label: 'direct: sign and send with the keypair',
						},
						{
							value: 'multisig',
							label: 'multisig: wrap every action in a Squads V4 proposal',
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
								'this squad cannot pass admin checks (fine for e.g. an upgrade-authority or treasury squad), save anyway?',
						})
					);
					if (!anyway) {
						die('aborted, nothing saved');
					}
				}
			}

			const profile: Profile = { url: profileUrl, keypair, env, multisig };
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
				rpcs: saveShared ? { ...cfg.rpcs, [env]: url } : cfg.rpcs,
				profiles: { ...cfg.profiles, [name]: profile },
				default: makeDefault ? name : cfg.default,
			};
			saveConfig(next);
			outro(
				`saved "${name}" to ${configPath()}${makeDefault ? ' (default)' : ''}`
			);
		});

	config
		.command('list')
		.description('List configured profiles and shared RPCs.')
		.action(() => {
			const cfg = loadConfig();
			const names = Object.keys(cfg.profiles);
			if (names.length === 0) {
				console.log(
					`no profiles configured (${configPath()}); run \`velocity-admin config init\``
				);
				return;
			}
			for (const [env, url] of Object.entries(cfg.rpcs ?? {})) {
				console.log(
					pc.dim(`shared rpc ${env.padEnd(12)} ${url.replace(/\?.*$/, '')}`)
				);
			}
			for (const name of names) {
				const p = cfg.profiles[name];
				const mark = cfg.default === name ? pc.bold('* ') : '  ';
				const mode = p.multisig ? `proposal → ${p.multisig}` : 'direct';
				console.log(
					`${mark}${pc.bold(name.padEnd(16))} ${p.env.padEnd(12)} ${mode}`
				);
				if (p.url) {
					console.log(pc.dim(`    rpc ${p.url.replace(/\?.*$/, '')} (own)`));
				}
				console.log(pc.dim(`    key ${p.keypair}`));
			}
		});

	config
		.command('set-rpc <env> <url>')
		.description(
			'Set the shared RPC for a cluster (devnet or mainnet-beta), used by every profile without its own url. Verifies the URL by genesis hash.'
		)
		.action(async (env: string, url: string) => {
			if (env !== 'mainnet-beta' && env !== 'devnet') {
				throw new Error(
					`unknown env "${env}" (expected mainnet-beta or devnet)`
				);
			}
			const cluster = await detectCluster(new Connection(url, 'confirmed'));
			if (cluster !== env) {
				throw new Error(
					`RPC genesis hash says ${cluster}, not ${env}; nothing saved`
				);
			}
			const cfg = loadConfig();
			saveConfig({ ...cfg, rpcs: { ...cfg.rpcs, [env]: url } });
			console.log(`shared ${env} RPC set (genesis hash verified)`);
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
