/**
 * Terminal layout shared by the read-heavy commands, where the output is
 * something an operator reads and reasons about rather than pipes.
 *
 * The shape mirrors `deploy-scripts/verify-buffer.sh`: a `▌` section marker
 * with an optional right-aligned verdict, then indented `label   value` rows
 * under it. Colour comes from picocolors, which disables itself when stdout is
 * not a TTY or NO_COLOR is set, so redirected output stays plain.
 */

import pc from 'picocolors';

/**
 * Renders a string that came off the chain. Every control character becomes
 * U+FFFD, so a proposal's simulated logs cannot forge terminal output.
 */
export function safe(text: string): string {
	// eslint-disable-next-line no-control-regex
	return text.replace(/[\u0000-\u001f\u007f-\u009f]/g, '\uFFFD');
}

/** Printable width, ignoring ANSI escapes. */
export function width(s: string): number {
	// eslint-disable-next-line no-control-regex
	return s.replace(/\x1b\[[0-9;]*m/g, '').length;
}

function terminalWidth(): number {
	const cols = process.stdout.columns ?? 0;
	return Math.min(Math.max(cols || 80, 60), 100);
}

/**
 * Section marker. `verdict` is pushed to the right edge, where a reader looks
 * for a pass or a fail.
 */
export function header(title: string, verdict?: string): void {
	const left = `${pc.cyan('▌')} ${pc.bold(title)}`;
	if (!verdict) {
		console.log(`\n${left}`);
		return;
	}
	const pad = Math.max(1, terminalWidth() - width(left) - width(verdict));
	console.log(`\n${left}${' '.repeat(pad)}${verdict}`);
}

/** `label   value` row, label dim and padded to a common column. */
export function kv(label: string, value: string, indent = '   '): void {
	console.log(`${indent}${pc.dim(label.padEnd(13))} ${value}`);
}

/** Free line at body indent. */
export function line(text = '', indent = '   '): void {
	console.log(text ? `${indent}${text}` : '');
}

export function note(text: string, indent = '   '): void {
	console.log(`${indent}${pc.dim(text)}`);
}

export function ok(text: string): string {
	return `${pc.green('✓')} ${text}`;
}

export function bad(text: string): string {
	return `${pc.red('✗')} ${text}`;
}

export function warn(text: string): string {
	return `${pc.yellow('!')} ${text}`;
}

/** `before → after`, with the new value carrying the emphasis. */
export function change(from: string, to: string): string {
	return `${pc.dim(from)} ${pc.dim('→')} ${pc.bold(to)}`;
}

/** Thousands separators, for counts a human compares by magnitude. */
export function count(n: number | bigint): string {
	return n.toLocaleString('en-US');
}

/** `abcd…wxyz`, for addresses shown as identity rather than for copying. */
export function shortKey(key: string): string {
	return key.length <= 12 ? key : `${key.slice(0, 4)}…${key.slice(-4)}`;
}

/**
 * Rows aligned into columns, each cell padded to the widest in its column.
 * The last cell is never padded, so trailing colour never adds whitespace.
 */
export function table(rows: string[][], indent = '   '): void {
	const widths: number[] = [];
	for (const row of rows) {
		row.forEach((cell, i) => {
			widths[i] = Math.max(widths[i] ?? 0, width(cell));
		});
	}
	for (const row of rows) {
		const out = row
			.map((cell, i) =>
				i === row.length - 1 ? cell : cell + ' '.repeat(widths[i] - width(cell))
			)
			.join('  ');
		console.log(indent + out.replace(/\s+$/, ''));
	}
}
