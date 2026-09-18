// Runtime generator for Pyth Lazer oracle messages, so the LiteSVM tests don't depend on
// frozen, pre-signed fixtures (which go stale against the on-chain wall-clock max-age check,
// PYTH_LAZER_MAX_STALENESS_SECONDS). We mint a throwaway Ed25519 signer, make the injected
// Pyth Lazer storage trust it (in addition to Pyth's real signer, so the remaining frozen
// fixtures still verify), and sign fresh messages stamped at ~now — always within the max-age
// window, no clock pinning required. See docs/DRIFT-TO-VELOCITY.md `lazer-max-staleness`.
import * as nacl from 'tweetnacl';
import { PYTH_STORAGE_DATA } from './pythLazerData';

const SOLANA_FORMAT_MAGIC = 2182742457; // SolanaMessage
const PAYLOAD_FORMAT_MAGIC = 2479346549; // PayloadData

// PriceFeedProperty discriminants (positional, see programs/pyth-lazer/src/lib.rs)
const PROP_PRICE = 0;
const PROP_BEST_BID = 1;
const PROP_BEST_ASK = 2;
const PROP_EXPONENT = 4;
const PROP_FEED_UPDATE_TS = 12;

// Throwaway Ed25519 signer. It is only ever trusted by the mock storage `mockLazerStorageData`
// injects into the (test-only) LiteSVM ledger, so it is not a sensitive key.
const TEST_LAZER_SEED = new Uint8Array(32).fill(7);
export const testLazerKeypair = nacl.sign.keyPair.fromSeed(TEST_LAZER_SEED);

function u8(v: number): Buffer {
	return Buffer.from([v & 0xff]);
}
function u16le(v: number): Buffer {
	const b = Buffer.alloc(2);
	b.writeUInt16LE(v);
	return b;
}
function u32le(v: number): Buffer {
	const b = Buffer.alloc(4);
	b.writeUInt32LE(v >>> 0);
	return b;
}
function u64le(v: bigint): Buffer {
	const b = Buffer.alloc(8);
	b.writeBigUInt64LE(v);
	return b;
}
function i64le(v: bigint): Buffer {
	const b = Buffer.alloc(8);
	b.writeBigInt64LE(v);
	return b;
}
function i16le(v: number): Buffer {
	const b = Buffer.alloc(2);
	b.writeInt16LE(v);
	return b;
}

export interface FreshLazerMessageOpts {
	feedId?: number;
	price?: bigint; // i64 mantissa (nonzero)
	bid?: bigint;
	ask?: bigint;
	exponent?: number; // i16
	channelId?: number;
}

/**
 * Builds a fresh, self-signed Pyth Lazer `SolanaMessage` (hex) for a single feed with its
 * `FeedUpdateTimestamp` set to `timestampUs`. Defaults reproduce the previous frozen SOL fixture's
 * price/bid/ask/exponent for feed 6, so downstream price-dependent assertions are unchanged.
 */
export function makeFreshLazerMessageHex(
	timestampUs: bigint,
	opts: FreshLazerMessageOpts = {}
): string {
	const feedId = opts.feedId ?? 6;
	const price = opts.price ?? 8388919459n;
	const bid = opts.bid ?? 8388790526n;
	const ask = opts.ask ?? 8390097236n;
	const exponent = opts.exponent ?? -8;
	const channelId = opts.channelId ?? 3;

	const feedProps = Buffer.concat([
		u8(PROP_PRICE),
		i64le(price),
		u8(PROP_BEST_BID),
		i64le(bid),
		u8(PROP_BEST_ASK),
		i64le(ask),
		u8(PROP_EXPONENT),
		i16le(exponent),
		u8(PROP_FEED_UPDATE_TS),
		u8(1), // present
		u64le(timestampUs),
	]);
	const feed = Buffer.concat([
		u32le(feedId),
		u8(5 /* num properties */),
		feedProps,
	]);
	const payload = Buffer.concat([
		u32le(PAYLOAD_FORMAT_MAGIC),
		u64le(timestampUs),
		u8(channelId),
		u8(1 /* num feeds */),
		feed,
	]);

	const signature = nacl.sign.detached(payload, testLazerKeypair.secretKey);
	const message = Buffer.concat([
		u32le(SOLANA_FORMAT_MAGIC),
		Buffer.from(signature),
		Buffer.from(testLazerKeypair.publicKey),
		u16le(payload.length),
		payload,
	]);
	return message.toString('hex');
}

/**
 * Convenience: a fresh SOL (feed 6) message stamped at the *LiteSVM* clock.
 * Pass `svmContextWrapper.connection.getTime()` (on-chain unix seconds), NOT `Date.now()`:
 * LiteSVM's clock advances ~1s per processed transaction, so in transaction-heavy tests it runs
 * well ahead of wall-clock; stamping off wall-clock would look stale on-chain.
 *
 * `leadSeconds` biases the stamp into the future to survive transactions that run between building
 * the message and its post executing. The program rejects a stamp more than
 * `PYTH_LAZER_MAX_FUTURE_SECONDS` ahead of the clock, so the lead must stay under that bound.
 * Keep it 0 for an immediate post: a future `publish_time` is fine for a fill but can wedge
 * time-delta math in the LP-pool settle/AUM path, so only lead when the post is genuinely
 * deferred (e.g. a crank bundled into a later-sent transaction).
 */
export function freshLazerSolHex(
	nowSeconds: number,
	leadSeconds = 0,
	opts: FreshLazerMessageOpts = {}
): string {
	const nowUs = BigInt(Math.floor(nowSeconds) + leadSeconds) * 1_000_000n;
	return makeFreshLazerMessageHex(nowUs, opts);
}

/**
 * The Pyth Lazer storage account data (base64) the tests inject, patched to also trust
 * {@link testLazerKeypair} as a second signer (slot 1) with a far-future expiry. Pyth's real
 * signer in slot 0 is preserved so any remaining frozen fixtures still pass signature verification.
 */
export function mockLazerStorageData(): string {
	const d = Buffer.from(PYTH_STORAGE_DATA, 'base64');
	// layout after 8-byte anchor discriminator: top_authority(32) treasury(32) fee(8)
	// num_trusted_signers(1)@80, then TrustedSignerInfo[]{pubkey(32) expires_at(i64)} @81
	d.writeUInt8(2, 80); // num_trusted_signers = 2
	Buffer.from(testLazerKeypair.publicKey).copy(d, 121); // signer[1].pubkey
	d.writeBigInt64LE(4102444800n, 153); // signer[1].expires_at = 2100-01-01
	return d.toString('base64');
}
