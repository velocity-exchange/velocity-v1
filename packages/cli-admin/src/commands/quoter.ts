import { Command } from 'commander';
import { PublicKey, SystemProgram, SYSVAR_RENT_PUBKEY } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getQuoterCrossConditionsPublicKey,
	getQuoterPublicKey,
	getQuoterSlabPublicKey,
	QuoterType,
} from '@velocity-exchange/sdk';
import { parseEnable } from '../lib/args';
import {
	readCrankCostUnits,
	withCrankCostUnitOptions,
} from '../lib/crankCostUnits';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';

function parseQuoterType(value: string): QuoterType {
	switch (value.toLowerCase()) {
		case 'vamm':
			return QuoterType.VAMM;
		case 'clob':
			return QuoterType.CLOB;
		case 'custom':
			return QuoterType.CUSTOM;
		default:
			throw new Error(
				`quoter type must be "vamm", "clob" or "custom", got "${value}"`
			);
	}
}

/** Whether a fetched entry (or a parsed `--type`) is the market's book. */
function isClobEntry(quoterType: unknown): boolean {
	return Boolean(
		quoterType && 'clob' in (quoterType as Record<string, unknown>)
	);
}

/** Parse a comma-separated list of indexes into the unified account list. */
function parseIndexList(value: string): number[] {
	if (value.trim() === '') return [];
	return value.split(',').map((part) => {
		const index = Number.parseInt(part.trim(), 10);
		if (!Number.isInteger(index) || index < 0 || index > 255) {
			throw new Error(`account index must be 0-255, got "${part}"`);
		}

		return index;
	});
}

/** Parse an 8-byte instruction discriminator given as 16 hex chars (optional 0x prefix). */
function parseDiscriminator(value: string): number[] {
	const hex = value.startsWith('0x') ? value.slice(2) : value;
	if (!/^[0-9a-fA-F]{16}$/.test(hex)) {
		throw new Error(
			`discriminator must be 8 bytes as 16 hex chars, got "${value}"`
		);
	}

	return Array.from(Buffer.from(hex, 'hex'));
}

/** Parse an account meta given as `<pubkey>` (readonly) or `<pubkey>:w` (writable). */
function parseAccountMeta(value: string): {
	pubkey: PublicKey;
	isWritable: boolean;
} {
	const [pubkey, suffix, ...rest] = value.split(':');
	if (rest.length > 0 || (suffix !== undefined && suffix !== 'w')) {
		throw new Error(
			`account meta must be "<pubkey>" or "<pubkey>:w", got "${value}"`
		);
	}

	return { pubkey: new PublicKey(pubkey), isWritable: suffix === 'w' };
}

/**
 * The market's quoter slab, when it exists on chain. The write-through
 * instructions `set-active`, `set-priority` and `set-oracle-band` take the slab
 * as an optional account. When the caller passes it, the value also lands in
 * the entry's live slot. The entry names its market, so this derives the slab
 * instead of asking the caller for it.
 */
async function liveSlabFor(
	client: {
		program: {
			programId: PublicKey;
			account: any;
		};
	},

	connection: { getAccountInfo(key: PublicKey): Promise<unknown | null> },
	quoterKey: PublicKey
): Promise<PublicKey | null> {
	const entry = await client.program.account.quoterV0.fetch(quoterKey);
	const quoterSlab = getQuoterSlabPublicKey(
		client.program.programId,
		entry.config.market
	);

	return (await connection.getAccountInfo(quoterSlab)) ? quoterSlab : null;
}

/**
 * Quoter registry operations for the PropAMM order flow. See
 * `state::prop_amm`.
 *
 * The `QuoterV0` entry is the staging half. Its authority proposes config
 * there, and no fill reads it. `set-approved` copies the staging config into
 * the market's `QuoterSlabV0` slot, and fills read only that copy. A staging
 * edit does not change the slab, so the approved copy keeps serving until the
 * admin approves again. For a Custom quoter, the quoted user's authority
 * creates the entry, and that creation is the user's consent. That authority
 * keeps a permanent kill switch in `set-active`, which writes through to the
 * live slot.
 */
