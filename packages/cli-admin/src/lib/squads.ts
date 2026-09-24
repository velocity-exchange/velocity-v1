import { AnchorProvider } from '@coral-xyz/anchor';
import {
	AddressLookupTableAccount,
	PublicKey,
	Transaction,
	TransactionInstruction,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import * as multisig from '@sqds/multisig';
import pc from 'picocolors';
import { confirmMainnetDirect } from './context';
import * as ui from './ui';
import { renderInstructions } from './decode';

/**
 * The authority that signs a dispatched instruction: the Squads vault PDA
 * under `--multisig`, or the local wallet otherwise. Pass it as the `admin`
 * account so the on-chain `check_warm`/`check_cold` guards match the signer.
 */
export function resolveAdminAuthority(
	provider: AnchorProvider,
	multisigPda: PublicKey | undefined,
	vaultIndex = 0
): PublicKey {
	if (!multisigPda) {
		return provider.wallet.publicKey;
	}
	const [vaultPda] = multisig.getVaultPda({ multisigPda, index: vaultIndex });
	return vaultPda;
}

export type DispatchResult =
	| { kind: 'dry-run' }
	| { kind: 'sent'; signature: string }
	| {
			kind: 'proposed';
			multisig: PublicKey;
			transactionIndex: bigint;
			signature: string;
	  };

/**
 * Process-wide dry-run flag. `readGlobalOpts` sets it, so `--dry-run` works on
 * every state-changing command without passing the flag through ~58 call sites.
 * `sendOrPropose` is the only path that signs or proposes, so gating it there
 * covers the whole CLI. No command can accept `--dry-run` and send anyway.
 *
 * Commands that print a fuller preview, such as `wallet swap`'s quote and
 * `lut extend`'s account diff, read `dryRun` from their own opts and return
 * before they reach dispatch. This is the fallback for everything else.
 */
let DRY_RUN = false;

export function setDryRun(value: boolean): void {
	DRY_RUN = value;
}

/**
 * Dispatch admin instructions either directly (signed by the wallet) or via a
 * Squads V4 multisig vault transaction + proposal. Mirrors helium-admin-cli's
 * `sendInstructionsOrSquadsV4`.
 *
 * If `multisigPda` is undefined, instructions are signed and sent directly.
 * Otherwise a proposal is mandatory: the multisig's vault PDA at `vaultIndex`
 * (default 0) must be a
 * required signer of at least one instruction, and the call errors out if it
 * is not (e.g. the target authority resolved to the wallet instead of the
 * vault) — `--multisig` never silently downgrades to a direct send. When the
 * vault must sign, a `vaultTransactionCreate` + `proposalCreate` is
 * submitted; the wallet pays rent and is recorded as the proposer.
 * Approval/execution still happen through the multisig members (CLI does not
 * auto-approve).
 */
export async function sendOrPropose(
	provider: AnchorProvider,
	instructions: TransactionInstruction[],
	multisigPda: PublicKey | undefined,
	memo: string,
	vaultIndex = 0,
	altAccounts: AddressLookupTableAccount[] = []
): Promise<DispatchResult> {
	if (multisigPda) {
		const [vaultPda] = multisig.getVaultPda({ multisigPda, index: vaultIndex });
		const vaultMustSign = instructions.some((ix) =>
			ix.keys.some((key) => key.isSigner && key.pubkey.equals(vaultPda))
		);
		if (!vaultMustSign) {
			throw new Error(
				`--multisig was passed but vault ${vaultPda.toBase58()} (index ${vaultIndex}) is not a required signer of any instruction — ` +
					`a proposal would not gate execution. Check that the target authority is the vault PDA, ` +
					`or drop --multisig to send directly with the local wallet.`
			);
		}
	}

	if (DRY_RUN) {
		await reportDryRun(
			provider,
			instructions,
			multisigPda,
			vaultIndex,
			altAccounts,
			memo
		);
		return { kind: 'dry-run' };
	}

	if (!multisigPda) {
		await confirmMainnetDirect(memo);
		if (altAccounts.length > 0) {
			// A lookup table requires a v0 message. A legacy Transaction cannot
			// carry one.
			const { blockhash } = await provider.connection.getLatestBlockhash();
			const message = new TransactionMessage({
				payerKey: provider.wallet.publicKey,
				recentBlockhash: blockhash,
				instructions,
			}).compileToV0Message(altAccounts);
			const signature = await provider.sendAndConfirm(
				new VersionedTransaction(message)
			);
			return { kind: 'sent', signature };
		}
		const tx = new Transaction().add(...instructions);
		const signature = await provider.sendAndConfirm(tx);
		return { kind: 'sent', signature };
	}

	const info = await multisig.accounts.Multisig.fromAccountAddress(
		provider.connection,
		multisigPda
	);
	const transactionIndex = BigInt(Number(info.transactionIndex) + 1);

	const [vaultPda] = multisig.getVaultPda({
		multisigPda,
		index: vaultIndex,
	});

	const { blockhash } = await provider.connection.getLatestBlockhash();
	const transactionMessage = new TransactionMessage({
		payerKey: vaultPda,
		recentBlockhash: blockhash,
		instructions,
	});

	const createIx = multisig.instructions.vaultTransactionCreate({
		multisigPda,
		transactionIndex,
		creator: provider.wallet.publicKey,
		vaultIndex,
		ephemeralSigners: 0,
		transactionMessage,
		addressLookupTableAccounts: altAccounts,
		memo,
	});

	const proposeIx = multisig.instructions.proposalCreate({
		multisigPda,
		transactionIndex,
		creator: provider.wallet.publicKey,
	});

	const tx = new Transaction().add(createIx, proposeIx);
	const signature = await provider.sendAndConfirm(tx);

	return {
		kind: 'proposed',
		multisig: multisigPda,
		transactionIndex,
		signature,
	};
}

/**
 * Print what a `sendOrPropose` call with the same arguments would do, without
 * sending anything: the instruction list, the dispatch mode, and the expected
 * costs. For a direct send that is just the network fee; for a proposal it is
 * the rent for the `VaultTransaction` + `Proposal` accounts (estimated from
 * the compiled inner message size and the multisig member count; both
 * accounts are closable after execution, so the rent is reclaimable) plus the
 * network fee.
 */
export async function reportDryRun(
	provider: AnchorProvider,
	instructions: TransactionInstruction[],
	multisigPda: PublicKey | undefined,
	vaultIndex = 0,
	altAccounts: AddressLookupTableAccount[] = [],
	/** The memo that `sendOrPropose` uses. The proposal transaction stores it
	 * inline, so the size estimate is only accurate with it. */
	memo = ''
): Promise<void> {
	ui.header('dry run', pc.dim('nothing sent'));
	// Decode rather than just counting accounts: the point of a dry run is to
	// see what the transaction actually does before it costs a proposal.
	renderInstructions(instructions);
	ui.line('');

	if (!multisigPda) {
		ui.kv('dispatch', 'direct send');
		ui.kv('network fee', pc.dim('~5000 lamports'));
		return;
	}

	const info = await multisig.accounts.Multisig.fromAccountAddress(
		provider.connection,
		multisigPda
	);
	const [vaultPda] = multisig.getVaultPda({ multisigPda, index: vaultIndex });
	const transactionIndex = BigInt(Number(info.transactionIndex) + 1);
	const members = info.members.length;

	const { blockhash } = await provider.connection.getLatestBlockhash();
	const message = new TransactionMessage({
		payerKey: vaultPda,
		recentBlockhash: blockhash,
		instructions,
	});
	const messageBytes = (
		altAccounts.length > 0
			? message.compileToV0Message(altAccounts)
			: message.compileToLegacyMessage()
	).serialize().length;

	// VaultTransaction: discriminator + multisig/creator pubkeys + index +
	// bumps/flags + the serialized inner message; Proposal: fixed fields plus
	// three member-sized vote vectors. Padded slack keeps this an upper bound.
	const vaultTxSize = 8 + 32 + 32 + 8 + 1 + 1 + 1 + 4 + messageBytes + 64;
	const proposalSize = 8 + 32 + 8 + 1 + 8 * 4 + (4 + 32 * members) * 3 + 64;
	const rent =
		(await provider.connection.getMinimumBalanceForRentExemption(vaultTxSize)) +
		(await provider.connection.getMinimumBalanceForRentExemption(proposalSize));

	// The proposal-create transaction carries the whole inner message inline, so
	// a batch that compiles can still exceed the 1232-byte transaction limit at
	// propose time. Measure the size here rather than let the send fail.
	const createIx = multisig.instructions.vaultTransactionCreate({
		multisigPda,
		transactionIndex,
		creator: provider.wallet.publicKey,
		vaultIndex,
		ephemeralSigners: 0,
		transactionMessage: message,
		addressLookupTableAccounts: altAccounts,
		memo,
	});
	const proposeIx = multisig.instructions.proposalCreate({
		multisigPda,
		transactionIndex,
		creator: provider.wallet.publicKey,
	});
	const outer = new Transaction().add(createIx, proposeIx);
	outer.recentBlockhash = blockhash;
	outer.feePayer = provider.wallet.publicKey;
	// The serialized message, plus the compact-u16 signature count, plus one
	// 64-byte signature.
	const outerSize = outer.serializeMessage().length + 1 + 64;
	const TX_LIMIT = 1232;

	ui.kv(
		'dispatch',
		`proposal, next index ${pc.bold(String(transactionIndex))}`
	);
	ui.kv('multisig', pc.dim(multisigPda.toBase58()));
	ui.kv('vault', `${pc.dim(vaultPda.toBase58())} ${pc.dim(`(${vaultIndex})`)}`);
	ui.kv(
		'size',
		outerSize > TX_LIMIT
			? pc.red(
					`${outerSize} bytes, ${
						outerSize - TX_LIMIT
					} over the ${TX_LIMIT} limit: this will fail to propose, split the batch`
			  )
			: `${ui.count(outerSize)} of ${ui.count(TX_LIMIT)} bytes ${pc.dim(
					`(${TX_LIMIT - outerSize} spare)`
			  )}`
	);
	ui.kv(
		'proposer rent',
		`~${(rent / 1e9).toFixed(4)} SOL ${pc.dim('reclaimable')}`
	);
	ui.kv('network fee', pc.dim('~5000 lamports'));
	ui.note('needs approval and execution');
}

export function reportDispatch(label: string, result: DispatchResult): void {
	if (result.kind === 'dry-run') {
		return;
	}
	if (result.kind === 'sent') {
		ui.header(label, ui.ok('sent'));
		ui.kv('signature', pc.dim(result.signature));
		return;
	}
	ui.header(label, ui.ok('proposed'));
	ui.kv('proposal', pc.bold(`#${result.transactionIndex.toString()}`));
	ui.kv('multisig', pc.dim(result.multisig.toBase58()));
	ui.kv('signature', pc.dim(result.signature));
	ui.note(`velocity-admin multisig inspect ${result.transactionIndex}`);
}
