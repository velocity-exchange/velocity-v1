/**
 * Shell, git and gh plumbing shared by the release commands, plus the global
 * flags (`--execute`, `--infra`, `--rpc`). Everything here is read-only unless
 * routed through `mutate()`, which is the single gate `--execute` opens.
 */

import { execSync, spawnSync } from 'child_process';
import path from 'path';
import { die, would, cmd as showCmd } from './ui';

export const repoRoot = path.resolve(__dirname, '../..');

export const flags = {
	execute: false,
	infraDir: process.env.VELOCITY_INFRA_DIR || '',
	rpc: '',
	verbose: false,
};

export function sh(command: string, cwd = repoRoot): string {
	return execSync(command, {
		cwd,
		stdio: ['ignore', 'pipe', 'pipe'],
		encoding: 'utf8',
		maxBuffer: 64 * 1024 * 1024,
	}).trim();
}

/** Like sh() but returns '' instead of throwing. */
export function shSoft(command: string, cwd = repoRoot): string {
	try {
		return sh(command, cwd);
	} catch {
		return '';
	}
}

/**
 * Streams the command's output; dies on a non-zero exit. `shown` replaces the
 * echoed command when the real one carries a secret (an RPC url with a key).
 */
export function run(command: string, cwd = repoRoot, shown = command): void {
	showCmd(shown);
	const r = spawnSync(command, { cwd, stdio: 'inherit', shell: true });
	if (r.status !== 0) die(`command failed (${r.status}): ${shown}`);
}

/**
 * The one mutation gate. Without --execute it prints what would happen and
 * returns false; with it, runs the job and returns true.
 */
export function mutate(description: string, job: () => void): boolean {
	if (!flags.execute) {
		would(description);
		return false;
	}
	job();
	return true;
}

export function ghJson<T>(args: string): T {
	const out = sh(`gh ${args}`);
	return JSON.parse(out || 'null') as T;
}

let cachedRepo = '';
export function ghRepo(): string {
	if (!cachedRepo) {
		cachedRepo =
			shSoft('gh repo view --json nameWithOwner -q .nameWithOwner') ||
			die('cannot resolve the GitHub repo (is gh authenticated?)');
	}
	return cachedRepo;
}

export type Run = {
	databaseId: number;
	headBranch: string;
	headSha: string;
	conclusion: string;
	status: string;
	createdAt: string;
	url: string;
	event: string;
};

const RUN_FIELDS =
	'databaseId,headBranch,headSha,conclusion,status,createdAt,url,event';

const runCache = new Map<string, Run[]>();

/**
 * Memoized per workflow+limit: status asks for the same listing several
 * times. Pollers that wait for a new run must pass `fresh` or they watch a
 * stale snapshot forever.
 */
export function runs(workflow: string, limit = 20, fresh = false): Run[] {
	const key = `${workflow}:${limit}`;
	let hit = fresh ? undefined : runCache.get(key);
	if (!hit) {
		hit =
			ghJson<Run[]>(
				`run list --workflow ${workflow} --limit ${limit} --json ${RUN_FIELDS}`
			) || [];
		runCache.set(key, hit);
	}
	return hit;
}

/**
 * One `git push` per tag. GitHub emits no push events at all when a single
 * push carries more than three tags, so a batched push silently skips every
 * tag-triggered workflow.
 */
export function pushTags(tags: string[]): void {
	for (const t of tags) run(`git push origin ${t}`);
}

export function fetchOrigin(): void {
	shSoft('git fetch origin master --tags --quiet');
}

export function originMasterSha(): string {
	return sh('git rev-parse origin/master');
}

export function fileAt(ref: string, file: string): string {
	return shSoft(`git show ${ref}:${file}`);
}

export function tagSha(tag: string): string {
	return shSoft(`git rev-list -n1 ${tag}`);
}

export function tagExists(tag: string): boolean {
	return Boolean(shSoft(`git tag --list '${tag}'`));
}

