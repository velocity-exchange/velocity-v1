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
	| { kind: 'dry-run' }
	| { kind: 'sent'; signature: string }
	| {
			kind: 'proposed';
			multisig: PublicKey;
			transactionIndex: bigint;
			signature: string;
	  }
	| {
			kind: 'batched';
			multisig: PublicKey;
			batchIndex: bigint;
			count: number;
			signatures: string[];
	  };

/** One would-be proposal: the instructions that must execute together. */
export type InstructionGroup = {
	/** Shown in dry-run output and in the batch memo. */
	label: string;
	instructions: TransactionInstruction[];
	altAccounts?: AddressLookupTableAccount[];
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
/**
 * Outer transaction size ceiling.
 *
 * 1232 bytes is the legacy/v0 limit, derived from the 1280-byte IPv6 minimum
 * MTU less headers. SIMD-0296 raised the ceiling to 4096, but only for the v1
 * transaction format (SIMD-0385, mainnet epoch 1035). v1 cannot be built from
 * `@solana/web3.js` 1.x at all: `MessageV1.serialize()` throws, and 1.x is
 * maintenance-only. Building v1 needs `@solana/kit` 8.x or web3.js 3.x.
 *
 * Batching is the better answer anyway, since it removes the aggregate limit
 * rather than raising it, with no dependency change. v1 would only help a
 * single group too big to fit alone, which is not a problem we have.
 */
const TX_LIMIT = 1232;

/**
 * Squads stores the memo inside the `batchCreate` instruction, so a memo built
 * by joining every group label grows the creation transaction with the number
 * of groups. The per-group dry run cannot see that: each group can fit while
 * creation does not.
 */
const MAX_MEMO_BYTES = 200;

/**
 * Turn the common simulation failures into a sentence.
 *
 * A reader sees `{"InstructionError":[0,{"Custom":1}]}` and has to know that
 * error 1 from the token program is an empty or short balance, while 101 from
 * the velocity program is an instruction the deployed build does not have.
 * Those two cover most of what goes wrong before a listing.
 */
function explainSimError(logs: string[]): string | undefined {
	const joined = logs.join('\n');
	if (/InstructionFallbackNotFound/.test(joined)) {
		return 'the deployed program has no such instruction, so this needs the program upgrade first';
	}
	if (/Token(keg|zQd)\S*\s+failed: custom program error: 0x1\b/.test(joined)) {
		return 'token program error 1: the source account does not hold that amount';
	}
	if (/insufficient lamports|Transfer: insufficient lamports/.test(joined)) {
		return 'not enough SOL to cover rent or fees';
	}
	if (/already in use/.test(joined)) {
		return 'an account this would create already exists';
	}
	return undefined;
}

function boundMemo(memo: string): string {
	if (Buffer.byteLength(memo, 'utf8') <= MAX_MEMO_BYTES) {
		return memo;
	}
	let cut = memo.slice(0, MAX_MEMO_BYTES);
	while (Buffer.byteLength(cut, 'utf8') > MAX_MEMO_BYTES - 1) {
		cut = cut.slice(0, -1);
	}
	return `${cut}…`;
}

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
			// Lookup tables require a v0 message; legacy Transaction can't carry them.
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
 * Propose many instruction groups as a single Squads batch: one proposal, one
 * approval round, one timelock, N inner transactions executed in order.
 *
 * `sendOrPropose` puts `vaultTransactionCreate` and `proposalCreate` in one
 * outer transaction, so the whole inner instruction set has to fit that
 * transaction's 1232 bytes. Listing a market needs more room, which is why it
 * took six proposals, each with its own 4-of-7 review and one-hour timelock.
 * A batch gives each group its own `batchAddTransaction`, so only one group has
 * to fit 1232 bytes, never the total. Members still approve one proposal.
 *
 * The program forces the order below:
 *   - the proposal must exist and be a draft before any transaction is added
 *     (`batch_add_transaction` requires `ProposalStatus::Draft`);
 *   - only the member who created the batch may add to it;
 *   - inner indices are 1-based and sequential, because the inner PDA is seeded
 *     with `batch.size + 1`;
 *   - activation freezes the set, so it goes last.
 *
 * Execution is not atomic across groups. Each inner transaction executes and
 * can fail on its own, leaving the batch part-executed, so order the groups so
 * that stopping after any one of them leaves a coherent state.
 */

/**
 * Ordering hazards that size checks cannot see.
 *
 * A batch executes its inner transactions back to back, seconds apart, with no
 * opportunity to do anything in between. That breaks any pair of actions where
 * the second one depends on state that only an *external* actor can produce
 * after the first one lands.
 *
 * `initializePythLazerOracle` creates an empty oracle PDA,
 * but `initializeSpotMarket` / `initializePerpMarket` validate that the oracle
 * has a readable price (`admin.rs`: "Unable to read oracle price for {}").
 * The price only appears once the pyth-lazer-cranker is configured to post to
 * that feed, which is a separate infrastructure deploy. Across two proposals
 * there is natural time for that. Inside one batch there is none.
 */
const ORACLE_IX = 'initializePythLazerOracle';
const NEEDS_PRICE = ['initializeSpotMarket', 'initializePerpMarket'];

function orderingHazards(groups: InstructionGroup[]): string[] {
	const problems: string[] = [];
	const has = (label: string, needle: string) => label.includes(needle);
	const firstOracle = groups.findIndex((g) => has(g.label, ORACLE_IX));

	for (let i = 0; i < groups.length; i++) {
		for (const dependent of NEEDS_PRICE) {
			if (!has(groups[i].label, dependent)) {
				continue;
			}
			// Sharing a group is worse than following one, not better: the two land
			// in a single transaction, so there is no point at which a price could
			// appear between them.
			const sameGroup = has(groups[i].label, ORACLE_IX);
			const afterOracle = firstOracle >= 0 && i > firstOracle;
			if (!sameGroup && !afterOracle) {
				continue;
			}
			problems.push(
				sameGroup
					? `${dependent} shares group ${i + 1} with ${ORACLE_IX}, so they ` +
							'would execute in one transaction and the oracle cannot have a ' +
							'price by the time the market is initialised.'
					: `${dependent} (group ${i + 1}) runs after ${ORACLE_IX} (group ${
							firstOracle + 1
					  }), but it validates that the oracle has a readable price.`
			);
		}
	}
	if (problems.length > 0) {
		problems.push(
			'Creating the oracle PDA does not post a price; the lazer cranker has to ' +
				'be configured for the feed first, which is a separate deploy. Propose ' +
				'the oracle alone, wait for the price, then batch the rest.'
		);
	}
	return problems;
}

export async function sendOrProposeBatch(
	provider: AnchorProvider,
	groups: InstructionGroup[],
	multisigPda: PublicKey,
	rawMemo: string,
	vaultIndex = 0
): Promise<DispatchResult> {
	if (groups.length === 0) {
		throw new Error('sendOrProposeBatch: no instruction groups');
	}
	const memo = boundMemo(rawMemo);
	const [vaultPda] = multisig.getVaultPda({ multisigPda, index: vaultIndex });
	for (const group of groups) {
		const vaultMustSign = group.instructions.some((ix) =>
			ix.keys.some((key) => key.isSigner && key.pubkey.equals(vaultPda))
		);
		if (!vaultMustSign) {
			throw new Error(
				`group "${
					group.label
				}" does not require vault ${vaultPda.toBase58()} ` +
					`(index ${vaultIndex}) as a signer, so a proposal would not gate its execution`
			);
		}
	}

	const { blockhash } = await provider.connection.getLatestBlockhash();
	const messageFor = (group: InstructionGroup) =>
		new TransactionMessage({
			payerKey: vaultPda,
			recentBlockhash: blockhash,
			instructions: group.instructions,
		});

	if (DRY_RUN) {
		await reportBatchDryRun(provider, groups, multisigPda, vaultIndex, memo);
		return { kind: 'dry-run' };
	}

	const hazards = orderingHazards(groups);
	if (hazards.length > 0) {
		throw new Error(
			`batch has an ordering hazard:\n  - ${hazards.join('\n  - ')}\n` +
				'Re-run with --dry-run to review, or split the batch.'
		);
	}

	const info = await multisig.accounts.Multisig.fromAccountAddress(
		provider.connection,
		multisigPda
	);
	const batchIndex = BigInt(Number(info.transactionIndex) + 1);
	const creator = provider.wallet.publicKey;
	const signatures: string[] = [];

	// Batch + draft proposal together: both are small, and a batch without its
	// proposal cannot accept transactions.
	signatures.push(
		await provider.sendAndConfirm(
			new Transaction().add(
				multisig.instructions.batchCreate({
					multisigPda,
					creator,
					batchIndex,
					vaultIndex,
					memo,
				}),
				multisig.instructions.proposalCreate({
					multisigPda,
					transactionIndex: batchIndex,
					creator,
					isDraft: true,
				})
			)
		)
	);

	for (let i = 0; i < groups.length; i++) {
		const group = groups[i];
		signatures.push(
			await provider.sendAndConfirm(
				new Transaction().add(
					multisig.instructions.batchAddTransaction({
						vaultIndex,
						multisigPda,
						member: creator,
						batchIndex,
						transactionIndex: i + 1,
						ephemeralSigners: 0,
						transactionMessage: messageFor(group),
						addressLookupTableAccounts: group.altAccounts ?? [],
					})
				)
			)
		);
	}

	// Draft -> Active. Until this lands the proposal cannot be voted on, so a
	// failure here leaves a batch nobody can approve rather than a half-approved
	// one; re-running activate is the fix.
	signatures.push(
		await provider.sendAndConfirm(
			new Transaction().add(
				multisig.instructions.proposalActivate({
					multisigPda,
					transactionIndex: batchIndex,
					member: creator,
				})
			)
		)
	);

	return {
		kind: 'batched',
		multisig: multisigPda,
		batchIndex,
		count: groups.length,
		signatures,
	};
}

/**
 * Per-group size preview for `sendOrProposeBatch`. The limit that matters is
 * per group, not on the total: each group rides in its own
 * `batchAddTransaction` transaction.
 */
export async function reportBatchDryRun(
	provider: AnchorProvider,
	groups: InstructionGroup[],
	multisigPda: PublicKey,
	vaultIndex = 0,
	rawMemo = ''
): Promise<void> {
	const memo = boundMemo(rawMemo);
	const [vaultPda] = multisig.getVaultPda({ multisigPda, index: vaultIndex });
	const { blockhash } = await provider.connection.getLatestBlockhash();
	const creator = provider.wallet.publicKey;

	ui.header('batch dry run', pc.dim(`${groups.length} group(s) -> 1 proposal`));
	ui.kv('multisig', pc.dim(multisigPda.toBase58()));
	ui.kv('vault', pc.dim(`${vaultPda.toBase58()} (index ${vaultIndex})`));
	if (memo) {
		ui.kv('memo', ui.safe(memo));
	}

	let worst = 0;
	const rows: string[][] = [
		[
			pc.dim('#'),
			pc.dim('group'),
			pc.dim('ix'),
			pc.dim('bytes'),
			pc.dim('limit'),
		],
	];
	for (let i = 0; i < groups.length; i++) {
		const group = groups[i];
		const addIx = multisig.instructions.batchAddTransaction({
			vaultIndex,
			multisigPda,
			member: creator,
			batchIndex: 1n,
			transactionIndex: i + 1,
			ephemeralSigners: 0,
			transactionMessage: new TransactionMessage({
				payerKey: vaultPda,
				recentBlockhash: blockhash,
				instructions: group.instructions,
			}),
			addressLookupTableAccounts: group.altAccounts ?? [],
		});
		const outer = new Transaction().add(addIx);
		outer.feePayer = creator;
		outer.recentBlockhash = blockhash;
		const size = outer.serializeMessage().length + 1 + 64;
		worst = Math.max(worst, size);
		rows.push([
			String(i + 1),
			ui.safe(group.label),
			String(group.instructions.length),
			String(size),
			size > TX_LIMIT ? pc.red(`over by ${size - TX_LIMIT}`) : pc.dim('ok'),
		]);
	}
	ui.table(rows);

	// Simulate each group as the vault would execute it. Size says a group can
	// be proposed; it says nothing about whether it will succeed an hour later
	// when the timelock clears. Inner transactions are not atomic with each
	// other, so a group that fails leaves the batch part-executed, and the
	// simulation runs before anyone approves it.
	//
	// Groups run in order against the same chain state, so a later group that
	// depends on an earlier one's effect (a market that does not exist yet) will
	// report an error here that is expected. The error text is printed rather
	// than judged, because only the operator knows which of those are real.
	ui.header('simulation', pc.dim('as the vault, in order'));
	let anyFailed = false;
	for (let i = 0; i < groups.length; i++) {
		const sim = await provider.connection
			.simulateTransaction(
				new VersionedTransaction(
					new TransactionMessage({
						// Fees come from the local wallet, not the vault. A vault holding
						// no lamports would otherwise fail every simulation on fee payment,
						// which reports nothing about the instructions. The vault is still
						// a required signer through the instruction account metas, and
						// `sigVerify: false` means no signature is checked. The inner
						// message that `batchAddTransaction` carries keeps the vault as its
						// payer, because that is what Squads executes as.
						payerKey: provider.wallet.publicKey,
						recentBlockhash: blockhash,
						instructions: groups[i].instructions,
					}).compileToV0Message(groups[i].altAccounts ?? [])
				),
				{ sigVerify: false, replaceRecentBlockhash: true }
			)
			.catch((err: unknown) => ({
				value: { err: String(err), logs: null as string[] | null },
			}));
		if (!sim.value.err) {
			ui.kv(`group ${i + 1}`, ui.ok(ui.safe(groups[i].label)));
			continue;
		}
		anyFailed = true;
		ui.kv(`group ${i + 1}`, ui.bad(ui.safe(groups[i].label)));
		ui.line(pc.red(`   ${ui.safe(JSON.stringify(sim.value.err))}`));
		const hint = explainSimError(sim.value.logs ?? []);
		if (hint) {
			ui.line(pc.yellow(`   ${hint}`));
		}
		for (const log of (sim.value.logs ?? []).slice(-4)) {
			ui.line(pc.dim(`   ${ui.safe(log)}`));
		}
	}
	if (anyFailed) {
		ui.line(
			ui.warn(
				'a group failed simulation. If it depends on an earlier group in this ' +
					'batch that is expected, since simulation runs against current state. ' +
					'Anything else will fail on execution and leave the batch part-done.'
			)
		);
	}

	// The creation transaction carries the memo and is sent once, so it has its
	// own budget that the per-group numbers above say nothing about.
	const createTx = new Transaction().add(
		multisig.instructions.batchCreate({
			multisigPda,
			creator,
			batchIndex: 1n,
			vaultIndex,
			memo,
		}),
		multisig.instructions.proposalCreate({
			multisigPda,
			transactionIndex: 1n,
			creator,
			isDraft: true,
		})
	);
	createTx.feePayer = creator;
	createTx.recentBlockhash = blockhash;
	const createSize = createTx.serializeMessage().length + 1 + 64;
	ui.kv(
		'creation tx',
		createSize > TX_LIMIT
			? pc.red(
					`${createSize}/${TX_LIMIT} bytes, over by ${createSize - TX_LIMIT}`
			  )
			: pc.dim(`${createSize}/${TX_LIMIT} bytes`)
	);

	if (createSize > TX_LIMIT) {
		ui.line(
			ui.bad(
				'the batch creation transaction does not fit. Its size is driven by the ' +
					'memo, which is capped, so this means the memo cap needs lowering.'
			)
		);
	}

	if (worst > TX_LIMIT) {
		ui.line(
			ui.bad(
				`a group exceeds the ${TX_LIMIT}-byte transaction limit. Split that group; ` +
					'batching does not raise the per-group ceiling'
			)
		);
	} else {
		ui.line(
			ui.ok(
				`largest group ${worst}/${TX_LIMIT} bytes, ${TX_LIMIT - worst} to spare`
			)
		);
	}
	ui.note(
		`members approve 1 proposal instead of ${groups.length}; one timelock, ` +
			'then the inner transactions execute in order'
	);
	ui.note(
		'execution is not atomic across groups: order them so stopping after any ' +
			'one leaves a coherent state'
	);
	for (const problem of orderingHazards(groups)) {
		ui.line(ui.bad(problem));
	}
	console.log('');
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
	/** The memo the real `sendOrPropose` will use. It is stored inline in the
	 * proposal transaction, so the size estimate is only accurate with it. */
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
	const rent = (
		await Promise.all([
			provider.connection.getMinimumBalanceForRentExemption(vaultTxSize),
			provider.connection.getMinimumBalanceForRentExemption(proposalSize),
		])
	).reduce((a, b) => a + b, 0);

	// The proposal-create transaction carries the whole inner message inline,
	// so a batch that compiles fine can still exceed the 1232-byte transaction
	// limit at propose time. Size it here rather than letting the send fail.
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
	// serialized message + compact-u16 signature count + one 64-byte signature
	const outerSize = outer.serializeMessage().length + 1 + 64;

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
	if (result.kind === 'batched') {
		ui.header(label, ui.ok('proposed as batch'));
		ui.kv('proposal', pc.bold(`#${result.batchIndex.toString()}`));
		ui.kv('multisig', pc.dim(result.multisig.toBase58()));
		ui.kv(
			'transactions',
			`${result.count} inner, executed in order after one approval`
		);
		ui.kv('signatures', pc.dim(`${result.signatures.length} sent`));
		ui.note(`velocity-admin multisig inspect ${result.batchIndex}`);
		return;
	}
	ui.header(label, ui.ok('proposed'));
	ui.kv('proposal', pc.bold(`#${result.transactionIndex.toString()}`));
	ui.kv('multisig', pc.dim(result.multisig.toBase58()));
	ui.kv('signature', pc.dim(result.signature));
	ui.note(`velocity-admin multisig inspect ${result.transactionIndex}`);
}
