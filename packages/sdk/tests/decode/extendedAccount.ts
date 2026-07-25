import { AnchorProvider, Idl, Program } from '@coral-xyz/anchor';
import velocityIDL from '../../src/idl/velocity.json';
import { Connection, Keypair } from '@solana/web3.js';
import { decodeUser, Wallet } from '../../src';
import { assert } from 'chai';
import { userAccountBufferStrings } from './userAccountBufferStrings';

// An on-chain account may be extended past the struct the client was built
// against (program upgrade + realloc). Decoding must read only the known
// prefix and ignore the trailing bytes.
describe('Extended account decode', () => {
	it('decodes a user account buffer with trailing bytes', () => {
		const connection = new Connection('http://localhost:8899');
		const wallet = new Wallet(new Keypair());
		// @ts-ignore
		const provider = new AnchorProvider(connection, wallet);
		const program = new Program(velocityIDL as Idl, provider);

		for (const userAccountBufferString of userAccountBufferStrings) {
			// captured buffers end at the old declared-field length; on-chain
			// accounts are 4496 bytes, so zero-extend to current size first
			const raw = Buffer.from(userAccountBufferString, 'base64');
			const buffer =
				raw.length < 4496
					? Buffer.concat([raw, Buffer.alloc(4496 - raw.length)])
					: raw;
			// non-zero tail so any decode that reads past the struct is caught
			const extended = Buffer.concat([buffer, Buffer.alloc(128, 0xaa)]);

			const anchorExpected = program.coder.accounts.decode('user', buffer);
			const anchorDecoded = program.coder.accounts.decode('user', extended);
			assert(
				JSON.stringify(anchorDecoded) === JSON.stringify(anchorExpected),
				'anchor coder decode must ignore trailing bytes'
			);

			const customExpected = decodeUser(buffer);
			const customDecoded = decodeUser(extended);
			assert(
				JSON.stringify(customDecoded) === JSON.stringify(customExpected),
				'custom user decode must ignore trailing bytes'
			);
		}
	});
});
