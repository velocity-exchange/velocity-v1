import { PublicKey } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	AmmAccountMeta,
	QuoterConfigV0,
	QuoterSlabV0Account,
	QuoterSlotV0,
	QuoterType,
} from './types';

/**
 * Where a `QuoterSlabV0` account's slot region starts: the 8-byte
 * discriminator plus the fixed 128-byte header. Mirrors
 * `QuoterSlabV0::SLOT_REGION_OFFSET` on-chain.
 */
export const QUOTER_SLAB_SLOT_REGION_OFFSET = 8 + 128;

/** Bytes one `QuoterSlotV0` occupies in the slot region. */
export const QUOTER_SLAB_SLOT_SIZE = 776;

/** Registered accounts one quoter's unified CPI list can hold. */
export const MAX_QUOTER_ACCOUNTS = 12;

function readPubkey(data: Buffer, offset: number): PublicKey {
	return new PublicKey(data.subarray(offset, offset + 32));
}

function readQuoterType(byte: number): QuoterType {
	switch (byte) {
		case 0:
			return QuoterType.VAMM;
		case 1:
			return QuoterType.CLOB;
		default:
			return QuoterType.CUSTOM;
	}
}

/**
 * Decode one `QuoterConfigV0` at `offset`. A hand-rolled mirror of the
 * on-chain layout: the slot region is raw bytes past the account struct, so
 * the type never reaches the IDL and the generated coder cannot decode it.
 * Field order and offsets follow `state/prop_amm.rs`.
 */
export function decodeQuoterConfig(
	data: Buffer,
	offset: number
): QuoterConfigV0 {
	const accounts: AmmAccountMeta[] = [];
	for (let index = 0; index < MAX_QUOTER_ACCOUNTS; index++) {
		const base = offset + 208 + index * 40;
		accounts.push({
			pubkey: readPubkey(data, base),
			isWritable: data.readUInt8(base + 32) !== 0,
			padding: Array.from(data.subarray(base + 33, base + 40)),
		});
	}
	return {
		approvedProgramSlot: new BN(data.subarray(offset, offset + 8), 'le'),
		bookTickSize: new BN(data.subarray(offset + 8, offset + 16), 'le'),
		bookMinOrderSize: new BN(data.subarray(offset + 16, offset + 24), 'le'),
		user: readPubkey(data, offset + 24),
		programId: readPubkey(data, offset + 56),
		responseAccount: readPubkey(data, offset + 88),
		authority: readPubkey(data, offset + 120),
		watchAccount: readPubkey(data, offset + 152),
		quoteV0Discriminator: Array.from(data.subarray(offset + 184, offset + 192)),
		executeV0Discriminator: Array.from(
			data.subarray(offset + 192, offset + 200)
		),
		quoteL3V0Discriminator: Array.from(
			data.subarray(offset + 200, offset + 208)
		),
		accounts,
		quoteAccountIndexes: Array.from(data.subarray(offset + 688, offset + 700)),
		executeAccountIndexes: Array.from(
			data.subarray(offset + 700, offset + 712)
		),
		watchOffset: data.readUInt32LE(offset + 712),
		watchLen: data.readUInt32LE(offset + 716),
		maxOracleDeviationBps: data.readUInt32LE(offset + 720),
		bookDefaultActivationDelaySlots: data.readUInt32LE(offset + 724),
		market: data.readUInt16LE(offset + 728),
		quoterType: readQuoterType(data.readUInt8(offset + 730)),
		isActive: data.readUInt8(offset + 731) !== 0,
		priority: data.readUInt8(offset + 732),
		accountsCount: data.readUInt8(offset + 733),
		quoteAccountsCount: data.readUInt8(offset + 734),
		executeAccountsCount: data.readUInt8(offset + 735),
	};
}

/**
 * Decode a `QuoterSlabV0` account: the fixed header plus every slot in the
 * tail region, vacant ones included so indexes stay stable. Slot 0 is the
 * market's book by convention; `Custom` quoters occupy slots 1+ and a vacant
 * slot's `entry` is the default pubkey.
 */
export function decodeQuoterSlab(data: Buffer): {
	header: QuoterSlabV0Account;
	slots: QuoterSlotV0[];
} {
	const header: QuoterSlabV0Account = {
		market: data.readUInt16LE(8),
		capacity: data.readUInt16LE(10),
		padding: [],
	};
	const slots: QuoterSlotV0[] = [];
	for (let index = 0; index < header.capacity; index++) {
		const start =
			QUOTER_SLAB_SLOT_REGION_OFFSET + index * QUOTER_SLAB_SLOT_SIZE;
		const end = start + QUOTER_SLAB_SLOT_SIZE;
		if (end > data.length) {
			throw new Error(
				`quoter slab declares ${header.capacity} slots but is too short to hold them`
			);
		}
		slots.push({
			entry: readPubkey(data, start),
			suspended: data.readUInt8(start + 32) !== 0,
			padding: Array.from(data.subarray(start + 33, start + 40)),
			config: decodeQuoterConfig(data, start + 40),
		});
	}
	return { header, slots };
}
