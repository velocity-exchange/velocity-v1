import { Command } from 'commander';
import {
	PublicKey,
	SystemProgram,
	SYSVAR_RENT_PUBKEY,
	TransactionInstruction,
} from '@solana/web3.js';
import {
	encodeName,
	getInsuranceFundVaultPublicKey,
	getPythLazerOraclePublicKey,
	getSpotMarketPublicKeySync,
	getSpotMarketVaultPublicKey,
	getPerpMarketPublicKeySync,
	getVelocitySignerPublicKey,
} from '@velocity-exchange/sdk';
import * as fs from 'fs';
import * as path from 'path';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import {
	InstructionGroup,
	reportDispatch,
	resolveAdminAuthority,
	sendOrProposeBatch,
} from '../lib/squads';
import { deriveAssociatedTokenAccount } from '../lib/userOps';
import { BN } from '@coral-xyz/anchor';
import {
	getInsuranceFundStakeAccountPublicKey,
	getUserAccountPublicKeySync,
	initialize,
	PythLazerClient,
} from '@velocity-exchange/sdk';
import * as ui from '../lib/ui';

/**
 * Turn a reviewed market-params entry into the payloads a listing needs.
 *
 * Params live in `deploy-scripts/params/` and are reviewed there. The payloads
 * that turn them into instructions used to be written by hand on one laptop,
 * so only their author could list a market. This derives them instead, with
 * every PDA computed rather than typed. Output keeps the
 * `{ instructions: [...] }` shape `propose-batch` consumes, so it stays
 * diffable instead of becoming a black box between params and chain.
 */

const camel = (snake: string): string =>
	snake.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());

/**
 * Anchor camelCases IDL names when it loads them, so `program.idl` reports
 * `optimalUtilization` where the params files (and the on-chain source) say
 * `optimal_utilization`. Everything is normalised to snake_case for lookup.
 */
const snake = (name: string): string =>
	name.replace(/([A-Z])/g, '_$1').toLowerCase();

/**
 * Build a v2 `params` struct from the params entry, driven by the IDL type
 * rather than a hand-written field list.
 *
 * Hand-mapping is how a listing ends up proposed with `funding_ramp_slope`
 * quietly absent: the payload looks complete, the IDL disagrees, and nothing
 * says so until the simulation fails or, worse, does not. Walking the IDL's own
 * field list makes a missing value a loud error. `overrides` carries what
 * cannot come from the params file as written, keyed by snake_case name.
 */
function structFromParams(
	idl: any,
	typeName: string,
	params: any,
	overrides: Record<string, unknown>
): Record<string, unknown> {
	// Anchor lowercases the first letter of type names when it loads an IDL, so
	// the on-chain `InitializeSpotMarketArgs` reads back as
	// `initializeSpotMarketArgs`. Match either spelling.
	const wanted = typeName.charAt(0).toLowerCase() + typeName.slice(1);
	const def = idl.types?.find(
		(t: any) => t.name === typeName || t.name === wanted
	);
	if (!def?.type?.fields) {
		throw new Error(`${typeName} not found in IDL types`);
	}
	const out: Record<string, unknown> = {};
	const missing: string[] = [];
	for (const field of def.type.fields) {
		const key = snake(field.name);
		if (key in overrides) {
			out[camel(field.name)] = overrides[key];
			continue;
		}
		if (params[key] !== undefined) {
			out[camel(field.name)] = params[key];
			continue;
		}
		missing.push(key);
	}
	if (missing.length > 0) {
		throw new Error(
			`${typeName}: no value for ${missing.join(', ')}. ` +
				`Add ${missing.length === 1 ? 'it' : 'them'} to the params entry for ` +
				`"${params.name}", or this listing would be proposed incomplete.`
		);
	}
	return out;
}

/**
 * Walk up from this file until a directory holds `deploy-scripts/params`.
 *
 * A fixed number of `..` hops encodes where this file sits in the tree, so it
 * breaks when the build output nests differently or the CLI is installed as a
 * package rather than run from a checkout. Searching for the directory the
 * params actually live in does not care about either, or about what the repo
 * checkout is named.
 */
function paramsDir(): string | undefined {
	let dir = __dirname;
	for (let i = 0; i < 8; i++) {
		const candidate = path.join(dir, 'deploy-scripts', 'params');
		if (fs.existsSync(candidate)) {
			return candidate;
		}
		const parent = path.dirname(dir);
		if (parent === dir) {
			break;
		}
		dir = parent;
	}
	return undefined;
}

