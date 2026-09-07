import { Command } from 'commander';
import {
	AddressLookupTableAccount,
	ComputeBudgetProgram,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	SystemProgram,
	Transaction,
	TransactionInstruction,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import { BorshInstructionCoder } from '@coral-xyz/anchor';
import * as multisig from '@sqds/multisig';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { confirmMainnetDirect } from '../lib/context';
import * as ui from '../lib/ui';

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
		ui.header(
			'proposals',
			pc.dim(
				`threshold ${info.threshold}/${info.members.length}` +
					`${timeLock > 0 ? `, timelock ${formatDuration(timeLock)}` : ''}`
			)
		);
		ui.kv('multisig', pc.dim(multisigPda.toBase58()));
		if (latest === 0) {
			ui.note('no proposals yet');
			return;
		}
		console.log('');

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
		const rows: string[][] = [];
		accounts.forEach((acc, offset) => {
			const index = latest - offset;
			if (!acc) {
				rows.push([
					pc.dim(`#${index}`),
					pc.dim('gone'),
					pc.dim('closed, or a vault transaction with no proposal'),
					'',
				]);
				return;
			}
			const [proposal] = multisig.accounts.Proposal.fromAccountInfo(acc);
			const status = proposal.status.__kind;
			const approvals = `${proposal.approved.length}/${info.threshold}`;
			let extra = '';
			let statusCell = pc.dim(status.toLowerCase());
			if (status === 'Executed') {
				statusCell = pc.green('executed');
			} else if (status === 'Approved') {
				const executableAt = Number(proposal.status.timestamp) + timeLock;
				statusCell = pc.green('approved');
				extra =
					executableAt <= now
						? pc.green('executable now')
						: pc.yellow(`executable in ${formatDuration(executableAt - now)}`);
			} else if (status === 'Active') {
				statusCell = index <= stale ? pc.red('stale') : pc.yellow('active');
				extra =
					index <= stale
						? pc.dim('superseded by a config change, cannot execute')
						: pc.dim(
								`${info.threshold - proposal.approved.length} more approval${
									info.threshold - proposal.approved.length === 1 ? '' : 's'
								} needed`
						  );
			}
			rows.push([
				pc.bold(`#${index}`),
				statusCell,
				pc.dim(`${approvals} approvals`),
				extra,
			]);
		});
		ui.table(rows);
		console.log('');
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
				'Read a pending proposal end to end: status and approvals, each inner ' +
					'instruction decoded to its name, arguments and named accounts, the ' +
					'account fields it would change (before -> after, simulated against ' +
					'current chain state), the program logs, and whether it can execute ' +
					'yet. Read-only: no keypair beyond the configured signer is used and ' +
					'nothing is sent.'
			)
			.option(
				'--raw',
				'also print full arguments and raw instruction data',
				false
			)
	).action(async (indexArg: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { raw: boolean };
		if (!opts.multisig) {
			throw new Error(
				'no multisig: pass --multisig <pda> or use a profile that has one'
			);
		}
		// subscribe: false: only the program's coders and the connection are
		// needed here, never cached account state.
		const client = await buildAdminClient(opts, false);
		const connection = client.connection;
		const program = client.program;
		const multisigPda = new PublicKey(opts.multisig);
		const transactionIndex = BigInt(Number.parseInt(indexArg, 10));

		const [txPda] = multisig.getTransactionPda({
			multisigPda,
			index: transactionIndex,
		});
		const acc = await connection.getAccountInfo(txPda);
		if (!acc) {
			throw new Error(
				`no vault transaction account for proposal #${transactionIndex} (closed, or a config transaction)`
			);
		}
		const [vaultTx] = multisig.accounts.VaultTransaction.fromAccountInfo(acc);
		const msg = vaultTx.message;
		const [vaultPda] = multisig.getVaultPda({
			multisigPda,
			index: vaultTx.vaultIndex,
		});

		const info = await multisig.accounts.Multisig.fromAccountAddress(
			connection,
			multisigPda
		);
		const [proposalPda] = multisig.getProposalPda({
			multisigPda,
			transactionIndex,
		});
		const proposalAcc = await connection.getAccountInfo(proposalPda);
		const proposal = proposalAcc
			? multisig.accounts.Proposal.fromAccountInfo(proposalAcc)[0]
			: undefined;

		// Resolve the message's account keys. v0 convention: static keys, then
		// every table's writable indexes, then every table's readonly indexes.
		const combined: string[] = msg.accountKeys.map((k) => k.toBase58());
		const lookupTableAccounts: AddressLookupTableAccount[] = [];
		for (const lookup of msg.addressTableLookups) {
			const alt = await connection.getAddressLookupTable(lookup.accountKey);
			if (!alt.value) {
				throw new Error(
					`lookup table ${lookup.accountKey.toBase58()} not found`
				);
			}
			lookupTableAccounts.push(alt.value);
		}
		msg.addressTableLookups.forEach((lookup, t) => {
			for (const i of lookup.writableIndexes) {
				combined.push(lookupTableAccounts[t].state.addresses[i].toBase58());
			}
		});
		msg.addressTableLookups.forEach((lookup, t) => {
			for (const i of lookup.readonlyIndexes) {
				combined.push(lookupTableAccounts[t].state.addresses[i].toBase58());
			}
		});
		const flags = accountFlags(msg, combined.length);

		// Names for the addresses a reader would otherwise have to look up.
		const known = new Map<string, string>([
			[vaultPda.toBase58(), `this multisig's vault ${vaultTx.vaultIndex}`],
			[multisigPda.toBase58(), 'this multisig'],
			[program.programId.toBase58(), 'velocity program'],
		]);

		const kind = proposal?.status.__kind ?? 'no proposal account';
		const approved = proposal?.approved.length ?? 0;
		const stale =
			Number(transactionIndex) <= Number(info.staleTransactionIndex);
		const verdict =
			kind === 'Executed'
				? ui.ok('executed')
				: kind === 'Active' && stale
				? ui.bad('stale')
				: kind === 'Active'
				? ui.warn(`${approved}/${info.threshold} approvals`)
				: kind === 'Approved'
				? ui.ok(`${approved}/${info.threshold} approvals`)
				: pc.dim(kind.toLowerCase());

		ui.header(`proposal #${transactionIndex}`, verdict);
		ui.kv('status', `${kind}, ${approved} of ${info.threshold} approvals`);
		if (kind === 'Active') {
			const need = info.threshold - approved;
			if (stale) {
				ui.kv(
					'',
					pc.red('superseded by a config change, it can no longer execute')
				);
			} else if (need > 0) {
				ui.kv(
					'',
					pc.yellow(
						`${need} more approval${need === 1 ? '' : 's'} needed to execute`
					)
				);
			}
		}
		ui.kv('multisig', pc.dim(multisigPda.toBase58()));
		ui.kv('proposer', pc.dim(vaultTx.creator.toBase58()));
		ui.kv(
			'runs as',
			`${vaultPda.toBase58()} ${pc.dim(`(vault ${vaultTx.vaultIndex})`)}`
		);

		// Rebuild the inner instructions so they can be decoded and simulated.
		const inner = msg.instructions.map(
			(ix) =>
				new TransactionInstruction({
					programId: new PublicKey(combined[ix.programIdIndex]),
					keys: Array.from(ix.accountIndexes).map((a) => ({
						pubkey: new PublicKey(combined[a]),
						isSigner: flags.isSigner(a),
						isWritable: flags.isWritable(a),
					})),
					data: Buffer.from(ix.data),
				})
		);

		ui.header(
			'what it does',
			pc.dim(`${inner.length} instruction${inner.length === 1 ? '' : 's'}`)
		);
		inner.forEach((ix, i) => {
			const isVelocity = ix.programId.equals(program.programId);
			const decoded = isVelocity
				? (program.coder.instruction as BorshInstructionCoder).decode(ix.data)
				: null;
			const system = describeSystemIx(ix);
			const step = pc.dim(`${i + 1}.`);
			if (decoded) {
				ui.line(`${step} ${pc.dim('velocity')} ${pc.bold(decoded.name)}`);
			} else if (system) {
				ui.line(`${step} ${pc.dim('system')} ${pc.bold(system.name)}`);
			} else {
				ui.line(
					`${step} ${pc.yellow('cannot decode')} ${pc.dim(
						isVelocity
							? 'unknown discriminator on the velocity program'
							: `program ${ix.programId.toBase58()}`
					)}`
				);
			}

			const rows: string[][] = [];
			if (decoded) {
				const args = flatten(decoded.data);
				const shown = local.raw ? args : args.slice(0, 20);
				for (const { path, value } of shown) {
					rows.push([pc.dim(path), pc.bold(value)]);
				}
				if (args.length > shown.length) {
					rows.push([
						pc.dim(`+${args.length - shown.length} more fields`),
						pc.dim('--raw for all'),
					]);
				}
			}
			for (const detail of system?.detail ?? []) {
				const [path, ...rest] = detail.split(': ');
				rows.push([pc.dim(path), pc.bold(rest.join(': '))]);
			}
			if (local.raw || (!decoded && !system)) {
				rows.push([
					pc.dim(`raw data (${ix.data.length}B)`),
					pc.dim(ix.data.toString('hex')),
				]);
			}
			if (rows.length > 0) {
				ui.table(rows, '      ');
			}

			const idlAccounts = decoded
				? (
						program.idl.instructions.find(
							(entry) => entry.name === decoded.name
						)?.accounts ?? []
				  ).map((a) => a.name)
				: [];
			ui.table(
				ix.keys.map((k, j) => {
					const note = known.get(k.pubkey.toBase58());
					return [
						pc.dim(idlAccounts[j] ?? `account ${j}`),
						k.isSigner ? pc.yellow('signer') : pc.dim('·'),
						k.isWritable ? pc.yellow('writable') : pc.dim('read-only'),
						pc.dim(k.pubkey.toBase58()),
						note ? pc.dim(`(${note})`) : '',
					];
				}),
				'      '
			);
			if (i < inner.length - 1) {
				console.log('');
			}
		});

		// Effect preview. Simulating the inner instructions directly, with the
		// vault as fee payer so it is marked a signer, shows what they would do
		// against current chain state. This works at any proposal status,
		// unlike simulating the Squads execute wrapper, which is gated on
		// approval.
		const writable = [
			...new Set(
				inner.flatMap((ix) =>
					ix.keys.filter((k) => k.isWritable).map((k) => k.pubkey.toBase58())
				)
			),
		];
		const { blockhash } = await connection.getLatestBlockhash();
		const simulate = (instructions: TransactionInstruction[]) =>
			connection.simulateTransaction(
				new VersionedTransaction(
					new TransactionMessage({
						payerKey: vaultPda,
						recentBlockhash: blockhash,
						instructions: [
							ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }),
							...instructions,
						],
					}).compileToV0Message(lookupTableAccounts)
				),
				{
					sigVerify: false,
					replaceRecentBlockhash: true,
					accounts: { encoding: 'base64', addresses: writable },
				}
			);
		// Baseline from a simulation rather than getMultipleAccountsInfo, so
		// both snapshots come from the same kind of call back to back. Accounts
		// other programs write constantly (a perp market's mm-oracle fields,
		// cranked every few hundred ms) would otherwise show up as changes this
		// proposal makes. `accounts.addresses` may not exceed the number of
		// accounts in the simulated transaction, so the baseline references
		// each watched account with a zero-lamport transfer, which touches
		// nothing: the vault is system-owned and carries no data, and crediting
		// zero leaves the destination byte-identical.
		const effect = await simulate(inner);
		const baseline = await simulate(
			writable
				.filter((k) => k !== vaultPda.toBase58())
				.map((k) =>
					SystemProgram.transfer({
						fromPubkey: vaultPda,
						toPubkey: new PublicKey(k),
						lamports: 0,
					})
				)
		);
		let before: (Buffer | undefined)[];
		let baselineNote: string | undefined;
		if (!baseline.value.err && baseline.value.accounts) {
			before = baseline.value.accounts.map((a) =>
				a ? Buffer.from(a.data[0], 'base64') : undefined
			);
		} else {
			// Some account rejected the no-op reference. Fall back to a plain
			// read, which is a slot or two off and can show unrelated writes.
			const fetched = await connection.getMultipleAccountsInfo(
				writable.map((k) => new PublicKey(k))
			);
			before = fetched.map((a) => a?.data);
			baselineNote =
				'before-state was read separately from the simulation, so a field ' +
				'another program writes continuously may appear below';
		}
		const slotDrift = Math.abs(effect.context.slot - baseline.context.slot);

		ui.header(
			'what changes on chain',
			effect.value.err
				? ui.bad('these instructions fail right now')
				: ui.ok(
						`simulates clean, ${ui.count(effect.value.unitsConsumed ?? 0)} CU`
				  )
		);
		if (effect.value.err) {
			ui.line(pc.red(JSON.stringify(effect.value.err)));
			ui.note('executing the proposal in this state would not go through.');
		} else {
			let changed = 0;
			writable.forEach((key, i) => {
				const prev = before[i];
				const raw = effect.value.accounts?.[i];
				if (!prev || !raw) {
					return;
				}
				const next = Buffer.from(raw.data[0], 'base64');
				if (prev.equals(next)) {
					return;
				}
				changed++;
				const type = accountType(program.idl.accounts ?? [], prev);
				const note = known.get(key);
				if (changed > 1) {
					console.log('');
				}
				const typeLabel = type
					? type.charAt(0).toUpperCase() + type.slice(1)
					: 'account';
				ui.line(
					`${pc.bold(typeLabel)} ${pc.dim(key)}${
						note ? ` ${pc.dim(`(${note})`)}` : ''
					}`
				);
				if (!type) {
					ui.note(
						`${prev.length}B → ${next.length}B, not a velocity account so fields are not decoded`,
						'      '
					);
					return;
				}
				const decodedAfter = program.coder.accounts.decode(type, next);
				const diff = diffFields(
					program.coder.accounts.decode(type, prev),
					decodedAfter
				);
				if (diff.length === 0) {
					ui.note('bytes differ but no decoded field changed', '      ');
					return;
				}
				ui.table(
					diff.map(({ path, from, to }) => [
						pc.dim(path),
						ui.change(from, to),
						pc.dim(explainChange(type, path, decodedAfter) ?? ''),
					]),
					'      '
				);
			});
			if (changed === 0) {
				ui.line(pc.dim("nothing: no writable account's data changes"));
				if (proposal?.status.__kind === 'Executed') {
					ui.note('this proposal already executed, its effect is on chain');
				}
			}
			if (baselineNote) {
				ui.note(baselineNote);
			} else if (slotDrift !== 0) {
				ui.note(
					`chain moved ${slotDrift} slot${
						slotDrift === 1 ? '' : 's'
					} between snapshots; a continuously-written field may show here`
				);
			}
		}

		const logs = effect.value.logs ?? [];
		const programLogs = logs.filter((l) => l.startsWith('Program log:'));
		const shownLogs = local.raw || effect.value.err ? logs : programLogs;
		if (shownLogs.length > 0) {
			ui.header('program logs');
			let truncated = false;
			for (const entry of shownLogs) {
				// Some admin handlers log a whole struct on one line; keep the
				// screen readable and leave the full text to --raw.
				const text = entry.replace(/^Program log: /, '');
				if (!local.raw && text.length > 160) {
					ui.line(pc.dim(`${text.slice(0, 160)}…`));
					truncated = true;
				} else {
					ui.line(pc.dim(text));
				}
			}
			if (
				!local.raw &&
				(truncated || (!effect.value.err && logs.length > shownLogs.length))
			) {
				ui.note('--raw for the full log, including invoke and success lines');
			}
		}

		// Readiness: can a member execute it right now, through Squads?
		const executor =
			info.members.find((m) =>
				Permissions.has(m.permissions, Permission.Execute)
			)?.key ?? client.wallet.publicKey;
		const execIx = await multisig.instructions.vaultTransactionExecute({
			connection,
			multisigPda,
			transactionIndex,
			member: new PublicKey(executor),
		});
		const readiness = await connection.simulateTransaction(
			new VersionedTransaction(
				new TransactionMessage({
					payerKey: new PublicKey(executor),
					recentBlockhash: blockhash,
					instructions: [
						ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }),
						execIx.instruction,
					],
				}).compileToV0Message(execIx.lookupTableAccounts)
			),
			{ sigVerify: false, replaceRecentBlockhash: true }
		);
		const settled = ['Executed', 'Cancelled', 'Rejected'];
		if (!readiness.value.err) {
			ui.header('can it execute now', ui.ok('yes'));
			ui.line(
				`${pc.bold(`velocity-admin multisig execute ${transactionIndex}`)}` +
					`  ${pc.dim(`(${ui.count(readiness.value.unitsConsumed ?? 0)} CU)`)}`
			);
			ui.note(
				'the Squads UI executes at the 200k default, too low for CPI-heavy inner transactions'
			);
		} else if (proposal && settled.includes(proposal.status.__kind)) {
			const kindLower = proposal.status.__kind.toLowerCase();
			ui.header('can it execute now', pc.dim(`already ${kindLower}`));
			ui.note(`nothing left to do with this proposal`);
		} else if (JSON.stringify(readiness.value.err).includes('6008')) {
			ui.header('can it execute now', ui.warn('not yet'));
			ui.note(
				'waiting on approvals. That is the approval gate, not a fault in the transaction.'
			);
		} else {
			ui.header('can it execute now', ui.bad('no'));
			ui.line(pc.red(JSON.stringify(readiness.value.err)));
			for (const entry of (readiness.value.logs ?? []).slice(-6)) {
				ui.note(entry);
			}
		}
		console.log('');
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

