'use strict';
// node --test deploy-scripts/upload-program-buffer.test.cjs
//
// Runs the uploader against the real upgradeable loader in LiteSVM, behind a
// fake JSON-RPC endpoint that can throttle, drop sends, or land a transaction
// and still answer 429. Time is virtual, so the pacing waits cost nothing.

const test = require('node:test');
const assert = require('node:assert/strict');
const { randomBytes } = require('node:crypto');
const { LiteSVM, Account } = require('litesvm');
const { Keypair, PublicKey } = require('@solana/web3.js');
const { createRpc, uploadProgramBuffer, maxChunkSize, base58, LOADER, GENESIS, BUFFER_HEADER } = require('./upload-program-buffer.cjs');

const SOL = 1_000_000_000n;

function artifact(size = 20_000, sbpf = 3) {
	const bytes = randomBytes(size);
	bytes.writeUInt32BE(0x7f454c46, 0);
	bytes.writeUInt32LE(sbpf, 48);
	return bytes;
}

function chain({ capacity = 30_000, upgradeAuthority } = {}) {
	const svm = new LiteSVM();
	const inner = svm.inner;
	const payer = Keypair.generate();
	inner.airdrop(payer.publicKey.toBytes(), 100n * SOL);
	const vault = upgradeAuthority ?? Keypair.generate().publicKey;
	const programId = Keypair.generate().publicKey;
	const programData = Keypair.generate().publicKey;

	const set = (address, data, executable) =>
		inner.setAccount(address.toBytes(), new Account(10n * SOL, data, LOADER.toBytes(), executable, 0n));
	set(programId, Buffer.concat([Buffer.from([2, 0, 0, 0]), programData.toBuffer()]), true);
	const pd = Buffer.alloc(45 + capacity);
	pd.writeUInt32LE(3, 0);
	pd[12] = 1;
	vault.toBuffer().copy(pd, 13);
	set(programData, pd, false);

	let height = 1_000;
	let issued = null;
	let firstSent = null;
	const statuses = new Map();
	const counts = {};
	const faults = { drop: () => false, http: () => null, landThen429: () => false, hideStatus: () => false };

	function handle(method, params) {
		switch (method) {
			case 'getGenesisHash':
				return GENESIS.devnet;
			case 'getAccountInfo': {
				const account = inner.getAccount(new PublicKey(params[0]).toBytes());
				if (!account) return { value: null };
				return {
					value: {
						data: [Buffer.from(account.data()).toString('base64'), 'base64'],
						owner: new PublicKey(account.owner()).toBase58(),
						executable: account.executable(),
						lamports: Number(account.lamports()),
					},
				};
			}
			case 'getBalance':
				return { value: Number(inner.getBalance(new PublicKey(params[0]).toBytes())) };
			case 'getMinimumBalanceForRentExemption':
				return Number(inner.minimumBalanceForRentExemption(BigInt(params[0])));
			case 'getLatestBlockhash':
				// A new blockhash only once the last one has expired, so a
				// re-signed transaction really differs from the original.
				if (issued && height > issued.lastValidBlockHeight) svm.expireBlockhash();
				if (!issued || height > issued.lastValidBlockHeight) {
					issued = { blockhash: inner.latestBlockhash(), lastValidBlockHeight: height + 150 };
				}
				return { value: issued };
			case 'getBlockHeight':
				return ++height;
			case 'sendTransaction': {
				const wire = Buffer.from(params[0], 'base64');
				const signature = base58(wire.subarray(1, 65));
				firstSent ??= signature;
				assert.equal(params[1].skipPreflight, true);
				if (statuses.has(signature) || faults.drop(signature)) return signature;
				const result = inner.sendLegacyTransaction(wire);
				const failed = typeof result?.err === 'function';
				statuses.set(signature, failed ? { err: String(result.err()), confirmationStatus: 'confirmed' } : { err: null, confirmationStatus: 'confirmed' });
				return signature;
			}
			case 'getSignatureStatuses':
				return { value: params[0].map((s) => (faults.hideStatus(s, firstSent) ? null : statuses.get(s) ?? null)) };
			default:
				throw new Error(`unexpected method ${method}`);
		}
	}

	async function fetchImpl(_url, { body }) {
		const { id, method, params } = JSON.parse(body);
		counts[method] = (counts[method] ?? 0) + 1;
		const status = faults.http(method, counts[method]);
		if (status) return new Response('', { status });
		const result = handle(method, params);
		if (faults.landThen429(method, params)) return new Response('', { status: 429 });
		return new Response(JSON.stringify({ jsonrpc: '2.0', id, result }), { status: 200 });
	}

	// Moves the chain past the issued blockhash, as hours between runs would.
	const expire = () => {
		if (issued) height = issued.lastValidBlockHeight + 1;
	};

	return { svm, inner, payer, vault, programId, faults, counts, fetchImpl, expire };
}

