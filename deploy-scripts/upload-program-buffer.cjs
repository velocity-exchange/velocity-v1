#!/usr/bin/env node
'use strict';
/**
 * Writes a program .so into a BPF upgradeable-loader buffer at a pace the RPC
 * accepts, checks the buffer byte for byte against the artifact, and hands the
 * buffer authority to the Squads vault.
 *
 * This replaces `solana program write-buffer` in CI. The CLI sends thousands of
 * writes in parallel under its own retry loop and logs nothing about why sends
 * stop landing. Against a rate-limited endpoint (Triton allows 1200 requests per
 * 10 s per IP) that can stall an upload until "Max retries exceeded". Here every
 * RPC request goes through one pacer, a 429 pauses all traffic, and each
 * transaction is sent with skipPreflight and maxRetries 0 and tracked by
 * signature, so resending one that already landed is a no-op instead of an
 * error.
 *
 * Usage:
 *   SOLANA_RPC=<url> DEPLOY_KEYPAIR_PATH=<keypair.json> \
 *   node deploy-scripts/upload-program-buffer.cjs <program.so> \
 *     --program-id <pubkey> --authority <vault> --cluster devnet \
 *     [--execute]
 *
 * Without --execute it only runs the read-only checks. DEPLOY_KEYPAIR may hold
 * the keypair JSON instead of DEPLOY_KEYPAIR_PATH. The buffer address goes to
 * $GITHUB_OUTPUT as `buffer=` when set.
 */

const fs = require('node:fs');
const { createHash } = require('node:crypto');
const {
	ComputeBudgetProgram,
	Keypair,
	PublicKey,
	SystemProgram,
	Transaction,
	TransactionInstruction,
} = require('@solana/web3.js');

const LOADER = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111');
const GENESIS = {
	devnet: 'EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG',
	'mainnet-beta': '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d',
};
// Buffer: u32 state tag + Option<Pubkey> authority.
const BUFFER_HEADER = 37;
// ProgramData: u32 state tag + u64 slot + Option<Pubkey> authority.
const PROGRAMDATA_HEADER = 45;
const PACKET_SIZE = 1232;
const COMPUTE_UNITS = 10_000;
const STATUS_BATCH = 256;

const DEFAULTS = {
	maxRequestsPer10s: 400,
	maxInFlight: 48,
	priorityFee: 100_000,
	rebroadcastMs: 4_000,
	blockhashMaxAgeMs: 15_000,
	pollGapMs: 500,
	progressMs: 5_000,
	stallWarnMs: 60_000,
	stallFailMs: 600_000,
	deadlineMs: 90 * 60_000,
	maxPasses: 3,
};

class RpcError extends Error {
	constructor(method, code, message) {
		super(`${method}: RPC error ${code}: ${message}`);
		this.code = code;
	}
}

const BASE58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
function base58(bytes) {
	let n = BigInt('0x' + (Buffer.from(bytes).toString('hex') || '0'));
	let out = '';
	while (n > 0n) {
		out = BASE58[Number(n % 58n)] + out;
		n /= 58n;
	}
	for (const b of bytes) {
		if (b !== 0) break;
		out = '1' + out;
	}
	return out;
}

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

function fmtDuration(ms) {
	const s = Math.max(0, Math.round(ms / 1000));
	return s >= 60 ? `${Math.floor(s / 60)}m${String(s % 60).padStart(2, '0')}s` : `${s}s`;
}

/**
 * JSON-RPC client that starts requests at least 10 s / maxRequestsPer10s apart,
 * however many callers run concurrently, and pauses all requests after HTTP 429
 * or 5xx (at least 10 s, longer if Retry-After asks). Requests are retried on
 * those and on network errors; a JSON-RPC error is thrown to the caller.
 */