/**
 * Signer/writable flags for a compiled multisig message. The header counts
 * describe the static keys only, which are ordered writable signers, read-only
 * signers, writable non-signers, read-only non-signers. Keys loaded from an
 * address lookup table follow, every table's writable indexes first, and are
 * never signers.
 */
function accountFlags(
	msg: multisig.generated.VaultTransactionMessage,
	totalKeys: number
): { isSigner: (i: number) => boolean; isWritable: (i: number) => boolean } {
	const staticLen = msg.accountKeys.length;
	const writableFromTables = msg.addressTableLookups.reduce(
		(n, l) => n + l.writableIndexes.length,
		0
	);
	return {
		isSigner: (i) => i < msg.numSigners,
		isWritable: (i) => {
			if (i < staticLen) {
				return (
					i < msg.numWritableSigners ||
					(i >= msg.numSigners &&
						i < msg.numSigners + msg.numWritableNonSigners)
				);
			}
			return i - staticLen < writableFromTables && i < totalKeys;
		},
	};
}

/** Render one decoded value as a single line: pubkeys, BNs, anchor enums. */
function formatValue(value: unknown): string {
	if (value === null || value === undefined) {
		return String(value);
	}
	if (value instanceof PublicKey) {
		return value.toBase58();
	}
	if (Buffer.isBuffer(value)) {
		return `0x${value.toString('hex')}`;
	}
	if (typeof value === 'object') {
		const obj = value as Record<string, unknown>;
		// Anchor renders a unit enum variant as { variantName: {} }.
		const keys = Object.keys(obj);
		if (
			keys.length === 1 &&
			typeof obj[keys[0]] === 'object' &&
			obj[keys[0]] !== null &&
			Object.keys(obj[keys[0]] as object).length === 0
		) {
			return keys[0];
		}
		if (typeof (obj as { toString?: unknown }).toString === 'function') {
			const s = String(value);
			if (s !== '[object Object]') {
				return s;
			}
		}
		return JSON.stringify(value);
	}
	return String(value);
}

