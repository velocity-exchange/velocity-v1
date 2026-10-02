import { expect } from 'chai';
import { createHash } from 'crypto';
import { Keypair, PublicKey, SystemProgram } from '@solana/web3.js';
import {
	getRegisterWatchIxs,
	RELAY_PROGRAM_ID,
	USER_CONDITIONS_BLOCK_OFFSET,
	userConditionsWatchSeed,
} from '../../src/relay/watch';

describe('relay watch registration', () => {
	const payer = Keypair.generate().publicKey;
	const user = Keypair.generate().publicKey;
	const target = Keypair.generate().publicKey;

	const registration = () =>
		getRegisterWatchIxs({
			payer,
			target,
			blockOffset: USER_CONDITIONS_BLOCK_OFFSET,
			seed: userConditionsWatchSeed(user),
			rentLamports: 1_670_400,
		});

	it('puts the watch at the payer seed address', async () => {
		const { watch, ixs } = await registration();
		expect(watch.toBase58()).to.equal(
			(
				await PublicKey.createWithSeed(
					payer,
					userConditionsWatchSeed(user),
					RELAY_PROGRAM_ID
				)
			).toBase58()
		);
		expect(ixs[0].programId.equals(SystemProgram.programId)).to.equal(true);
		expect(ixs[0].keys.some((meta) => meta.pubkey.equals(watch))).to.equal(
			true
		);
	});

	it('calls register_watch_v0 with the block offset', async () => {
		const { watch, ixs } = await registration();
		const register = ixs[1];
		const discriminator = createHash('sha256')
			.update('global:register_watch_v0')
			.digest()
			.subarray(0, 8);

		expect(register.programId.equals(RELAY_PROGRAM_ID)).to.equal(true);
		expect(register.data.subarray(0, 8).equals(discriminator)).to.equal(true);
		expect(register.data.readUInt32LE(8)).to.equal(8);
		expect(register.keys.map((meta) => meta.pubkey.toBase58())).to.deep.equal(
			[payer, target, watch].map((key) => key.toBase58())
		);
		expect(register.keys[0].isSigner).to.equal(true);
	});

	it('fits a seed in 32 bytes', () => {
		expect(userConditionsWatchSeed(user).length).to.equal(32);
	});
});
