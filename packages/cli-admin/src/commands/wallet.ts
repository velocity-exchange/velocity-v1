import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import {
	AddressLookupTableAccount,
	Connection,
	PublicKey,
	SystemProgram,
	TransactionInstruction,
} from '@solana/web3.js';
import {
	fetchUserStatsAccount,
	getInsuranceFundStakeAccountPublicKey,
	getTokenAmount,
	getUserAccountPublicKeySync,
	SpotBalanceType,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
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

const JUPITER_API = 'https://lite-api.jup.ag/swap/v1';

/**
 * Programs a Jupiter swap response is allowed to invoke. Instructions
 * targeting anything else are rejected before signing/proposing: the swap
 * endpoint returns opaque instruction bytes, and a compromised API must not
 * be able to smuggle an arbitrary instruction under a "swap" label.
 */
const SWAP_PROGRAM_ALLOWLIST = new Set([
	'JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4', // Jupiter v6
	TOKEN_PROGRAM_ID.toBase58(),
	'TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb', // Token-2022
	ASSOCIATED_TOKEN_PROGRAM_ID.toBase58(),
	SystemProgram.programId.toBase58(),
]);

/**
 * Reject any instruction outside the swap program allowlist, and any
 * instruction that requires a signature from an account other than the owner
 * (a rogue signer would let the vault/wallet co-sign an arbitrary transfer).
 */
function validateSwapInstructions(
	ixs: TransactionInstruction[],
	owner: PublicKey
): void {
	for (const ix of ixs) {
		const program = ix.programId.toBase58();
		if (!SWAP_PROGRAM_ALLOWLIST.has(program)) {
			throw new Error(
				`swap response contains an instruction for non-allowlisted program ${program} — refusing to sign/propose`
			);
		}
		for (const key of ix.keys) {
			if (key.isSigner && !key.pubkey.equals(owner)) {
				throw new Error(
					`swap response requires a signature from ${key.pubkey.toBase58()} (not the owner) — refusing to sign/propose`
				);
			}
		}
	}
}

interface JupiterInstruction {
	programId: string;
	accounts: { pubkey: string; isSigner: boolean; isWritable: boolean }[];
	data: string;
}

function deserializeJupiterIx(ix: JupiterInstruction): TransactionInstruction {
	return new TransactionInstruction({
		programId: new PublicKey(ix.programId),
		keys: ix.accounts.map((a) => ({
			pubkey: new PublicKey(a.pubkey),
			isSigner: a.isSigner,
			isWritable: a.isWritable,
		})),
		data: Buffer.from(ix.data, 'base64'),
	});
}

async function fetchJson(url: string, body?: unknown): Promise<any> {
	const res = await fetch(url, {
		method: body ? 'POST' : 'GET',
		headers: body ? { 'Content-Type': 'application/json' } : undefined,
		body: body ? JSON.stringify(body) : undefined,
	});
	if (!res.ok) {
		throw new Error(
			`jupiter api ${res.status} ${res.statusText}: ${await res.text()}`
		);
	}
	return res.json();
}

async function fetchAltAccounts(
	connection: Connection,
	addresses: string[]
): Promise<AddressLookupTableAccount[]> {
	const accounts: AddressLookupTableAccount[] = [];
	for (const address of addresses) {
		const alt = await connection.getAddressLookupTable(new PublicKey(address));
		if (!alt.value) {
			throw new Error(`address lookup table ${address} not found on chain`);
		}
		accounts.push(alt.value);
	}
	return accounts;
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

	withGlobalOptions(
		wallet
			.command('swap <inputMint> <outputMint> <amount>')
			.description(
				'Swap tokens in the owner wallet via Jupiter. <amount> is raw input-token units. ' +
					'With --multisig the owner defaults to the vault PDA at --vault-index and the swap ' +
					'is proposed as a vault transaction — the route is quoted NOW but executes after ' +
					'approval, so a stale route can fail at execution and needs re-proposing. ' +
					'Approve and execute promptly; on a timelocked multisig prefer --only-direct-routes.'
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
			.option('--slippage-bps <bps>', 'max slippage in basis points', '50')
			.option(
				'--only-direct-routes',
				'single-hop routes only: fewer accounts, more robust against staleness',
				false
			)
			.option(
				'--dry-run',
				'print the quote, instructions and expected proposal rent/fees, send nothing',
				false
			)
	).action(
		async (
			inputMint: string,
			outputMint: string,
			amount: string,
			_flags,
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const local = cmd.opts() as {
				authority?: string;
				vaultIndex: string;
				slippageBps: string;
				onlyDirectRoutes: boolean;
				dryRun: boolean;
			};
			const inMint = new PublicKey(inputMint);
			const outMint = new PublicKey(outputMint);
			const rawAmount = Number.parseInt(amount, 10);
			if (!Number.isSafeInteger(rawAmount) || rawAmount <= 0) {
				throw new Error(`<amount> must be a positive integer, got "${amount}"`);
			}
			const slippageBps = Number.parseInt(local.slippageBps, 10);
			if (!Number.isInteger(slippageBps) || slippageBps < 1) {
				throw new Error(
					`--slippage-bps must be a positive integer, got "${local.slippageBps}"`
				);
			}
			const vaultIndex = Number.parseInt(local.vaultIndex, 10);
			const owner = resolveAuthority(opts, local.authority, vaultIndex);
			const provider = buildProvider(opts);

			const quote = await fetchJson(
				`${JUPITER_API}/quote?inputMint=${inMint.toBase58()}` +
					`&outputMint=${outMint.toBase58()}&amount=${rawAmount}` +
					`&slippageBps=${slippageBps}` +
					`&restrictIntermediateTokens=true` +
					(local.onlyDirectRoutes ? '&onlyDirectRoutes=true' : '')
			);
			const route = (quote.routePlan ?? [])
				.map((r: any) => r.swapInfo?.label ?? '?')
				.join(' → ');
			console.log(
				`quote: ${quote.inAmount} in → ${quote.outAmount} out ` +
					`(min ${quote.otherAmountThreshold}, impact ${quote.priceImpactPct}%)` +
					(route ? ` via ${route}` : '')
			);

			const swap = await fetchJson(`${JUPITER_API}/swap-instructions`, {
				quoteResponse: quote,
				userPublicKey: owner.toBase58(),
			});
			// Compute-budget instructions cannot ride in the inner message: the
			// vault executes it via CPI and the ComputeBudget program is not
			// CPI-able. The executor sets the budget on the outer transaction.
			const ixs: TransactionInstruction[] = [
				...(swap.setupInstructions ?? []).map(deserializeJupiterIx),
				deserializeJupiterIx(swap.swapInstruction),
				...(swap.cleanupInstruction
					? [deserializeJupiterIx(swap.cleanupInstruction)]
					: []),
			];
			validateSwapInstructions(ixs, owner);
			const altAccounts = await fetchAltAccounts(
				provider.connection,
				swap.addressLookupTableAddresses ?? []
			);
			// Show reviewers what is actually being signed, not just the memo.
			console.log('instruction programs:');
			for (const ix of ixs) {
				console.log(`  ${ix.programId.toBase58()}`);
			}

			const label =
				`swap ${rawAmount} of ${inMint.toBase58()} → ${outMint.toBase58()} ` +
				`owner=${owner.toBase58()} slippage=${slippageBps}bps`;
			if (local.dryRun) {
				console.log(label);
				await reportDryRun(
					provider,
					ixs,
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					vaultIndex,
					altAccounts
				);
				return;
			}
			const result = await sendOrPropose(
				provider,
				ixs,
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin wallet swap',
				vaultIndex,
				altAccounts
			);
			reportDispatch(label, result);
		}
	);

	withGlobalOptions(
		wallet
			.command('transfer <mint> <recipient> <amount>')
			.description(
				"Transfer SPL tokens from the owner wallet to <recipient>'s ATA (created " +
					'idempotently, owner pays the rent). <amount> is raw token base units; the ' +
					'transfer is checked against the mint decimals read on chain. With --multisig ' +
					'the owner defaults to the vault PDA at --vault-index and the transfer is ' +
					'proposed as a vault transaction.'
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
				'--to-token-account',
				'treat <recipient> as the destination token account itself (e.g. a program ' +
					'vault) instead of deriving its ATA; no account creation is attempted',
				false
			)
			.option(
				'--dry-run',
				'print the instructions and expected proposal rent/fees, send nothing',
				false
			)
	).action(
		async (
			mintArg: string,
			recipientArg: string,
			amountArg: string,
			_flags,
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const local = cmd.opts() as {
				authority?: string;
				vaultIndex: string;
				toTokenAccount: boolean;
				dryRun: boolean;
			};
			const mint = new PublicKey(mintArg);
			const recipient = new PublicKey(recipientArg);
			const amount = BigInt(amountArg);
			if (amount <= 0n) {
				throw new Error(`<amount> must be positive, got "${amountArg}"`);
			}
			const vaultIndex = Number.parseInt(local.vaultIndex, 10);
			const owner = resolveAuthority(opts, local.authority, vaultIndex);
			const provider = buildProvider(opts);

			const mintInfo = await provider.connection.getParsedAccountInfo(mint);
			const parsed = (mintInfo.value?.data as any)?.parsed;
			if (parsed?.type !== 'mint') {
				throw new Error(`${mint.toBase58()} is not a token mint`);
			}
			const decimals: number = parsed.info.decimals;
			const tokenProgram = mintInfo.value!.owner;

			const sourceAta = deriveAssociatedTokenAccount(mint, owner, tokenProgram);
			let destAta: PublicKey;
			if (local.toTokenAccount) {
				destAta = recipient;
				const destInfo = await provider.connection.getParsedAccountInfo(
					destAta
				);
				const destParsed = (destInfo.value?.data as any)?.parsed;
				if (
					destParsed?.type !== 'account' ||
					destParsed.info.mint !== mint.toBase58()
				) {
					throw new Error(
						`${destAta.toBase58()} is not a token account for mint ${mint.toBase58()}`
					);
				}
			} else {
				destAta = deriveAssociatedTokenAccount(mint, recipient, tokenProgram);
			}
			const balance = await provider.connection
				.getTokenAccountBalance(sourceAta)
				.then((r) => BigInt(r.value.amount))
				.catch(() => 0n);
			if (balance < amount) {
				throw new Error(
					`source ATA ${sourceAta.toBase58()} holds ${balance}, need ${amount}`
				);
			}

			const transferCheckedData = Buffer.alloc(10);
			transferCheckedData.writeUInt8(12, 0); // TransferChecked
			transferCheckedData.writeBigUInt64LE(amount, 1);
			transferCheckedData.writeUInt8(decimals, 9);
			const ixs = [];
			if (!local.toTokenAccount) {
				ixs.push(
					new TransactionInstruction({
						programId: ASSOCIATED_TOKEN_PROGRAM_ID,
						keys: [
							{ pubkey: owner, isSigner: true, isWritable: true },
							{ pubkey: destAta, isSigner: false, isWritable: true },
							{ pubkey: recipient, isSigner: false, isWritable: false },
							{ pubkey: mint, isSigner: false, isWritable: false },
							{
								pubkey: SystemProgram.programId,
								isSigner: false,
								isWritable: false,
							},
							{ pubkey: tokenProgram, isSigner: false, isWritable: false },
						],
						data: Buffer.from([1]), // CreateIdempotent
					})
				);
			}
			ixs.push(
				new TransactionInstruction({
					programId: tokenProgram,
					keys: [
						{ pubkey: sourceAta, isSigner: false, isWritable: true },
						{ pubkey: mint, isSigner: false, isWritable: false },
						{ pubkey: destAta, isSigner: false, isWritable: true },
						{ pubkey: owner, isSigner: true, isWritable: false },
					],
					data: transferCheckedData,
				})
			);

			const label =
				`transfer ${amount} of ${mint.toBase58()} ` +
				`from ${owner.toBase58()} to ${recipient.toBase58()}`;
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
				'velocity-admin wallet transfer',
				vaultIndex
			);
			reportDispatch(label, result);
		}
	);

	withGlobalOptions(
		wallet
			.command('balances')
			.description(
				'Read-only: native SOL and token balances of the owner wallet, plus its Velocity ' +
					'holdings — spot deposits/borrows per sub-account and insurance-fund stakes. ' +
					'Token balances for spot-market mints are labeled with the market name. ' +
					'With --multisig the owner defaults to the vault PDA at --vault-index.'
			)
			.option(
				'--authority <pubkey>',
				'wallet owner (default: signer, or vault PDA with --multisig)'
			)
			.option(
				'--vault-index <index>',
				'with --multisig, vault index used to derive the owner PDA',
				'0'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { authority?: string; vaultIndex: string };
		const vaultIndex = Number.parseInt(local.vaultIndex, 10);
		const owner = resolveAuthority(opts, local.authority, vaultIndex);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, true);
		try {
			const markets = (client as any).getSpotMarketAccounts() as any[];
			markets.sort((a, b) => a.marketIndex - b.marketIndex);
			const byMint = new Map<string, any>();
			for (const m of markets) {
				byMint.set(m.mint.toBase58(), m);
			}
			const nameOf = (m: any) =>
				Buffer.from(m.name).toString('utf8').trim() || `spot[${m.marketIndex}]`;
			const fmt = (raw: BN | bigint | number, decimals: number) =>
				(Number(raw.toString()) / 10 ** decimals).toLocaleString('en-US', {
					maximumFractionDigits: decimals > 6 ? 6 : decimals,
				});

			console.log(`owner ${owner.toBase58()}`);
			const lamports = await provider.connection.getBalance(owner);
			console.log(`native SOL: ${lamports / 1e9}`);

			console.log('wallet tokens:');
			let any = false;
			for (const tokenProgram of [
				TOKEN_PROGRAM_ID,
				new PublicKey('TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb'),
			]) {
				const res = await provider.connection.getParsedTokenAccountsByOwner(
					owner,
					{ programId: tokenProgram }
				);
				for (const a of res.value) {
					const info = a.account.data.parsed.info;
					if (info.tokenAmount.uiAmount === 0) {
						continue;
					}
					const market = byMint.get(info.mint);
					const tag = market ? nameOf(market) : info.mint;
					console.log(`  ${tag}  ${info.tokenAmount.uiAmountString}`);
					any = true;
				}
			}
			if (!any) {
				console.log('  (none)');
			}

			const userStats = await fetchUserStatsAccount(
				provider.connection,
				client.program,
				owner
			);
			if (!userStats) {
				console.log('velocity: no UserStats for this owner');
				return;
			}

			console.log('velocity spot positions:');
			any = false;
			const count = userStats.numberOfSubAccountsCreated;
			for (let subAccountId = 0; subAccountId < count; subAccountId++) {
				const userPda = getUserAccountPublicKeySync(
					client.program.programId,
					owner,
					subAccountId
				);
				const user = await (client.program.account as any).user.fetchNullable(
					userPda
				);
				if (!user) {
					continue;
				}
				for (const pos of user.spotPositions) {
					if (new BN(pos.scaledBalance).isZero()) {
						continue;
					}
					const market = markets.find((m) => m.marketIndex === pos.marketIndex);
					if (!market) {
						continue;
					}
					const isDeposit = pos.balanceType.deposit !== undefined;
					const amount = getTokenAmount(
						new BN(pos.scaledBalance),
						market,
						isDeposit ? SpotBalanceType.DEPOSIT : SpotBalanceType.BORROW
					);
					console.log(
						`  sub ${subAccountId}: ${nameOf(market)} ${
							isDeposit ? 'deposit' : 'borrow'
						} ${fmt(amount, market.decimals)}`
					);
					any = true;
				}
			}
			if (!any) {
				console.log('  (none)');
			}

			console.log('insurance fund stakes:');
			any = false;
			for (const market of markets) {
				const stakePda = getInsuranceFundStakeAccountPublicKey(
					client.program.programId,
					owner,
					market.marketIndex
				);
				const stake = await (
					client.program.account as any
				).insuranceFundStake.fetchNullable(stakePda);
				if (!stake || new BN(stake.ifShares).isZero()) {
					continue;
				}
				const totalShares = new BN(market.insuranceFund.totalShares);
				const vaultBalance = new BN(
					(
						await provider.connection.getTokenAccountBalance(
							market.insuranceFund.vault
						)
					).value.amount
				);
				const amount = totalShares.isZero()
					? new BN(0)
					: new BN(stake.ifShares).mul(vaultBalance).div(totalShares);
				console.log(
					`  ${nameOf(market)}: ${fmt(amount, market.decimals)} ` +
						`(${stake.ifShares.toString()} of ${totalShares.toString()} shares)`
				);
				any = true;
			}
			if (!any) {
				console.log('  (none)');
			}
		} finally {
			await client.unsubscribe();
		}
	});
}
