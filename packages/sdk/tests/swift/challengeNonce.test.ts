import { expect } from 'chai';
import { Keypair } from '@solana/web3.js';
import nacl from 'tweetnacl';
import {
	SWIFT_AUTH_DOMAIN,
	isValidChallengeNonce,
	signChallengeNonce,
} from '../../src/swift/challengeNonce';

describe('swift auth challenge nonce', () => {
	const keypair = Keypair.generate();

	it('signs a ws-server nonce', () => {
		const nonce = 'aZ09'.repeat(7) + 'xy';
		const signature = Buffer.from(signChallengeNonce(nonce, keypair), 'base64');

		expect(
			nacl.sign.detached.verify(
				Buffer.from(nonce),
				signature,
				keypair.publicKey.toBytes()
			)
		).to.equal(true);
	});

	it('refuses the hex of an order message', () => {
		// The smallest order message is well over 32 bytes, so its hex exceeds the bound.
		const orderMessageHex = Buffer.alloc(33, 0xab).toString('hex');

		expect(isValidChallengeNonce(orderMessageHex)).to.equal(false);
		expect(() => signChallengeNonce(orderMessageHex, keypair)).to.throw(
			'refusing to sign'
		);
	});

	it('refuses non-alphanumeric and empty nonces', () => {
		expect(isValidChallengeNonce('')).to.equal(false);
		expect(isValidChallengeNonce('abc def')).to.equal(false);
		expect(isValidChallengeNonce(undefined)).to.equal(false);
	});

	it('prefixes the advertised auth domain', () => {
		const nonce = 'aZ09'.repeat(7) + 'xy';
		const signature = Buffer.from(
			signChallengeNonce(nonce, keypair, SWIFT_AUTH_DOMAIN),
			'base64'
		);

		expect(
			nacl.sign.detached.verify(
				Buffer.from(SWIFT_AUTH_DOMAIN + nonce),
				signature,
				keypair.publicKey.toBytes()
			)
		).to.equal(true);
	});

	it('ignores an unknown auth domain', () => {
		const nonce = 'aZ09'.repeat(7) + 'xy';
		const signature = Buffer.from(
			signChallengeNonce(nonce, keypair, 'some-other-prefix:'),
			'base64'
		);

		expect(
			nacl.sign.detached.verify(
				Buffer.from(nonce),
				signature,
				keypair.publicKey.toBytes()
			)
		).to.equal(true);
	});
});
