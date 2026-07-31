import { Command } from 'commander';
import { PublicKey, SystemProgram, SYSVAR_RENT_PUBKEY } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	QuoterCpiLeg,
	QuoterType,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';

/** Parse a CLI truthy/falsy flag argument (`true|false|on|off|1|0|enable|disable`). */
function parseEnable(value: string): boolean {
	const v = value.trim().toLowerCase();
	if (['true', 'on', '1', 'enable', 'enabled', 'yes'].includes(v)) return true;
	if (['false', 'off', '0', 'disable', 'disabled', 'no'].includes(v))
		return false;
	throw new Error(
		`expected true|false (got "${value}"). Use on/off, 1/0, enable/disable.`
	);
}

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

function parseLeg(value: string): QuoterCpiLeg {
	switch (value.toLowerCase()) {
		case 'quote':
			return QuoterCpiLeg.QUOTE;
		case 'execute':
			return QuoterCpiLeg.EXECUTE;
		default:
			throw new Error(`leg must be "quote" or "execute", got "${value}"`);
	}
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

/** Derive the `QuoterV0` PDA from seeds `["quoter", marketIndex as u16 LE, quoterProgram, user]`. */
function getQuoterPublicKey(
	programId: PublicKey,
	marketIndex: number,
	quoterProgram: PublicKey,
	user: PublicKey
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[
			Buffer.from('quoter'),
			new BN(marketIndex).toArrayLike(Buffer, 'le', 2),
			quoterProgram.toBuffer(),
			user.toBuffer(),
		],
		programId
	)[0];
}