/** True for values that render as one line rather than being walked into. */
function isLeaf(value: unknown): boolean {
	if (value === null || value === undefined) {
		return true;
	}
	if (typeof value !== 'object') {
		return true;
	}
	if (value instanceof PublicKey || Buffer.isBuffer(value)) {
		return true;
	}
	// BN and friends: objects that stringify to something meaningful.
	return (
		!Array.isArray(value) &&
		String(value) !== '[object Object]' &&
		Object.keys(value).length > 0 &&
		formatValue(value) !== JSON.stringify(value)
	);
}

/** Flatten a decoded struct to `path: value` lines, deepest field last. */
function flatten(
	value: unknown,
	prefix = ''
): { path: string; value: string }[] {
	if (isLeaf(value)) {
		return [{ path: prefix || 'value', value: formatValue(value) }];
	}
	const out: { path: string; value: string }[] = [];
	const entries = Array.isArray(value)
		? value.map((v, i) => [String(i), v] as const)
		: Object.entries(value as Record<string, unknown>);
	for (const [key, child] of entries) {
		out.push(
			...flatten(child, prefix ? `${key}` : key).map((e) => ({
				path: prefix ? `${prefix}.${e.path}` : e.path,
				value: e.value,
			}))
		);
	}
	return out;
}

/** Fields whose rendered value differs between two decoded accounts. */
function diffFields(
	before: unknown,
	after: unknown
): { path: string; from: string; to: string }[] {
	const b = new Map(flatten(before).map((e) => [e.path, e.value]));
	const a = new Map(flatten(after).map((e) => [e.path, e.value]));
	const out: { path: string; from: string; to: string }[] = [];
	for (const [path, from] of b) {
		const to = a.get(path);
		if (to !== undefined && to !== from) {
			out.push({ path, from, to });
		}
	}
	return out;
}

