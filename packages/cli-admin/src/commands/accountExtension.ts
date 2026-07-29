import { Command } from 'commander';
import {
	PublicKey,
	Transaction,
	TransactionInstruction,
} from '@solana/web3.js';
import { utils } from '@coral-xyz/anchor';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';

/**
 * CLI type flag → IDL account name for every zero-copy account the program's
 * `extend_account` instruction supports. Borsh accounts are excluded on
 * purpose (their migrations are per-type deserialization changes, not
 * trailing-bytes growth).
 */
const EXTENDABLE_ACCOUNT_TYPES: Record<string, string> = {
	user: 'User',
	'user-stats': 'UserStats',
	'referrer-name': 'ReferrerName',
	'perp-market': 'PerpMarket',
	'spot-market': 'SpotMarket',
	state: 'State',
	'insurance-fund-stake': 'InsuranceFundStake',
	'prelaunch-oracle': 'PrelaunchOracle',
	'pyth-lazer-oracle': 'PythLazerOracle',
	'revenue-share': 'RevenueShare',
	'lp-pool': 'LPPool',
	constituent: 'Constituent',
};

export function registerAccountExtension(parent: Command): void {
	withGlobalOptions(
		parent
			.command('extend-account [account]')
			.description(
				'Grow zero-copy accounts to the size the deployed program expects, as the migration ' +
					'crank after a program upgrade that appended fields to an account struct ' +
					'(see docs/ACCOUNT-EXTENSION.md). Pass a single account pubkey, or --type to scan ' +
					'and extend every account of that type. The signer must hold the AccountExtension ' +
					'hot role (or be the warm/cold admin) and pays the rent-exempt shortfalls; assign ' +
					'the role with `auth set-hot-admin accountExtension <pubkey>`. --multisig is not ' +
					'supported (assign the role to a hot keypair instead — a crank is many transactions). ' +
					'Idempotent; accounts already at size are skipped (and are a no-op on-chain even when raced).'
			)
			.option(
				'--type <type>',
				`extend every account of a type (${Object.keys(
					EXTENDABLE_ACCOUNT_TYPES
				).join('|')})`
			)
			.option(
				'--batch-size <n>',
				'extend instructions per transaction in --type mode',
				'8'
			)
			.option(
				'--dry-run',
				'report which accounts would be extended and the rent cost, send nothing',
				false
			)
	).action(async (account: string | undefined, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as {
			type?: string;
			batchSize: string;
			dryRun: boolean;
		};
		if (opts.multisig) {
			throw new Error(
				'extend-account does not support --multisig; assign the AccountExtension hot role ' +
					'to a keypair (auth set-hot-admin accountExtension <pubkey>) and sign with it'
			);
		}
		if ((account === undefined) === (local.type === undefined)) {
			throw new Error('pass exactly one of <account> or --type');
		}
		const batchSize = Number.parseInt(local.batchSize, 10);
		if (!Number.isInteger(batchSize) || batchSize < 1) {
			throw new Error(
				`--batch-size must be a positive integer, got "${local.batchSize}"`
			);
		}

		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, false);
		try {
			const coder = client.program.coder.accounts as unknown as {
				accountDiscriminator(name: string): Buffer;
				size(name: string): number;
			};

			let candidates: PublicKey[];
			let targetLen: number;
			if (account !== undefined) {
				const pubkey = new PublicKey(account);
				const info = await provider.connection.getAccountInfo(pubkey);
				if (!info) {
					throw new Error(`account ${account} does not exist`);
				}
				if (!info.owner.equals(client.program.programId)) {
					throw new Error(`account ${account} is not velocity-owned`);
				}
				const name = Object.values(EXTENDABLE_ACCOUNT_TYPES).find((n) =>
					info.data.subarray(0, 8).equals(coder.accountDiscriminator(n))
				);
				if (!name) {
					throw new Error(
						`account ${account} is not a supported zero-copy account`
					);
				}
				targetLen = coder.size(name);
				candidates = info.data.length < targetLen ? [pubkey] : [];
				console.log(
					`${name} ${account}: current=${info.data.length} target=${targetLen}` +
						(candidates.length === 0 ? ' (already at size)' : '')
				);
			} else {
				const name = EXTENDABLE_ACCOUNT_TYPES[local.type!];
				if (!name) {
					throw new Error(
						`unknown --type "${local.type}"; expected one of ${Object.keys(
							EXTENDABLE_ACCOUNT_TYPES
						).join('|')}`
					);
				}
				targetLen = coder.size(name);
				const accounts = await provider.connection.getProgramAccounts(
					client.program.programId,
					{
						filters: [
							{
								memcmp: {
									offset: 0,
									bytes: utils.bytes.bs58.encode(
										coder.accountDiscriminator(name)
									),
								},
							},
						],
					}
				);
				candidates = accounts
					.filter(({ account }) => account.data.length < targetLen)
					.map(({ pubkey }) => pubkey);
				console.log(
					`${name}: ${accounts.length} scanned, ${candidates.length} below target size ${targetLen}`
				);
			}

			if (candidates.length === 0) {
				console.log('nothing to extend');
				return;
			}
			if (local.dryRun) {
				const rentTarget =
					await provider.connection.getMinimumBalanceForRentExemption(
						targetLen
					);
				console.log(
					`dry run: would extend ${candidates.length} account(s) to ${targetLen} bytes ` +
						`(rent-exempt minimum at target size: ${
							rentTarget / 1e9
						} SOL each, ` +
						`actual top-up is the per-account shortfall)`
				);
				for (const pubkey of candidates.slice(0, 20)) {
					console.log(`  ${pubkey.toBase58()}`);
				}
				if (candidates.length > 20) {
					console.log(`  ... and ${candidates.length - 20} more`);
				}
				return;
			}

			let extended = 0;
			for (let i = 0; i < candidates.length; i += batchSize) {
				const chunk = candidates.slice(i, i + batchSize);
				const ixs: TransactionInstruction[] = await Promise.all(
					chunk.map((pubkey) => client.getExtendAccountIx(pubkey))
				);
				const signature = await provider.sendAndConfirm(
					new Transaction().add(...ixs)
				);
				extended += chunk.length;
				console.log(`extended ${extended}/${candidates.length} (${signature})`);
			}
		} finally {
			await client.unsubscribe();
		}
	});
}