function createRpc(url, { maxRequestsPer10s, log, fetchImpl = fetch, sleep, now }) {
	const gapMs = 10_000 / maxRequestsPer10s;
	let nextAt = 0;
	let lastStart = -Infinity;
	let pausedUntil = 0;
	const stats = { requests: 0, throttled: 0, networkErrors: 0, rateLimit: '' };

	// Reserve a start slot before sleeping so concurrent callers queue up
	// instead of all waking at the same instant. Timers can fire late, which
	// would bunch reserved slots together, so the gap is also checked against
	// the last request that actually started. A pause that begins while we sleep
	// sends us back for a slot after it.
	async function acquire() {
		for (;;) {
			const slot = Math.max(nextAt, pausedUntil, lastStart + gapMs, now());
			nextAt = slot + gapMs;
			if (slot > now()) await sleep(slot - now());
			if (now() < pausedUntil || now() - lastStart < gapMs) continue;
			lastStart = now();
			return;
		}
	}

	async function call(method, params = []) {
		for (let attempt = 1; ; attempt++) {
			await acquire();
			stats.requests++;

			let res;
			try {
				res = await fetchImpl(url, {
					method: 'POST',
					headers: { 'content-type': 'application/json' },
					body: JSON.stringify({ jsonrpc: '2.0', id: stats.requests, method, params }),
					signal: AbortSignal.timeout(20_000),
				});
			} catch {
				stats.networkErrors++;
				if (attempt >= 8) throw new Error(`${method}: network error, gave up after ${attempt} attempts`);
				pausedUntil = now() + 2_000 * attempt;
				log(`rpc: network error on ${method}, retrying (attempt ${attempt})`);
				continue;
			}

			const limits = [];
			res.headers.forEach((value, key) => {
				if (key.toLowerCase().startsWith('x-ratelimit')) limits.push(`${key.slice(12)}=${value}`);
			});
			if (limits.length) stats.rateLimit = limits.join(' ');

			if (res.status === 429 || res.status >= 500) {
				if (res.status === 429) stats.throttled++;
				const retryAfter = Number(res.headers.get('retry-after'));
				const pauseMs = Math.min(60_000, Math.max(10_000, Number.isFinite(retryAfter) ? retryAfter * 1000 : 0));
				pausedUntil = now() + pauseMs;
				await res.body?.cancel?.();
				log(`rpc: HTTP ${res.status} on ${method}, pausing all requests ${fmtDuration(pauseMs)} (429s so far: ${stats.throttled})`);
				if (attempt >= 12) throw new Error(`${method}: HTTP ${res.status}, gave up after ${attempt} attempts`);
				continue;
			}
			if (!res.ok) throw new Error(`${method}: HTTP ${res.status}`);

			const body = await res.json();
			if (body.error) throw new RpcError(method, body.error.code, body.error.message);
			return body.result;
		}
	}

	return { call, stats };
}

// --- loader instructions --------------------------------------------------

function initializeBufferIx(buffer, authority) {
	return new TransactionInstruction({
		programId: LOADER,
		keys: [
			{ pubkey: buffer, isSigner: false, isWritable: true },
			{ pubkey: authority, isSigner: false, isWritable: false },
		],
		data: Buffer.from([0, 0, 0, 0]),
	});
}

function writeIx(buffer, authority, offset, bytes) {
	const data = Buffer.alloc(16 + bytes.length);
	data.writeUInt32LE(1, 0);
	data.writeUInt32LE(offset, 4);
	data.writeBigUInt64LE(BigInt(bytes.length), 8);
	Buffer.from(bytes).copy(data, 16);
	return new TransactionInstruction({
		programId: LOADER,
		keys: [
			{ pubkey: buffer, isSigner: false, isWritable: true },
			{ pubkey: authority, isSigner: true, isWritable: false },
		],
		data,
	});
}

function setBufferAuthorityIx(buffer, authority, newAuthority) {
	return new TransactionInstruction({
		programId: LOADER,
		keys: [
			{ pubkey: buffer, isSigner: false, isWritable: true },
			{ pubkey: authority, isSigner: true, isWritable: false },
			{ pubkey: newAuthority, isSigner: false, isWritable: false },
		],
		data: Buffer.from([4, 0, 0, 0]),
	});
}

function buildTx(payer, blockhash, instructions, signers, priorityFee) {
	const tx = new Transaction({ feePayer: payer.publicKey, recentBlockhash: blockhash }).add(
		ComputeBudgetProgram.setComputeUnitLimit({ units: COMPUTE_UNITS }),
		ComputeBudgetProgram.setComputeUnitPrice({ microLamports: priorityFee }),
		...instructions
	);
	tx.sign(payer, ...signers);
	return tx;
}

/**
 * Largest write payload whose signed transaction fits in one packet. The probe
 * carries 256 bytes so the instruction data length is encoded in the same two
 * bytes it takes for a full chunk.
 */
