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
 * Agent mode (`--agent`). The same commands rendered for a program: no box
 * drawing, no alignment padding, no colour, no truncation.
 *
 * Alignment is what matters most. Padding a column to its widest cell makes a
 * table readable and makes it ambiguous to parse, since a value cannot be told
 * apart from the whitespace holding it in place. Agent mode emits
 * tab-separated rows and `key=value` pairs instead.
 *
 * `shortKey` also stops truncating. `2Jsi…B8pW` is enough for a human who
 * already knows the address and useless to anything that has to look it up.
 */

let AGENT = false;
let consolePatched = false;

// eslint-disable-next-line no-control-regex
const ANSI = /\x1b\[[0-9;]*m/g;
const plain = (s: string): string => String(s).replace(ANSI, '');

export function setAgentMode(value: boolean): void {
	AGENT = value;
	// Not every command renders through these helpers; several build lines with
	// `console.log` and picocolors directly. picocolors resolves colour support
	// at import, too late to switch off by env, so strip at the sink instead.
	// One wrapper keeps agent output plain whatever produced it.
	if (value && !consolePatched) {
		consolePatched = true;
		const original = console.log.bind(console);
		console.log = (...args: unknown[]) =>
			original(...args.map((a) => (typeof a === 'string' ? plain(a) : a)));
	}
}

export function isAgentMode(): boolean {
	return AGENT;
}

/**
 * Render a string that came off the chain: program logs, decoded instruction
 * arguments, decoded account fields. Every control character becomes U+FFFD,
 * so an escape sequence embedded in a `msg!` or a borsh string cannot move the
 * cursor, erase lines, or repaint the screen with forged output.
 *
 * This matters because a proposal's inner instructions are simulated during
 * review, before approval, so a proposer can put any program they like in a
 * proposal and have its logs reach the reviewer's terminal. Substituting
 * rather than dropping keeps tampering visible instead of silent.
 *
 * Apply it to the untrusted text, then wrap the result in our own colour.
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
 * Section marker. `verdict` is pushed to the right edge, which is where a
 * reader's eye goes for pass/fail.
 */
export function header(title: string, verdict?: string): void {
	if (AGENT) {
		console.log(
			`\n# ${plain(title)}${verdict ? ` \u00b7 ${plain(verdict)}` : ''}`
		);
		return;
	}
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
	if (AGENT) {
		console.log(`${plain(label).trim().replace(/\s+/g, '_')}=${plain(value)}`);
		return;
	}
	console.log(`${indent}${pc.dim(label.padEnd(13))} ${value}`);
}

/** Free line at body indent. */
export function line(text = '', indent = '   '): void {
	if (AGENT) {
		console.log(plain(text));
		return;
	}
	console.log(text ? `${indent}${text}` : '');
}

export function note(text: string, indent = '   '): void {
	if (AGENT) {
		console.log(`# ${plain(text)}`);
		return;
	}
	console.log(`${indent}${pc.dim(text)}`);
}

export function ok(text: string): string {
	return AGENT ? `ok ${plain(text)}` : `${pc.green('✓')} ${text}`;
}

export function bad(text: string): string {
	return AGENT ? `fail ${plain(text)}` : `${pc.red('✗')} ${text}`;
}

export function warn(text: string): string {
	return AGENT ? `warn ${plain(text)}` : `${pc.yellow('!')} ${text}`;
}

/** `before → after`, with the new value carrying the emphasis. */
export function change(from: string, to: string): string {
	return AGENT
		? `${plain(from)} -> ${plain(to)}`
		: `${pc.dim(from)} ${pc.dim('→')} ${pc.bold(to)}`;
}

/** Thousands separators, for counts a human compares by magnitude. */
export function count(n: number | bigint): string {
	// Thousands separators are for comparing magnitudes by eye; they only get in
	// the way of anything that has to turn the string back into a number.
	return AGENT ? String(n) : n.toLocaleString('en-US');
}

/** `abcd…wxyz`, for addresses shown as identity rather than for copying. */
export function shortKey(key: string): string {
	if (AGENT) {
		return key;
	}
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
	if (AGENT) {
		for (const row of rows) {
			console.log(row.map(plain).join('\t'));
		}
		return;
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