/**
 * The System program instructions a vault realistically proposes. Moving SOL
 * is the common non-velocity case in a treasury multisig, and "raw data (12B):
 * 0200…" tells a reviewer nothing about how much is leaving.
 */
function describeSystemIx(
	ix: TransactionInstruction
): { name: string; detail: string[] } | undefined {
	if (!ix.programId.equals(SystemProgram.programId) || ix.data.length < 4) {
		return undefined;
	}
	const sol = (lamports: bigint) =>
		`${lamports} lamports (${(Number(lamports) / LAMPORTS_PER_SOL).toFixed(
			9
		)} SOL)`;
	switch (ix.data.readUInt32LE(0)) {
		case 0:
			return ix.data.length >= 12
				? {
						name: 'createAccount',
						detail: [`lamports: ${sol(ix.data.readBigUInt64LE(4))}`],
				  }
				: undefined;
		case 2:
			return ix.data.length >= 12
				? {
						name: 'transfer',
						detail: [`amount: ${sol(ix.data.readBigUInt64LE(4))}`],
				  }
				: undefined;
		default:
			return undefined;
	}
}

/**
 * Plain-language consequence for the field diffs whose raw value hides what
 * actually changed for users. A fee numerator is meaningless without its
 * denominator, and a promo tier index is meaningless without the schedule it
 * points into; both are read from the simulated post-state, so the note is
 * derived from the same bytes as the diff rather than from assumptions.
 */
