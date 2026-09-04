/**
 * Terminal output and prompts, on @clack/prompts. Every command opens with
 * `header`, walks numbered `step`s, and closes with `done`. Prompts only ever
 * choose *what* to act on (which program, apps, stages); whether anything is
 * mutated is decided by `--execute` alone, never by a prompt.
 *
 * Plain lines (`line`, `kv`, `detail`, `note`, `cmd`) are buffered and flushed
 * as one clack block by the next structural call, so consecutive lines sit
 * together instead of each getting its own gutter gap.
 *
 * `checklist` prints markdown to stdout and does not go through this module.
 */

import * as p from '@clack/prompts';

const useColor = !process.env.NO_COLOR && process.env.TERM !== 'dumb';

const paint = (code: number) => (s: string) =>
	useColor ? `\x1b[${code}m${s}\x1b[0m` : s;

export const bold = paint(1);
export const dim = paint(2);
export const red = paint(31);
export const green = paint(32);
export const yellow = paint(33);
export const cyan = paint(36);

const pending: string[] = [];

function flush(): void {
	if (pending.length === 0) return;
	p.log.message(pending.join('\n'));
	pending.length = 0;
}

function emit(s: string): void {
	pending.push(s);
}

export function header(title: string, sub = ''): void {
	flush();
	p.intro(`${bold(title)}${sub ? ' ' + dim(sub) : ''}`);
}

export function done(msg: string): void {
	flush();
	p.outro(msg);
}

export function section(title: string): void {
	flush();
	p.log.step(bold(title));
}

export function kv(key: string, value: string): void {
	emit(`${dim(key.padEnd(12))} ${value}`);
}

/** An empty `line()` ends the current block. */
export function line(s = ''): void {
	if (s === '') flush();
	else emit(s);
}

export function detail(s: string): void {
	emit(s);
}

export function note(s: string): void {
	emit(dim(s));
}

/** Printed immediately: the command's own output follows right after it. */
export function cmd(command: string): void {
	flush();
	p.log.message(`${dim('$')} ${command}`);
}

export function warn(s: string): void {
	flush();
	p.log.warn(s);
}

export function ok(s: string): void {
	flush();
	p.log.success(s);
}

export function die(msg: string): never {
	flush();
	p.cancel(msg);
	process.exit(1);
}

let stepNo = 0;
let stepsTotal = 0;

export function plan(total: number): void {
	stepNo = 0;
	stepsTotal = total;
}

export function step(title: string): void {
	flush();
	p.log.step(`${cyan(`[${stepNo + 1}/${stepsTotal}]`)} ${bold(title)}`);
	stepNo += 1;
}

/** Dry-run marker: what --execute would do at this point. */
export function would(command: string): void {
	flush();
	p.log.message(`${yellow('would run')} ${command}`);
}

/**
 * Left-aligned columns as one block. Cells may carry ANSI codes; width is
 * computed on the stripped text so colored cells still line up.
 */
export function table(rows: string[][]): void {
	flush();
	// eslint-disable-next-line no-control-regex
	const strip = (s: string) => s.replace(/\x1b\[[0-9;]*m/g, '');
	const widths: number[] = [];
	for (const row of rows) {
		row.forEach((cell, i) => {
			widths[i] = Math.max(widths[i] ?? 0, strip(cell).length);
		});
	}
	const lines = rows.map((row) =>
		row
			.map((cell, i) =>
				i === row.length - 1
					? cell
					: cell + ' '.repeat(widths[i] - strip(cell).length)
			)
			.join('  ')
			.trimEnd()
	);
	p.log.message(lines.join('\n'));
}

/** Run an async job behind a spinner; the spinner line stays as the result. */
export async function spin<T>(
	label: string,
	job: () => Promise<T>
): Promise<T> {
	flush();
	const s = p.spinner();
	s.start(label);
	try {
		const out = await job();
		s.stop(label);
		return out;
	} catch (e) {
		s.stop(label, 1);
		throw e;
	}
}

export type Choice<T> = { value: T; label: string; hint?: string };

const interactive = () => Boolean(process.stdin.isTTY && process.stdout.isTTY);

function cancelled(): never {
	p.cancel('cancelled');
	process.exit(130);
}

/**
 * Single choice. Returns `fallback` without prompting when stdin is not a
 * TTY, so scripted runs stay deterministic.
 */
export async function pick<T>(
	message: string,
	choices: Choice<T>[],
	fallback: T
): Promise<T> {
	if (!interactive()) return fallback;
	flush();
	const r = await p.select<T>({
		message,
		options: choices,
		initialValue: fallback,
	});
	if (p.isCancel(r)) cancelled();
	return r as T;
}

/** Multiple choice; `fallback` is both the non-TTY answer and the preselection. */
export async function pickMany<T>(
	message: string,
	choices: Choice<T>[],
	fallback: T[]
): Promise<T[]> {
	if (!interactive()) return fallback;
	flush();
	const r = await p.multiselect<T>({
		message,
		options: choices,
		initialValues: fallback,
		required: true,
	});
	if (p.isCancel(r)) cancelled();
	return r as T[];
}

export function shortSha(sha: string | undefined): string {
	return sha ? sha.slice(0, 7) : '?';
}

export function shortDate(iso: string | undefined): string {
	return iso ? iso.slice(5, 10) : '?';
}
