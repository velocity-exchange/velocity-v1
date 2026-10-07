import { Keypair } from '@solana/web3.js';
import nacl from 'tweetnacl';
import { decodeUTF8 } from 'tweetnacl-util';

/**
 * The ws-server nonce is 30 alphanumeric characters. An order signature covers
 * the hex of a borsh order message, which is far longer than this bound.
 */
const MAX_CHALLENGE_NONCE_LENGTH = 64;
const CHALLENGE_NONCE_PATTERN = /^[A-Za-z0-9]+$/;

export function isValidChallengeNonce(nonce: unknown): nonce is string {
	return (
		typeof nonce === 'string' &&
		nonce.length <= MAX_CHALLENGE_NONCE_LENGTH &&
		CHALLENGE_NONCE_PATTERN.test(nonce)
	);
}

/**
 * Prefix for the signed challenge when the server advertises it in `auth_domain`.
 * An order message never starts with it. Must match `SWIFT_AUTH_DOMAIN` in velocity-rs.
 */
export const SWIFT_AUTH_DOMAIN = 'velocity-swift-auth:v1:';

/**
 * Signs a ws-server auth challenge. Throws on a nonce that is not a challenge.
 * Pass the auth message's `auth_domain`; only the known domain is prefixed.
 */
export function signChallengeNonce(
	nonce: string,
	keypair: Keypair,
	authDomain?: unknown
): string {
	if (!isValidChallengeNonce(nonce)) {
		throw new Error(
			`refusing to sign auth nonce of length ${String(nonce).length}: ` +
				`expected at most ${MAX_CHALLENGE_NONCE_LENGTH} alphanumeric characters`
		);
	}

	const message =
		authDomain === SWIFT_AUTH_DOMAIN ? SWIFT_AUTH_DOMAIN + nonce : nonce;
	const signature = nacl.sign.detached(decodeUTF8(message), keypair.secretKey);
	return Buffer.from(signature).toString('base64');
}
