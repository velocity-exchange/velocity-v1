import * as anchor from '@coral-xyz/anchor';
import { AnchorProvider, Wallet } from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';
import {
	AdminClient,
	BulkAccountLoader,
	VelocityEnv,
	initialize,
} from '@velocity-exchange/sdk';
import * as fs from 'fs';
import * as os from 'os';
import { announceContext } from './context';

export type GlobalOpts = {
	url: string;
	keypair: string;
	env: VelocityEnv;
	multisig?: string;
	/**
	 * Whether env came from a flag or profile (true) or is the legacy
	 * fallback default (false). A declared env that contradicts the RPC's
	 * genesis hash is a fatal mismatch; the fallback is silently replaced by
	 * the detected cluster instead.
	 */
	envExplicit?: boolean;
	/** Name of the config profile these opts were resolved from, if any. */
	profile?: string;
	/** Skip the interactive mainnet direct-send confirmation. */
	yes?: boolean;
};

export function loadKeypair(path: string): Keypair {
	const expanded = path.startsWith('~')
		? path.replace(/^~/, os.homedir())
		: path;
	const bytes = JSON.parse(fs.readFileSync(expanded, 'utf-8'));
	return Keypair.fromSecretKey(Uint8Array.from(bytes));
}

export function buildProvider(opts: GlobalOpts): AnchorProvider {
	const connection = new Connection(opts.url, 'confirmed');
	const wallet = new Wallet(loadKeypair(opts.keypair));
	const provider = new AnchorProvider(connection, wallet, {
		commitment: 'confirmed',
		preflightCommitment: 'confirmed',
	});
	anchor.setProvider(provider);
	return provider;
}

/**
 * Build an AdminClient.
 *
 * `subscribe` should be `false` for ops where the State account doesn't yet
 * exist (e.g. `initialize`) or where we don't need cached state — this avoids
 * a network round-trip per command.
 *
 * `user` scopes the client to a user authority other than the signer (e.g. a
 * Squads vault PDA): user/userStats PDAs derive from it and its sub-account is
 * loaded on subscribe so ix builders that read positions (withdraw) work.
 */
export async function buildAdminClient(
	opts: GlobalOpts,
	subscribe = true,
	user?: { authority: PublicKey; subAccountId?: number }
): Promise<AdminClient> {
	const provider = buildProvider(opts);
	const env = (await announceContext(provider.connection, {
		env: opts.env,
		envDeclared: opts.envExplicit ?? true,
		profile: opts.profile,
		signer: provider.wallet.publicKey,
		multisig: opts.multisig ? new PublicKey(opts.multisig) : undefined,
		yes: opts.yes,
	})) as VelocityEnv;
	const sdkConfig = initialize({ env });
	const programId = new PublicKey(sdkConfig.VELOCITY_PROGRAM_ID);

	const client = new AdminClient({
		connection: provider.connection,
		wallet: provider.wallet,
		programID: programId,
		env,
		opts: { commitment: 'confirmed', preflightCommitment: 'confirmed' },
		authority: user?.authority,
		activeSubAccountId: user?.subAccountId,
		subAccountIds: user ? [user.subAccountId ?? 0] : undefined,
		accountSubscription: subscribe
			? {
					type: 'polling',
					accountLoader: new BulkAccountLoader(
						provider.connection,
						'confirmed',
						1000
					),
			  }
			: undefined,
	});

	if (subscribe) {
		await client.subscribe();
	}

	return client;
}
