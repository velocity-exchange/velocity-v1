import { Command } from 'commander';
import {
	PublicKey,
	SystemProgram,
	TransactionInstruction,
} from '@solana/web3.js';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildProvider } from '../lib/provider';
import { reportDispatch, reportDryRun, sendOrPropose } from '../lib/squads';
import { deriveAssociatedTokenAccount, resolveAuthority } from '../lib/userOps';

const NATIVE_MINT = new PublicKey(
	'So11111111111111111111111111111111111111112'
);
const TOKEN_PROGRAM_ID = new PublicKey(
	'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
);
const ASSOCIATED_TOKEN_PROGRAM_ID = new PublicKey(
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
);

/** SyncNative instruction discriminant in the SPL token program. */
const SYNC_NATIVE_DISCRIMINANT = 17;

function createAtaIdempotentIx(
	payer: PublicKey,
	ata: PublicKey,
	owner: PublicKey,
	mint: PublicKey
): TransactionInstruction {
	return new TransactionInstruction({
		programId: ASSOCIATED_TOKEN_PROGRAM_ID,
		keys: [
			{ pubkey: payer, isSigner: true, isWritable: true },
			{ pubkey: ata, isSigner: false, isWritable: true },
			{ pubkey: owner, isSigner: false, isWritable: false },
			{ pubkey: mint, isSigner: false, isWritable: false },
			{ pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
			{ pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
		],
		data: Buffer.from([1]), // CreateIdempotent
	});
}

function syncNativeIx(ata: PublicKey): TransactionInstruction {
	return new TransactionInstruction({
		programId: TOKEN_PROGRAM_ID,
		keys: [{ pubkey: ata, isSigner: false, isWritable: true }],
		data: Buffer.from([SYNC_NATIVE_DISCRIMINANT]),
	});
}

export function registerWallet(parent: Command): void {
	const wallet = parent
		.command('wallet')
		.description('Token operations on the signer or multisig vault wallet.');

	withGlobalOptions(
		wallet
			.command('wrap-sol <lamports>')
			.description(
				'Wrap native SOL from the owner wallet into its wSOL ATA (created idempotently; ' +
					'the syncNative rides in the same transaction). <lamports> is raw lamports. ' +
					'With --multisig the owner defaults to the vault PDA at --vault-index and the ' +
					'wrap is proposed as a vault transaction.'
			)
			.option(
				'--authority <pubkey>',
				'wallet owner (default: signer, or vault PDA with --multisig)'
			)
			.option(
				'--vault-index <index>',
				'with --multisig, vault index used to derive the owner PDA and propose against',
				'0'
			)
			.option(
				'--min-remaining <sol>',
				'refuse to wrap if fewer than this many SOL would remain in the wallet for rent and fees',
				'2'
			)
			.option(
				'--dry-run',
				'print the instructions and expected proposal rent/fees, send nothing',
				false
			)
	).action(async (lamportsArg: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as {
			authority?: string;
			vaultIndex: string;
			minRemaining: string;
			dryRun: boolean;
		};
		const lamports = Number.parseInt(lamportsArg, 10);
		if (!Number.isSafeInteger(lamports) || lamports <= 0) {
			throw new Error(
				`<lamports> must be a positive integer, got "${lamportsArg}"`
			);
		}
		const minRemaining = Number(local.minRemaining);
		if (!Number.isFinite(minRemaining) || minRemaining < 0) {
			throw new Error(
				`--min-remaining must be a non-negative number of SOL, got "${local.minRemaining}"`
			);
		}
		const vaultIndex = Number.parseInt(local.vaultIndex, 10);
		const owner = resolveAuthority(opts, local.authority, vaultIndex);
		const provider = buildProvider(opts);

		const wsolAta = deriveAssociatedTokenAccount(
			NATIVE_MINT,
			owner,
			TOKEN_PROGRAM_ID
		);

		const balance = await provider.connection.getBalance(owner);
		if (balance < lamports + minRemaining * 1e9) {
			throw new Error(
				`wallet ${owner.toBase58()} holds ${balance / 1e9} SOL; wrapping ` +
					`${
						lamports / 1e9
					} would leave less than ${minRemaining} SOL for rent/fees ` +
					'(override with --min-remaining)'
			);
		}

		const ixs = [
			createAtaIdempotentIx(owner, wsolAta, owner, NATIVE_MINT),
			SystemProgram.transfer({
				fromPubkey: owner,
				toPubkey: wsolAta,
				lamports,
			}),
			syncNativeIx(wsolAta),
		];

		const label =
			`wrap-sol amount=${lamports / 1e9} SOL owner=${owner.toBase58()} ` +
			`ata=${wsolAta.toBase58()}`;
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
			'velocity-admin wallet wrap-sol',
			vaultIndex
		);
		reportDispatch(label, result);
	});
}