function maxChunkSize(payer, buffer, priorityFee) {
	const probeBytes = 256;
	const probe = buildTx(payer, PublicKey.default.toBase58(), [writeIx(buffer, payer.publicKey, 0, Buffer.alloc(probeBytes))], [], priorityFee);
	return PACKET_SIZE - (probe.serialize().length - probeBytes);
}

// --- account reads ----------------------------------------------------------

async function getAccount(rpc, address) {
	const res = await rpc.call('getAccountInfo', [address.toBase58(), { encoding: 'base64', commitment: 'confirmed' }]);
	if (!res.value) return null;
	return { ...res.value, data: Buffer.from(res.value.data[0], 'base64') };
}

/** Decodes a loader buffer; returns null when the account is not one. */
function decodeBuffer(account) {
	if (!account || account.owner !== LOADER.toBase58()) return null;
	if (account.data.length < BUFFER_HEADER || account.data.readUInt32LE(0) !== 1) return null;
	const authority = account.data[4] === 1 ? new PublicKey(account.data.subarray(5, 37)) : null;
	return { authority, bytes: account.data.subarray(BUFFER_HEADER) };
}

async function readBuffer(rpc, buffer, length) {
	const decoded = decodeBuffer(await getAccount(rpc, buffer));
	if (!decoded) throw new Error(`${buffer.toBase58()} is not an upgradeable-loader buffer`);
	if (decoded.bytes.length !== length) {
		throw new Error(`buffer ${buffer.toBase58()} holds ${decoded.bytes.length} bytes, the artifact is ${length}`);
	}
	return decoded;
}

function missingOffsets(actual, wanted, chunk) {
	const offsets = [];
	for (let offset = 0; offset < wanted.length; offset += chunk) {
		const end = Math.min(offset + chunk, wanted.length);
		if (!actual.subarray(offset, end).equals(wanted.subarray(offset, end))) offsets.push(offset);
	}
	return offsets;
}

// --- sending ----------------------------------------------------------------

/**
 * Lands one transaction per item. Keeps up to maxInFlight unconfirmed, re-sends
 * the same signed bytes every rebroadcastMs until the blockhash expires, then
 * re-signs the item with a fresh blockhash. An onchain error aborts, since the
 * same transaction would fail again.
 */
