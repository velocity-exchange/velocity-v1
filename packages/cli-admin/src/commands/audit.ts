import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import { BN, BorshInstructionCoder, utils } from '@coral-xyz/anchor';
import {
	SpotBalanceType,
	calculateWithdrawLimit,
	decodeName,
	getTokenAmount,
	getUserStatsAccountPublicKey,
} from '@velocity-exchange/sdk';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import * as ui from '../lib/ui';
import { buildAdminClient, buildProvider } from '../lib/provider';

/**
 * One rendering path, three shapes: the terminal layout below, the same calls
 * flattened by the global `--agent` flag, and `--json`.
 *
 * The command renders no verdict and applies no thresholds. The thresholds that
 * would decide "concerning or not" are the judgement that does not survive a
 * real incident: a $27 dust borrow is noise on one account and the whole story
 * on another. It reports the facts a reader needs in one block, and the reader
 * decides.
 */

/**
 * Percent of the withdraw circuit breaker consumed, matching the Grafana rule
 * `Spot Withdraw Breaker {25,50,75}% Consumed` bit for bit:
 *
 *   clamp((twap - current) / clamp_min(twap - min_deposit, 1e-9), 0, 1) * 100
 *
 * The asymmetry is deliberate and load-bearing: market-stats-bot emits
 * `spot_deposit_token_twap` from the RAW `depositTokenTwap` field, while
 * `spot_min_deposit_amount` / `spot_current_deposit_amount` come out of
 * `calculateWithdrawLimit`, which projects the TWAP forward to `now`. Using the
 * live TWAP for all three here would produce a number that quietly disagrees
 * with the alert that fired.
 */
function breakerConsumed(market: any) {
	const div = 10 ** market.decimals;
	const { minDepositAmount, currentDepositAmount, currentBorrowAmount } =
		calculateWithdrawLimit(market, new BN(Math.floor(Date.now() / 1000)));
	const twap = Number(market.depositTokenTwap.toString()) / div;
	const current = Number(currentDepositAmount.toString()) / div;
	const minDeposit = Number(minDepositAmount.toString()) / div;
	const borrows = Number(currentBorrowAmount.toString()) / div;
	const denom = Math.max(twap - minDeposit, 1e-9);
	return {
		consumedPct: Math.min(Math.max((twap - current) / denom, 0), 1) * 100,
		twap,
		current,
		minDeposit,
		borrows,
		headroom: current - minDeposit,
		utilPct: current > 0 ? (borrows / current) * 100 : 0,
	};
}

/** Full account key list for a tx: static keys, then ALT writable, then ALT readonly. */
function accountKeys(tx: any): string[] {
	const statics = tx.transaction.message.staticAccountKeys.map((k: PublicKey) =>
		k.toBase58()
	);
	const writable = (tx.meta?.loadedAddresses?.writable ?? []).map(
		(k: PublicKey) => k.toBase58()
	);
	const readonly = (tx.meta?.loadedAddresses?.readonly ?? []).map(
		(k: PublicKey) => k.toBase58()
	);
	return [...statics, ...writable, ...readonly];
}

/**
 * Every velocity instruction in a tx, top-level and CPI alike. Inner
 * instructions matter: a Swift-routed withdrawal reaches the program through a
 * CPI, so a top-level-only scan silently misses it.
 */
function velocityIxs(tx: any, keys: string[], programId: string) {
	const out: { data: Buffer; accounts: number[] }[] = [];
	for (const ix of tx.transaction.message.compiledInstructions ?? []) {
		if (keys[ix.programIdIndex] === programId) {
			out.push({
				data: Buffer.from(ix.data),
				accounts: Array.from(ix.accountKeyIndexes ?? []),
			});
		}
	}
	for (const group of tx.meta?.innerInstructions ?? []) {
		for (const ix of group.instructions ?? []) {
			if (keys[ix.programIdIndex] !== programId) {
				continue;
			}
			out.push({
				data: Buffer.from(utils.bytes.bs58.decode(ix.data)),
				accounts: Array.from(ix.accounts ?? []),
			});
		}
	}
	return out;
}