// Virtual time. Sleepers wake one at a time in wake-up order, each after the
// previous one has run to its next await, so concurrent waits behave as they
// would on a real clock without taking real time. With lateMs, timers fire that
// much late and every timer due by then fires in the same tick, like a busy
// event loop.
function clock({ lateMs = 0 } = {}) {
	let t = 0;
	let scheduled = false;
	const timers = [];
	const drain = () => {
		scheduled = false;
		timers.sort((a, b) => a.at - b.at);
		if (!timers.length) return;
		t = Math.max(t, timers[0].at + lateMs);
		const due = lateMs ? timers.filter((x) => x.at <= t) : [timers[0]];
		for (const timer of due) timers.splice(timers.indexOf(timer), 1);
		for (const timer of due) timer.resolve();
		if (timers.length) schedule();
	};
	const schedule = () => {
		if (!scheduled) {
			scheduled = true;
			setImmediate(drain);
		}
	};
	return {
		now: () => t,
		sleep: (ms) =>
			new Promise((resolve) => {
				timers.push({ at: t + ms, resolve });
				schedule();
			}),
	};
}

async function run(c, art, extra = {}) {
	const time = clock();
	const lines = [];
	const log = (line) => lines.push(line);
	const rpc = createRpc('http://fake', { maxRequestsPer10s: 400, log, fetchImpl: c.fetchImpl, ...time });
	const buffer = await uploadProgramBuffer({
		rpc,
		artifact: art,
		programId: c.programId,
		authority: c.vault,
		cluster: 'devnet',
		payer: c.payer,
		execute: true,
		log,
		...time,
		...extra,
		opts: { stallFailMs: 120_000, ...extra.opts },
	});
	return { buffer, lines, rpc, time };
}

function bufferState(c, buffer) {
	const account = c.inner.getAccount(buffer.toBytes());
	const data = Buffer.from(account.data());
	return { authority: new PublicKey(data.subarray(5, 37)), bytes: data.subarray(BUFFER_HEADER) };
}

test('uploads, verifies, and hands the buffer to the vault', async () => {
	const c = chain();
	const art = artifact();
	const { buffer, lines } = await run(c, art);
	const state = bufferState(c, buffer);
	assert.ok(state.bytes.equals(art));
	assert.ok(state.authority.equals(c.vault));
	assert.ok(lines.some((l) => l.startsWith('write: ') && l.includes('(100.0%)')));
	assert.ok(lines.some((l) => l.startsWith('done ')));
});

test('a write transaction fits in one packet', () => {
	const payer = Keypair.generate();
	const chunk = maxChunkSize(payer, Keypair.generate().publicKey, 100_000);
	assert.ok(chunk > 900 && chunk < 1232, `chunk ${chunk}`);
});

test('HTTP 429 pauses every request for at least 10 s and the upload finishes', async () => {
	const c = chain();
	c.faults.http = (method, n) => (method === 'sendTransaction' && n % 7 === 0 ? 429 : null);
	const art = artifact();
	const { buffer, lines, rpc } = await run(c, art);
	assert.ok(rpc.stats.throttled > 0);
	assert.ok(lines.some((l) => l.includes('HTTP 429') && l.includes('pausing all requests 10s')));
	assert.ok(bufferState(c, buffer).bytes.equals(art));
});

test('sends the RPC swallows are rebroadcast until they land', async () => {
	const c = chain();
	const seen = new Set();
	c.faults.drop = (sig) => (seen.has(sig) ? false : (seen.add(sig), true));
	const art = artifact();
	const { buffer, lines } = await run(c, art);
	assert.ok(bufferState(c, buffer).bytes.equals(art));
	assert.ok(lines.some((l) => /resent [1-9]/.test(l)));
});

test('an authority transfer that lands but answers 429 still succeeds', async () => {
	const c = chain();
	let tripped = false;
	c.faults.landThen429 = (method, params) => {
		if (tripped || method !== 'sendTransaction') return false;
		const wire = Buffer.from(params[0], 'base64');
		// SetAuthority is the only transaction with three loader accounts and tag 4.
		if (wire.includes(Buffer.from([4, 0, 0, 0])) && wire.length < 400) return (tripped = true);
		return false;
	};
	const art = artifact();
	const { buffer } = await run(c, art);
	assert.ok(tripped);
	assert.ok(bufferState(c, buffer).authority.equals(c.vault));
});

test('a create that landed unseen and was re-signed after expiry is accepted', async () => {
	const c = chain();
	// The first send is the create. It lands, but its status never shows up, so
	// the uploader re-signs it after the blockhash expires and that copy fails.
	c.faults.hideStatus = (signature, firstSent) => signature === firstSent;
	const art = artifact();
	const { buffer, lines } = await run(c, art);
	assert.ok(lines.some((l) => l.startsWith('create: ') && l.includes('continuing')), lines.join('\n'));
	assert.ok(bufferState(c, buffer).bytes.equals(art));
	assert.ok(bufferState(c, buffer).authority.equals(c.vault));
});

