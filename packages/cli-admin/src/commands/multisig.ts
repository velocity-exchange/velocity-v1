import { Command } from 'commander';
import { Keypair, PublicKey, Transaction } from '@solana/web3.js';
import * as multisig from '@sqds/multisig';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildProvider } from '../lib/provider';

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
				'List recent proposals on the multisig (from --multisig or the profile): status, approval count, and — for approved proposals — when the timelock allows execution.'
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
		const limit = Number.parseInt(
			cmd.optsWithGlobals().limit as string,
			10
		);

		const info = await multisig.accounts.Multisig.fromAccountAddress(
			provider.connection,
			multisigPda
		);
		const latest = Number(info.transactionIndex);
		const stale = Number(info.staleTransactionIndex);
		const timeLock = Number(info.timeLock);
		console.log(
			`multisig ${multisigPda.toBase58()} — threshold ${
				info.threshold
			}/${info.members.length}, timelock ${timeLock}s, ${latest} proposal(s) total`
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
				console.log(`  #${index}  (no proposal account — closed or vault tx only)`);
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
						? ' — executable NOW'
						: ` — executable in ${formatDuration(executableAt - now)}`;
			}
			if (status === 'Active' && index <= stale) {
				extra = ' — STALE (superseded, cannot execute)';
			}
			console.log(`  #${index}  ${status.padEnd(9)} approvals ${approvals}${extra}`);
		});
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
