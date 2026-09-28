import { sha256 } from '@noble/hashes/sha256';
import type { PublicKey } from '@solana/web3.js';
import type {
	SignedMsgOrderParamsDelegateMessage,
	SignedMsgOrderParamsMessage,
} from '../types';

/**
 * Minimal shape of an Anchor `program.coder.types` needed to (de)serialize the
 * `signedMsgOrderParamsMessage` / `signedMsgOrderParamsDelegateMessage` IDL types —
 * lets these helpers accept a plain coder without depending on a full `Program`.
 */
export type AnchorTypesCoder = {
	encode: (typeName: string, value: any) => Buffer;
	decode: (typeName: string, buffer: Buffer) => any;
};

/**
 * The entries a new `SignedMsgUserOrders` record gets. A market or IOC message holds its entry for
 * the fill window at the shortest slot duration, about 64 s at 400 ms slots. The record refuses a
 * new message while every entry is held, so 32 entries allow one such order per 2 s.
 */
export const DEFAULT_SIGNED_MSG_USER_ORDERS_LEN = 32;

/**
 * The prefix a taker signs ahead of the hex message. It names the program, so the signature is
 * valid for no other program. Its first byte is not a hex digit, so a verifier that hex-decodes
 * the signed bytes, as Drift does, refuses a Velocity message.
 */
export function signedMsgDomainPrefix(programId: PublicKey): Buffer {
	return Buffer.from(`velocity-signed-msg:${programId.toBase58()}:`);
}

/**
 * The bytes a taker signs for `orderParams`, the hex message the instruction carries: the domain
 * prefix, then the message. The instruction carries only the message. The program adds the
 * prefix when it verifies.
 */
export function signedMsgSigningBytes(
	programId: PublicKey,
	orderParams: Uint8Array
): Buffer {
	return Buffer.concat([signedMsgDomainPrefix(programId), orderParams]);
}

/**
 * Borsh-encodes a Swift signed-order message envelope: an 8-byte Anchor type
 * discriminator (`sha256("global:SignedMsgOrderParamsMessage" | "...Delegate...")[0..8]`)
 * followed by the Borsh-serialized message body. This is the exact byte layout the
 * taker/delegate signs off-chain and the Swift server / `placeAndMakeSignedMsgPerpOrder`
 * verifies and decodes on submission.
 *
 * `maxMarginRatio`/`isolatedPositionDeposit` default to `null` when omitted, and
 * `builderIdx`/`builderFeeTenthBps` default to `null` when omitted — both for
 * Borsh-Option compatibility with the IDL type, which requires the fields be present.
 *
 * @param args.coderTypes - an Anchor types coder (or `AnchorTypesCoder`) that knows the `signedMsgOrderParamsMessage`/`signedMsgOrderParamsDelegateMessage` IDL types.
 * @param args.orderParamsMessage - the order params message to encode; a plain `SignedMsgOrderParamsMessage` or, if `delegateSigner` is true, a `SignedMsgOrderParamsDelegateMessage`.
 * @param args.delegateSigner - whether this message is signed by a delegate rather than the account owner; selects both the discriminator and the IDL type used to encode the body. Defaults to `false`.
 * @returns the discriminator-prefixed Borsh buffer, ready to be signed and submitted.
 */
export function encodeSignedMsgOrderParamsMessage(args: {
	coderTypes: AnchorTypesCoder;
	orderParamsMessage:
		| SignedMsgOrderParamsMessage
		| SignedMsgOrderParamsDelegateMessage;
	delegateSigner?: boolean;
}): Buffer {
	const { coderTypes, delegateSigner } = args;
	const orderParamsMessage: any = { ...args.orderParamsMessage };

	if (orderParamsMessage.maxMarginRatio === undefined) {
		orderParamsMessage.maxMarginRatio = null;
	}
	if (orderParamsMessage.isolatedPositionDeposit === undefined) {
		orderParamsMessage.isolatedPositionDeposit = null;
	}

	const anchorIxName = delegateSigner
		? 'global' + ':' + 'SignedMsgOrderParamsDelegateMessage'
		: 'global' + ':' + 'SignedMsgOrderParamsMessage';
	const prefix = Buffer.from(sha256(anchorIxName).slice(0, 8));

	const withBuilderDefaults = {
		...orderParamsMessage,
		builderIdx:
			orderParamsMessage.builderIdx !== undefined
				? orderParamsMessage.builderIdx
				: null,
		builderFeeTenthBps:
			orderParamsMessage.builderFeeTenthBps !== undefined
				? orderParamsMessage.builderFeeTenthBps
				: null,
	};

	const body = delegateSigner
		? coderTypes.encode(
				'signedMsgOrderParamsDelegateMessage',
				withBuilderDefaults as SignedMsgOrderParamsDelegateMessage
		  )
		: coderTypes.encode(
				'signedMsgOrderParamsMessage',
				withBuilderDefaults as SignedMsgOrderParamsMessage
		  );

	return Buffer.concat([prefix, body]);
}

/**
 * Decodes a Swift signed-order message envelope produced by
 * `encodeSignedMsgOrderParamsMessage`: strips the leading 8-byte discriminator and
 * Borsh-deserializes the remainder as either `signedMsgOrderParamsMessage` or
 * `signedMsgOrderParamsDelegateMessage`.
 *
 * Zero-pads the buffer with 128 extra bytes before decoding, so a message encoded by an
 * older/smaller IDL (missing newer optional fields) still deserializes instead of
 * throwing on a short read. This only works because new fields are `Option`s — if the
 * current IDL's type were ever more than 128 bytes larger than the incoming message,
 * decoding would still fail.
 *
 * @param args.coderTypes - an Anchor types coder (or `AnchorTypesCoder`) that knows the `signedMsgOrderParamsMessage`/`signedMsgOrderParamsDelegateMessage` IDL types.
 * @param args.encodedMessage - the discriminator-prefixed buffer to decode (as produced by `encodeSignedMsgOrderParamsMessage`).
 * @param args.delegateSigner - whether to decode as a `SignedMsgOrderParamsDelegateMessage` rather than `SignedMsgOrderParamsMessage`. Must match how the message was encoded. Defaults to `false`.
 * @returns the decoded `SignedMsgOrderParamsMessage` or `SignedMsgOrderParamsDelegateMessage`.
 */
export function decodeSignedMsgOrderParamsMessage(args: {
	coderTypes: AnchorTypesCoder;
	encodedMessage: Buffer;
	delegateSigner?: boolean;
}): SignedMsgOrderParamsMessage | SignedMsgOrderParamsDelegateMessage {
	const decodeStr = args.delegateSigner
		? 'signedMsgOrderParamsDelegateMessage'
		: 'signedMsgOrderParamsMessage';
	return args.coderTypes.decode(
		decodeStr,
		Buffer.concat([args.encodedMessage.slice(8), Buffer.alloc(128)])
	);
}