export function registerQuoter(parent: Command): void {
	const quoter = parent
		.command('quoter')
		.description(
			'Quoter registry: register external quoter programs (PropAMM order flow), update their CPI surface, toggle the maker kill switch, admin-approve entries.'
		);

	withGlobalOptions(
		quoter
			.command(
				'init <market> <quoterProgram> <user> <responseAccount> <quoteDisc> <executeDisc>'
			)
			.description(
				"Initialize a QuoterV0 registry entry for one perp market, quoter program and quoted user. The entry starts active but unapproved, and nothing fills from it until the admin approves it with set-approved. For a custom-type entry the signing authority must be the quoted user's authority, because that creation is the user's consent. Routing priority defaults by type: vamm 0, clob 10, custom 20. The admin changes it with set-priority. <quoteDisc> and <executeDisc> are the 8-byte instruction discriminators on the quoter program, as 16 hex chars. Set the account lists afterwards with update-accounts."
			)
			.option(
				'-t, --type <type>',
				'quoter type: vamm | clob | custom',
				'custom'
			)
			.option(
				'-a, --authority <pubkey>',
				'entry authority (must sign; defaults to the wallet)'
			)
			.option(
				'--l3-disc <hex|name>',
				'discriminator of the optional quote_l3_v0 leg, which reports the resting orders behind a ladder and who each belongs to (omit for a quoter that fills from one account)'
			)
	).action(
		async (
			market: string,
			quoterProgramArg: string,
			userArg: string,
			responseAccountArg: string,
			quoteDisc: string,
			executeDisc: string,
			flags: { type: string; authority?: string; l3Disc?: string },
			cmd: Command
		) => {
			const marketIndex = Number.parseInt(market, 10);
			const quoterProgram = new PublicKey(quoterProgramArg);
			const user = new PublicKey(userArg);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const authority = flags.authority
					? new PublicKey(flags.authority)
					: provider.wallet.publicKey;
				const quoterPda = getQuoterPublicKey(
					client.program.programId,
					marketIndex,
					quoterProgram,
					user
				);
				const ix = await client.getInitializeQuoterIx(
					marketIndex,
					{
						quoterType: parseQuoterType(flags.type),
						responseAccount: new PublicKey(responseAccountArg),
						quoteV0Discriminator: parseDiscriminator(quoteDisc),
						quoteL3V0Discriminator: flags.l3Disc
							? parseDiscriminator(flags.l3Disc)
							: new Array(8).fill(0),
						executeV0Discriminator: parseDiscriminator(executeDisc),
					},

					quoterProgram,
					user,
					authority
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter init'
				);

				reportDispatch(
					`quoter ${quoterPda.toBase58()} initialized (market ${marketIndex}, type ${flags.type.toLowerCase()})`,
					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		quoter
			.command('init-slab <market>')
			.description(
				"Initialize a perp market's QuoterSlabV0. The slab holds every approved quoter config for the market, and velocity signs every external quoter CPI as the slab. There is one slab per market. Approval with set-approved copies a staging entry into a slot and grows the account to fit, so the slab starts with one slot for the book and stays at the size its slots need. Permissionless, and the signer pays the rent."
			)
	).action(async (market: string, cmd: Command) => {
		const marketIndex = Number.parseInt(market, 10);
		const opts = readGlobalOpts(cmd);
		if (opts.multisig) {
			throw new Error('init-slab is permissionless — direct-send only');
		}

		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, false);
		try {
			const quoterSlab = getQuoterSlabPublicKey(
				client.program.programId,
				marketIndex
			);
			const ix = await client.getInitializeQuoterSlabIx(
				marketIndex,
				provider.wallet.publicKey
			);
			const result = await sendOrPropose(provider, [ix], undefined, '');
			reportDispatch(
				`quoter slab ${quoterSlab.toBase58()} initialized (market ${marketIndex})`,
				result
			);
		} finally {
			if ((client as any).isSubscribed) {
				await client.unsubscribe();
			}
		}
	});

	withGlobalOptions(
		quoter
			.command('update-accounts <quoter> <metas...>')
			.description(
				'Replace a quoter\'s registered CPI account list whole. Each meta is "<pubkey>" for readonly or "<pubkey>:w" for writable. --quote-indexes and --execute-indexes pick which metas each leg forwards, in CPI order. The approved slab copy keeps serving until the admin approves again with set-approved. The signer must be the entry authority.'
			)
			.requiredOption(
				'--quote-indexes <list>',
				'comma-separated indexes into <metas...> forwarded to quote_v0 / quote_l3_v0'
			)
			.requiredOption(
				'--execute-indexes <list>',
				'comma-separated indexes into <metas...> forwarded to execute_v0'
			)
			.option(
				'-a, --authority <pubkey>',
				'entry authority (must sign; defaults to the wallet)'
			)
	).action(
		async (
			quoterArg: string,
			metas: string[],
			flags: {
				quoteIndexes: string;
				executeIndexes: string;
				authority?: string;
			},

			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				// A book's entry answers to the State admin roles rather than to the
				// key that registered it. A Custom entry answers to its own stored
				// authority and ignores this one.
				const ix = await client.getUpdateQuoterAccountsIx(
					new PublicKey(quoterArg),
					metas.map(parseAccountMeta),
					parseIndexList(flags.quoteIndexes),
					parseIndexList(flags.executeIndexes),
					flags.authority ? new PublicKey(flags.authority) : undefined
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter update-accounts'
				);

				reportDispatch(
					`quoter ${quoterArg} staged ${metas.length} accounts (quote [${flags.quoteIndexes}], execute [${flags.executeIndexes}]); re-approve to publish`,
					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		quoter
			.command('update-config <quoter>')
			.description(
				"Update a quoter's scalar CPI config on the staging entry. Only the passed options change. The approved slab copy keeps serving until the admin approves again with set-approved. The signer must be the entry authority."
			)
			.option('--response-account <pubkey>', 'new response account')
			.option('--quote-disc <hex>', 'new quote_v0 discriminator (16 hex chars)')
			.option(
				'--l3-disc <hex>',
				'new quote_l3_v0 discriminator (16 hex chars). An all-zero value withdraws the leg'
			)
			.option(
				'--execute-disc <hex>',
				'new execute_v0 discriminator (16 hex chars)'
			)
			.option(
				'-a, --authority <pubkey>',
				'entry authority (must sign; defaults to the wallet)'
			)
	).action(
		async (
			quoterArg: string,
			flags: {
				responseAccount?: string;
				quoteDisc?: string;
				l3Disc?: string;
				executeDisc?: string;
				authority?: string;
			},

			cmd: Command
		) => {
			if (
				!flags.responseAccount &&
				!flags.quoteDisc &&
				!flags.l3Disc &&
				!flags.executeDisc
			) {
				throw new Error(
					'nothing to update — pass at least one of --response-account, --quote-disc, --l3-disc, --execute-disc'
				);
			}

			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const ix = client.program.instruction.updateQuoterConfig(
					{
						responseAccount: flags.responseAccount
							? new PublicKey(flags.responseAccount)
							: null,
						quoteV0Discriminator: flags.quoteDisc
							? parseDiscriminator(flags.quoteDisc)
							: null,
						quoteL3V0Discriminator: flags.l3Disc
							? parseDiscriminator(flags.l3Disc)
							: null,
						executeV0Discriminator: flags.executeDisc
							? parseDiscriminator(flags.executeDisc)
							: null,
					},

					{
						accounts: {
							authority: flags.authority
								? new PublicKey(flags.authority)
								: provider.wallet.publicKey,
							quoter: new PublicKey(quoterArg),
							// A book's entry answers to the State admin roles rather
							// than to the key that registered it. A Custom entry
							// answers to its own stored authority and ignores this.
							state: await client.getStatePublicKey(),
						},
					}
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter update-config'
				);

				reportDispatch(
					`quoter ${quoterArg} config staged; re-approve to publish`,
					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		quoter
			.command('set-active <quoter> <active>')
			.description(
				"The maker's own kill switch. Enable or disable the entry. The value is written through to the market's slab slot when one exists, so the change takes effect without a new approval. The entry authority can always run this. For a Custom quoter that authority is the quoted user's authority. <active> = true|false."
			)
			.option(
				'-a, --authority <pubkey>',
				'entry authority (must sign; defaults to the wallet)'
			)
	).action(
		async (
			quoterArg: string,
			active: string,
			flags: { authority?: string },
			cmd: Command
		) => {
			const on = parseEnable(active);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const quoterKey = new PublicKey(quoterArg);
				const ix = client.program.instruction.updateQuoterActive(
					{ active: on },
					{
						accounts: {
							authority: flags.authority
								? new PublicKey(flags.authority)
								: provider.wallet.publicKey,
							quoter: quoterKey,
							quoterSlab: await liveSlabFor(
								client,
								provider.connection,
								quoterKey
							),

							// A book's entry answers to the State admin roles rather
							// than to the key that registered it. A Custom entry
							// answers to its own stored authority and ignores this.
							state: await client.getStatePublicKey(),
						},
					}
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter set-active'
				);

				reportDispatch(
					`quoter ${quoterArg} ${on ? 'activated' : 'deactivated'}`,
					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		quoter
			.command('set-approved <quoter> <approved>')
			.description(
				"Admin vetting gate for the warm or cold admin. Copies the staging config into the market's slab slot, or revokes that slot. Approval requires a non-empty account list on both legs, and each list must contain the response account. Fills read only the slab copy, so a staged edit serves nothing until it is approved here. <approved> = true|false."
			)
			.option(
				'--admin <pubkey>',
				'admin signer (defaults to the wallet; pass the vault PDA with --multisig)'
			)
	).action(
		async (
			quoterArg: string,
			approved: string,
			flags: { admin?: string },
			cmd: Command
		) => {
			const on = parseEnable(approved);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				// Approval approves a binary, so the program behind the entry must
				// be frozen. The program-data account records whether it is. The
				// program id comes from the entry rather than from a flag, because
				// an operator who names the wrong program would approve the wrong
				// code.
				const quoterKey = new PublicKey(quoterArg);
				const entry = await (client.program.account as any).quoterV0.fetch(
					quoterKey
				);
				// A book approval asks the book for its own placement rules, so a slot
				// that would fail every fill is refused rather than approved. No other
				// type reads a book.
				const ix = await client.getUpdateQuoterApprovedIx(
					quoterKey,
					on,
					entry.config.market,
					new PublicKey(entry.config.programId),
					isClobEntry(entry.config.quoterType)
						? new PublicKey(entry.config.responseAccount)
						: null,
					flags.admin ? new PublicKey(flags.admin) : undefined
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter set-approved'
				);

				reportDispatch(
					`quoter ${quoterArg} ${on ? 'approved' : 'unapproved'}`,
					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		quoter
			.command('set-priority <quoter> <priority>')
			.description(
				"Set a quoter registry entry's routing priority. The warm or cold admin signs. At one price, lower-priority tiers fill first, pro rata within a tier. Registration defaults by type: vamm 0, clob 10, custom 20. The value is written through to the market's slab slot when one exists. Only the admin can set it, because a maker who chose their own priority could fill ahead of the vAMM and the book. <priority> = 0-255."
			)
			.option(
				'--admin <pubkey>',
				'admin signer (defaults to the wallet; pass the vault PDA with --multisig)'
			)
	).action(
		async (
			quoterArg: string,
			priorityArg: string,
			flags: { admin?: string },
			cmd: Command
		) => {
			const priority = Number.parseInt(priorityArg, 10);
			if (!Number.isInteger(priority) || priority < 0 || priority > 255) {
				throw new Error(`priority must be 0-255, got "${priorityArg}"`);
			}

			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const quoterKey = new PublicKey(quoterArg);
				const ix = client.program.instruction.updateQuoterPriority(
					{ priority },
					{
						accounts: {
							admin: flags.admin
								? new PublicKey(flags.admin)
								: provider.wallet.publicKey,
							state: await client.getStatePublicKey(),
							quoter: quoterKey,
							quoterSlab: await liveSlabFor(
								client,
								provider.connection,
								quoterKey
							),
						},
					}
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter set-priority'
				);

				reportDispatch(`quoter ${quoterArg} priority = ${priority}`, result);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		withCrankCostUnitOptions(
			quoter
				.command(
					'set-market-clob <market> <quoter> <clobMarket> [expireFallbackSlots]'
				)
				.description(
					"Name a perp market's canonical CLOB quoter entry. The warm or cold admin signs. Once set, every router fill must carry that entry. The same instruction also creates or re-prices the market's cranks: the lamport reservoir that pays relay keepers, and the registration that tells the book which resolver answers each of its own conditions. Those conditions are an expired order, a side at its cap, a crossed book, and an order reaching its activation slot. Each crank's payment is derived here from the cost units it requests and State.transactionFeeRails, so running this again is how a market is re-priced after the network's fee model changes. Add lamports to the reservoir with a plain transfer to the conditions PDA. [expireFallbackSlots] is the cross fallback poll interval, default 1500 slots or about 10 minutes. It is the liveness floor for a cross that a PropAMM created by repricing."
				)
		)
			.option(
				'--min-cross-surplus <quote>',
				"floor on what the protocol must net from a cross-match crank, in QUOTE_PRECISION (1e6). Cranking a cross pays the reservoir's keeper fee, so a cross that clears by a cent costs the protocol more than it earns, and anyone can create one. Must be above zero. Set it high enough to cover the cross payout with margin",
				'10000'
			)
			.option(
				'--admin <pubkey>',
				'admin signer (defaults to the wallet; pass the vault PDA with --multisig)'
			)
	).action(
		async (
			market: string,
			quoterArg: string,
			clobMarket: string,
			expireFallbackSlots: string | undefined,
			flags: { admin?: string; minCrossSurplus: string },
			cmd: Command
		) => {
			const crankCostUnits = readCrankCostUnits(
				flags as unknown as Record<string, string | undefined>
			);
			const marketIndex = Number.parseInt(market, 10);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				// The book's program comes from the entry, not from the command
				// line. This instruction registers velocity's resolvers on the
				// book, and the entry is what that registration is checked
				// against.
				const entryInfo = await provider.connection.getAccountInfo(
					new PublicKey(quoterArg)
				);

				if (!entryInfo) {
					throw new Error(`quoter entry ${quoterArg} not found`);
				}

				const clobProgramId = new PublicKey(
					(
						client.program.coder.accounts.decode(
							'quoterV0',
							entryInfo.data
						) as { config: { programId: PublicKey } }
					).config.programId
				);
				const ix = await client.getUpdatePerpMarketClobQuoterIx(
					marketIndex,
					new PublicKey(quoterArg),
					new PublicKey(clobMarket),
					clobProgramId,
					{
						crankCostUnits,
						expireFallbackSlots: new BN(expireFallbackSlots ?? 1500),
						minCrossSurplus: new BN(flags.minCrossSurplus),
					},

					flags.admin ? new PublicKey(flags.admin) : undefined
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter set-market-clob'
				);

				reportDispatch(
					`perp-market[${market}] clob_quoter = ${quoterArg}, cranks priced from ${JSON.stringify(
						crankCostUnits
					)} cost units`,

					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);
	withGlobalOptions(
		quoter
			.command('set-watch <quoter>')
			.description(
				"Declare or clear a Custom quoter's reprice-watch region. The region is the account bytes whose change means the quoter may quote differently, such as a midpoint's mid region. Relay cross-discovery conditions wake on it. The declaration is staged on the entry, and the admin publishes it to the slab with set-approved. The signer must be the entry authority."
			)
			.option(
				'--watch-account <pubkey>',
				'account whose bytes the watch covers'
			)
			.requiredOption('--offset <n>', 'watch region offset (account data)')
			.requiredOption(
				'--len <n>',
				'watch region length. 0 clears the declaration'
			)
			.option(
				'-a, --authority <pubkey>',
				'entry authority (must sign; defaults to the wallet)'
			)
	).action(
		async (
			quoterArg: string,
			flags: {
				watchAccount?: string;
				offset: string;
				len: string;
				authority?: string;
			},

			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const watchLen = Number.parseInt(flags.len, 10);
				if (watchLen > 0 && !flags.watchAccount) {
					throw new Error('--watch-account is required unless --len is 0');
				}

				const ix = client.program.instruction.updateQuoterWatch(
					{
						watchOffset: Number.parseInt(flags.offset, 10),
						watchLen,
					},

					{
						accounts: {
							authority: flags.authority
								? new PublicKey(flags.authority)
								: provider.wallet.publicKey,
							quoter: new PublicKey(quoterArg),
							watchAccount: flags.watchAccount
								? new PublicKey(flags.watchAccount)
								: PublicKey.default,
							// A book's entry answers to the State admin roles rather
							// than to the key that registered it. A Custom entry
							// answers to its own stored authority and ignores this.
							state: await client.getStatePublicKey(),
						},
					}
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter set-watch'
				);

				reportDispatch(
					`quoter ${quoterArg} watch ${
						watchLen > 0 ? 'declared' : 'cleared'
					}; re-approve to publish`,

					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		quoter
			.command('set-oracle-band <quoter> <bps>')
			.description(
				"Declare or clear how far from oracle a Custom quoter's fills may price, in basis points. Velocity already bounds every external leg by the market's own band. This asks for a tighter one, so a maker caps what its own program can lose if an attacker takes that program over. The program applies the smaller of this value and the market's margin ratio, so the declaration can only tighten a bound the admin already approved. That is why it is written through to the market's slab slot without a new approval. <bps> = 0 clears the declaration. The signer must be the entry authority."
			)
			.option(
				'-a, --authority <pubkey>',
				'entry authority (must sign; defaults to the wallet)'
			)
	).action(
		async (
			quoterArg: string,
			bpsArg: string,
			flags: { authority?: string },
			cmd: Command
		) => {
			const bps = Number.parseInt(bpsArg, 10);
			if (!Number.isInteger(bps) || bps < 0 || bps >= 10_000) {
				throw new Error(`bps must be 0-9999, got "${bpsArg}"`);
			}

			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const quoterKey = new PublicKey(quoterArg);
				const ix = client.program.instruction.updateQuoterMaxOracleDeviation(
					{ maxOracleDeviationBps: bps },
					{
						accounts: {
							authority: flags.authority
								? new PublicKey(flags.authority)
								: provider.wallet.publicKey,
							quoter: quoterKey,
							quoterSlab: await liveSlabFor(
								client,
								provider.connection,
								quoterKey
							),
						},
					}
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter set-oracle-band'
				);

				reportDispatch(
					`quoter ${quoterArg} oracle band ${
						bps > 0 ? `= ${bps} bps` : 'cleared (market band stands)'
					}`,

					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		quoter
			.command('attach-cross <quoter>')
			.description(
				"Create or re-price a Custom quoter's relay cross-discovery conditions. That per-entry account holds the resolver which prices the quoter through its registered quote_v0 surface and stages crank_cross_match. Permissionless, and the signer pays the rent. The entry must be active and approved, and the market's canonical CLOB must be attached."
			)
			.option('--fallback-slots <n>', 'periodic poll interval (slots)', '1500')
	).action(
		async (
			quoterArg: string,
			flags: { fallbackSlots: string },
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			if (opts.multisig) {
				throw new Error('attach-cross is permissionless — direct-send only');
			}

			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const quoterKey = new PublicKey(quoterArg);
				const entry = await (client.program.account as any).quoterV0.fetch(
					quoterKey
				);
				const perpMarket = getPerpMarketPublicKeySync(
					client.program.programId,
					entry.config.market
				);
				const marketConditions = getClobCrankConditionsPublicKey(
					client.program.programId,
					entry.config.market
				);
				const crossConditions = getQuoterCrossConditionsPublicKey(
					client.program.programId,
					quoterKey
				);
				const ix = client.program.instruction.initializeQuoterCrossConditions(
					{ expireFallbackSlots: new BN(flags.fallbackSlots) },
					{
						accounts: {
							payer: provider.wallet.publicKey,
							state: await client.getStatePublicKey(),
							quoter: quoterKey,
							perpMarket,
							quoterSlab: getQuoterSlabPublicKey(
								client.program.programId,
								entry.config.market
							),

							marketConditions,
							crossConditions,
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
				);
				const result = await sendOrPropose(provider, [ix], undefined, '');
				reportDispatch(
					`cross conditions ${crossConditions.toBase58()} attached for quoter ${quoterArg}`,
					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);
}