export function tagDate(tag: string): string {
	return shSoft(`git log -1 --format=%cI ${tag}`);
}

/** Tags matching `<prefix>v*`, newest version last. */
export function tagsWithPrefix(prefix: string): string[] {
	const raw = shSoft(`git tag --list '${prefix}v*'`);
	return raw
		.split('\n')
		.filter(Boolean)
		.filter((t) => /v\d+\.\d+\.\d+$/.test(t))
		.sort((a, b) => semverCmp(versionOfTag(a), versionOfTag(b)));
}

export function latestTag(prefix: string): string | undefined {
	const all = tagsWithPrefix(prefix);
	return all[all.length - 1];
}

/** Everything after the last `-v` (tag keys may contain `-v` themselves). */
export function versionOfTag(tag: string): string {
	return tag.slice(tag.lastIndexOf('-v') + 2);
}

export type Commit = { sha: string; subject: string };

export function commitsBetween(
	from: string,
	to: string,
	paths: string[] = []
): Commit[] {
	if (!from) return [];
	const scope = paths.length ? ` -- ${paths.join(' ')}` : '';
	const raw = shSoft(`git log --format='%h%x09%s' ${from}..${to}${scope}`);
	return raw
		.split('\n')
		.filter(Boolean)
		.map((l) => {
			const [sha, ...rest] = l.split('\t');
			return { sha, subject: rest.join('\t') };
		});
}

export function countBehind(sha: string, tip: string): number | undefined {
	const n = shSoft(`git rev-list --count ${sha}..${tip}`);
	return n === '' ? undefined : Number(n);
}

export function diffTouches(
	from: string,
	to: string,
	paths: string[],
	pattern: RegExp
): boolean {
	if (!from) return false;
	const diff = shSoft(`git diff ${from}..${to} -- ${paths.join(' ')}`);
	return diff.split('\n').some((l) => /^[+-][^+-]/.test(l) && pattern.test(l));
}

/**
 * Warm the gpg-agent passphrase cache before a signed commit, so pinentry
 * asks on its own step instead of after screens of other output, where a
 * missed prompt times out and leaves a half-done deploy behind. No-op when
 * commit.gpgsign is off for that checkout.
 */
export function unlockSigningKey(cwd = repoRoot): void {
	if (shSoft('git config --get commit.gpgsign', cwd) !== 'true') return;
	run(
		'echo | gpg --clearsign >/dev/null',
		cwd,
		'gpg --clearsign  (unlock your signing key; pinentry asks here)'
	);
}

/** Tracked changes only: untracked files never reach the explicit `git add`. */
export function workingTreeDirty(): boolean {
	return Boolean(shSoft('git status --porcelain --untracked-files=no'));
}

export function currentBranch(): string {
	return shSoft('git rev-parse --abbrev-ref HEAD');
}

export function parseSemver(v: string): [number, number, number] {
	const m = /^v?(\d+)\.(\d+)\.(\d+)/.exec(v);
	if (!m) return [0, 0, 0];
	return [Number(m[1]), Number(m[2]), Number(m[3])];
}

export function semverCmp(a: string, b: string): number {
	const pa = parseSemver(a);
	const pb = parseSemver(b);
	return pa[0] - pb[0] || pa[1] - pb[1] || pa[2] - pb[2];
}

export function bumpMinor(v: string): string {
	const [a, b] = parseSemver(v);
	return `${a}.${b + 1}.0`;
}

export function bumpPatch(v: string): string {
	const [a, b, c] = parseSemver(v);
	return `${a}.${b}.${c + 1}`;
}

export function isSemver(v: string): boolean {
	return /^\d+\.\d+\.\d+$/.test(v);
}

export function maxVersion(...vs: (string | undefined)[]): string {
	return vs
		.filter((v): v is string => Boolean(v))
		.sort(semverCmp)
		.pop() as string;
}

export function sleep(ms: number): Promise<void> {
	return new Promise((r) => setTimeout(r, ms));
}
