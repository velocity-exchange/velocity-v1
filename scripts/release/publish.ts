/**
 * `release npm` and `release docker` — push the tags that trigger
 * npm-publish.yml (`npm-<pkg>-v<version>`) and velocity-publish.yml
 * (`docker-<app>-v<version>`), then watch the runs they start.
 *
 * npm versions come from packages/<pkg>/package.json at origin/master (set by
 * the changesets "Version Packages" PR); docker versions come from the tag
 * history (the apps' package.json versions are not what the images are named by).
 */

import {
	DOCKER_WORKFLOW,
	DockerInfo,
	NPM_WORKFLOW,
	dockerDefs,
	dockerInfo,
	packages,
	versionPackagesPr,
} from './inventory';
import {
	Run,
	flags,
	ghRepo,
	isSemver,
	mutate,
	originMasterSha,
	pushTags,
	run,
	runs,
	sleep,
	tagExists,
} from './sh';
import {
	bold,
	detail,
	die,
	dim,
	done,
	header,
	kv,
	line,
	note,
	ok,
	pickMany,
	plan,
	shortSha,
	spin,
	step,
	table,
	warn,
} from './ui';

function dryRunHint(): void {
	if (!flags.execute) note('dry run: add --execute to do it');
}

type PublishOpts = {
	/** stay attached and watch the publish runs (default) */
	watch: boolean;
};

function actionsUrl(workflow: string): string {
	return `https://github.com/${ghRepo()}/actions/workflows/${workflow}`;
}

export async function npm(args: string[], opts: PublishOpts): Promise<void> {
	const sha = originMasterSha();
	const all = packages();
	const known = all.map((p) => p.dir);
	for (const a of args) {
		if (!known.includes(a)) {
			die(`unknown package "${a}" (known: ${known.join(', ')})`);
		}
	}
	const picked = args.length ? all.filter((p) => args.includes(p.dir)) : all;

	header('release npm', flags.execute ? '' : '(dry run)');
	kv('source', `packages/*/package.json @ origin/master ${shortSha(sha)}`);
	kv('workflow', NPM_WORKFLOW);

	const pr = versionPackagesPr();
	if (pr) {
		warn(
			`Version Packages PR #${pr.number} is open: merge it first if you want the versions it carries\n${pr.url}`
		);
	}

	const todo = picked.filter((p) => !p.tagged && !p.ciSkipped);
	table(
		picked.map((p) => [
			bold(p.dir),
			p.version,
			p.tagged
				? dim('already tagged')
				: p.ciSkipped
				? dim('skipped: npm-publish.yml ignores this package')
				: `→ ${p.tag}`,
		])
	);
	if (todo.length === 0) {
		done('nothing to publish');
		return;
	}

	plan(opts.watch ? 2 : 1);
	step(`tag ${todo.length} package(s) at ${shortSha(sha)} and push the tags`);
	const tags = todo.map((p) => p.tag);
	const pushed = mutate(
		`git tag <tag> ${shortSha(
			sha
		)} && git push origin <tag>, for each of ${tags.join(', ')}`,
		() => {
			for (const t of tags) {
				if (tagExists(t)) die(`${t} already exists locally`);
				run(`git tag ${t} ${sha}`);
			}
			pushTags(tags);
		}
	);

	if (!opts.watch) {
		note(`not watching: ${actionsUrl(NPM_WORKFLOW)}`);
	} else {
		step('watch the publish runs');
		if (pushed) {
			await watchTagRuns(NPM_WORKFLOW, tags);
		} else {
			note(
				`would wait for one ${NPM_WORKFLOW} run per tag and \`gh run watch\` each`
			);
		}
	}

	dryRunHint();
	done(
		'infra-v3 apps pin @velocity-exchange/sdk explicitly; bump there if they need this version'
	);
}