async function land(rpc, items, build, ctx) {
	const { opts, log, now, sleep, label } = ctx;
	const queue = [...items];
	const total = items.length;
	const inFlight = new Map();
	const stats = { confirmed: 0, sent: 0, rebroadcasts: 0, expired: 0, sendErrors: 0 };
	const started = now();
	let lastProgressAt = started;
	let lastLogAt = started;
	let lastStallWarnAt = 0;
	let blockhash = null;
	let blockhashAt = 0;

	const send = async (wire) => {
		try {
			await rpc.call('sendTransaction', [wire, { encoding: 'base64', skipPreflight: true, maxRetries: 0 }]);
		} catch (err) {
			if (!(err instanceof RpcError)) throw err;
			stats.sendErrors++;
			if (stats.sendErrors <= 5) log(`${label}: sendTransaction rejected (${err.message}); it will be retried`);
		}
	};

	const progress = (force) => {
		if (!force && now() - lastLogAt < opts.progressMs) return;
		lastLogAt = now();
		const elapsed = now() - started;
		const rate = stats.confirmed / Math.max(elapsed / 1000, 1);
		const eta = rate > 0 ? fmtDuration(((total - stats.confirmed) / rate) * 1000) : '?';
		const pct = ((100 * stats.confirmed) / Math.max(total, 1)).toFixed(1);
		const limit = rpc.stats.rateLimit ? ` | ratelimit ${rpc.stats.rateLimit}` : '';
		log(
			`${label}: ${stats.confirmed}/${total} (${pct}%) | ${rate.toFixed(1)}/s | in flight ${inFlight.size} | ` +
				`resent ${stats.rebroadcasts} | expired ${stats.expired} | 429s ${rpc.stats.throttled} | ` +
				`elapsed ${fmtDuration(elapsed)} | eta ${eta}${limit}`
		);
	};

	while (queue.length || inFlight.size) {
		if (now() > ctx.deadline) throw new Error(`${label}: deadline reached with ${total - stats.confirmed}/${total} unconfirmed`);

		if (!blockhash || now() - blockhashAt > opts.blockhashMaxAgeMs) {
			blockhash = (await rpc.call('getLatestBlockhash', [{ commitment: 'confirmed' }])).value;
			blockhashAt = now();
		}

		// Sends run concurrently; the RPC client's pacer keeps the request rate
		// in bounds, so latency to the endpoint does not cap throughput.
		const sends = [];
		while (queue.length && inFlight.size < opts.maxInFlight) {
			const item = queue.shift();
			const tx = build(item, blockhash.blockhash);
			const signature = base58(tx.signature);
			const wire = tx.serialize().toString('base64');
			inFlight.set(signature, { item, wire, lastValidBlockHeight: blockhash.lastValidBlockHeight, sentAt: now() });
			stats.sent++;
			sends.push(send(wire));
		}
		await Promise.all(sends);

		const signatures = [...inFlight.keys()];
		const batches = [];
		for (let i = 0; i < signatures.length; i += STATUS_BATCH) batches.push(signatures.slice(i, i + STATUS_BATCH));
		const [height, ...results] = await Promise.all([
			rpc.call('getBlockHeight', [{ commitment: 'confirmed' }]),
			...batches.map((batch) => rpc.call('getSignatureStatuses', [batch, { searchTransactionHistory: false }])),
		]);
		const statuses = new Map();
		batches.forEach((batch, b) => batch.forEach((signature, j) => statuses.set(signature, results[b].value[j])));

		const rebroadcasts = [];
		for (const [signature, entry] of inFlight) {
			const status = statuses.get(signature);
			if (status?.err) {
				throw new Error(`${label}: transaction ${signature} failed onchain: ${JSON.stringify(status.err)}`);
			}
			if (status && (status.confirmationStatus === 'confirmed' || status.confirmationStatus === 'finalized')) {
				inFlight.delete(signature);
				stats.confirmed++;
				lastProgressAt = now();
			} else if (status) {
				// Processed but not confirmed yet: it is in a block, wait for it.
			} else if (height > entry.lastValidBlockHeight) {
				inFlight.delete(signature);
				queue.push(entry.item);
				stats.expired++;
			} else if (now() - entry.sentAt >= opts.rebroadcastMs) {
				entry.sentAt = now();
				stats.rebroadcasts++;
				rebroadcasts.push(send(entry.wire));
			}
		}
		await Promise.all(rebroadcasts);

		const quietMs = now() - lastProgressAt;
		if (quietMs > opts.stallFailMs) {
			throw new Error(
				`${label}: nothing confirmed for ${fmtDuration(quietMs)} while the RPC accepted sends; ` +
					`${total - stats.confirmed}/${total} left. Pending signatures: ${[...inFlight.keys()].slice(0, 3).join(', ')}`
			);
		}
		if (quietMs > opts.stallWarnMs && now() - lastStallWarnAt > opts.stallWarnMs) {
			lastStallWarnAt = now();
			log(
				`${label}: WARNING nothing confirmed for ${fmtDuration(quietMs)} ` +
					`(send errors ${stats.sendErrors}, 429s ${rpc.stats.throttled}, network errors ${rpc.stats.networkErrors}). ` +
					`Sample pending: ${[...inFlight.keys()].slice(0, 3).join(', ') || 'none'}`
			);
		}
		progress(false);
		if (inFlight.size && queue.length === 0) await sleep(opts.pollGapMs);
	}
	progress(true);
	return stats;
}

// --- the upload -------------------------------------------------------------

function checkArtifact(artifact) {
	if (artifact.length < 64 || artifact.readUInt32BE(0) !== 0x7f454c46) throw new Error('artifact is not an ELF file');
	const sbpf = artifact.readUInt32LE(48);
	if (sbpf !== 3) throw new Error(`artifact is SBPF v${sbpf}, expected v3`);
}

async function checkProgram(rpc, programId, authority, artifactLength) {
	const program = await getAccount(rpc, programId);
	if (!program || program.owner !== LOADER.toBase58() || !program.executable || program.data.readUInt32LE(0) !== 2) {
		throw new Error(`${programId.toBase58()} is not an upgradeable program`);
	}
	const programData = await getAccount(rpc, new PublicKey(program.data.subarray(4, 36)));
	if (!programData || programData.data.readUInt32LE(0) !== 3) throw new Error('program data account is missing or malformed');
	const upgradeAuthority = programData.data[12] === 1 ? new PublicKey(programData.data.subarray(13, 45)) : null;
	if (!upgradeAuthority || !upgradeAuthority.equals(authority)) {
		throw new Error(`program upgrade authority is ${upgradeAuthority?.toBase58() ?? 'none'}, expected ${authority.toBase58()}`);
	}
	const capacity = programData.data.length - PROGRAMDATA_HEADER;
	if (capacity < artifactLength) {
		throw new Error(`program data holds ${capacity} bytes, the artifact needs ${artifactLength}; extend the program by ${artifactLength - capacity} first`);
	}
	return capacity;
}

