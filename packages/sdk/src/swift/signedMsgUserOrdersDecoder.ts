/**
 * Decoder for `SignedMsgUserOrders`, mirroring the layout rules in
 * `programs/velocity/src/state/signed_msg_user.rs`.
 *
 * The header's `version` names the entry layout. Version 1 stores 40-byte
 * entries. An account created before the version field stores 24-byte entries
 * with no routing state. It has version 0 and exactly `legacySpace(len)`
 * bytes. The Anchor coder reads every account at 40 bytes, so it fails on a
 * legacy account.
 */
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import { SignedMsgOrderId, SignedMsgUserOrdersAccount } from '../types';

/** The discriminator, the authority, `version` and `len`. */
const HEADER_LEN = 8 + 32 + 4 + 4;
const ENTRY_LEN = 40;
const LEGACY_ENTRY_LEN = 24;
const ROUTE_DIGEST_LEN = 8;

/** The account size before the entries, which `space` counts on chain. */
const SPACE_BASE = 8 + 32 + 4 + 32;

/** Mirrors `SignedMsgUserOrders::space`. */
export function signedMsgUserOrdersSpace(numOrders: number): number {
	return SPACE_BASE + numOrders * ENTRY_LEN;
}

/** Mirrors `SignedMsgUserOrders::legacy_space`. */
export function signedMsgUserOrdersLegacySpace(numOrders: number): number {
	return SPACE_BASE + numOrders * LEGACY_ENTRY_LEN;
}

/** Mirrors `is_legacy_layout`. `dataLen` counts the discriminator. */
export function isLegacySignedMsgUserOrdersLayout(
	version: number,
	len: number,
	dataLen: number
): boolean {
	return version === 0 && dataLen === signedMsgUserOrdersLegacySpace(len);
}

function decodeEntry(data: Buffer, offset: number): SignedMsgOrderId {
	return {
		uuid: Uint8Array.from(data.subarray(offset, offset + 8)),
		maxSlot: new BN(data.subarray(offset + 8, offset + 16), 'le'),
		clobOrderId: new BN(data.subarray(offset + 16, offset + 24), 'le'),
		orderId: data.readUInt32LE(offset + 24),
		marketIndex: data.readUInt16LE(offset + 28),
		padding: data.readUInt16LE(offset + 30),
		routeDigest: Array.from(data.subarray(offset + 32, offset + 40)),
	};
}

function decodeLegacyEntry(data: Buffer, offset: number): SignedMsgOrderId {
	return {
		uuid: Uint8Array.from(data.subarray(offset, offset + 8)),
		maxSlot: new BN(data.subarray(offset + 8, offset + 16), 'le'),
		clobOrderId: new BN(0),
		orderId: data.readUInt32LE(offset + 16),
		marketIndex: 0,
		padding: 0,
		routeDigest: new Array(ROUTE_DIGEST_LEN).fill(0),
	};
}

/**
 * Decode a `SignedMsgUserOrders` account in either layout. A legacy account
 * decodes as its stored 24-byte entries, with every route field zero. The
 * program migrates such an account on its next write.
 */
export function decodeSignedMsgUserOrdersAccount(
	data: Buffer
): SignedMsgUserOrdersAccount {
	if (data.length < HEADER_LEN) {
		throw new Error('SignedMsgUserOrders account is shorter than its header');
	}

	const authorityPubkey = new PublicKey(data.subarray(8, 40));
	const version = data.readUInt32LE(40);
	const len = data.readUInt32LE(44);
	const legacy = isLegacySignedMsgUserOrdersLayout(version, len, data.length);
	const entryLen = legacy ? LEGACY_ENTRY_LEN : ENTRY_LEN;
	if (HEADER_LEN + len * entryLen > data.length) {
		throw new Error(`SignedMsgUserOrders len ${len} exceeds the account data`);
	}

	const decode = legacy ? decodeLegacyEntry : decodeEntry;
	const signedMsgOrderData = Array.from({ length: len }, (_, index) =>
		decode(data, HEADER_LEN + index * entryLen)
	);

	return { authorityPubkey, version, signedMsgOrderData };
}
