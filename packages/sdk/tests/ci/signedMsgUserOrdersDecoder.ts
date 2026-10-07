/**
 * Parity tests for `decodeSignedMsgUserOrdersAccount`, against the layout
 * rules of `state/signed_msg_user.rs`.
 */
import { assert } from 'chai';
import { PublicKey } from '@solana/web3.js';
import {
	decodeSignedMsgUserOrdersAccount,
	isLegacySignedMsgUserOrdersLayout,
	signedMsgUserOrdersLegacySpace,
	signedMsgUserOrdersSpace,
} from '../../src/swift/signedMsgUserOrdersDecoder';

const DISCRIMINATOR = Buffer.alloc(8, 7);

function header(authority: PublicKey, version: number, len: number): Buffer {
	const fields = Buffer.alloc(8);
	fields.writeUInt32LE(version, 0);
	fields.writeUInt32LE(len, 4);
	return Buffer.concat([DISCRIMINATOR, authority.toBuffer(), fields]);
}

function legacyEntry(uuid: number, maxSlot: number, orderId: number): Buffer {
	const entry = Buffer.alloc(24);
	entry.fill(uuid, 0, 8);
	entry.writeBigUInt64LE(BigInt(maxSlot), 8);
	entry.writeUInt32LE(orderId, 16);
	return entry;
}

function currentEntry(
	uuid: number,
	maxSlot: number,
	clobOrderId: number
): Buffer {
	const entry = Buffer.alloc(40);
	entry.fill(uuid, 0, 8);
	entry.writeBigUInt64LE(BigInt(maxSlot), 8);
	entry.writeBigUInt64LE(BigInt(clobOrderId), 16);
	entry.writeUInt32LE(9, 24);
	entry.writeUInt16LE(3, 28);
	entry.fill(0xab, 32, 40);
	return entry;
}

function padTo(data: Buffer, size: number): Buffer {
	return Buffer.concat([data, Buffer.alloc(size - data.length)]);
}

describe('decodeSignedMsgUserOrdersAccount', () => {
	const authority = new PublicKey(Buffer.alloc(32, 1));

	it('detects the legacy layout by version and size alone', () => {
		const legacy8 = signedMsgUserOrdersLegacySpace(8);
		assert.isTrue(isLegacySignedMsgUserOrdersLayout(0, 8, legacy8));
		assert.isFalse(
			isLegacySignedMsgUserOrdersLayout(0, 8, signedMsgUserOrdersSpace(8))
		);
		// A migrated 1-entry account keeps the legacy size of one entry.
		assert.isFalse(
			isLegacySignedMsgUserOrdersLayout(1, 1, signedMsgUserOrdersLegacySpace(1))
		);
	});

	it('decodes a legacy account at 24 bytes with no route', () => {
		const data = padTo(
			Buffer.concat([
				header(authority, 0, 8),
				legacyEntry(5, 1234, 2),
				legacyEntry(6, 99, 3),
			]),
			signedMsgUserOrdersLegacySpace(8)
		);

		const account = decodeSignedMsgUserOrdersAccount(data);
		assert.isTrue(account.authorityPubkey.equals(authority));
		assert.equal(account.version, 0);
		assert.equal(account.signedMsgOrderData.length, 8);
		const first = account.signedMsgOrderData[0];
		assert.deepEqual(Array.from(first.uuid), new Array(8).fill(5));
		assert.equal(first.maxSlot.toNumber(), 1234);
		assert.equal(first.orderId, 2);
		assert.isTrue(first.clobOrderId.isZero());
		assert.deepEqual(first.routeDigest, new Array(8).fill(0));
		assert.equal(account.signedMsgOrderData[1].maxSlot.toNumber(), 99);
		assert.isTrue(account.signedMsgOrderData[2].maxSlot.isZero());
	});

	it('decodes a current account at 40 bytes', () => {
		const data = padTo(
			Buffer.concat([header(authority, 1, 2), currentEntry(4, 77, 12)]),
			signedMsgUserOrdersSpace(2)
		);

		const account = decodeSignedMsgUserOrdersAccount(data);
		assert.equal(account.version, 1);
		assert.equal(account.signedMsgOrderData.length, 2);
		const first = account.signedMsgOrderData[0];
		assert.equal(first.maxSlot.toNumber(), 77);
		assert.equal(first.clobOrderId.toNumber(), 12);
		assert.equal(first.orderId, 9);
		assert.equal(first.marketIndex, 3);
		assert.deepEqual(first.routeDigest, new Array(8).fill(0xab));
	});

	it('refuses a header that names more entries than the data holds', () => {
		const data = Buffer.concat([header(authority, 1, 3), Buffer.alloc(80)]);
		assert.throws(() => decodeSignedMsgUserOrdersAccount(data));
	});
});
