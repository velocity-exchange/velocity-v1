import { AnchorProvider } from '@coral-xyz/anchor';
import {
	PublicKey,
	Transaction,
	TransactionInstruction,
	TransactionMessage,
} from '@solana/web3.js';
import * as multisig from '@sqds/multisig';

/**
 * The authority that will actually sign a dispatched instruction: the Squads
 * vault PDA when going through `--multisig`, otherwise the local wallet. Pass
 * this as the `admin` account on instruction builders so the listed authority
 * matches the signer — the on-chain `check_warm`/`check_cold` guard then
 * validates it. Without this, a builder that defaults to a fixed role (e.g.
 * warm admin) produces an instruction the actual signer cannot satisfy.
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
	| { kind: 'sent'; signature: string }
	| {
			kind: 'proposed';
			multisig: PublicKey;
			transactionIndex: bigint;
			signature: string;
	  };

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
	vaultIndex = 0
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

	if (!multisigPda) {
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
	vaultIndex = 0
): Promise<void> {
	console.log('dry run, nothing sent');
	instructions.forEach((ix, i) => {
		console.log(
			`  ix[${i}] program=${ix.programId.toBase58()} accounts=${
				ix.keys.length
			} data=${ix.data.length}B`
		);
	});

	if (!multisigPda) {
		console.log('  dispatch: direct send (1 tx, 1 signature)');
		console.log('  network fee: ~5000 lamports');
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
	const messageBytes = new TransactionMessage({
		payerKey: vaultPda,
		recentBlockhash: blockhash,
		instructions,
	})
		.compileToLegacyMessage()
		.serialize().length;

	// VaultTransaction: discriminator + multisig/creator pubkeys + index +
	// bumps/flags + the serialized inner message; Proposal: fixed fields plus
	// three member-sized vote vectors. Padded slack keeps this an upper bound.
	const vaultTxSize = 8 + 32 + 32 + 8 + 1 + 1 + 1 + 4 + messageBytes + 64;
	const proposalSize = 8 + 32 + 8 + 1 + 8 * 4 + (4 + 32 * members) * 3 + 64;
	const rent =
		(await provider.connection.getMinimumBalanceForRentExemption(vaultTxSize)) +
		(await provider.connection.getMinimumBalanceForRentExemption(proposalSize));

	console.log(
		`  dispatch: proposal to multisig ${multisigPda.toBase58()}, vault ${vaultIndex} (${vaultPda.toBase58()}), next tx index ${transactionIndex}`
	);
	console.log(
		`  proposer rent: ~${rent} lamports (~${(rent / 1e9).toFixed(
			4
		)} SOL) for VaultTransaction + Proposal accounts, reclaimable after execution`
	);
	console.log('  network fee: ~5000 lamports');
	console.log(
		'  (members must still approve + execute via Squads UI / CLI before it lands)'
	);
}

export function reportDispatch(label: string, result: DispatchResult): void {
	if (result.kind === 'sent') {
		console.log(`✓ ${label}`);
		console.log(`  signature: ${result.signature}`);
	} else {
		console.log(
			`✓ ${label} proposed to multisig ${result.multisig.toBase58()}`
		);
		console.log(`  transactionIndex: ${result.transactionIndex.toString()}`);
		console.log(`  signature: ${result.signature}`);
		console.log(
			`  (members must approve + execute via Squads UI / CLI before it lands)`
		);
	}
}