export async function docker(
	args: string[],
	opts: PublishOpts & { as?: string }
): Promise<void> {
	const sha = originMasterSha();
	const infos = dockerDefs().map(dockerInfo);
	const known = infos.map((d) => d.def.app);
	const changed = infos.filter((d) => d.commitsSince.length > 0 || !d.lastTag);

	let picked: DockerInfo[];
	if (args.length === 0) {
		const names = await pickMany(
			'which images? (preselected: changes since their last tag)',
			infos.map((d) => ({
				value: d.def.app,
				label: d.def.app,
				hint: d.lastTag
					? `v${d.lastVersion}, ${d.commitsSince.length} commit(s) since`
					: 'never tagged',
			})),
			changed.map((d) => d.def.app)
		);
		picked = infos.filter((d) => names.includes(d.def.app));
	} else if (args[0] === 'changed') {
		picked = changed;
	} else if (args[0] === 'all') {
		picked = infos;
	} else {
		for (const a of args) {
			if (!known.includes(a)) {
				die(`unknown app "${a}" (known: ${known.join(', ')}, all, changed)`);
			}
		}
		picked = infos.filter((d) => args.includes(d.def.app));
	}
	if (opts.as) {
		if (!isSemver(opts.as)) die(`--as must be X.Y.Z, got ${opts.as}`);
		if (picked.length !== 1) die('--as only applies to a single app');
		picked[0].nextVersion = opts.as;
	}

	header('release docker', flags.execute ? '' : '(dry run)');
	kv('source', `${shortSha(sha)} (origin/master)`);
	kv('workflow', DOCKER_WORKFLOW);

	if (picked.length === 0) {
		done(
			'no image has changes since its last tag; name apps explicitly or use `all`'
		);
		return;
	}

	table(
		picked.map((d) => [
			bold(d.def.app),
			d.lastTag
				? `v${d.lastVersion} @${shortSha(d.lastSha)}`
				: dim('never tagged'),
			`→ v${d.nextVersion}`,
			d.commitsSince.length
				? `${d.commitsSince.length} commit(s)`
				: dim('no changes'),
		])
	);
	for (const d of picked) {
		if (d.lastSha === sha) {
			warn(`${d.def.app}: last tag already points at ${shortSha(sha)}`);
		}
		for (const c of d.commitsSince.slice(0, 5)) {
			detail(`${dim(d.def.app)}  ${c.sha} ${c.subject}`);
		}
	}

	const tags = picked.map((d) => `docker-${d.def.app}-v${d.nextVersion}`);

	plan(opts.watch ? 2 : 1);
	step(`tag at ${shortSha(sha)} and push the tags`);
	const pushed = mutate(
		`git tag <tag> ${shortSha(
			sha
		)} && git push origin <tag>, for each of ${tags.join(', ')}`,
		() => {
			for (const t of tags) {
				if (tagExists(t)) die(`${t} already exists locally`);
				run(`git tag ${t} ${sha}`);
			}
			pushTags(tags);
		}
	);

	if (!opts.watch) {
		note(`not watching: ${actionsUrl(DOCKER_WORKFLOW)}`);
		note(
			'`release infra` checks ECR-published tags via infra-v3, so run it once the builds are green'
		);
	} else {
		step('watch the image builds');
		if (pushed) {
			await watchTagRuns(DOCKER_WORKFLOW, tags);
		} else {
			note(
				`would wait for one ${DOCKER_WORKFLOW} run per tag and \`gh run watch\` each`
			);
		}
	}

	dryRunHint();
	done('then `release infra <stage>` to pin the new versions in gitops');
}

async function watchTagRuns(workflow: string, tags: string[]): Promise<void> {
	const found = await spin(
		`waiting for ${tags.length} ${workflow} run(s)`,
		async () => {
			const hits = new Map<string, Run>();
			for (let i = 0; i < 40 && hits.size < tags.length; i++) {
				for (const r of runs(workflow, 30, true)) {
					if (tags.includes(r.headBranch)) hits.set(r.headBranch, r);
				}
				if (hits.size < tags.length) await sleep(3000);
			}
			return hits;
		}
	);
	for (const t of tags) {
		const r = found.get(t);
		if (!r) {
			warn(`no run appeared for ${t}; check the Actions tab`);
			continue;
		}
		detail(`${t} → run ${bold(String(r.databaseId))} ${dim(r.url)}`);
		run(`gh run watch ${r.databaseId} --exit-status`);
		ok(`${t} published`);
	}
	line();
}
