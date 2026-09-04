/**
 * Read-only view of where every release artifact stands: program crates vs
 * their `program-<name>-v*` tags and deploy runs, npm packages vs
 * `npm-<pkg>-v*` tags, docker apps vs `docker-<app>-v*` tags, and (with an
 * infra checkout) the gitops pins per stage. Sources of truth are the files
 * CI itself reads: Cargo.toml, the package.json under packages/, docker-info.json,
 * the workflow files' tag conventions.
 */

import fs from 'fs';
import path from 'path';
import {
	Commit,
	Run,
	commitsBetween,
	countBehind,
	diffTouches,
	fileAt,
	flags,
	ghJson,
	latestTag,
	repoRoot,
	runs,
	semverCmp,
	shSoft,
	tagDate,
	tagSha,
	versionOfTag,
} from './sh';

export const MASTER = 'origin/master';

export type ProgramDef = {
	/** lib name, as used in tags and workflow inputs */
	name: string;
	/** cargo package name (jit-proxy's crate is hyphenated) */
	crate: string;
	path: string;
	/** root package.json script that regenerates its IDL */
	idlScript: string;
	/** regenerated rust IDL (velocity-rs build.rs) */
	rustIdl: boolean;
	/** has a mainnet tag flow (token_faucet is devnet-only) */
	mainnet: boolean;
};

export const PROGRAMS: ProgramDef[] = [
	{
		name: 'velocity',
		crate: 'velocity',
		path: 'programs/velocity',
		idlScript: 'program:idl',
		rustIdl: true,
		mainnet: true,
	},
	{
		name: 'jit_proxy',
		crate: 'jit-proxy',
		path: 'programs/jit-proxy',
		idlScript: 'program:idl:jit-proxy',
		rustIdl: false,
		mainnet: true,
	},
	{
		name: 'token_faucet',
		crate: 'token_faucet',
		path: 'programs/token_faucet',
		idlScript: '',
		rustIdl: false,
		mainnet: false,
	},
];

export function programDef(name: string): ProgramDef {
	const p = PROGRAMS.find((x) => x.name === name);
	if (!p) {
		throw new Error(
			`unknown program "${name}" (known: ${PROGRAMS.map((x) => x.name).join(
				', '
			)})`
		);
	}
	return p;
}

export const MAINNET_WORKFLOW = 'release-program.yaml';
export const DEVNET_WORKFLOW = 'manual-devnet-deploy.yaml';
export const NPM_WORKFLOW = 'npm-publish.yml';
export const DOCKER_WORKFLOW = 'velocity-publish.yml';

export function programTagPrefix(name: string): string {
	return `program-${name}-`;
}

export type ProgramInfo = {
	def: ProgramDef;
	cargoVersion: string;
	lastTag?: string;
	lastTagVersion?: string;
	lastTagSha?: string;
	lastTagDate?: string;
	/** commits under the program path since the last tag */
	commitsSinceTag: Commit[];
	mainnetRun?: Run;
	devnetRun?: Run;
	devnetBehind?: number;
	/** cargo version is not ahead of the last tag: bump before tagging */
	needsBump: boolean;
	warnings: string[];
};

export function cargoVersion(def: ProgramDef, ref = MASTER): string {
	const toml = fileAt(ref, `${def.path}/Cargo.toml`);
	const m = /^version\s*=\s*"([^"]+)"/m.exec(toml);
	return m ? m[1] : '?';
}

