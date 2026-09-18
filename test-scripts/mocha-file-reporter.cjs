'use strict';
// One result line per test FILE, plus any failures in full. Mocha's own
// reporters group by describe block, which over 70+ files reads as several
// hundred flat lines; `test.file` is the only way back to file-level reporting
// inside a single mocha process.
//
// Every line is prefixed with U+0001 so the caller can tell the report apart
// from test output. Tests and the SDK log to both stdout and stderr, and a spare
// file descriptor is not an option because ts-mocha spawns mocha as a child and
// passes through only fds 0, 1 and 2. run-anchor-tests.sh shows the marked lines
// and keeps the rest in a file it prints only on failure.
//
// Colors follow TEST_REPORT_COLOR, which the caller sets from its own tty, and
// use the same palette as deploy-scripts/_ui.sh.
const fs = require('fs');
const path = require('path');
const Mocha = require('mocha');

const MARK = '';
const C = process.env.TEST_REPORT_COLOR === '1';
const BOLD = C ? '[1m' : '';
const DIM = C ? '[2m' : '';
const RED = C ? '[31m' : '';
const GRN = C ? '[32m' : '';
const YLW = C ? '[33m' : '';
const RST = C ? '[0m' : '';
const OK = '✔';
const BAD = '✘';

module.exports = function FileReporter(runner, options) {
	Mocha.reporters.Base.call(this, runner, options);

	// Mark every line: a payload containing a newline would otherwise emit an
	// unmarked line that the caller's filter drops.
	//
	// writeSync rather than process.stdout.write, because Node block-buffers
	// stdout when it is a pipe. This only matters for a consumer reading the
	// child directly; run-anchor-tests.sh ends its pipeline at the terminal,
	// where the downstream stages line-buffer anyway.
	const write = (s) => {
		const out = String(s)
			.split('\n')
			.map((l) => MARK + l + '\n')
			.join('');
		try {
			fs.writeSync(1, out);
		} catch (e) {
			// EAGAIN on a non-blocking pipe; fall back to the buffered path.
			process.stdout.write(out);
		}
	};

	const stats = new Map(); // file -> { pass, fail, pending, ms }
	const failures = [];
	let current = null;

	const rel = (f) => (f ? path.relative(process.cwd(), f) : '(unknown file)');
	const statsFor = (f) => {
		if (!stats.has(f)) stats.set(f, { pass: 0, fail: 0, pending: 0, ms: 0 });
		return stats.get(f);
	};

	// A file's line can only be printed once the next file starts, or at the
	// end, because results stream test by test.
	const flush = (file) => {
		const s = statsFor(file);
		const counts = [
			s.pass + ' passed',
			s.fail ? RED + s.fail + ' failed' + RST : '',
			s.pending ? YLW + s.pending + ' pending' + RST : '',
		]
			.filter(Boolean)
			.join(', ');
		const mark = s.fail ? RED + BAD + RST : GRN + OK + RST;
		write(
			mark +
				' ' +
				file.padEnd(52) +
				' ' +
				counts +
				'  ' +
				DIM +
				s.ms +
				'ms' +
				RST
		);
	};

	const record = (test, kind) => {
		const file = rel(test.file);
		if (current !== null && current !== file) flush(current);
		current = file;
		const s = statsFor(file);
		s[kind] += 1;
		s.ms += test.duration || 0;
	};

	runner.on('pass', (t) => record(t, 'pass'));
	runner.on('pending', (t) => record(t, 'pending'));
	runner.on('fail', (t, err) => {
		record(t, 'fail');
		failures.push({ file: rel(t.file), title: t.title, err });
	});

	runner.once('end', () => {
		if (current !== null) flush(current);

		const total = { pass: 0, fail: 0, pending: 0 };
		for (const s of stats.values()) {
			total.pass += s.pass;
			total.fail += s.fail;
			total.pending += s.pending;
		}

		if (failures.length) {
			write('');
			write(RED + BOLD + failures.length + ' failing' + RST);
			for (const f of failures) {
				write(
					'  ' + RED + BAD + RST + ' ' + BOLD + f.file + RST + '  ' + f.title
				);
				const msg = String((f.err && f.err.message) || f.err);
				for (const line of msg.split('\n').slice(0, 6)) {
					write('      ' + DIM + line + RST);
				}
			}
		}

		// A per-file runner renders its own summary across children, so instead of
		// a totals block it gets one machine-readable line (marker + U+0002) to
		// add up. It filters that line out of what it displays.
		if (process.env.TEST_REPORT_TOTALS === '0') {
			write('\u0002' + total.pass + ' ' + total.fail + ' ' + total.pending);
			return;
		}

		const secs = ((this.stats.duration || 0) / 1000).toFixed(1);
		write('');
		write(
			'  ' +
				GRN +
				total.pass +
				' passed' +
				RST +
				(total.fail ? '  ' + RED + total.fail + ' failed' + RST : '') +
				(total.pending ? '  ' + YLW + total.pending + ' pending' + RST : '') +
				'  ' +
				DIM +
				'across ' +
				stats.size +
				' files in ' +
				secs +
				's' +
				RST
		);
	});
};