function defaultParamsPath(kind: 'spot' | 'perp'): string | undefined {
	const dir = paramsDir();
	return dir ? path.join(dir, `relaunch-${kind}-markets.json`) : undefined;
}

function loadParams(kind: 'spot' | 'perp', file: string | undefined): any[] {
	const resolved = file ?? defaultParamsPath(kind);
	if (!resolved) {
		throw new Error(
			'could not find deploy-scripts/params above this CLI. Run from a repo ' +
				`checkout, or pass --${kind}-params <file>.`
		);
	}
	if (!fs.existsSync(resolved)) {
		throw new Error(
			`params file not found: ${resolved}\nPass --${kind}-params <file> to point at it.`
		);
	}
	const parsed = JSON.parse(fs.readFileSync(resolved, 'utf-8'));
	const list = Array.isArray(parsed)
		? parsed
		: parsed.markets ?? parsed[`${kind}_markets`];
	if (!Array.isArray(list)) {
		throw new Error(`${resolved}: expected an array of markets`);
	}
	return list;
}

/**
 * Spot params name the market `ZEC`, perp params name it `ZEC-PERP`, so a
 * single symbol from the operator has to match either spelling.
 */
function pick(list: any[], symbol: string): any {
	const candidates = [
		symbol.toLowerCase(),
		`${symbol.toLowerCase()}-perp`,
		symbol.toLowerCase().replace(/-perp$/, ''),
	];
	const found = list.find(
		(m) =>
			candidates.includes(String(m.name).toLowerCase()) ||
			String(m.market_index) === symbol
	);
	if (!found) {
		throw new Error(
			`no market "${symbol}" in params (have: ${list
				.map((m) => m.name)
				.join(', ')})`
		);
	}
	return found;
}

type Payload = {
	file: string;
	body: { instructions: Array<{ ix: string; args: any; accounts: any }> };
};

function write(payloads: Payload[], outDir: string | undefined): void {
	if (!outDir) {
		for (const p of payloads) {
			ui.header(p.file);
			console.log(JSON.stringify(p.body, null, 2));
		}
		return;
	}
	fs.mkdirSync(outDir, { recursive: true });
	for (const p of payloads) {
		const target = path.join(outDir, p.file);
		fs.writeFileSync(target, `${JSON.stringify(p.body, null, 2)}\n`);
		ui.kv(p.file, pc.dim(target));
	}
}