export function programInfo(def: ProgramDef, withRuns = true): ProgramInfo {
	const version = cargoVersion(def);
	const lastTag = def.mainnet
		? latestTag(programTagPrefix(def.name))
		: undefined;
	const lastTagVersion = lastTag ? versionOfTag(lastTag) : undefined;
	const lastTagSha = lastTag ? tagSha(lastTag) : undefined;
	const commitsSinceTag = lastTagSha
		? commitsBetween(lastTagSha, MASTER, [def.path])
		: [];

	let mainnetRun: Run | undefined;
	let devnetRun: Run | undefined;
	if (withRuns) {
		if (lastTag) {
			mainnetRun = runs(MAINNET_WORKFLOW, 30).find(
				(r) => r.headBranch === lastTag
			);
		}
		// Dispatch runs carry no program input in the listing; the newest
		// run is shown as "devnet last touched" with its sha.
		devnetRun = runs(DEVNET_WORKFLOW, 5)[0];
	}

	// only meaningful when there is something new to ship
	const needsBump = Boolean(
		lastTagVersion &&
			commitsSinceTag.length > 0 &&
			semverCmp(version, lastTagVersion) <= 0
	);

	const warnings: string[] = [];
	if (def.name === 'velocity' && lastTagSha) {
		warnings.push(...velocityDomainChecks(lastTagSha));
	}

	return {
		def,
		cargoVersion: version,
		lastTag,
		lastTagVersion,
		lastTagSha,
		lastTagDate: lastTag ? tagDate(lastTag) : undefined,
		commitsSinceTag,
		mainnetRun,
		devnetRun,
		devnetBehind: devnetRun
			? countBehind(devnetRun.headSha, MASTER)
			: undefined,
		needsBump,
		warnings,
	};
}

/**
 * Things a velocity upgrade needs staged alongside it. Each check is a diff
 * grep between the last deployed tag and master, so it fires only when the
 * pending upgrade actually carries the change.
 */
function velocityDomainChecks(fromSha: string): string[] {
	const out: string[] = [];
	const src = 'programs/velocity/src';

	if (
		diffTouches(
			fromSha,
			MASTER,
			[
				`${src}/state/state.rs`,
				`${src}/math/fees.rs`,
				`${src}/math/constants.rs`,
			],
			/perps_default|determine_perp_fee_tier|PERP_FEE_TIER_MAX_INDEX|VOLUME_THRESHOLDS/
		)
	) {
		out.push(
			'fee schedule code changed since the last tag: run `velocity-admin fees set-schedule` BEFORE the upgrade (State keeps the old tiers; see deploy-scripts/README.md)'
		);
	}

	if (diffTouches(fromSha, MASTER, [`${src}/error.rs`], /./)) {
		out.push(
			'error enum changed since the last tag: confirm variants were only appended (ABI-stable codes)'
		);
	}

	if (
		diffTouches(
			fromSha,
			MASTER,
			['packages/sdk/src/idl/velocity.json'],
			/"(accounts|instructions|types|events)"|"name":/
		)
	) {
		out.push(
			'IDL changed since the last tag: SDK consumers (bots, dlob-server, infra apps) need the new @velocity-exchange/sdk'
		);
	}

	return out;
}

/** Packages the npm-publish workflow skips (its `if:` guard). */
export const NPM_CI_SKIPPED = ['vaults-sdk', 'cli-admin'];

export type PackageInfo = {
	dir: string;
	name: string;
	version: string;
	tag: string;
	tagged: boolean;
	ciSkipped: boolean;
	lastTag?: string;
};

export function packages(ref = MASTER): PackageInfo[] {
	const dirs = shSoft(`git ls-tree --name-only ${ref} packages/`)
		.split('\n')
		.filter(Boolean)
		.map((p) => path.basename(p));
	const out: PackageInfo[] = [];
	for (const dir of dirs) {
		const raw = fileAt(ref, `packages/${dir}/package.json`);
		if (!raw) continue;
		const pkg = JSON.parse(raw);
		if (pkg.private) continue;
		const tag = `npm-${dir}-v${pkg.version}`;
		out.push({
			dir,
			name: pkg.name,
			version: pkg.version,
			tag,
			tagged: Boolean(tagSha(tag)),
			ciSkipped: NPM_CI_SKIPPED.includes(dir),
			lastTag: latestTag(`npm-${dir}-`),
		});
	}
	return out;
}

export type PullRequest = {
	number: number;
	title: string;
	url: string;
	headRefOid: string;
};

/** The open changesets "Version Packages" PR, if any. */
export function versionPackagesPr(): PullRequest | undefined {
	const prs = ghJson<PullRequest[]>(
		'pr list --head changeset-release/master --state open --json number,title,url,headRefOid'
	);
	return prs?.[0];
}

