import { Wallet } from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';
import { AdminClient, VelocityEnv, initialize } from '@velocity-exchange/sdk';

/**
 * Read-only State access that needs no signer and no subscription: an
 * ephemeral throwaway wallet satisfies the client constructor, the account is
 * fetched raw and decoded through the program coder. Used by `config init`
 * (verifying a pasted multisig against the live admins before a keypair is
 * even configured) and `whoami`.
 */

/** The decoded fields consulted for authority checks. */
export type StateAdmins = {
	coldAdmin: PublicKey;
	warmAdmin: PublicKey;
	pauseAdmin: PublicKey;
	hotRoles: Record<string, PublicKey>;
};

export async function fetchStateAdmins(
	connection: Connection,
	env: VelocityEnv
): Promise<StateAdmins> {
	const sdkConfig = initialize({ env });
	const client = new AdminClient({
		connection,
		wallet: new Wallet(Keypair.generate()),
		programID: new PublicKey(sdkConfig.VELOCITY_PROGRAM_ID),
		env,
	});
	const statePk = await client.getStatePublicKey();
	const info = await connection.getAccountInfo(statePk);
	if (!info) {
		throw new Error(
			`State account not found on this cluster for env "${env}"; wrong RPC or env`
		);
	}
	const state = client.program.coder.accounts.decode('state', info.data) as {
		coldAdmin: PublicKey;
		warmAdmin: PublicKey;
		pauseAdmin: PublicKey;
	} & Record<string, unknown>;

	const hotRoles: Record<string, PublicKey> = {};
	for (const [key, value] of Object.entries(state)) {
		if (key.startsWith('hot') && value instanceof PublicKey) {
			hotRoles[key] = value;
		}
	}
	return {
		coldAdmin: state.coldAdmin,
		warmAdmin: state.warmAdmin,
		pauseAdmin: state.pauseAdmin,
		hotRoles,
	};
}