export function registerMarket(parent: Command): void {
	const market = parent
		.command('market')
		.description(
			'Derive listing payloads from the reviewed params in deploy-scripts/params.'
		);

	withGlobalOptions(
		market
			.command('fund <symbol>')
			.description(
				'Fund a listed market from one Squads batch: lending deposit, insurance ' +
					'fund stake, vAMM fee pool, pnl pool. All four sign as the same vault, ' +
					'so they are one proposal and one approval rather than four. Amounts are ' +
					'raw base units of the market they fund; omit a flag to skip that pool. ' +
					'Requires --multisig, and the vault must hold the tokens and the ' +
					'VaultDeposit hot role.'
			)
			.option('--deposit <raw>', 'lending deposit into the spot market')
			.option('--if-stake <raw>', 'insurance fund stake on the spot market')
			.option('--fee-pool <raw>', 'vAMM fee pool, quote base units')
			.option('--pnl-pool <raw>', 'pnl pool, quote base units')
			.option('--sub-account <n>', 'sub-account for the lending deposit', '0')
			.option('--spot-params <file>', 'override the spot params file')
			.option('--perp-params <file>', 'override the perp params file')
	).action(async (symbol: string, flags: any, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		if (!opts.multisig) {
			throw new Error(
				'market fund builds a Squads batch, so it needs --multisig (or a ' +
					'profile that sets one). Use the individual commands to send directly.'
			);
		}
		const multisigPda = new PublicKey(opts.multisig);
		const authority = resolveAdminAuthority(provider, multisigPda, 0);
		const subAccountId = Number.parseInt(flags.subAccount, 10);

		// Scoping the client to a sub-account subscribes to it, and that fails with
		// `User account not loaded after force fetch` when the account does not
		// exist, which says nothing about the cause. Only the lending deposit needs
		// the scope, so check first and say what to do.
		if (flags.deposit) {
			const userPk = getUserAccountPublicKeySync(
				new PublicKey(initialize({ env: opts.env }).VELOCITY_PROGRAM_ID),
				authority,
				subAccountId
			);
			const exists = await provider.connection.getAccountInfo(userPk);
			if (!exists) {
				throw new Error(
					`vault ${authority.toBase58()} has no sub-account ${subAccountId} ` +
						`(${userPk.toBase58()}), so it cannot make a lending deposit. ` +
						'Create it first with `velocity-admin user init <name> --authority ' +
						`${authority.toBase58()}\`, or drop --deposit to fund the other pools.`
				);
			}
		}

		const client = await buildAdminClient(
			opts,
			true,
			flags.deposit ? { authority, subAccountId } : undefined
		);
		try {
			const spot =
				flags.deposit || flags.ifStake
					? pick(loadParams('spot', flags.spotParams), symbol)
					: undefined;
			const perp =
				flags.feePool || flags.pnlPool
					? pick(loadParams('perp', flags.perpParams), symbol)
					: undefined;
			if (!spot && !perp) {
				throw new Error(
					'nothing to fund: pass at least one of --deposit, --if-stake, ' +
						'--fee-pool, --pnl-pool'
				);
			}

			const groups: InstructionGroup[] = [];

			if (spot) {
				const marketIndex = spot.market_index;
				const spotMarket = (client as any).getSpotMarketAccountOrThrow(
					marketIndex
				);
				const tokenProgram = (client as any).getTokenProgramForSpotMarket(
					spotMarket
				);
				const userTokenAccount = deriveAssociatedTokenAccount(
					spotMarket.mint,
					authority,
					tokenProgram
				);

				if (flags.deposit) {
					groups.push({
						label: `deposit ${flags.deposit} into spot[${marketIndex}]`,
						instructions: [
							await (client as any).getDepositInstruction(
								new BN(flags.deposit),
								marketIndex,
								userTokenAccount,
								subAccountId,
								false,
								true,
								{ authority }
							),
						],
					});
				}

				if (flags.ifStake) {
					const ixs: TransactionInstruction[] = [];
					const stakePk = getInsuranceFundStakeAccountPublicKey(
						client.program.programId,
						authority,
						marketIndex
					);
					// A market listed today has no stake account yet, so the init rides
					// in the same group. Splitting them would let the batch stop between
					// an empty stake account and its first deposit.
					if (!(await provider.connection.getAccountInfo(stakePk))) {
						ixs.push(
							await (client as any).getInitializeInsuranceFundStakeIx(
								marketIndex,
								{ authority }
							)
						);
					}
					ixs.push(
						await (client as any).getAddInsuranceFundStakeIx(
							marketIndex,
							new BN(flags.ifStake),
							userTokenAccount,
							{ authority }
						)
					);
					groups.push({
						label: `if stake ${flags.ifStake} on spot[${marketIndex}]${
							ixs.length > 1 ? ' (+init)' : ''
						}`,
						instructions: ixs,
					});
				}
			}

			if (perp) {
				const marketIndex = perp.market_index;
				const quote = client.getQuoteSpotMarketAccount();
				const sourceVault = deriveAssociatedTokenAccount(
					quote.mint,
					authority,
					(client as any).getTokenProgramForSpotMarket(quote)
				);
				if (flags.feePool) {
					groups.push({
						label: `fee pool ${flags.feePool} on perp[${marketIndex}]`,
						instructions: [
							await client.getDepositIntoPerpMarketFeePoolIx(
								marketIndex,
								new BN(flags.feePool),
								sourceVault,
								authority
							),
						],
					});
				}
				if (flags.pnlPool) {
					groups.push({
						label: `pnl pool ${flags.pnlPool} on perp[${marketIndex}]`,
						instructions: [
							await client.getDepositIntoPerpMarketPnlPoolIx(
								marketIndex,
								new BN(flags.pnlPool),
								sourceVault,
								authority
							),
						],
					});
				}
			}

			const result = await sendOrProposeBatch(
				provider,
				groups,
				multisigPda,
				`velocity-admin market fund ${symbol}: ${groups.length} pool(s)`
			);
			reportDispatch(`fund ${symbol}, ${groups.length} group(s)`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		market
			.command('payloads <symbol>')
			.description(
				'Write the payload files that list a market, derived from its entry in ' +
					'deploy-scripts/params. Every PDA is computed from the market index and ' +
					'feed id rather than typed. Prints to stdout unless --out is given. Feed ' +
					'the results to `propose-batch` in the printed order, and read the ' +
					'ordering note it emits: the oracle payload cannot go in the same batch ' +
					'as the market init.'
			)
			.option('--spot-params <file>', 'override the spot params file')
			.option('--perp-params <file>', 'override the perp params file')
			.option(
				'--out <dir>',
				'write the payloads into this directory, creating it if needed. Without ' +
					'it the payloads are printed instead. Generated payloads are ' +
					'disposable: regenerate them rather than keeping them around, and do ' +
					'not write them somewhere they will get committed.'
			)
			.option('--spot-only', 'only the spot market payloads')
			.option('--perp-only', 'only the perp market payloads')
	).action(async (symbol: string, flags: any, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const programId = client.program.programId;
			const statePk = await (client as any).getStatePublicKey();
			const admin = resolveAdminAuthority(
				provider,
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				0
			);

			const wantSpot = !flags.perpOnly;
			const wantPerp = !flags.spotOnly;
			const payloads: Payload[] = [];

			const spot = wantSpot
				? pick(loadParams('spot', flags.spotParams), symbol)
				: undefined;
			const perp = wantPerp
				? pick(loadParams('perp', flags.perpParams), symbol)
				: undefined;

			// Spot and perp index spaces are independent: spot 1 is SOL while perp 1
			// is BTC-PERP. Looking a market up by index in both files can therefore
			// resolve two different assets, and deriving one oracle from one of them
			// would list the other against the wrong price feed.
			const base = (name: string) =>
				String(name)
					.toUpperCase()
					.replace(/-PERP$/, '');
			if (spot && perp && base(spot.name) !== base(perp.name)) {
				throw new Error(
					`"${symbol}" resolves to spot ${spot.name} (index ${spot.market_index}) ` +
						`but perp ${perp.name} (index ${perp.market_index}). Spot and perp ` +
						'market indexes are independent, so look the market up by symbol, ' +
						'or generate each side separately with --spot-only / --perp-only.'
				);
			}

			const feedOf = (m: any, kind: string) => {
				if (m.lazer_feed_id === undefined) {
					throw new Error(
						`${kind} params for "${m.name}" have no lazer_feed_id`
					);
				}
				return m.lazer_feed_id as number;
			};
			const spotFeed = spot ? feedOf(spot, 'spot') : undefined;
			const perpFeed = perp ? feedOf(perp, 'perp') : undefined;
			const spotOracle =
				spotFeed === undefined
					? undefined
					: getPythLazerOraclePublicKey(programId, spotFeed);
			const perpOracle =
				perpFeed === undefined
					? undefined
					: getPythLazerOraclePublicKey(programId, perpFeed);
			// One oracle payload per distinct feed: a spot and perp sharing a feed
			// share the PDA, and initialising it twice fails.
			const feeds = [
				...new Set([spotFeed, perpFeed].filter((f) => f !== undefined)),
			] as number[];

			ui.header(
				`${symbol} listing payloads`,
				pc.dim(`feed ${feeds.join(', ')}`)
			);
			ui.kv('admin', pc.dim(admin.toBase58()));
			if (spotOracle) {
				ui.kv(
					'spot oracle',
					pc.dim(`${spotOracle.toBase58()} (feed ${spotFeed})`)
				);
			}
			if (perpOracle) {
				ui.kv(
					'perp oracle',
					pc.dim(`${perpOracle.toBase58()} (feed ${perpFeed})`)
				);
			}

			feeds.forEach((feed, i) => {
				payloads.push({
					file: feeds.length === 1 ? '1-oracle.json' : `1-oracle-${feed}.json`,
					body: {
						instructions: [
							{
								ix: 'initializePythLazerOracle',
								args: { feedId: feed },
								accounts: {
									admin: admin.toBase58(),
									lazerOracle: getPythLazerOraclePublicKey(
										programId,
										feed
									).toBase58(),
									state: statePk.toBase58(),
									rent: SYSVAR_RENT_PUBKEY.toBase58(),
									systemProgram: SystemProgram.programId.toBase58(),
								},
							},
						],
					},
				});
				void i;
			});

			if (spot) {
				const index = spot.market_index;
				const spotMarket = getSpotMarketPublicKeySync(programId, index);
				const spotVault = await getSpotMarketVaultPublicKey(programId, index);
				const ifVault = await getInsuranceFundVaultPublicKey(programId, index);
				const signer = getVelocitySignerPublicKey(programId);
				const mint = new PublicKey(spot.mint);
				const mintInfo = await provider.connection.getAccountInfo(mint);
				if (!mintInfo) {
					throw new Error(`mint ${spot.mint} not found on this cluster`);
				}
				ui.kv(
					'spot market',
					pc.dim(`${spotMarket.toBase58()} (index ${index})`)
				);
				ui.kv('spot vault', pc.dim(spotVault.toBase58()));
				ui.kv('if vault', pc.dim(ifVault.toBase58()));

				payloads.push({
					file: '2-init-spot.json',
					body: {
						instructions: [
							{
								ix: 'initializeSpotMarket',
								args: {
									args: structFromParams(
										client.program.idl,
										'InitializeSpotMarketArgs',
										spot,
										{
											oracle_source: { pythLazer: {} },
											asset_tier: {
												[String(spot.asset_tier).toLowerCase()]: {},
											},
											name: encodeName(spot.name),
										}
									),
								},
								accounts: {
									spotMarket: spotMarket.toBase58(),
									spotMarketMint: mint.toBase58(),
									spotMarketVault: spotVault.toBase58(),
									insuranceFundVault: ifVault.toBase58(),
									velocitySigner: signer.toBase58(),
									state: statePk.toBase58(),
									oracle: spotOracle!.toBase58(),
									admin: admin.toBase58(),
									rent: SYSVAR_RENT_PUBKEY.toBase58(),
									systemProgram: SystemProgram.programId.toBase58(),
									tokenProgram: mintInfo.owner.toBase58(),
								},
							},
						],
					},
				});
			}

			if (perp) {
				const index = perp.market_index;
				const perpMarket = getPerpMarketPublicKeySync(programId, index);
				ui.kv(
					'perp market',
					pc.dim(`${perpMarket.toBase58()} (index ${index})`)
				);

				payloads.push({
					file: '3-init-perp.json',
					body: {
						instructions: [
							{
								ix: 'initializePerpMarket',
								args: {
									args: structFromParams(
										client.program.idl,
										'InitializePerpMarketArgs',
										perp,
										{
											oracle_source: { pythLazer: {} },
											contract_tier: {
												[String(perp.contract_tier).toLowerCase()]: {},
											},
											name: encodeName(perp.name),
											market_index: index,
										}
									),
								},
								accounts: {
									admin: admin.toBase58(),
									state: statePk.toBase58(),
									perpMarket: perpMarket.toBase58(),
									oracle: perpOracle!.toBase58(),
									rent: SYSVAR_RENT_PUBKEY.toBase58(),
									systemProgram: SystemProgram.programId.toBase58(),
								},
							},
						],
					},
				});
			}

			write(payloads, flags.out);

			// The peg is copied from params, and params are written days before a
			// listing executes. Compare it with the oracle when there is one to
			// compare against: an oracle that does not exist yet is the normal case
			// for a new market, and is not a failure.
			if (perp && perpOracle) {
				const pegPrice = Number(perp.amm_peg_multiplier) / 1e6;
				const live = await new PythLazerClient(provider.connection)
					.getOraclePriceData(perpOracle)
					.then((d) => Number(d.price.toString()) / 1e6)
					.catch(() => undefined);
				if (live === undefined) {
					ui.note(
						`peg implies $${pegPrice.toFixed(
							2
						)}; the oracle has no price yet, ` +
							'so it cannot be checked here. Re-check before proposing.'
					);
				} else {
					const driftPct = ((pegPrice - live) / live) * 100;
					const line =
						`peg $${pegPrice.toFixed(2)} vs oracle $${live.toFixed(2)}, ` +
						`${driftPct >= 0 ? '+' : ''}${driftPct.toFixed(2)}%`;
					if (Math.abs(driftPct) >= 1) {
						ui.line(
							ui.warn(
								`${line}. Re-derive amm_peg_multiplier before proposing, and ` +
									'plan a recenterPerpMarketAmm if it has moved since.'
							)
						);
					} else {
						ui.note(line);
					}
				}
			} else {
				ui.note(
					'peg and reserves come from the params file as written. Re-derive ' +
						'them against the live oracle before proposing if the params are old.'
				);
			}
			ui.note(
				'propose the oracle first, wait for the lazer cranker to post a price ' +
					`for feed ${feeds.join(' and ')}, then batch the rest`
			);
			console.log('');
		} finally {
			await client.unsubscribe();
		}
	});
}
