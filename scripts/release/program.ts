/**
 * `release program bump|devnet|mainnet` — the program upgrade flow.
 *
 * bump     version bump PR branch (Cargo.toml, both lockfiles, regenerated
 *          IDLs), committed with your git identity and pushed
 * devnet   dispatch manual-devnet-deploy.yaml, watch it, verify the buffer
 * mainnet  push the program-<name>-v<version> tag, watch release-program.yaml,
 *          verify the buffer
 *
 * Nothing here approves or executes a Squads proposal; the flow stops at the
 * verified buffer and tells the signer what to open.
 */

import fs from 'fs';
import os from 'os';
import path from 'path';
import {
	DEVNET_WORKFLOW,
	MAINNET_WORKFLOW,
	MASTER,
	PROGRAMS,
	ProgramDef,
	cargoVersion,
	programDef,
	programInfo,
	programTagPrefix,
} from './inventory';
import {
	Run,
	bumpMinor,
	currentBranch,
	flags,
	ghRepo,
	isSemver,
	maxVersion,
	mutate,
	originMasterSha,
	repoRoot,
	run,
	runs,
	semverCmp,
	sh,
	sleep,
	tagExists,
	unlockSigningKey,
	workingTreeDirty,
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
	pick,
	plan,
	shortSha,
	spin,
	step,
	warn,
} from './ui';

type DeployOpts = {
	branch?: string;
	/** stay attached: wait for the run, then verify (default) */
	watch: boolean;
	verify: boolean;
	skipBuild?: boolean;
};

function actionsUrl(workflow: string): string {
	return `https://github.com/${ghRepo()}/actions/workflows/${workflow}`;
}

/** Steps after the trigger: watch, and verify only when watching. */
function followUpSteps(opts: DeployOpts): number {
	return opts.watch ? 1 + (opts.verify ? 1 : 0) : 0;
}

async function chooseProgram(
	given: string | undefined,
	mainnetOnly: boolean
): Promise<ProgramDef> {
	if (given) return programDef(given);
	const candidates = PROGRAMS.filter((p) => !mainnetOnly || p.mainnet);
	const name = await pick(
		'which program?',
		candidates.map((p) => ({
			value: p.name,
			label: p.name,
			hint: `Cargo ${cargoVersion(p)}`,
		})),
		'velocity'
	);
	return programDef(name);
}

function preflightWarnings(def: ProgramDef): void {
	const info = programInfo(def, false);
	for (const w of info.warnings) warn(w);
	if (info.commitsSinceTag.length === 0 && info.lastTag) {
		warn(`no commits under ${def.path} since ${info.lastTag}`);
	}
}

function dryRunHint(): void {
	if (!flags.execute) note('dry run: add --execute to do it');
}

