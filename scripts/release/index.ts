/**
 * `bun run release <command>` — the velocity-v1 release CLI.
 *
 * Read-only by default: every command prints what it found and what
 * `--execute` would do. Nothing commits, tags, pushes, dispatches or shells
 * into the infra repo without `--execute`, and Squads approval is never
 * automated.
 *
 * Commands are defined here with commander; the work lives in status.ts
 * (status, checklist), program.ts (bump, devnet, mainnet), publish.ts (npm,
 * docker) and infra.ts (infra). inventory.ts reads the repo, sh.ts wraps git
 * and gh, ui.ts wraps @clack/prompts.
 */

import { Argument, Command } from 'commander';
import { infra } from './infra';
import { PROGRAMS } from './inventory';
import { bump, devnet, mainnet } from './program';
import { docker, npm } from './publish';
import { fetchOrigin, flags } from './sh';
import { checklist, status } from './status';
import { die } from './ui';

const programNames = PROGRAMS.map((p) => p.name);
const mainnetPrograms = PROGRAMS.filter((p) => p.mainnet).map((p) => p.name);

const cli = new Command('release')
	.description(
		'Release velocity-v1: program upgrades, npm packages, docker images, gitops pins.\n' +
			'Read-only by default; add --execute to act. Never commits or touches Squads.'
	)
	.option('--execute', 'actually do it (default: dry run, read-only)')
	.option(
		'--infra <path>',
		'infrastructure-v3 checkout (default: $VELOCITY_INFRA_DIR)',
		process.env.VELOCITY_INFRA_DIR
	)
	.option('--rpc <url>', 'RPC passed to verify-buffer.sh')
	.option('--no-fetch', 'skip the initial `git fetch origin master --tags`')
	.option('--verbose', "stream verify-buffer's build output")
	.showHelpAfterError()
	.hook('preAction', (root) => {
		const o = root.opts();
		flags.execute = Boolean(o.execute);
		flags.verbose = Boolean(o.verbose);
		flags.infraDir = o.infra || '';
		flags.rpc = o.rpc || '';
		if (o.fetch) fetchOrigin();
	});

cli
	.command('status')
	.description(
		'where programs, npm packages, docker images (and gitops pins with --infra) stand'
	)
	.action(status);

cli
	.command('checklist')
	.description(
		'the release runbook, filled in with current versions, as markdown on stdout'
	)
	.action(checklist);

const prog = cli
	.command('program')
	.description('program upgrades: bump → devnet → mainnet');

prog
	.command('bump')
	.description(
		'branch off origin/master, bump Cargo.toml + lockfiles + IDLs, commit (your key), push, print the PR link'
	)
	.addArgument(
		new Argument('[prog]', 'program (prompted if omitted)').choices(
			mainnetPrograms
		)
	)
	.argument('[version]', 'X.Y.Z (default: next minor)')
	.action(bump);

prog
	.command('devnet')
	.description(
		'dispatch manual-devnet-deploy.yaml, watch the run, verify the buffer with --devnet'
	)
	.addArgument(
		new Argument('[prog]', 'program (prompted if omitted)').choices(
			programNames
		)
	)
	.option('--branch <ref>', 'source branch for the build', 'master')
	.option('--no-watch', 'dispatch and exit; watch/verify later yourself')
	.option('--no-verify', 'skip verify-buffer.sh')
	.option('--skip-build', 'verify-buffer reuses target/deploy/<prog>.so')
	.action(devnet);

prog
	.command('mainnet')
	.description(
		'push program-<prog>-v<Cargo version> at origin/master, watch release-program.yaml, verify the buffer'
	)
	.addArgument(
		new Argument('[prog]', 'program (prompted if omitted)').choices(
			mainnetPrograms
		)
	)
	.option('--no-watch', 'push the tag and exit; watch/verify later yourself')
	.option('--no-verify', 'skip verify-buffer.sh')
	.option('--skip-build', 'verify-buffer reuses target/deploy/<prog>.so')
	.action(mainnet);

cli
	.command('npm')
	.description(
		'tag every untagged packages/<pkg> version at origin/master, push, watch npm-publish'
	)
	.argument('[pkg...]', 'package dirs (default: all untagged)')
	.option('--no-watch', 'push the tags and exit')
	.action(npm);

cli
	.command('docker')
	.description(
		'tag docker-<app>-v<next patch> at origin/master, push, watch the image builds'
	)
	.argument(
		'[app...]',
		'docker-info.json apps, or "all" / "changed" (default: prompt, changed)'
	)
	.option('--as <X.Y.Z>', 'explicit version (single app only)')
	.option('--no-watch', 'push the tags and exit')
	.action(docker);

cli
	.command('infra')
	.description(
		'run infrastructure-v3 `yarn deploy <app> <stage>` for every image whose gitops pin is behind'
	)
	.argument('[stage...]', 'gitops stages (default: prompt, master)')
	.option('--images <a,b>', 'restrict to these images')
	.action(infra);

cli.parseAsync(process.argv).catch((e) => {
	die(e instanceof Error ? e.message : String(e));
});
