import { Command } from 'commander';
import {
	ComputeBudgetProgram,
	Keypair,
	PublicKey,
	Transaction,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import * as multisig from '@sqds/multisig';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildProvider } from '../lib/provider';
import { confirmMainnetDirect } from '../lib/context';

const { Permission, Permissions } = multisig.types;

export function registerMultisig(parent: Command): void {
	const ms = parent
		.command('multisig')
		.description('Squads V4 multisig management.');

	withGlobalOptions(
		ms
			.command('create')
			.description(
				[
					'Create a Squads V4 multisig with the current wallet as a 1/1 signer',
					'(full Initiate/Vote/Execute permissions) plus a proposer member that',
					'can only Initiate transactions. Intended for devnet bring-up.',
				].join('\n')
			)
			.requiredOption(
				'--proposer <pubkey>',
				'member granted Initiate-only permission (can propose, not vote/execute)'
			)
			.option(
				'--name <name>',
				'multisig name (stored as the create memo)',
				'Velocity Devnet Multisig'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const localOpts = cmd.optsWithGlobals();
		const proposer = new PublicKey(localOpts.proposer as string);
		const name = localOpts.name as string;

		const creator = provider.wallet.publicKey;

		// The createKey is an ephemeral signer that seeds the multisig PDA.
		const createKey = Keypair.generate();
		const [multisigPda] = multisig.getMultisigPda({
			createKey: createKey.publicKey,
		});
		const [vaultPda] = multisig.getVaultPda({ multisigPda, index: 0 });

		// Treasury is read from the on-chain Squads program config.
		const [programConfigPda] = multisig.getProgramConfigPda({});
		const programConfig =
			await multisig.accounts.ProgramConfig.fromAccountAddress(
				provider.connection,
				programConfigPda
			);

		const createIx = multisig.instructions.multisigCreateV2({
			createKey: createKey.publicKey,
			creator,
			multisigPda,
			configAuthority: null,
			timeLock: 0,
			threshold: 1,
			rentCollector: null,
			treasury: programConfig.treasury,
			memo: name,
			members: [
				{ key: creator, permissions: Permissions.all() },
				{
					key: proposer,
					permissions: Permissions.fromPermissions([Permission.Initiate]),
				},
			],
		});

		const tx = new Transaction().add(createIx);
		const signature = await provider.sendAndConfirm(tx, [createKey]);

		console.log(`✓ created multisig "${name}"`);
		console.log(`  multisig PDA: ${multisigPda.toBase58()}`);
		console.log(`  vault (index 0): ${vaultPda.toBase58()}`);
		console.log(`  threshold: 1/1`);
		console.log(`  signer (all perms): ${creator.toBase58()}`);
		console.log(`  proposer (initiate-only): ${proposer.toBase58()}`);
		console.log(`  signature: ${signature}`);
	});

	withGlobalOptions(
		ms
			.command('proposals')
			.description(
				'List recent proposals on the multisig (from --multisig or the profile): status, approval count, and, for approved proposals, when the timelock allows execution.'
			)
			.option('--limit <n>', 'how many recent proposals to list', '10')
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		if (!opts.multisig) {
			throw new Error(
				'no multisig: pass --multisig <pda> or use a profile that has one'
			);
		}
		const provider = buildProvider(opts);
		const multisigPda = new PublicKey(opts.multisig);
		const limit = Number.parseInt(cmd.optsWithGlobals().limit as string, 10);

		const info = await multisig.accounts.Multisig.fromAccountAddress(
			provider.connection,
			multisigPda
		);
		const latest = Number(info.transactionIndex);
		const stale = Number(info.staleTransactionIndex);
		const timeLock = Number(info.timeLock);
		console.log(
			`multisig ${multisigPda.toBase58()}: threshold ${info.threshold}/${
				info.members.length
			}, timelock ${timeLock}s, ${latest} proposal(s) total`
		);
		if (latest === 0) {
			return;
		}

		const first = Math.max(1, latest - limit + 1);
		const pdas: PublicKey[] = [];
		for (let i = latest; i >= first; i--) {
			pdas.push(
				multisig.getProposalPda({
					multisigPda,
					transactionIndex: BigInt(i),
				})[0]
			);
		}
		const accounts = await provider.connection.getMultipleAccountsInfo(pdas);

		const now = Math.floor(Date.now() / 1000);
		accounts.forEach((acc, offset) => {
			const index = latest - offset;
			if (!acc) {
				console.log(
					`  #${index}  (no proposal account: closed or vault tx only)`
				);
				return;
			}
			const [proposal] = multisig.accounts.Proposal.fromAccountInfo(acc);
			const status = proposal.status.__kind;
			const approvals = `${proposal.approved.length}/${info.threshold}`;
			let extra = '';
			if (status === 'Approved') {
				const approvedAt = Number(proposal.status.timestamp);
				const executableAt = approvedAt + timeLock;
				extra =
					executableAt <= now
						? ', executable NOW'
						: `, executable in ${formatDuration(executableAt - now)}`;
			}
			if (status === 'Active' && index <= stale) {
				extra = ', STALE (superseded, cannot execute)';
			}
			console.log(
				`  #${index}  ${status.padEnd(9)} approvals ${approvals}${extra}`
			);
		});
	});

	withGlobalOptions(
		ms
			.command('execute <index>')
			.description(
				'Execute an approved vault transaction as the signer (must be a multisig member ' +
					'with Execute permission). Sets a compute-unit limit on the execute transaction — ' +
					'the Squads UI executes with the 200k default, which CPI-heavy inner transactions ' +
					'(e.g. Jupiter swaps) exceed.'
			)
			.option(
				'--cu-limit <units>',
				'compute-unit limit for the execute transaction',
				'1400000'
			)
			.option(
				'--cu-price <microLamports>',
				'priority fee per compute unit (optional)'
			)
	).action(async (indexArg: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { cuLimit: string; cuPrice?: string };
		if (!opts.multisig) {
			throw new Error(
				'no multisig: pass --multisig <pda> or use a profile that has one'
			);
		}
		const transactionIndex = BigInt(Number.parseInt(indexArg, 10));
		const cuLimit = Number.parseInt(local.cuLimit, 10);
		if (!Number.isInteger(cuLimit) || cuLimit < 1 || cuLimit > 1_400_000) {
			throw new Error(
				`--cu-limit must be between 1 and 1400000, got "${local.cuLimit}"`
			);
		}
		const provider = buildProvider(opts);
		const multisigPda = new PublicKey(opts.multisig);
		const member = provider.wallet.publicKey;

		const { instruction, lookupTableAccounts } =
			await multisig.instructions.vaultTransactionExecute({
				connection: provider.connection,
				multisigPda,
				transactionIndex,
				member,
			});

		const ixs = [ComputeBudgetProgram.setComputeUnitLimit({ units: cuLimit })];
		if (local.cuPrice !== undefined) {
			ixs.push(
				ComputeBudgetProgram.setComputeUnitPrice({
					microLamports: Number.parseInt(local.cuPrice, 10),
				})
			);
		}
		ixs.push(instruction);

		await confirmMainnetDirect(
			`execute proposal #${transactionIndex} on ${multisigPda.toBase58()}`
		);
		const { blockhash } = await provider.connection.getLatestBlockhash();
		const message = new TransactionMessage({
			payerKey: member,
			recentBlockhash: blockhash,
			instructions: ixs,
		}).compileToV0Message(lookupTableAccounts);
		const signature = await provider.sendAndConfirm(
			new VersionedTransaction(message)
		);
		console.log(
			`✓ executed proposal #${transactionIndex} (cu-limit ${cuLimit})`
		);
		console.log(`  signature: ${signature}`);
	});

	withGlobalOptions(
		ms
			.command('inspect <index>')
			.description(
				'Decode a vault transaction: vault, inner instructions with resolved account ' +
					'keys (lookup tables fetched), and a simulation of its execution with a full ' +
					'compute budget. The simulation reports InvalidProposalStatus until the ' +
					'proposal is approved — that is the proposal gate, not a broken transaction.'
			)
			.option(
				'--accounts',
				'also print every resolved account key per instruction',
				false
			)
	).action(async (indexArg: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { accounts: boolean };
		if (!opts.multisig) {
			throw new Error(
				'no multisig: pass --multisig <pda> or use a profile that has one'
			);
		}
		const provider = buildProvider(opts);
		const multisigPda = new PublicKey(opts.multisig);
		const transactionIndex = BigInt(Number.parseInt(indexArg, 10));

		const [txPda] = multisig.getTransactionPda({
			multisigPda,
			index: transactionIndex,
		});
		const acc = await provider.connection.getAccountInfo(txPda);
		if (!acc) {
			throw new Error(
				`no vault transaction account for proposal #${transactionIndex} (closed, or a config transaction)`
			);
		}
		const [vaultTx] = multisig.accounts.VaultTransaction.fromAccountInfo(acc);
		const msg = vaultTx.message;

		// v0 convention: combined keys = static, then every table's writable
		// indexes, then every table's readonly indexes.
		const combined: string[] = msg.accountKeys.map((k) => k.toBase58());
		const tables: { key: PublicKey; addresses: PublicKey[] }[] = [];
		for (const lookup of msg.addressTableLookups) {
			const alt = await provider.connection.getAddressLookupTable(
				lookup.accountKey
			);
			if (!alt.value) {
				throw new Error(
					`lookup table ${lookup.accountKey.toBase58()} not found`
				);
			}
			tables.push({
				key: lookup.accountKey,
				addresses: alt.value.state.addresses,
			});
		}
		msg.addressTableLookups.forEach((lookup, t) => {
			for (const i of lookup.writableIndexes) {
				combined.push(tables[t].addresses[i].toBase58());
			}
		});
		msg.addressTableLookups.forEach((lookup, t) => {
			for (const i of lookup.readonlyIndexes) {
				combined.push(tables[t].addresses[i].toBase58());
			}
		});

		const [vaultPda] = multisig.getVaultPda({
			multisigPda,
			index: vaultTx.vaultIndex,
		});
		console.log(
			`#${transactionIndex} vault ${
				vaultTx.vaultIndex
			} (${vaultPda.toBase58()}), ` +
				`${msg.instructions.length} instruction(s), ${msg.accountKeys.length} static keys, ` +
				`${msg.addressTableLookups.length} lookup table(s)`
		);
		msg.instructions.forEach((ix, i) => {
			console.log(
				`  ix[${i}] program=${combined[ix.programIdIndex]} ` +
					`accounts=${ix.accountIndexes.length} data=${ix.data.length}B`
			);
			if (local.accounts) {
				Array.from(ix.accountIndexes).forEach((a, j) => {
					console.log(`    [${j}] ${combined[a]}`);
				});
			}
		});

		// Simulate execution with a full compute budget. Use any member with
		// Execute permission so the member gate passes.
		const info = await multisig.accounts.Multisig.fromAccountAddress(
			provider.connection,
			multisigPda
		);
		const executor =
			info.members.find((m) =>
				Permissions.has(m.permissions, Permission.Execute)
			)?.key ?? provider.wallet.publicKey;
		const { instruction, lookupTableAccounts } =
			await multisig.instructions.vaultTransactionExecute({
				connection: provider.connection,
				multisigPda,
				transactionIndex,
				member: new PublicKey(executor),
			});
		const { blockhash } = await provider.connection.getLatestBlockhash();
		const simMessage = new TransactionMessage({
			payerKey: new PublicKey(executor),
			recentBlockhash: blockhash,
			instructions: [
				ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }),
				instruction,
			],
		}).compileToV0Message(lookupTableAccounts);
		const sim = await provider.connection.simulateTransaction(
			new VersionedTransaction(simMessage),
			{ sigVerify: false, replaceRecentBlockhash: true }
		);
		if (sim.value.err) {
			console.log(
				`simulation FAILED: ${JSON.stringify(sim.value.err)} ` +
					`(consumed ${sim.value.unitsConsumed ?? '?'} CU)`
			);
			for (const line of (sim.value.logs ?? []).slice(-6)) {
				console.log(`  ${line}`);
			}
			if (JSON.stringify(sim.value.err).includes('6008')) {
				console.log(
					'  (InvalidProposalStatus: proposal not approved yet — expected before approval)'
				);
			}
		} else {
			console.log(
				`simulation OK: consumed ${sim.value.unitsConsumed} CU ` +
					'(execute via `multisig execute` — the Squads UI executes at the 200k default)'
			);
		}
	});

	withGlobalOptions(
		ms
			.command('set-rent-collector <pubkey>')
			.description(
				'Propose a config transaction setting the multisig rent collector — the ' +
					'account paid when settled proposal accounts are closed (`close-accounts` ' +
					'requires one). WARNING: executing any config transaction marks every ' +
					'still-Active vault proposal stale (Approved ones survive); do this when ' +
					'nothing important is pending. Signer must be a member with Initiate.'
			)
	).action(async (pubkey: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		if (!opts.multisig) {
			throw new Error(
				'no multisig: pass --multisig <pda> or use a profile that has one'
			);
		}
		const provider = buildProvider(opts);
		const multisigPda = new PublicKey(opts.multisig);
		const rentCollector = new PublicKey(pubkey);
		const info = await multisig.accounts.Multisig.fromAccountAddress(
			provider.connection,
			multisigPda
		);
		if (
			info.configAuthority &&
			!PublicKey.default.equals(new PublicKey(info.configAuthority))
		) {
			throw new Error(
				`multisig has a config authority (${new PublicKey(
					info.configAuthority
				).toBase58()}) — ` +
					'config changes go through it directly, not through proposals'
			);
		}
		const transactionIndex = BigInt(Number(info.transactionIndex) + 1);
		const createIx = multisig.instructions.configTransactionCreate({
			multisigPda,
			transactionIndex,
			creator: provider.wallet.publicKey,
			actions: [
				{ __kind: 'SetRentCollector', newRentCollector: rentCollector },
			],
			memo: 'velocity-admin multisig set-rent-collector',
		});
		const proposeIx = multisig.instructions.proposalCreate({
			multisigPda,
			transactionIndex,
			creator: provider.wallet.publicKey,
		});
		const tx = new Transaction().add(createIx, proposeIx);
		const signature = await provider.sendAndConfirm(tx);
		console.log(
			`✓ proposed rent collector ${rentCollector.toBase58()} as config tx #${transactionIndex}`
		);
		console.log(`  signature: ${signature}`);
		console.log(
			'  (members approve + execute via Squads UI; then `multisig close-accounts` can reclaim rent)'
		);
	});

	withGlobalOptions(
		ms
			.command('close-accounts')
			.description(
				'Reclaim rent from settled proposals: close the VaultTransaction + Proposal ' +
					'accounts of every Executed / Rejected / Cancelled proposal (and stale ' +
					'non-approved ones). Permissionless, but the multisig must have a rent ' +
					'collector configured — rent is paid to it, not to the original proposer. ' +
					'Approved-but-unexecuted proposals are never touched.'
			)
			.option(
				'--dry-run',
				'list closable proposals and expected rent, close nothing',
				false
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { dryRun: boolean };
		if (!opts.multisig) {
			throw new Error(
				'no multisig: pass --multisig <pda> or use a profile that has one'
			);
		}
		const provider = buildProvider(opts);
		const multisigPda = new PublicKey(opts.multisig);
		const info = await multisig.accounts.Multisig.fromAccountAddress(
			provider.connection,
			multisigPda
		);
		if (!info.rentCollector) {
			throw new Error(
				'multisig has no rent collector configured — set one (config transaction) ' +
					'before accounts can be closed'
			);
		}
		const rentCollector = new PublicKey(info.rentCollector);
		const latest = Number(info.transactionIndex);
		const stale = Number(info.staleTransactionIndex);

		const closable: bigint[] = [];
		let reclaimable = 0;
		for (let index = 1; index <= latest; index++) {
			const [txPda] = multisig.getTransactionPda({
				multisigPda,
				index: BigInt(index),
			});
			const [proposalPda] = multisig.getProposalPda({
				multisigPda,
				transactionIndex: BigInt(index),
			});
			const [txAcc, proposalAcc] =
				await provider.connection.getMultipleAccountsInfo([txPda, proposalPda]);
			if (!txAcc) {
				continue; // already closed, or a config transaction
			}
			let status = 'None';
			if (proposalAcc) {
				const [proposal] =
					multisig.accounts.Proposal.fromAccountInfo(proposalAcc);
				status = proposal.status.__kind;
			}
			const terminal =
				status === 'Executed' ||
				status === 'Rejected' ||
				status === 'Cancelled';
			// A stale approved vault tx is still executable — never close it.
			const staleClosable =
				index <= stale && (status === 'Active' || status === 'None');
			if (!terminal && !staleClosable) {
				continue;
			}
			closable.push(BigInt(index));
			reclaimable += txAcc.lamports + (proposalAcc ? proposalAcc.lamports : 0);
			console.log(
				`  #${index}  ${status.padEnd(9)} rent ${(
					(txAcc.lamports + (proposalAcc ? proposalAcc.lamports : 0)) /
					1e9
				).toFixed(4)} SOL`
			);
		}
		if (closable.length === 0) {
			console.log('nothing to close');
			return;
		}
		console.log(
			`${closable.length} proposal(s), ~${(reclaimable / 1e9).toFixed(
				4
			)} SOL to rent collector ${rentCollector.toBase58()}`
		);
		if (local.dryRun) {
			console.log('dry run, nothing closed');
			return;
		}

		// Batch a few closes per transaction to stay under the tx size limit.
		const BATCH = 8;
		for (let i = 0; i < closable.length; i += BATCH) {
			const batch = closable.slice(i, i + BATCH);
			const tx = new Transaction();
			for (const transactionIndex of batch) {
				tx.add(
					multisig.instructions.vaultTransactionAccountsClose({
						multisigPda,
						rentCollector,
						transactionIndex,
					})
				);
			}
			const signature = await provider.sendAndConfirm(tx);
			console.log(
				`✓ closed #${batch[0]}..#${batch[batch.length - 1]}: ${signature}`
			);
		}
	});
}

function formatDuration(seconds: number): string {
	const h = Math.floor(seconds / 3600);
	const m = Math.floor((seconds % 3600) / 60);
	if (h > 0) {
		return `${h}h ${m}m`;
	}
	const s = seconds % 60;
	return m > 0 ? `${m}m ${s}s` : `${s}s`;
}