export async function bump(
	progArg: string | undefined,
	versionArg: string | undefined
): Promise<void> {
	const def = await chooseProgram(progArg, true);
	const info = programInfo(def, false);
	const target =
		versionArg || bumpMinor(maxVersion(info.cargoVersion, info.lastTagVersion));
	if (!isSemver(target)) die(`not a version: ${target}`);
	if (semverCmp(target, info.cargoVersion) <= 0) {
		die(`${target} is not ahead of Cargo ${info.cargoVersion}`);
	}
	if (info.lastTagVersion && semverCmp(target, info.lastTagVersion) <= 0) {
		die(`${target} is not ahead of the last tag v${info.lastTagVersion}`);
	}

	const branch = `release/${def.name}-${target}`;
	const cargoToml = `${def.path}/Cargo.toml`;
	const files = [cargoToml, 'Cargo.lock'];
	if (def.rustIdl) files.push('rust/Cargo.lock');
	if (def.idlScript) files.push(...idlOutputs(def));
	if (def.rustIdl) files.push('rust/velocity-rs/crates/src/velocity_idl.rs');

	header(`program bump ${def.name}`, flags.execute ? '' : '(dry run)');
	kv('current', `Cargo ${info.cargoVersion}, tag ${info.lastTag ?? 'none'}`);
	kv('target', target);
	kv('branch', `${branch} from ${MASTER}`);
	kv('files', files.join('\n' + ' '.repeat(13)));

	plan(def.rustIdl ? 6 : 5);

	step('checkout a fresh branch');
	if (flags.execute && workingTreeDirty()) {
		die('working tree is dirty; commit or stash first');
	}
	mutate(`git switch -c ${branch} ${MASTER}`, () => {
		run(`git switch -c ${branch} ${MASTER}`);
	});

	step(`set version = "${target}" in ${cargoToml}`);
	mutate(`edit ${cargoToml}`, () => {
		const p = path.join(repoRoot, cargoToml);
		const src = fs.readFileSync(p, 'utf8');
		const next = src.replace(
			/^version\s*=\s*"[^"]+"/m,
			`version = "${target}"`
		);
		if (next === src) die(`no version line found in ${cargoToml}`);
		fs.writeFileSync(p, next);
	});

	step('resync the program workspace lockfile (offline)');
	// rust/Cargo.lock cannot be resolved offline (solana-sdk 3.x tree); the
	// cargo check below refreshes it as a side effect.
	mutate(`cargo update -p ${def.crate} --offline`, () => {
		run(`cargo update -p ${def.crate} --offline`);
	});

	step('regenerate the IDL (carries the version)');
	if (def.idlScript) {
		mutate(`bun run ${def.idlScript}`, () => {
			// anchor idl build writes -o target/idl/... and does not create the dir
			fs.mkdirSync(path.join(repoRoot, 'target/idl'), { recursive: true });
			run(`bun run ${def.idlScript}`);
		});
	} else {
		note('no IDL script for this program');
	}

	if (def.rustIdl) {
		step('regenerate velocity_idl.rs + rust/Cargo.lock (velocity-rs build.rs)');
		mutate('cargo check --manifest-path rust/Cargo.toml -p velocity-rs', () => {
			run('cargo check --manifest-path rust/Cargo.toml -p velocity-rs');
		});
	}

	// git commit inherits the terminal, so a signing key's pinentry prompt
	// shows up here like it would for a hand-typed commit.
	step('commit and push the branch');
	const message = `bump ${def.name} to ${target}`;
	mutate(`git add <files> && git commit -m "${message}"`, () => {
		unlockSigningKey();
		run(`git add ${files.join(' ')}`);
		const status = sh('git status --porcelain --untracked-files=no');
		const unstaged = status
			.split('\n')
			.filter((l) => l && !l.startsWith('M ') && !l.startsWith('A '));
		if (unstaged.length) warn(`unstaged leftovers:\n${unstaged.join('\n')}`);
		run(`git commit -m "${message}"`);
	});
	mutate(`git push -u origin ${branch}`, () => {
		run(`git push -u origin ${branch}`);
	});

	line(
		`open the PR: ${bold(
			`https://github.com/${ghRepo()}/compare/master...${branch}?expand=1`
		)}`
	);
	if (flags.execute) note(`you are now on ${currentBranch()}`);
	dryRunHint();
	done(
		'merge the PR, then `release program devnet` / `release program mainnet`'
	);
}

function idlOutputs(def: ProgramDef): string[] {
	switch (def.name) {
		case 'velocity':
			return [
				'packages/sdk/src/idl/velocity.json',
				'packages/sdk/src/idl/velocity.ts',
			];
		case 'jit_proxy':
			return [
				'packages/jit-proxy/src/idl/jit_proxy.json',
				'packages/jit-proxy/src/types/jit_proxy.ts',
			];
		default:
			return [];
	}
}

export async function devnet(
	progArg: string | undefined,
	opts: DeployOpts
): Promise<void> {
	const def = await chooseProgram(progArg, false);
	const ref = opts.branch || 'master';
	const sha = sh(`git rev-parse origin/${ref}`);

	header(`program devnet ${def.name}`, flags.execute ? '' : '(dry run)');
	kv('workflow', DEVNET_WORKFLOW);
	kv('branch', `${ref} @${shortSha(sha)}`);
	kv('version', `Cargo ${cargoVersion(def, `origin/${ref}`)}`);
	preflightWarnings(def);

	plan(1 + followUpSteps(opts));

	step('dispatch the workflow');
	const dispatch = `gh workflow run ${DEVNET_WORKFLOW} --ref master -f program=${def.name} -f branch=${ref}`;
	const since = Date.now();
	const dispatched = mutate(dispatch, () => run(dispatch));

	if (!opts.watch) {
		note(`not watching: ${actionsUrl(DEVNET_WORKFLOW)}`);
		note(
			`verify later with: bash deploy-scripts/verify-buffer.sh ${def.name} --devnet`
		);
	} else {
		step('watch the run');
		let picked: Run | undefined;
		if (dispatched) {
			picked = await waitForRun(
				DEVNET_WORKFLOW,
				(r) =>
					r.event === 'workflow_dispatch' &&
					new Date(r.createdAt).getTime() >= since - 15_000
			);
			watchRun(picked);
		} else {
			note(
				`would wait for the new ${DEVNET_WORKFLOW} run and \`gh run watch\` it`
			);
		}

		if (opts.verify) {
			step('verify the buffer from source');
			verifyBuffer(def, picked?.url, true, opts);
		}
	}

	dryRunHint();
	done(
		'next: approve + execute the proposal in the devnet Squads' +
			(def.name === 'velocity'
				? ', then `velocity-admin -e devnet show state` and a smoke trade'
				: '')
	);
}