function explainChange(
	accountType: string,
	path: string,
	after: Record<string, unknown>
): string | undefined {
	// Anchor lowercases IDL account names when it builds the Program's coder,
	// so this sees "state" where the IDL file says "State".
	if (accountType.toLowerCase() !== 'state') {
		return undefined;
	}
	const bps = (tier: Record<string, unknown>): string | undefined => {
		const numerator = Number(tier?.feeNumerator);
		const denominator = Number(tier?.feeDenominator);
		if (!Number.isFinite(numerator) || !denominator) {
			return undefined;
		}
		return `${+((numerator / denominator) * 10_000).toFixed(4)} bps`;
	};
	const tiers = ((
		after.perpFeeStructure as { feeTiers?: Record<string, unknown>[] }
	)?.feeTiers ?? []) as Record<string, unknown>[];

	if (path === 'promoFeeTier') {
		const index = Number(after.promoFeeTier);
		if (index === 0) {
			return 'promo off, every account pays its own volume tier';
		}
		const rate = tiers[index] ? bps(tiers[index]) : undefined;
		return `every account now pays at least tier ${index}${
			rate ? ` (${rate} taker)` : ''
		}`;
	}
	const tierMatch = /^perpFeeStructure\.feeTiers\.(\d+)\.feeNumerator$/.exec(
		path
	);
	if (tierMatch) {
		const rate = tiers[Number(tierMatch[1])]
			? bps(tiers[Number(tierMatch[1])])
			: undefined;
		return rate ? `tier ${tierMatch[1]} taker fee is now ${rate}` : undefined;
	}
	return undefined;
}

/** Which IDL account type this data is, by its 8-byte discriminator. */
function accountType(
	idlAccounts: { name: string; discriminator?: number[] }[],
	data: Buffer
): string | undefined {
	if (data.length < 8) {
		return undefined;
	}
	const head = data.subarray(0, 8);
	for (const account of idlAccounts) {
		if (
			account.discriminator &&
			head.equals(Buffer.from(account.discriminator))
		) {
			return account.name;
		}
	}
	return undefined;
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
