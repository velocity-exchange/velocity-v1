/**
 * Relay `WatchV0` registration. A relay turner cranks a conditions block only
 * after a watch names it, and registration is permissionless.
 */
import {
	PublicKey,
	SystemProgram,
	TransactionInstruction,
} from '@solana/web3.js';

export const RELAY_PROGRAM_ID = new PublicKey(
	'4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu'
);

/** `relay_spec::WATCH_V0_LEN`: the discriminator, then `WatchV0`. */
export const RELAY_WATCH_V0_LEN = 112;

/** The first 8 bytes of `sha256("global:register_watch_v0")`. */
const REGISTER_WATCH_V0_DISCRIMINATOR = Buffer.from([
	235, 1, 142, 88, 158, 181, 47, 4,
]);

/** `UserConditionsV0` holds its relay block as its first field. */
export const USER_CONDITIONS_BLOCK_OFFSET = 8;

export type WatchRegistration = {
	payer: PublicKey;
	target: PublicKey;
	blockOffset: number;
	/** At most 32 bytes. With the payer it fixes the watch address. */
	seed: string;
	rentLamports: number;
	relayProgram?: PublicKey;
};

export type RegisterWatchIxs = {
	watch: PublicKey;
	ixs: TransactionInstruction[];
};

/** The watch seed for one user's conditions. */
export function userConditionsWatchSeed(user: PublicKey): string {
	return user.toBase58().slice(0, 32);
}

/**
 * Create the zeroed watch account from the payer's seed, then register it.
 * The payer is the only signer, and it alone can close the watch later.
 */
export async function getRegisterWatchIxs(
	registration: WatchRegistration
): Promise<RegisterWatchIxs> {
	const relayProgram = registration.relayProgram ?? RELAY_PROGRAM_ID;
	const watch = await PublicKey.createWithSeed(
		registration.payer,
		registration.seed,
		relayProgram
	);

	const create = SystemProgram.createAccountWithSeed({
		fromPubkey: registration.payer,
		basePubkey: registration.payer,
		seed: registration.seed,
		newAccountPubkey: watch,
		lamports: registration.rentLamports,
		space: RELAY_WATCH_V0_LEN,
		programId: relayProgram,
	});

	const offset = Buffer.alloc(4);
	offset.writeUInt32LE(registration.blockOffset);
	const register = new TransactionInstruction({
		programId: relayProgram,
		keys: [
			{ pubkey: registration.payer, isSigner: true, isWritable: false },
			{ pubkey: registration.target, isSigner: false, isWritable: false },
			{ pubkey: watch, isSigner: false, isWritable: true },
		],
		data: Buffer.concat([REGISTER_WATCH_V0_DISCRIMINATOR, offset]),
	});

	return { watch, ixs: [create, register] };
}