test('stops with a clear error when nothing lands', async () => {
	const c = chain();
	c.faults.drop = () => true;
	await assert.rejects(run(c, artifact(), { opts: { stallFailMs: 30_000 } }), /nothing confirmed for/);
});

test('refuses before spending when the program authority is not the vault', async () => {
	const c = chain({ upgradeAuthority: Keypair.generate().publicKey });
	const time = clock();
	const rpc = createRpc('http://fake', { maxRequestsPer10s: 400, log: () => {}, fetchImpl: c.fetchImpl, ...time });
	await assert.rejects(
		uploadProgramBuffer({ rpc, artifact: artifact(), programId: c.programId, authority: Keypair.generate().publicKey, cluster: 'devnet', payer: c.payer, execute: true, log: () => {}, ...time }),
		/upgrade authority/
	);
	assert.equal(c.counts.sendTransaction, undefined);
});

test('refuses an artifact that is not SBPF v3', async () => {
	const c = chain();
	await assert.rejects(run(c, artifact(20_000, 0)), /SBPF v0, expected v3/);
	assert.equal(c.counts.sendTransaction, undefined);
});

test('refuses a program that is too small for the artifact', async () => {
	const c = chain({ capacity: 10_000 });
	await assert.rejects(run(c, artifact(20_000)), /extend the program by 10000/);
});

test('without --execute it sends nothing', async () => {
	const c = chain();
	const time = clock();
	const lines = [];
	const rpc = createRpc('http://fake', { maxRequestsPer10s: 400, log: () => {}, fetchImpl: c.fetchImpl, ...time });
	const result = await uploadProgramBuffer({ rpc, artifact: artifact(), programId: c.programId, authority: c.vault, cluster: 'devnet', payer: c.payer, execute: false, log: (l) => lines.push(l), ...time });
	assert.equal(result, null);
	assert.equal(c.counts.sendTransaction, undefined);
	assert.ok(lines.some((l) => l.includes('read-only checks passed')));
});

test('the pacer spaces concurrent requests and holds them through a 429 pause', async () => {
	const time = clock();
	const starts = [];
	let n = 0;
	const fetchImpl = async (_u, { body }) => {
		starts.push(time.now());
		if (++n === 50) return new Response('', { status: 429 });
		return new Response(JSON.stringify({ jsonrpc: '2.0', id: JSON.parse(body).id, result: 1 }), { status: 200 });
	};
	const rpc = createRpc('http://fake', { maxRequestsPer10s: 100, log: () => {}, fetchImpl, ...time });
	await Promise.all(Array.from({ length: 250 }, () => rpc.call('getBlockHeight')));
	assert.equal(starts.length, 251);
	for (let i = 1; i < starts.length; i++) assert.ok(starts[i] - starts[i - 1] >= 100 - 1e-6, `gap at ${i}`);
	assert.ok(starts[50] - starts[49] >= 10_000, 'requests after the 429 wait out the pause');
	assert.equal(rpc.stats.throttled, 1);
});

test('the pacer keeps its spacing when timers fire late in a batch', async () => {
	const time = clock({ lateMs: 250 });
	const starts = [];
	const fetchImpl = async (_u, { body }) => {
		starts.push(time.now());
		return new Response(JSON.stringify({ jsonrpc: '2.0', id: JSON.parse(body).id, result: 1 }), { status: 200 });
	};
	const rpc = createRpc('http://fake', { maxRequestsPer10s: 100, log: () => {}, fetchImpl, ...time });
	await Promise.all(Array.from({ length: 100 }, () => rpc.call('getBlockHeight')));
	assert.equal(starts.length, 100);
	for (let i = 1; i < starts.length; i++) assert.ok(starts[i] - starts[i - 1] >= 100 - 1e-6, `gap at ${i}: ${starts[i] - starts[i - 1]}`);
});

test('the pacer keeps requests under the configured rate', async () => {
	const time = clock();
	const starts = [];
	const fetchImpl = async (_u, { body }) => {
		starts.push(time.now());
		return new Response(JSON.stringify({ jsonrpc: '2.0', id: JSON.parse(body).id, result: 1 }), { status: 200 });
	};
	const rpc = createRpc('http://fake', { maxRequestsPer10s: 100, log: () => {}, fetchImpl, ...time });
	for (let i = 0; i < 250; i++) await rpc.call('getBlockHeight');
	for (let i = 100; i < starts.length; i++) assert.ok(starts[i] - starts[i - 100] >= 10_000 - 1e-6);
});
