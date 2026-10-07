/**
 * Parity tests for the signed-message domain prefix, transcribed from
 * `validation/sig_verification/tests.rs`.
 */

import { assert } from 'chai';
import nacl from 'tweetnacl';
import { Keypair, PublicKey } from '@solana/web3.js';
import { VELOCITY_PROGRAM_ID } from '../../src/config';
import {
	signedMsgDomainPrefix,
	signedMsgSigningBytes,
} from '../../src/core/signedMsg';

describe('signedMsgDomainPrefix', () => {
	const programId = new PublicKey(VELOCITY_PROGRAM_ID);

	it('matches the prefix the program verifies', () => {
		assert.equal(
			signedMsgDomainPrefix(programId).toString(),
			'velocity-signed-msg:vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P:'
		);
	});

	it('puts a byte that is not a hex digit first', () => {
		const signed = signedMsgSigningBytes(programId, Buffer.from('c8d5a65e'));
		assert.notMatch(signed.toString()[0], /[0-9a-fA-F]/);
	});

	it('signs a different message for another program', () => {
		const message = Buffer.from('c8d5a65e2234f55d');
		const keypair = Keypair.generate();
		const signature = nacl.sign.detached(
			signedMsgSigningBytes(programId, message),
			keypair.secretKey
		);

		assert.isTrue(
			nacl.sign.detached.verify(
				signedMsgSigningBytes(programId, message),
				signature,
				keypair.publicKey.toBytes()
			)
		);
		assert.isFalse(
			nacl.sign.detached.verify(message, signature, keypair.publicKey.toBytes())
		);
		assert.isFalse(
			nacl.sign.detached.verify(
				signedMsgSigningBytes(Keypair.generate().publicKey, message),
				signature,
				keypair.publicKey.toBytes()
			)
		);
	});
});