export async function mainnet(
	progArg: string | undefined,
	opts: DeployOpts
): Promise<void> {
	const def = await chooseProgram(progArg, true);
	const info = programInfo(def, false);
	const sha = originMasterSha();
	const tag = `${programTagPrefix(def.name)}v${info.cargoVersion}`;

	header(`program mainnet ${def.name}`, flags.execute ? '' : '(dry run)');
	kv('workflow', MAINNET_WORKFLOW);
	kv('tag', `${tag} → ${MASTER} @${shortSha(sha)}`);
	kv('last tag', info.lastTag ?? 'none');
	preflightWarnings(def);

	if (info.needsBump) {
		die(
			`Cargo ${info.cargoVersion} is not ahead of ${info.lastTag}; land \`release program bump ${def.name}\` first`
		);
	}
	if (tagExists(tag)) die(`${tag} already exists locally`);

	plan(1 + followUpSteps(opts));

	step('tag origin/master and push the tag');
	const tagged = mutate(
		`git tag ${tag} ${sha} && git push origin ${tag}`,
		() => {
			run(`git tag ${tag} ${sha}`);
			run(`git push origin ${tag}`);
		}
	);

	if (!opts.watch) {
		note(`not watching: ${actionsUrl(MAINNET_WORKFLOW)}`);
		note(`verify later with: bash deploy-scripts/verify-buffer.sh ${def.name}`);
	} else {
		step('watch the release run');
		let picked: Run | undefined;
		if (tagged) {
			picked = await waitForRun(MAINNET_WORKFLOW, (r) => r.headBranch === tag);
			watchRun(picked);
		} else {
			note(
				`would wait for the ${MAINNET_WORKFLOW} run for ${tag} and \`gh run watch\` it`
			);
		}

		if (opts.verify) {
			step('verify the buffer from source');
			verifyBuffer(def, picked?.url, false, opts);
		}
	}

	dryRunHint();
	done('next: approve + execute the proposal in the mainnet Squads');
}

async function waitForRun(
	workflow: string,
	match: (r: Run) => boolean
): Promise<Run> {
	const found = await spin(`waiting for the ${workflow} run`, async () => {
		for (let i = 0; i < 40; i++) {
			const hit = runs(workflow, 10, true).find(match);
			if (hit) return hit;
			await sleep(3000);
		}
		return undefined;
	});
	if (!found) {
		die(`no ${workflow} run appeared within 2 minutes; check the Actions tab`);
	}
	detail(`run ${bold(String(found.databaseId))} ${dim(found.url)}`);
	return found;
}

function watchRun(r: Run): void {
	run(`gh run watch ${r.databaseId} --exit-status`);
	ok(`run ${r.databaseId} succeeded`);
}

function verifyBuffer(
	def: ProgramDef,
	runUrl: string | undefined,
	devnetFlavor: boolean,
	opts: DeployOpts
): void {
	const parts = ['bash deploy-scripts/verify-buffer.sh', def.name];
	if (runUrl) parts.push(runUrl);
	if (devnetFlavor) parts.push('--devnet');
	// verify-buffer falls back to the solana CLI config, which usually points
	// at mainnet: a devnet buffer is then "not found" and the script compares
	// against the mainnet program instead. Always name the cluster.
	const { url, source } = resolveRpc(devnetFlavor ? 'devnet' : 'mainnet-beta');
	parts.push('--rpc', url);
	if (opts.skipBuild) parts.push('--skip-build');
	if (flags.verbose) parts.push('--verbose');
	const shown = parts.map((p) => (p === url ? `<${source}>` : p)).join(' ');
	note(`rpc: ${new URL(url).host} (${source})`);
	if (!flags.execute) {
		note(`would run ${shown}`);
		note(
			'(needs solana-verify + docker; --no-verify skips, --skip-build reuses target/deploy/*.so)'
		);
		return;
	}
	// run() echoes the command; keep the URL (api key) out of the transcript.
	run(parts.join(' '), repoRoot, shown);
}

/**
 * RPC for on-chain reads: `--rpc`, else the cluster's shared `rpcs` entry in
 * the admin CLI config (~/.config/velocity-admin/config.json or
 * $VELOCITY_ADMIN_CONFIG), else the public endpoint.
 */
function resolveRpc(cluster: 'devnet' | 'mainnet-beta'): {
	url: string;
	source: string;
} {
	if (flags.rpc) return { url: flags.rpc, source: '--rpc' };
	const file =
		process.env.VELOCITY_ADMIN_CONFIG ||
		path.join(os.homedir(), '.config', 'velocity-admin', 'config.json');
	try {
		const cfg = JSON.parse(fs.readFileSync(file, 'utf8'));
		const url = cfg?.rpcs?.[cluster];
		if (typeof url === 'string' && url) {
			return { url, source: `admin-cli config rpcs.${cluster}` };
		}
	} catch {
		// no config, or unreadable: fall through to the public endpoint
	}
	return {
		url:
			cluster === 'devnet'
				? 'https://api.devnet.solana.com'
				: 'https://api.mainnet-beta.solana.com',
		source: 'public endpoint',
	};
}