async function uploadProgramBuffer(args) {
	const { rpc, artifact, programId, authority, cluster, payer, execute, log } = args;
	const now = args.now ?? Date.now;
	const sleep = args.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
	const opts = { ...DEFAULTS, ...args.opts };
	const deadline = now() + opts.deadlineMs;

	checkArtifact(artifact);
	const genesis = await rpc.call('getGenesisHash');
	if (genesis !== GENESIS[cluster]) throw new Error(`RPC genesis ${genesis} is not ${cluster}`);
	const capacity = await checkProgram(rpc, programId, authority, artifact.length);

	const chunk = maxChunkSize(payer ?? Keypair.generate(), programId, opts.priorityFee);
	const chunks = Math.ceil(artifact.length / chunk);
	log(`artifact    ${artifact.length} bytes, sha256 ${sha256(artifact)}, SBPF v3`);
	log(`program     ${programId.toBase58()} (${cluster}), capacity ${capacity} bytes, upgrade authority ${authority.toBase58()}`);
	log(`plan        ${chunks} writes of ${chunk} bytes, <= ${opts.maxRequestsPer10s} requests / 10 s, ${opts.maxInFlight} in flight`);
	if (!execute) {
		log('read-only checks passed; add --execute to upload');
		return null;
	}
	if (!payer) throw new Error('a deployer keypair is required with --execute');
	log(`deployer    ${payer.publicKey.toBase58()}`);

	const ctx = { opts, log, now, sleep, deadline };
	// Every run writes a fresh buffer, so nothing from an earlier run can end up
	// in the upgrade.
	const space = BUFFER_HEADER + artifact.length;
	const rent = await rpc.call('getMinimumBalanceForRentExemption', [space]);
	const balance = (await rpc.call('getBalance', [payer.publicKey.toBase58(), { commitment: 'confirmed' }])).value;
	const fees = chunks * (5_000 + Math.ceil((COMPUTE_UNITS * opts.priorityFee) / 1e6)) * 2;
	log(`funds       balance ${(balance / 1e9).toFixed(3)} SOL, rent ${(rent / 1e9).toFixed(3)} SOL, fees <= ${(fees / 1e9).toFixed(3)} SOL`);
	if (balance < rent + fees) throw new Error('deployer balance does not cover buffer rent and write fees');

	const bufferKeypair = Keypair.generate();
	const buffer = bufferKeypair.publicKey;
	log(`buffer      ${buffer.toBase58()} (new)`);
	const create = [
		SystemProgram.createAccount({
			fromPubkey: payer.publicKey,
			newAccountPubkey: buffer,
			lamports: rent,
			space,
			programId: LOADER,
		}),
		initializeBufferIx(buffer, payer.publicKey),
	];
	try {
		await land(rpc, ['create'], (_, bh) => buildTx(payer, bh, create, [bufferKeypair], opts.priorityFee), {
			...ctx,
			label: 'create',
		});
	} catch (err) {
		// A create that landed without us seeing its status gets re-signed
		// after the blockhash expires, and that copy fails because the
		// account exists. Accept the buffer if it is the one we asked for.
		const existing = decodeBuffer(await getAccount(rpc, buffer));
		if (!existing?.authority?.equals(payer.publicKey) || existing.bytes.length !== artifact.length) throw err;
		log(`create: ${err.message}; the buffer exists with the expected authority and size, continuing`);
	}

	for (let pass = 1; ; pass++) {
		const current = await readBuffer(rpc, buffer, artifact.length);
		const missing = missingOffsets(current.bytes, artifact, chunk);
		log(`pass ${pass}     ${missing.length}/${chunks} chunks to write`);
		if (missing.length === 0) break;
		if (pass > opts.maxPasses) throw new Error(`${missing.length} chunks still differ after ${opts.maxPasses} passes`);
		await land(
			rpc,
			missing,
			(offset, bh) =>
				buildTx(payer, bh, [writeIx(buffer, payer.publicKey, offset, artifact.subarray(offset, offset + chunk))], [], opts.priorityFee),
			{ ...ctx, label: 'write' }
		);
	}

	const written = await readBuffer(rpc, buffer, artifact.length);
	if (!written.bytes.equals(artifact)) throw new Error('buffer does not match the artifact; not transferring authority');
	log(`verified    buffer sha256 ${sha256(written.bytes)} matches the artifact`);

	if (!written.authority?.equals(authority)) {
		try {
			await land(
				rpc,
				['authority'],
				(_, bh) => buildTx(payer, bh, [setBufferAuthorityIx(buffer, payer.publicKey, authority)], [], opts.priorityFee),
				{ ...ctx, label: 'authority' }
			);
		} catch (err) {
			// The transfer may have landed even though we lost track of it.
			const check = await readBuffer(rpc, buffer, artifact.length);
			if (!check.authority?.equals(authority)) throw err;
			log(`authority: ${err.message}; the buffer shows the transfer landed anyway`);
		}
	}
	const final = await readBuffer(rpc, buffer, artifact.length);
	if (!final.authority?.equals(authority) || !final.bytes.equals(artifact)) {
		throw new Error('final check failed: buffer authority or contents changed');
	}
	log(`done        buffer ${buffer.toBase58()} verified, authority ${authority.toBase58()}`);
	return buffer;
}

