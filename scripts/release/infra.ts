/**
 * `release infra <stage...>` — hand the images this repo just published to
 * infrastructure-v3's `yarn deploy`, which rewrites the gitops pins on a
 * deploy branch and opens the PR. Needs an infra checkout (`--infra <path>`
 * or VELOCITY_INFRA_DIR); the infra script itself needs a clean tree there
 * and `aws sso login --sso-session velocity`.
 */

import fs from 'fs';
import path from 'path';
import {
	Stage,
	dockerDefs,
	dockerInfo,
	infraDir,
	infraStages,
	pinnedVersions,
} from './inventory';
import { flags, mutate, run, semverCmp, shSoft, unlockSigningKey } from './sh';
import {
	bold,
	die,
	dim,
	done,
	green,
	header,
	kv,
	note,
	pickMany,
	plan,
	step,
	table,
	warn,
	yellow,
} from './ui';

export async function infra(
	args: string[],
	opts: { images?: string }
): Promise<void> {
	const root = infraDir();
	if (!root) {
		die('no infra checkout: pass --infra <path> or set VELOCITY_INFRA_DIR');
	}
	if (!fs.existsSync(path.join(root, 'scripts/deploy.js'))) {
		die(`${root} does not look like infrastructure-v3 (no scripts/deploy.js)`);
	}
	const stages = infraStages();
	if (stages.length === 0) die(`no gitops/*/workloads/* stages under ${root}`);
	const stageNames = stages.map((s) => s.stage);

	let wanted: string[];
	if (args.length) {
		for (const s of args) {
			if (!stageNames.includes(s)) {
				die(`unknown stage "${s}" (known: ${stageNames.join(', ')})`);
			}
		}
		wanted = args;
	} else {
		wanted = await pickMany(
			'which stages?',
			stages.map((s) => ({ value: s.stage, label: s.stage, hint: s.env })),
			['master']
		);
	}
	const picked = stages.filter((s) => wanted.includes(s.stage));

	const infos = dockerDefs().map(dockerInfo);
	const only = opts.images ? opts.images.split(',').filter(Boolean) : null;
	if (only) {
		for (const i of only) {
			if (!infos.some((d) => d.def.app === i)) die(`unknown image "${i}"`);
		}
	}

	header('release infra', flags.execute ? '' : '(dry run)');
	kv(
		'infra',
		`${root} ${dim(shSoft('git rev-parse --abbrev-ref HEAD', root))}`
	);
	kv('stages', picked.map((s) => `${s.stage} (${s.env})`).join(', '));

	// per image: latest published tag vs what each stage pins
	const todo: { app: string; stages: string[] }[] = [];
	const rows: string[][] = [];
	for (const d of infos) {
		if (only && !only.includes(d.def.app)) continue;
		if (!d.lastVersion) continue;
		const behind: string[] = [];
		const cells = picked.map((s) =>
			pinCell(s, d.def.app, d.lastVersion as string, behind)
		);
		rows.push([bold(d.def.app), `latest v${d.lastVersion}`, ...cells]);
		if (behind.length) todo.push({ app: d.def.app, stages: behind });
	}
	table(rows);

	if (todo.length === 0) {
		done('every stage already pins the latest published version');
		return;
	}

	const dirty = shSoft('git status --porcelain', root);
	if (flags.execute && dirty) {
		die(`infra working tree is dirty (${root}); yarn deploy needs it clean`);
	}
	if (dirty)
		warn(
			'infra working tree is dirty; yarn deploy will refuse until it is clean'
		);
	note(
		'yarn deploy resolves digests from ECR: run `aws sso login --sso-session velocity` first'
	);

	// One `yarn deploy a,b,c <stages>` per distinct stage set, so images that
	// are behind in the same stages land in one deploy branch / one PR.
	const groups = new Map<string, string[]>();
	for (const t of todo) {
		const key = t.stages.join(' ');
		groups.set(key, [...(groups.get(key) ?? []), t.app]);
	}

	plan(groups.size);
	let first = true;
	for (const [stageList, apps] of groups) {
		step(`pin ${apps.join(', ')} in ${stageList.replace(/ /g, ', ')}`);
		const command = `yarn deploy ${apps.join(',')} ${stageList}`;
		mutate(`(cd ${root} && ${command})`, () => {
			// yarn deploy commits the pins; get the passphrase prompt out of the
			// way before it scrolls 20 lines of rewritten manifests.
			if (first) unlockSigningKey(root);
			first = false;
			run(command, root);
		});
	}

	if (!flags.execute) note('dry run: add --execute to do it');
	done(
		'each yarn deploy opens one deploy/<...> PR; merge it and ArgoCD rolls the stage' +
			(picked.some((s) => s.env === 'prod')
				? `\n${yellow(
						'prod pins only take effect after the infra master → mainnet-beta release PR'
				  )}`
				: '')
	);
}

function pinCell(
	stage: Stage,
	app: string,
	latest: string,
	behind: string[]
): string {
	const pins = pinnedVersions(stage, app);
	if (pins.length === 0) return dim(`${stage.stage}: not used`);
	const stale = pins.some((v) => semverCmp(v, latest) < 0);
	if (stale) behind.push(stage.stage);
	return `${stage.stage}: ${(stale ? yellow : green)(pins.join('/'))}`;
}
