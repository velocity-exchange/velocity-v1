/**
 * Parity test: the staged config hash must match `QuoterConfigV0::content_hash`.
 */
import { assert } from 'chai';
import { QUOTER_CONFIG_SIZE, quoterConfigHash } from '../../src';

describe('quoter config hash', () => {
	it('matches the value the program pins', () => {
		// A `QuoterV0` account: the discriminator, then the config. The response
		// account sits at config offset 88 and the market index at 728.
		const data = Buffer.alloc(8 + QUOTER_CONFIG_SIZE + 48);
		data.fill(0xaa, 0, 8);
		data.fill(1, 8 + 88, 8 + 120);
		data.writeUInt16LE(7, 8 + 728);

		assert.equal(
			Buffer.from(quoterConfigHash(data)).toString('hex'),
			'bf8e4cf4c40690cd61432dfb7c7849c5de9d5cf0134685e3cb2b1e2639ea93db'
		);
	});
});