export function pendingChangesets(): string[] {
	const dir = path.join(repoRoot, '.changeset');
	if (!fs.existsSync(dir)) return [];
	return fs
		.readdirSync(dir)
		.filter((f) => f.endsWith('.md') && f !== 'README.md');
}

export type DockerDef = {
	app: string;
	lang: 'ts' | 'rust';
	path: string;
	ecr: string;
};

export function dockerDefs(): DockerDef[] {
	const info = JSON.parse(
		fs.readFileSync(path.join(repoRoot, 'docker-info.json'), 'utf8')
	);
	return Object.entries(info)
		.filter(([k]) => !k.startsWith('_'))
		.map(([app, v]) => {
			const d = v as { lang: 'ts' | 'rust'; path: string; ecr: string };
			return { app, lang: d.lang, path: d.path, ecr: d.ecr };
		});
}

export type DockerInfo = {
	def: DockerDef;
	lastTag?: string;
	lastVersion?: string;
	lastSha?: string;
	lastDate?: string;
	/** commits since the tag under the app path or the libs it bundles */
	commitsSince: Commit[];
	nextVersion: string;
};

/** Paths whose changes end up inside the image. */
export function dockerWatchPaths(def: DockerDef): string[] {
	return def.lang === 'ts'
		? [
				def.path,
				'packages/sdk',
				'packages/jit-proxy',
				'docker/ts-app.Dockerfile',
		  ]
		: [
				def.path,
				'rust/velocity-rs',
				'packages/sdk/src/idl',
				'docker/rust-app.Dockerfile',
		  ];
}

export function dockerInfo(def: DockerDef): DockerInfo {
	const lastTag = latestTag(`docker-${def.app}-`);
	const lastVersion = lastTag ? versionOfTag(lastTag) : undefined;
	const lastSha = lastTag ? tagSha(lastTag) : undefined;
	const [a, b, c] = lastVersion
		? lastVersion.split('.').map(Number)
		: [0, 1, -1];
	return {
		def,
		lastTag,
		lastVersion,
		lastSha,
		lastDate: lastTag ? tagDate(lastTag) : undefined,
		commitsSince: lastSha
			? commitsBetween(lastSha, MASTER, dockerWatchPaths(def))
			: [],
		nextVersion: `${a}.${b}.${c + 1}`,
	};
}

export type Stage = { stage: string; env: string; dir: string };

export function infraDir(): string {
	return flags.infraDir ? path.resolve(flags.infraDir) : '';
}

export function infraStages(): Stage[] {
	const root = infraDir();
	if (!root) return [];
	const gitops = path.join(root, 'gitops');
	if (!fs.existsSync(gitops)) return [];
	const out: Stage[] = [];
	for (const env of fs.readdirSync(gitops)) {
		const w = path.join(gitops, env, 'workloads');
		if (!fs.existsSync(w)) continue;
		for (const stage of fs.readdirSync(w)) {
			const dir = path.join(w, stage);
			if (fs.statSync(dir).isDirectory()) out.push({ stage, env, dir });
		}
	}
	// non-prod first, then prod; stable by name inside an env
	return out.sort(
		(a, b) => a.env.localeCompare(b.env) || a.stage.localeCompare(b.stage)
	);
}

function yamlFiles(dir: string): string[] {
	return fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
		const full = path.join(dir, e.name);
		if (e.isDirectory()) return yamlFiles(full);
		return /\.ya?ml$/.test(e.name) ? [full] : [];
	});
}

/** Distinct `<image>:vX.Y.Z` versions pinned for one image in one stage. */
export function pinnedVersions(stage: Stage, image: string): string[] {
	const re = new RegExp(`/${image}:v(\\d+\\.\\d+\\.\\d+)(?=[@\\s"']|$)`, 'g');
	const found = new Set<string>();
	for (const f of yamlFiles(stage.dir)) {
		const text = fs.readFileSync(f, 'utf8');
		for (const m of text.matchAll(re)) found.add(m[1]);
	}
	return [...found].sort(semverCmp);
}