/**
 * Oldest signature reachable for an account within `maxPages` pages. The user
 * account is created on first deposit, so this dates the account. Bounded
 * rather than exhaustive: `truncated` says the real first activity is older.
 */
async function firstSeen(
	connection: any,
	address: PublicKey,
	maxPages: number
): Promise<{ ts: number | null; truncated: boolean; count: number }> {
	let before: string | undefined;
	let oldest: number | null = null;
	let count = 0;
	for (let page = 0; page < maxPages; page++) {
		const sigs = await connection
			.getSignaturesForAddress(address, { limit: 1000, before })
			.catch(() => []);
		if (sigs.length === 0) {
			return { ts: oldest, truncated: false, count };
		}
		count += sigs.length;
		const last = sigs[sigs.length - 1];
		oldest = last.blockTime ?? oldest;
		before = last.signature;
		if (sigs.length < 1000) {
			return { ts: oldest, truncated: false, count };
		}
	}
	return { ts: oldest, truncated: true, count };
}

/** Bucket for a transaction whose outflow cannot be attributed to one account. */
const SHARED_DELTA = '(shared, multiple withdrawals in one transaction)';

const n = (v: number, dp = 2) => v.toFixed(dp);

export function registerAudit(parent: Command): void {
	const audit = parent
		.command('audit')
		.description(
			'Incident-time fact dumps. These commands report, they do not judge.'
		);

	withGlobalOptions(
		audit
			.command('withdrawals [market]')
			.description(
				'Dump everything needed to assess the Spot Withdraw Breaker alert: per-market ' +
					'breaker consumption at Grafana parity, then for the audited market every ' +
					'withdrawal attributed to its Velocity sub-account (NOT the fee payer, which is ' +
					"usually our own sponsor wallet), with each withdrawer's lifetime flows, P&L " +
					'decomposition, live spot/perp positions, 30d volume and account age. ' +
					'Reports facts only: no verdict, no thresholds. Defaults to the ' +
					'worst-consumed market.'
			)
			.option('--limit <n>', 'vault signatures to consider', '1000')
			.option(
				'--hours <n>',
				'only look at withdrawals this recent; 0 = no time limit. Defaults to the ' +
					"breaker's own 24h TWAP window",
				'24'
			)
			.option('--min-usd <n>', 'ignore withdrawals below this USD value', '100')
			.option(
				'--history-pages <n>',
				'pages of signature history for account age',
				'2'
			)
			.option('--json', 'emit JSON')
	).action(async (market: string | undefined, flags: any, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const scanLimit = Number.parseInt(flags.limit, 10);
			const hours = Number.parseFloat(flags.hours);
			const minUsd = Number.parseFloat(flags.minUsd);
			const historyPages = Number.parseInt(flags.historyPages, 10);

			const spots = ((client as any).getSpotMarketAccounts() as any[]).sort(
				(a, b) => a.marketIndex - b.marketIndex
			);
			const perps = (client as any).getPerpMarketAccounts() as any[];
			const spotPrice = (idx: number) =>
				Number(
					(client as any).getOracleDataForSpotMarket(idx).price.toString()
				) / 1e6;
			const perpPrice = (idx: number) =>
				Number(
					(client as any).getOracleDataForPerpMarket(idx).price.toString()
				) / 1e6;

			const markets = spots.map((m) => ({
				symbol: decodeName(m.name),
				marketIndex: m.marketIndex,
				price: spotPrice(m.marketIndex),
				cap: Number(m.maxTokenDeposits.toString()) / 10 ** m.decimals,
				...breakerConsumed(m),
				account: m,
			}));

			const target =
				market !== undefined
					? markets.find(
							(r) =>
								String(r.marketIndex) === market ||
								r.symbol.toLowerCase() === market.toLowerCase()
					  )
					: [...markets].sort((a, b) => b.consumedPct - a.consumedPct)[0];
			if (!target) {
				throw new Error(`no spot market matching "${market}"`);
			}

			const prog = (client as any).program;
			const programId = prog.programId.toBase58();
			const coder = new BorshInstructionCoder(prog.idl);
			const idlByName = new Map<string, any>(
				prog.idl.instructions.map((i: any) => [
					i.name.replace(/_/g, '').toLowerCase(),
					i,
				])
			);
			const vault = target.account.vault as PublicKey;

			const allSigs = await provider.connection.getSignaturesForAddress(vault, {
				limit: scanLimit,
			});
			// Signatures already carry blockTime, so the window is applied before any
			// transaction is fetched. Fetching a full transaction is by far the most
			// expensive thing this command does, and an alert about a 24h TWAP breaker
			// has no use for a withdrawal from last week.
			const cutoff =
				hours > 0 ? Math.floor(Date.now() / 1000) - hours * 3600 : 0;
			const sigs = allSigs.filter((s) => (s.blockTime ?? 0) >= cutoff);

			type Move = {
				ts: string;
				amount: number;
				user: string;
				feePayer: string;
				signature: string;
				/** Every withdrawer in the transaction, when the delta cannot be split. */
				sharedUsers: string[];
			};
			// One JSON-RPC batch per chunk instead of one request per signature:
			// `getTransactions` pipes the whole array through `_rpcBatchRequest`.
			// Chunked because providers cap batch size, and the chunks run
			// concurrently, since they are independent. In series this was the
			// slowest thing the command did.
			const TX_BATCH = 50;
			const chunks: string[][] = [];
			for (let i = 0; i < sigs.length; i += TX_BATCH) {
				chunks.push(sigs.slice(i, i + TX_BATCH).map((s) => s.signature));
			}
			const MAX_INFLIGHT = 6;
			const results: any[][] = new Array(chunks.length);
			for (let i = 0; i < chunks.length; i += MAX_INFLIGHT) {
				const wave = chunks.slice(i, i + MAX_INFLIGHT);
				const done = await Promise.all(
					wave.map((c) =>
						provider.connection
							.getTransactions(c, { maxSupportedTransactionVersion: 0 })
							.catch(() => c.map(() => null) as any[])
					)
				);
				done.forEach((d, k) => {
					results[i + k] = d;
				});
			}
			const fetched: any[] = [];
			for (let c = 0; c < chunks.length; c++) {
				for (let k = 0; k < chunks[c].length; k++) {
					fetched.push(results[c]?.[k] ?? null);
				}
			}
			// A transaction that could not be fetched is not a transaction with no
			// withdrawals in it. Counting it as scanned would let an RPC outage read
			// as "nothing happened" during an incident.
			const unreadable = fetched.filter((t) => t === null).length;

			const moves: Move[] = [];
			{
				for (let j = 0; j < sigs.length; j++) {
					const tx: any = fetched[j];
					if (!tx || tx.meta?.err) {
						continue;
					}
					const keys = accountKeys(tx);
					const vi = keys.indexOf(vault.toBase58());
					if (vi < 0) {
						continue;
					}
					const pre = tx.meta.preTokenBalances?.find(
						(b: any) => b.accountIndex === vi
					);
					const post = tx.meta.postTokenBalances?.find(
						(b: any) => b.accountIndex === vi
					);
					const delta =
						(post?.uiTokenAmount.uiAmount ?? 0) -
						(pre?.uiTokenAmount.uiAmount ?? 0);
					if (Math.abs(delta) * target.price < minUsd || delta >= 0) {
						continue;
					}
					// The vault delta is a property of the transaction, not of each
					// instruction in it. Crediting it to every matching withdraw would
					// report a 300-token outflow as 600 when a transaction carries two.
					// Only withdrawals against the audited market count, and a
					// transaction with several of them is reported once and marked, since
					// the per-instruction split is not recoverable from the delta alone.
					const candidates: { user: string; marketIndex?: number }[] = [];
					for (const ix of velocityIxs(tx, keys, programId)) {
						let decoded: any = null;
						try {
							decoded = coder.decode(ix.data);
						} catch {
							continue;
						}
						if (!decoded || !/^withdraw/i.test(decoded.name)) {
							continue;
						}
						const marketIndex = (decoded.data as any)?.marketIndex;
						if (
							marketIndex !== undefined &&
							Number(marketIndex) !== target.marketIndex
						) {
							continue;
						}
						const fmt = idlByName.get(
							decoded.name.replace(/_/g, '').toLowerCase()
						);
						const named: Record<string, string> = {};
						(fmt?.accounts ?? []).forEach((a: any, idx: number) => {
							const key = keys[ix.accounts[idx]];
							if (key) {
								named[a.name.replace(/_/g, '')] = key;
							}
						});
						candidates.push({
							user: named.user ?? '(unknown)',
							marketIndex:
								marketIndex === undefined ? undefined : Number(marketIndex),
						});
					}
					if (candidates.length > 0) {
						// With one withdrawal the delta is that user's. With several it
						// belongs to the transaction and cannot be split from the delta
						// alone, so it is reported against no user rather than credited to
						// whichever decoded first, which credits that user with an amount
						// that is not theirs and drops the rest.
						const shared = candidates.length > 1;
						moves.push({
							ts: new Date((sigs[j].blockTime ?? 0) * 1000).toISOString(),
							amount: delta,
							user: shared ? SHARED_DELTA : candidates[0].user,
							sharedUsers: shared ? candidates.map((c) => c.user) : [],
							feePayer: keys[0],
							signature: sigs[j].signature,
						});
					}
				}
			}
			moves.sort((a, b) => (a.ts < b.ts ? 1 : -1));

			// Keyed by the sub-account named in the instruction: no guessing a
			// sub-account id, and no collapsing a user's sub-accounts together.
			const byUser = new Map<string, { total: number; moves: Move[] }>();
			for (const m of moves) {
				const e = byUser.get(m.user) ?? { total: 0, moves: [] };
				e.total += m.amount;
				e.moves.push(m);
				byUser.set(m.user, e);
			}

			const allEntries = [...byUser.entries()].sort(
				(a, b) => a[1].total - b[1].total
			);
			// `(unknown)` and the shared-delta label are buckets, not addresses, so
			// they must be split out before anything tries to parse them as
			// pubkeys. They are still reported: dropping them would remove real
			// outflow from the totals.
			const isAddress = (k: string) => {
				try {
					new PublicKey(k);
					return true;
				} catch {
					return false;
				}
			};
			const entries = allEntries.filter(([k]) => isAddress(k));
			const unattributedEntries = allEntries.filter(([k]) => !isAddress(k));

			// Two getMultipleAccounts round-trips for every withdrawer's User and
			// UserStats, rather than two fetches each, and the age scans run
			// concurrently since they are independent.
			const userPks = entries.map(([k]) => new PublicKey(k));
			// `fetchMultiple` returns null for an account that does not exist, which
			// is a real closure. A thrown error is an RPC failure and means nothing
			// about the accounts, so the two must not collapse into the same null.
			let enrichmentFailed = false;
			const users: any[] = await prog.account.user
				.fetchMultiple(userPks)
				.catch(() => {
					enrichmentFailed = true;
					return userPks.map(() => null);
				});
			const statsPks = users.map((u, i) =>
				u
					? getUserStatsAccountPublicKey(prog.programId, u.authority)
					: userPks[i]
			);
			const statsAll: any[] = await prog.account.userStats
				.fetchMultiple(statsPks)
				.catch(() => {
					enrichmentFailed = true;
					return statsPks.map(() => null);
				});
			const seenAll = await Promise.all(
				userPks.map((pk) => firstSeen(provider.connection, pk, historyPages))
			);

			const withdrawers: any[] = [];

			for (const [label, agg] of unattributedEntries) {
				withdrawers.push({
					authority: label,
					user: label,
					subAccountId: null,
					subAccountsOpen: null,
					withdrawnTokens: Math.abs(agg.total),
					withdrawnUsd: Math.abs(agg.total) * target.price,
					withdrawTxs: agg.moves.length,
					unattributed: true,
					sharedUsers: [
						...new Set(agg.moves.flatMap((m: any) => m.sharedUsers ?? [])),
					],
					spotPositions: [],
					perpPositions: [],
					moves: agg.moves,
				});
			}

			for (let wi = 0; wi < entries.length; wi++) {
				const [userKey, agg] = entries[wi];
				const user = users[wi];
				const stats = statsAll[wi];
				// A User or UserStats that no longer exists means the account was
				// closed, not that the withdrawals did not happen. Dropping the row
				// here would make someone who withdrew and then closed disappear from
				// the report entirely.
				if (!user || !stats) {
					withdrawers.push({
						authority: enrichmentFailed
							? '(lookup failed)'
							: '(account closed)',
						enrichmentFailed,
						user: userKey,
						subAccountId: null,
						subAccountsOpen: null,
						withdrawnTokens: Math.abs(agg.total),
						withdrawnUsd: Math.abs(agg.total) * target.price,
						withdrawTxs: agg.moves.length,
						enrichmentUnavailable: true,
						spotPositions: [],
						perpPositions: [],
						moves: agg.moves,
					});
					continue;
				}

				const spotPositions: any[] = [];
				let spotNetUsd = 0;
				for (const p of user.spotPositions) {
					if (p.scaledBalance.isZero()) {
						continue;
					}
					const sm = spots.find((s) => s.marketIndex === p.marketIndex);
					const isDeposit = p.balanceType.deposit !== undefined;
					const amount =
						Number(
							getTokenAmount(
								p.scaledBalance,
								sm,
								isDeposit ? SpotBalanceType.DEPOSIT : SpotBalanceType.BORROW
							).toString()
						) /
						10 ** sm.decimals;
					const usd = amount * spotPrice(sm.marketIndex);
					spotNetUsd += isDeposit ? usd : -usd;
					spotPositions.push({
						symbol: decodeName(sm.name),
						side: isDeposit ? 'deposit' : 'borrow',
						amount,
						usd,
					});
				}

				const perpPositions: any[] = [];
				let upnlUsd = 0;
				for (const p of user.perpPositions) {
					if (
						p.baseAssetAmount.isZero() &&
						p.quoteAssetAmount.isZero() &&
						p.openOrders === 0
					) {
						continue;
					}
					const pm = perps.find((x) => x.marketIndex === p.marketIndex);
					const base = Number(p.baseAssetAmount.toString()) / 1e9;
					const quote = Number(p.quoteAssetAmount.toString()) / 1e6;
					const entryQuote = Number(p.quoteEntryAmount.toString()) / 1e6;
					const mark = perpPrice(p.marketIndex);
					const upnl = base * mark + quote;
					upnlUsd += upnl;
					perpPositions.push({
						symbol: decodeName(pm.name),
						base,
						entryPrice: base !== 0 ? Math.abs(entryQuote / base) : 0,
						markPrice: mark,
						notionalUsd: Math.abs(base) * mark,
						unrealizedPnlUsd: upnl,
						settledPnlUsd: Number(p.settledPnl.toString()) / 1e6,
						openOrders: p.openOrders,
					});
				}

				const seen = seenAll[wi];
				const deposits = user.totalDeposits.toNumber() / 1e6;
				const withdraws = user.totalWithdraws.toNumber() / 1e6;
				const price = target.price;

				withdrawers.push({
					authority: user.authority.toBase58(),
					user: userKey,
					subAccountId: user.subAccountId,
					subAccountsOpen: stats.numberOfSubAccounts,
					withdrawnTokens: Math.abs(agg.total),
					withdrawnUsd: Math.abs(agg.total) * price,
					withdrawTxs: agg.moves.length,
					lifetimeDepositsUsd: deposits,
					lifetimeWithdrawsUsd: withdraws,
					netInUsd: deposits - withdraws,
					withdrawnPctOfDeposits:
						deposits > 0 ? (withdraws / deposits) * 100 : 0,
					settledPerpPnlUsd: user.settledPerpPnl.toNumber() / 1e6,
					cumulativePerpFundingUsd: user.cumulativePerpFunding.toNumber() / 1e6,
					cumulativeSpotFeesUsd: user.cumulativeSpotFees.toNumber() / 1e6,
					totalSocialLossUsd: user.totalSocialLoss.toNumber() / 1e6,
					liquidations: user.nextLiquidationId - 1,
					takerVolume30dUsd: stats.takerVolume30D.toNumber() / 1e6,
					makerVolume30dUsd: stats.makerVolume30D.toNumber() / 1e6,
					feesPaidUsd: stats.fees.totalFeePaid.toNumber() / 1e6,
					feesRebateUsd: stats.fees.totalFeeRebate.toNumber() / 1e6,
					marginTradingEnabled: user.isMarginTradingEnabled,
					firstSeen: seen.ts ? new Date(seen.ts * 1000).toISOString() : null,
					sigsScanned: seen.count,
					firstSeenTruncated: seen.truncated,
					ageDays: seen.ts ? (Date.now() / 1000 - seen.ts) / 86400 : null,
					spotNetUsd,
					unrealizedPnlUsd: upnlUsd,
					spotPositions,
					perpPositions,
					moves: agg.moves,
				});
			}

			const payload = {
				now: new Date().toISOString(),
				parity: 'grafana:Spot Withdraw Breaker {25,50,75}% Consumed',
				markets: markets.map(({ account: _a, ...rest }) => rest),
				audited: {
					symbol: target.symbol,
					marketIndex: target.marketIndex,
					vault: vault.toBase58(),
					price: target.price,
					scannedTxs: sigs.length - unreadable,
					unreadableTxs: unreadable,
					signaturesSeen: allSigs.length,
					windowHours: hours,
					minUsd,
				},
				withdrawers,
			};

			if (flags.json) {
				console.log(JSON.stringify(payload, null, 2));
				return;
			}

			const usd = (v: number) =>
				`${v < 0 ? '-' : ''}$${Math.abs(v).toLocaleString('en-US', {
					minimumFractionDigits: 2,
					maximumFractionDigits: 2,
				})}`;
			const sorted = [...markets].sort((a, b) => b.consumedPct - a.consumedPct);

			const paint = (c: number) =>
				c >= 75 ? pc.red : c >= 50 ? pc.yellow : c >= 25 ? pc.reset : pc.dim;
			ui.header(
				'withdraw breaker',
				pc.dim(
					`${sorted[0].symbol} ${sorted[0].consumedPct.toFixed(2)}% consumed`
				)
			);
			ui.table([
				[
					pc.dim('market'),
					pc.dim('consumed'),
					pc.dim('deposits'),
					pc.dim('24h twap'),
					pc.dim('halt floor'),
					pc.dim('headroom'),
					pc.dim('borrows'),
					pc.dim('util'),
					pc.dim('price'),
				],
				...sorted.map((m) => [
					pc.bold(m.symbol),
					paint(m.consumedPct)(`${m.consumedPct.toFixed(2)}%`),
					n(m.current, 4),
					n(m.twap, 4),
					n(m.minDeposit, 4),
					n(m.headroom, 4),
					n(m.borrows, 4),
					`${n(m.utilPct)}%`,
					`$${n(m.price)}`,
				]),
			]);
			ui.note(
				'consumed = (twap - deposits) / (twap - halt floor); 100% halts withdrawals'
			);

			ui.header(
				`${target.symbol} withdrawals`,
				pc.dim(
					`${withdrawers.length} withdrawer(s) · ${sigs.length}/${allSigs.length} txs · ` +
						`${
							hours > 0 ? `${hours}h window` : 'no time limit'
						} · over $${minUsd}`
				)
			);
			ui.note(
				`vault ${vault.toBase58()} · oracle $${n(target.price)} · ` +
					'fee payer is the tx sponsor, not the withdrawer'
			);
			if (unreadable > 0) {
				ui.line(
					ui.warn(
						`${unreadable} transaction(s) could not be fetched, so this audit is ` +
							'incomplete. Re-run before drawing a conclusion.'
					)
				);
			}
			if (withdrawers.length === 0) {
				ui.line(pc.dim('(none)'));
			}

			for (const w of withdrawers) {
				ui.header(
					`${n(w.withdrawnTokens, 4)} ${target.symbol}  ${usd(w.withdrawnUsd)}`,
					pc.dim(ui.shortKey(w.authority))
				);
				if (w.unattributed) {
					ui.line(
						ui.warn(
							'this transaction carried more than one withdrawal, so the vault ' +
								'delta cannot be split between them. Accounts involved:'
						)
					);
					for (const u of w.sharedUsers) {
						ui.kv('user', pc.dim(u));
					}
					for (const m of w.moves) {
						ui.kv('tx', pc.dim(`${m.ts}  ${n(m.amount, 4)}  ${m.signature}`));
					}
					continue;
				}
				if (w.enrichmentUnavailable) {
					ui.kv('sub-account', pc.dim(w.user));
					ui.line(
						ui.warn(
							w.enrichmentFailed
								? 'User/UserStats could not be read, so nothing is known about ' +
										'this account. This is an RPC failure, not evidence the ' +
										'account was closed. Re-run before concluding anything.'
								: 'User/UserStats do not exist, so this account was closed after ' +
										'withdrawing. Amounts below come from the vault transfers; ' +
										'lifetime flows and P&L are unavailable.'
						)
					);
					for (const m of w.moves) {
						ui.kv('tx', pc.dim(`${m.ts}  ${n(m.amount, 4)}  ${m.signature}`));
					}
					continue;
				}
				ui.kv('authority', pc.dim(w.authority));
				ui.kv(
					'sub-account',
					pc.dim(`#${w.subAccountId} ${w.user}  (${w.subAccountsOpen} open)`)
				);
				ui.kv(
					'lifetime',
					`in ${usd(w.lifetimeDepositsUsd)} · out ${usd(
						w.lifetimeWithdrawsUsd
					)} · ` +
						(w.netInUsd >= 0
							? pc.green(`net ${usd(w.netInUsd)} in`)
							: pc.red(`net ${usd(-w.netInUsd)} OUT`)) +
						pc.dim(` · ${n(w.withdrawnPctOfDeposits)}% of deposits out`)
				);
				ui.kv(
					'pnl',
					`settled ${usd(w.settledPerpPnlUsd)} · unrealized ${usd(
						w.unrealizedPnlUsd
					)} · funding ${usd(w.cumulativePerpFundingUsd)} · social loss ${usd(
						w.totalSocialLossUsd
					)} · ${w.liquidations} liquidations`
				);
				ui.kv(
					'activity',
					`taker ${usd(w.takerVolume30dUsd)} · maker ${usd(
						w.makerVolume30dUsd
					)} · fees ${usd(w.feesPaidUsd)} · ` +
						(w.marginTradingEnabled
							? pc.yellow('margin enabled')
							: pc.dim('margin disabled'))
				);
				ui.kv(
					'age',
					w.ageDays === null
						? pc.dim('unknown')
						: w.firstSeenTruncated
						? `${pc.dim('≥')}${n(w.ageDays, 1)}d ${pc.dim(
								`(truncated at ${w.sigsScanned} sigs; the account is older than this)`
						  )}`
						: `${n(w.ageDays, 1)}d ${pc.dim(`since ${w.firstSeen}`)}`
				);
				ui.kv(
					'holdings',
					`net ${usd(w.spotNetUsd)}` +
						(w.spotPositions.length
							? '  ' +
							  w.spotPositions
									.map(
										(p: any) =>
											`${p.symbol} ${p.side === 'deposit' ? '+' : '-'}${n(
												p.amount,
												4
											)} ${pc.dim(
												`(${p.side === 'deposit' ? '' : '-'}${usd(p.usd)})`
											)}`
									)
									.join('  ')
							: pc.dim('  (none)'))
				);
				for (const p of w.perpPositions) {
					ui.kv(
						'perp',
						`${pc.bold(p.symbol)} ${p.base >= 0 ? '+' : ''}${n(
							p.base,
							4
						)} @ ${n(p.entryPrice)} → ${n(p.markPrice)} · notional ${usd(
							p.notionalUsd
						)} · upnl ${usd(p.unrealizedPnlUsd)} · ${p.openOrders} orders`
					);
				}
				for (const m of w.moves) {
					ui.kv('tx', pc.dim(`${m.ts}  ${n(m.amount, 4)}  ${m.signature}`));
				}
			}
			console.log('');
		} finally {
			await client.unsubscribe();
		}
	});
}
