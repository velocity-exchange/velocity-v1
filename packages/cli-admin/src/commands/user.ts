import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey, Transaction } from '@solana/web3.js';
import {
	fetchUserStatsAccount,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, reportDryRun, sendOrPropose } from '../lib/squads';
import { deriveAssociatedTokenAccount, resolveAuthority } from '../lib/userOps';

export function registerUser(parent: Command): void {
	const user = parent.command('user').description('Per-user admin actions.');

	withGlobalOptions(
		user
			.command('init <name>')
			.description(
				'Initialize velocity user accounts for an authority: UserStats (if missing) plus ' +
					'sequential sub-accounts named "<name>-<id>" until --sub-accounts exist. Creation is ' +
					'permissionless — the local keypair signs and pays rent, no proposal is created; ' +
					'--multisig is only used to derive the vault PDA authority. Idempotent: skips ' +
					'sub-accounts that already exist, safe to rerun after a partial failure.'
			)
			.option(
				'--authority <pubkey>',
				'user authority (default: signer, or vault PDA with --multisig)'
			)
			.option(
				'--vault-index <index>',
				'with --multisig, vault index used to derive the authority PDA',
				'0'
			)
			.option(
				'--sub-accounts <n>',
				'total number of sub-accounts that should exist',
				'1'
			)
			.option(
				'--dry-run',
				'print what would be created and the expected rent/fees, send nothing',
				false
			)
	).action(async (name: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as {
			authority?: string;
			vaultIndex: string;
			subAccounts: string;
			dryRun: boolean;
		};
		const count = Number.parseInt(local.subAccounts, 10);
		if (!Number.isInteger(count) || count < 1) {
			throw new Error(
				`--sub-accounts must be a positive integer, got "${local.subAccounts}"`
			);
		}
		const vaultIndex = Number.parseInt(local.vaultIndex, 10);
		const authority = resolveAuthority(opts, local.authority, vaultIndex);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, true, { authority });
		try {
			const userStats = await fetchUserStatsAccount(
				provider.connection,
				client.program,
				authority
			);
			// sub-account ids are sequential on-chain (id must equal
			// numberOfSubAccountsCreated), so resume from the created count.
			const created = userStats ? userStats.numberOfSubAccountsCreated : 0;
			console.log(
				`authority ${authority.toBase58()}: ${created} sub-account(s) created, target ${count}`
			);
			if (created >= count) {
				console.log('nothing to create');
				return;
			}

			const toCreate = count - created;
			const needStats = !userStats;
			// `as any`: indexing the typed account namespace blows tsc's
			// instantiation depth on the velocity IDL (TS2589).
			const accounts = client.program.account as any;
			const userRent =
				await provider.connection.getMinimumBalanceForRentExemption(
					accounts.user.size
				);
			const statsRent = needStats
				? await provider.connection.getMinimumBalanceForRentExemption(
						accounts.userStats.size
				  )
				: 0;
			const totalRent = userRent * toCreate + statsRent;
			console.log(
				`will create ${toCreate} sub-account(s)${
					needStats ? ' + UserStats' : ''
				}:`
			);
			for (let subAccountId = created; subAccountId < count; subAccountId++) {
				console.log(
					`  sub-account ${subAccountId}: ${getUserAccountPublicKeySync(
						client.program.programId,
						authority,
						subAccountId
					).toBase58()} name="${name}-${subAccountId}"`
				);
			}
			console.log(
				`rent: ${totalRent} lamports (~${(totalRent / 1e9).toFixed(
					4
				)} SOL), paid by ${provider.wallet.publicKey.toBase58()}`
			);
			console.log(
				`network fees: ~${5000 * toCreate} lamports (${toCreate} tx)`
			);
			if (local.dryRun) {
				console.log('dry run, nothing sent');
				return;
			}

			for (let subAccountId = created; subAccountId < count; subAccountId++) {
				const [ixs, userPk] = await client.getInitializeUserAccountIxs(
					subAccountId,
					`${name}-${subAccountId}`
				);
				const tx = new Transaction().add(...ixs);
				const signature = await provider.sendAndConfirm(tx);
				console.log(
					`✓ sub-account ${subAccountId}: ${userPk.toBase58()} name="${name}-${subAccountId}"`
				);
				console.log(`  signature: ${signature}`);
			}
			console.log(
				`userStats: ${getUserStatsAccountPublicKey(
					client.program.programId,
					authority
				).toBase58()}`
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		user
			.command('set-delegate <delegate>')
			.description(
				"Set the delegate wallet on the authority's sub-accounts (pass the system program id " +
					'11111111111111111111111111111111 to clear). The delegate can trade on the sub-accounts ' +
					"but can never withdraw. Requires the authority's signature — with --multisig the " +
					'instructions are batched into a single vault transaction proposal against the vault ' +
					'at --vault-index. Use --allow-transfer to also toggle the UserStats flag that lets ' +
					'the delegate move collateral between sub-accounts it controls.'
			)
			.option(
				'--authority <pubkey>',
				'user authority (default: signer, or vault PDA with --multisig)'
			)
			.option(
				'--vault-index <index>',
				'with --multisig, vault index used to derive the authority PDA and propose against',
				'0'
			)
			.option(
				'--sub-accounts <n>',
				'apply to sub-account ids 0..n-1 (default: all created sub-accounts)'
			)
			.option(
				'--allow-transfer <true|false>',
				'also set the authority-wide allowDelegateTransfer flag on UserStats'
			)
			.option(
				'--dry-run',
				'print the instructions and expected proposal rent/fees, send nothing',
				false
			)
	).action(async (delegate: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as {
			authority?: string;
			vaultIndex: string;
			subAccounts?: string;
			allowTransfer?: string;
			dryRun: boolean;
		};
		const delegatePk = new PublicKey(delegate);
		const vaultIndex = Number.parseInt(local.vaultIndex, 10);
		let allowTransfer: boolean | undefined;
		if (local.allowTransfer !== undefined) {
			if (local.allowTransfer !== 'true' && local.allowTransfer !== 'false') {
				throw new Error(
					`--allow-transfer must be "true" or "false", got "${local.allowTransfer}"`
				);
			}
			allowTransfer = local.allowTransfer === 'true';
		}
		const authority = resolveAuthority(opts, local.authority, vaultIndex);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, false);
		try {
			const userStats = await fetchUserStatsAccount(
				provider.connection,
				client.program,
				authority
			);
			if (!userStats) {
				throw new Error(
					`no UserStats for authority ${authority.toBase58()} — run "user init" first`
				);
			}
			const created = userStats.numberOfSubAccountsCreated;
			const count = local.subAccounts
				? Number.parseInt(local.subAccounts, 10)
				: created;
			if (!Number.isInteger(count) || count < 1 || count > created) {
				throw new Error(
					`--sub-accounts must be between 1 and ${created} (created sub-accounts), got "${local.subAccounts}"`
				);
			}
			const ixs = [];
			for (let subAccountId = 0; subAccountId < count; subAccountId++) {
				ixs.push(
					await client.getUpdateUserDelegateIx(delegatePk, {
						subAccountId,
						userAccountPublicKey: getUserAccountPublicKeySync(
							client.program.programId,
							authority,
							subAccountId
						),
						authority,
					})
				);
			}
			if (allowTransfer !== undefined) {
				// SDK builder hardcodes the wallet as authority; build directly so a
				// vault PDA authority works.
				ixs.push(
					await client.program.instruction.updateUserAllowDelegateTransfer(
						allowTransfer,
						{
							accounts: {
								userStats: getUserStatsAccountPublicKey(
									client.program.programId,
									authority
								),
								authority,
							},
						}
					)
				);
			}
			const label =
				`delegate=${delegatePk.toBase58()} on sub-accounts 0..${count - 1} ` +
				`authority=${authority.toBase58()}` +
				(allowTransfer !== undefined
					? ` allowDelegateTransfer=${allowTransfer}`
					: '');
			if (local.dryRun) {
				console.log(label);
				await reportDryRun(
					provider,
					ixs,
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					vaultIndex
				);
				return;
			}
			const result = await sendOrPropose(
				provider,
				ixs,
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin user set-delegate',
				vaultIndex
			);
			reportDispatch(label, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		user
			.command('deposit <market> <amount>')
			.description(
				'Deposit into a spot market as the user authority. <amount> is raw token units. ' +
					'With --multisig the authority defaults to the vault 0 PDA and the deposit is proposed as a vault transaction.'
			)
			.option(
				'--authority <pubkey>',
				'user authority (default: signer, or vault PDA with --multisig)'
			)
			.option(
				'--vault-index <index>',
				'with --multisig, vault index used to derive the authority PDA and propose against',
				'0'
			)
			.option('--sub-account <id>', 'user sub-account id', '0')
			.option(
				'--user-token-account <pubkey>',
				"source token account (default: the authority's ATA for the market mint)"
			)
			.option('--reduce-only', 'only reduce an existing borrow', false)
			.option(
				'--dry-run',
				'print the instruction and expected proposal rent/fees, send nothing',
				false
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as {
			authority?: string;
			vaultIndex: string;
			subAccount: string;
			userTokenAccount?: string;
			reduceOnly: boolean;
			dryRun: boolean;
		};
		const marketIndex = Number.parseInt(market, 10);
		const subAccountId = Number.parseInt(local.subAccount, 10);
		const vaultIndex = Number.parseInt(local.vaultIndex, 10);
		const authority = resolveAuthority(opts, local.authority, vaultIndex);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, true, {
			authority,
			subAccountId,
		});
		try {
			const spotMarket = (client as any).getSpotMarketAccountOrThrow(
				marketIndex
			);
			const tokenProgram = (client as any).getTokenProgramForSpotMarket(
				spotMarket
			);
			const userTokenAccount = local.userTokenAccount
				? new PublicKey(local.userTokenAccount)
				: deriveAssociatedTokenAccount(
						spotMarket.mint,
						authority,
						tokenProgram
				  );
			const ix = await (client as any).getDepositInstruction(
				new BN(amount),
				marketIndex,
				userTokenAccount,
				subAccountId,
				local.reduceOnly,
				true,
				{ authority }
			);
			const label = `deposit spot[${marketIndex}] amount=${amount} authority=${authority.toBase58()} sub=${subAccountId}`;
			if (local.dryRun) {
				console.log(label);
				await reportDryRun(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					vaultIndex
				);
				return;
			}
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin user deposit',
				vaultIndex
			);
			reportDispatch(label, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		user
			.command('withdraw <market> <amount>')
			.description(
				'Withdraw from a spot market as the user authority. <amount> is raw token units. ' +
					'With --multisig the authority defaults to the vault 0 PDA and the withdraw is proposed as a vault transaction.'
			)
			.option(
				'--authority <pubkey>',
				'user authority (default: signer, or vault PDA with --multisig)'
			)
			.option(
				'--vault-index <index>',
				'with --multisig, vault index used to derive the authority PDA and propose against',
				'0'
			)
			.option('--sub-account <id>', 'user sub-account id', '0')
			.option(
				'--user-token-account <pubkey>',
				"destination token account (default: the authority's ATA for the market mint; must exist)"
			)
			.option('--reduce-only', 'never flip the position into a borrow', false)
			.option(
				'--dry-run',
				'print the instruction and expected proposal rent/fees, send nothing',
				false
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as {
			authority?: string;
			vaultIndex: string;
			subAccount: string;
			userTokenAccount?: string;
			reduceOnly: boolean;
			dryRun: boolean;
		};
		const marketIndex = Number.parseInt(market, 10);
		const subAccountId = Number.parseInt(local.subAccount, 10);
		const vaultIndex = Number.parseInt(local.vaultIndex, 10);
		const authority = resolveAuthority(opts, local.authority, vaultIndex);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, true, {
			authority,
			subAccountId,
		});
		try {
			const spotMarket = (client as any).getSpotMarketAccountOrThrow(
				marketIndex
			);
			const tokenProgram = (client as any).getTokenProgramForSpotMarket(
				spotMarket
			);
			const userTokenAccount = local.userTokenAccount
				? new PublicKey(local.userTokenAccount)
				: deriveAssociatedTokenAccount(
						spotMarket.mint,
						authority,
						tokenProgram
				  );
			const ix = await (client as any).getWithdrawIx(
				new BN(amount),
				marketIndex,
				userTokenAccount,
				local.reduceOnly,
				subAccountId,
				{ authority }
			);
			const label = `withdraw spot[${marketIndex}] amount=${amount} authority=${authority.toBase58()} sub=${subAccountId}`;
			if (local.dryRun) {
				console.log(label);
				await reportDryRun(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					vaultIndex
				);
				return;
			}
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin user withdraw',
				vaultIndex
			);
			reportDispatch(label, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		user
			.command('set-special-status <user> <status>')
			.description(
				'Toggle a per-user special status flag. <status> is a u8 bitfield.'
			)
	).action(async (userPk: string, status: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await (client as any).getUpdateUserSpecialStatusIx(
				new PublicKey(userPk),
				Number.parseInt(status, 10)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin user set-special-status'
			);
			reportDispatch(`user[${userPk}] special-status = ${status}`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		user
			.command('set-equity-floor <user> <floor>')
			.description(
				'Set a user account equity floor (warm admin). <floor> is QUOTE_PRECISION (1e6) raw units; ' +
					'below the floor the program rejects risk-increasing orders, fills, withdrawals and transfers. 0 disables.'
			)
	).action(async (userPk: string, floor: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await (client as any).getUpdateUserEquityFloorIx(
				new PublicKey(userPk),
				new BN(floor)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin user set-equity-floor'
			);
			reportDispatch(`user[${userPk}] equity-floor = ${floor}`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		user
			.command('reset-equity-breaker <userStats>')
			.description(
				'Clear the authority-wide equity floor breaker on a UserStats account (warm admin). ' +
					'Unfreezes all subaccounts of the authority after a breach has been reviewed.'
			)
	).action(async (userStatsPk: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await (client as any).getResetEquityFloorBreakerIx(
				new PublicKey(userStatsPk)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin user reset-equity-breaker'
			);
			reportDispatch(`userStats[${userStatsPk}] equity breaker reset`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		user
			.command('admin-deposit <market> <amount>')
			.description(
				'Admin deposit on behalf of a user. <amount> is raw token units.'
			)
			.requiredOption('--user <pubkey>', 'target user account being credited')
			.requiredOption(
				'--user-token-account <pubkey>',
				'admin signer ATA funding the deposit'
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { user: string; userTokenAccount: string };
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await (client as any).getAdminDepositIx(
				Number.parseInt(market, 10),
				new BN(amount),
				new PublicKey(local.user),
				new PublicKey(local.userTokenAccount)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin user admin-deposit'
			);
			reportDispatch(
				`admin-deposit spot[${market}] amount=${amount} → ${local.user}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});
}