// --- CLI --------------------------------------------------------------------

function parseArgs(argv) {
	const out = { flags: {} };
	for (let i = 0; i < argv.length; i++) {
		const arg = argv[i];
		if (arg === '--execute') out.flags.execute = true;
		else if (arg.startsWith('--')) out.flags[arg.slice(2)] = argv[++i];
		else if (!out.file) out.file = arg;
		else throw new Error(`unexpected argument ${arg}`);
	}
	return out;
}

async function main() {
	const { file, flags } = parseArgs(process.argv.slice(2));
	if (!file || !flags['program-id'] || !flags.authority || !flags.cluster) {
		throw new Error('usage: upload-program-buffer.cjs <program.so> --program-id <pk> --authority <pk> --cluster <devnet|mainnet-beta> [--execute]');
	}
	if (!GENESIS[flags.cluster]) throw new Error(`unknown cluster ${flags.cluster}`);
	const url = process.env.SOLANA_RPC;
	if (!url) throw new Error('set SOLANA_RPC');

	let payer = null;
	const secret = process.env.DEPLOY_KEYPAIR || (process.env.DEPLOY_KEYPAIR_PATH && fs.readFileSync(process.env.DEPLOY_KEYPAIR_PATH, 'utf8'));
	if (secret) {
		try {
			payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(secret)));
		} catch {
			throw new Error('deployer keypair is not a JSON byte array');
		}
	}

	const started = Date.now();
	const log = (line) => console.log(`[${fmtDuration(Date.now() - started).padStart(6)}] ${line}`);
	const opts = {};
	if (flags['max-requests-per-10s']) opts.maxRequestsPer10s = Number(flags['max-requests-per-10s']);
	if (flags['max-in-flight']) opts.maxInFlight = Number(flags['max-in-flight']);
	if (flags['priority-fee']) opts.priorityFee = Number(flags['priority-fee']);
	const rpc = createRpc(url, {
		maxRequestsPer10s: opts.maxRequestsPer10s ?? DEFAULTS.maxRequestsPer10s,
		log,
		sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
		now: Date.now,
	});

	try {
		const buffer = await uploadProgramBuffer({
			rpc,
			artifact: fs.readFileSync(file),
			programId: new PublicKey(flags['program-id']),
			authority: new PublicKey(flags.authority),
			cluster: flags.cluster,
			payer,
			execute: Boolean(flags.execute),
			opts,
			log,
		});
		if (buffer && process.env.GITHUB_OUTPUT) fs.appendFileSync(process.env.GITHUB_OUTPUT, `buffer=${buffer.toBase58()}\n`);
	} finally {
		log(`rpc totals  ${rpc.stats.requests} requests, ${rpc.stats.throttled} x 429, ${rpc.stats.networkErrors} network errors`);
	}
}

module.exports = { createRpc, uploadProgramBuffer, maxChunkSize, missingOffsets, base58, LOADER, GENESIS, BUFFER_HEADER, PROGRAMDATA_HEADER };

if (require.main === module) {
	main().catch((err) => {
		const url = process.env.SOLANA_RPC;
		const message = url ? String(err.message).split(url).join('<rpc>') : String(err.message);
		console.error(`error: ${message}`);
		process.exitCode = 1;
	});
}