/**
 * Quoter registry operations (PropAMM order flow, see `state::prop_amm`).
 *
 * For Custom quoters the quoted user's authority creates the entry — creation
 * is consent — and keeps a permanent kill switch (`set-active`). Nothing
 * fills until the admin vets the CPI surface (`set-approved`), and any config
 * or account-list change clears that approval for re-vetting.
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
				"Initialize a QuoterV0 registry entry for (perp market, quoter program, quoted user). Born active but unapproved — nothing fills until the admin vets it (set-approved). For custom-type entries the signing authority must be the quoted user's authority (creation is consent). Routing priority defaults by type (vamm 0, clob 10, custom 20); admin-adjustable via set-priority. <quoteDisc>/<executeDisc> are the 8-byte instruction discriminators on the quoter program, as 16 hex chars. Set account lists afterwards via update-accounts."
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
	).action(
		async (
			market: string,
			quoterProgramArg: string,
			userArg: string,
			responseAccountArg: string,
			quoteDisc: string,
			executeDisc: string,
			flags: { type: string; authority?: string },
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
				const ix = client.program.instruction.initializeQuoter(
					{
						marketIndex,
						quoterType: parseQuoterType(flags.type),
						responseAccount: new PublicKey(responseAccountArg),
						quoteV0Discriminator: parseDiscriminator(quoteDisc),
						executeV0Discriminator: parseDiscriminator(executeDisc),
					},
					{
						accounts: {
							payer: authority,
							authority,
							quoter: quoterPda,
							perpMarket: getPerpMarketPublicKeySync(
								client.program.programId,
								marketIndex
							),
							quoterProgram,
							user,
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
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
			.command('update-accounts <quoter> <leg> <index> <metas...>')
			.description(
				'Write a slice of a quoter\'s registered CPI account list, starting at <index>. <leg> is "quote" or "execute"; each meta is "<pubkey>" (readonly) or "<pubkey>:w" (writable). Truncates the list to the end of the slice and clears admin approval (admin re-vets). Signer must be the entry authority.'
			)
			.option(
				'-a, --authority <pubkey>',
				'entry authority (must sign; defaults to the wallet)'
			)
	).action(
		async (
			quoterArg: string,
			leg: string,
			index: string,
			metas: string[],
			flags: { authority?: string },
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const ix = client.program.instruction.updateQuoterAccounts(
					{
						leg: parseLeg(leg),
						index: Number.parseInt(index, 10),
						metas: metas.map(parseAccountMeta),
					},
					{
						accounts: {
							authority: flags.authority
								? new PublicKey(flags.authority)
								: provider.wallet.publicKey,
							quoter: new PublicKey(quoterArg),
						},
					}
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter update-accounts'
				);
				reportDispatch(
					`quoter ${quoterArg} ${leg.toLowerCase()} accounts [${index}, ${
						Number.parseInt(index, 10) + metas.length
					}) updated (approval cleared)`,
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
				"Update a quoter's scalar CPI config; only the passed options change. Clears admin approval (admin re-vets). Signer must be the entry authority."
			)
			.option('--response-account <pubkey>', 'new response account')
			.option('--quote-disc <hex>', 'new quote_v0 discriminator (16 hex chars)')
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
				executeDisc?: string;
				authority?: string;
			},
			cmd: Command
		) => {
			if (!flags.responseAccount && !flags.quoteDisc && !flags.executeDisc) {
				throw new Error(
					'nothing to update — pass at least one of --response-account, --quote-disc, --execute-disc'
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
					`quoter ${quoterArg} config updated (approval cleared)`,
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
				"The maker's own kill switch: enable or disable the entry. Always available to the entry authority — for Custom quoters the quoted user's authority. <active> = true|false."
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
				const ix = client.program.instruction.updateQuoterActive(on, {
					accounts: {
						authority: flags.authority
							? new PublicKey(flags.authority)
							: provider.wallet.publicKey,
						quoter: new PublicKey(quoterArg),
					},
				});
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
				'Admin vetting gate (warm/cold admin): approve or unapprove a quoter registry entry. Approving validates non-empty account lists on both legs, each containing the response account. Any config or account-list change clears approval. <approved> = true|false.'
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
				const ix = client.program.instruction.updateQuoterApproved(on, {
					accounts: {
						admin: flags.admin
							? new PublicKey(flags.admin)
							: provider.wallet.publicKey,
						state: await client.getStatePublicKey(),
						quoter: new PublicKey(quoterArg),
					},
				});
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
				'Set a quoter registry entry\'s routing priority (warm/cold admin): at a price, lower-priority tiers fill first, pro rata within a tier. Registration defaults by type (vamm 0, clob 10, custom 20). Admin-only — a maker choosing their own priority could jump the vAMM/CLOB. <priority> = 0-255.'
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
				const ix = client.program.instruction.updateQuoterPriority(priority, {
					accounts: {
						admin: flags.admin
							? new PublicKey(flags.admin)
							: provider.wallet.publicKey,
						state: await client.getStatePublicKey(),
						quoter: new PublicKey(quoterArg),
					},
				});
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
		quoter
			.command(
				'set-market-clob <market> <quoter> <clobMarket> <keeperPaymentLamports> [expireFallbackSlots]'
			)
			.description(
				"Name a perp market's canonical CLOB quoter entry (warm/cold admin): once set, every router fill must carry it (mandatory baseline). Also stands up (or re-prices) the market's relay crank conditions account in the same instruction — the evict/expire condition block plus the lamport reservoir that pays relay keepers <keeperPaymentLamports> per crank. Top the reservoir off with a plain lamport transfer to the conditions PDA. [expireFallbackSlots] is the expire fallback poll interval (default 1500 slots, ~10 min)."
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
			keeperPaymentLamports: string,
			expireFallbackSlots: string | undefined,
			flags: { admin?: string },
			cmd: Command
		) => {
			const marketIndex = Number.parseInt(market, 10);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const ix = client.program.instruction.updatePerpMarketClobQuoter(
					new BN(keeperPaymentLamports),
					new BN(expireFallbackSlots ?? 1500),
					{
						accounts: {
							admin: flags.admin
								? new PublicKey(flags.admin)
								: provider.wallet.publicKey,
							state: await client.getStatePublicKey(),
							perpMarket: getPerpMarketPublicKeySync(
								client.program.programId,
								marketIndex
							),
							quoter: new PublicKey(quoterArg),
							clobMarket: new PublicKey(clobMarket),
							crankConditions: getClobCrankConditionsPublicKey(
								client.program.programId,
								marketIndex
							),
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin quoter set-market-clob'
				);
				reportDispatch(
					`perp-market[${market}] clob_quoter = ${quoterArg}, crank pays ${keeperPaymentLamports} lamports`,
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
				"Declare (or clear) a Custom quoter's reprice-watch region — the account bytes whose change means the quoter may quote differently (a midpoint's mid region). Relay cross-discovery conditions wake on it. Clears admin approval (admin re-vets). Signer must be the entry authority."
			)
			.option('--watch-account <pubkey>', 'account whose bytes the watch covers')
			.requiredOption('--offset <n>', 'watch region offset (account data)')
			.requiredOption('--len <n>', 'watch region length; 0 clears the declaration')
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
					`quoter ${quoterArg} watch ${watchLen > 0 ? 'declared' : 'cleared'} (approval cleared)`,
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
				"Stand up (or re-price) a Custom quoter's relay cross-discovery conditions — the per-entry account whose resolver prices the quoter through its registered quote_v0 surface and stages crank_cross_match. Permissionless; the signer pays the rent. Requires the entry active + approved and the market's canonical CLOB attached."
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
					entry.market
				);
				const marketConditions = getClobCrankConditionsPublicKey(
					client.program.programId,
					entry.market
				);
				const crossConditions = PublicKey.findProgramAddressSync(
					[Buffer.from('quoter_cross_conditions'), quoterKey.toBuffer()],
					client.program.programId
				)[0];
				const market = await (
					client.program.account as any
				).perpMarket.fetch(perpMarket);
				const ix = client.program.instruction.initializeQuoterCrossConditions(
					new BN(flags.fallbackSlots),
					{
						accounts: {
							payer: provider.wallet.publicKey,
							state: await client.getStatePublicKey(),
							quoter: quoterKey,
							perpMarket,
							clobQuoter: market.clobQuoter,
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

