import { expect } from 'chai';
import { Keypair } from '@solana/web3.js';
import nacl from 'tweetnacl';
import {
	SWIFT_AUTH_DOMAIN,
	isValidChallengeNonce,
	signChallengeNonce,
} from '../../src/swift/challengeNonce';
import { SwiftOrderSubscriber } from '../../src/swift/swiftOrderSubscriber';
import { IndicativeQuotesSender } from '../../src/indicative-quotes/indicativeQuotesSender';

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

/**
 * The server accepts both forms, so only these tests fail if a client stops
 * passing the server's `auth_domain` through to the signature.
 */
describe('swift auth message handling', () => {
	const keypair = Keypair.generate();
	const nonce = 'aZ09'.repeat(7) + 'xy';

	type AuthClient = { handleAuthMessage(message: unknown): void };

	function signedBytesVerify(
		client: AuthClient,
		authMessage: Record<string, string>,
		expected: string
	): boolean {
		const sent: string[] = [];
		(client as unknown as { ws: unknown }).ws = {
			send: (data: string) => sent.push(data),
		};

		client.handleAuthMessage({ channel: 'auth', ...authMessage });

		const signature = Buffer.from(JSON.parse(sent[0]).signature, 'base64');
		return nacl.sign.detached.verify(
			Buffer.from(expected),
			signature,
			keypair.publicKey.toBytes()
		);
	}

	const clients: [string, () => AuthClient][] = [
		[
			'SwiftOrderSubscriber',
			() =>
				new SwiftOrderSubscriber({
					velocityEnv: 'devnet',
					marketIndexes: [0],
					keypair,
					velocityClient: {} as never,
				}),
		],
		[
			'IndicativeQuotesSender',
			() => new IndicativeQuotesSender('ws://127.0.0.1:9', keypair),
		],
	];

	for (const [name, makeClient] of clients) {
		it(`${name} signs the advertised domain`, () => {
			const authMessage = { nonce, auth_domain: SWIFT_AUTH_DOMAIN };

			expect(
				signedBytesVerify(makeClient(), authMessage, SWIFT_AUTH_DOMAIN + nonce)
			).to.equal(true);
			expect(signedBytesVerify(makeClient(), authMessage, nonce)).to.equal(
				false
			);
		});

		it(`${name} signs the bare nonce when no domain is advertised`, () => {
			expect(signedBytesVerify(makeClient(), { nonce }, nonce)).to.equal(true);
		});
	}
});
