import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import {
	escrowHasReferrer,
	getCrankTreasuryPublicKey,
	getRevenueShareAccountPublicKey,
	isBuilderOrderReferral,
	isVariant,
	MarketType,
	RevenueShareEscrowAccount,
	RevenueShareEscrowMap,
	TransferFeeAndPnlPoolDirection,
	ACCELERATED_REFERRER_REWARD_PERCENT,
} from '@velocity-exchange/sdk';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import * as ui from '../lib/ui';
import {
	reportDispatch,
	reportDryRun,
	resolveAdminAuthority,
	sendOrPropose,
} from '../lib/squads';

function parseMarketType(value: string): MarketType {
	switch (value.toLowerCase()) {
		case 'perp':
			return MarketType.PERP;
		case 'spot':
			return MarketType.SPOT;
		default:
			throw new Error(`marketType must be "perp" or "spot", got "${value}"`);
	}
}

function parseTransferDirection(value: string): TransferFeeAndPnlPoolDirection {
	switch (value.toLowerCase()) {
		case 'fee-to-pnl':
			return TransferFeeAndPnlPoolDirection.FEE_TO_PNL_POOL;
		case 'pnl-to-fee':
			return TransferFeeAndPnlPoolDirection.PNL_TO_FEE_POOL;
		default:
			throw new Error(
				`direction must be "fee-to-pnl" or "pnl-to-fee", got "${value}"`
			);
	}
}

/**
 * Protocol fee operations.
 *
 * The fee design (see FEES.md) gives the protocol a directly-withdrawable
 * `protocol_fee_pool` on every market, fed by explicit per-fill carveouts and
 * materialized by the streaming sweep. The commands here cover the routine
 * ops: setting the recipient (cold admin), withdrawing (FeeWithdraw hot key),
 * sweeping a market on demand, and tuning the global trade-fee split.
 */
export function registerFees(parent: Command): void {
	const fees = parent
		.command('fees')
		.description(
			'Protocol fee operations: recipient, withdrawals, sweeps, split.'
		);

	withGlobalOptions(
		fees
			.command(
				'set-liquidation-crank-reimbursement <shareBps> <solSpotMarketIndex>'
			)
			.description(
				'Set what the protocol spends to get a liquidation cranked, and the spot market whose oracle prices that spend in SOL. The warm or cold admin signs. A liquidation crank repays the priority fee its keeper paid, so the crank stays worth landing when the fee market moves. <shareBps> caps that repayment at a share of what the liquidation recovered, so the protocol does not subsidise a recovery too small to cover its own gas. 2000 is a fifth. A zero in either argument leaves the flat payment, which is where every market starts.'
			)
	).action(
		async (
			shareBps: string,
			solSpotMarketIndex: string,
			_flags,
			cmd: Command
		) => {
			const share = Number.parseInt(shareBps, 10);
			const market = Number.parseInt(solSpotMarketIndex, 10);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getUpdateLiquidationCrankReimbursementIx(
					share,
					market
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees set-liquidation-crank-reimbursement'
				);

				reportDispatch(
					`liquidation_crank_reimbursement = ${share}bps, sol spot market ${market}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command(
				'set-transaction-rails <inclusionLamports> <signatureLamports> <resourceFeeNumerator> <resourceFeeDenominator> <maxPriorityMicroLamportsPerCu>'
			)
			.description(
				'Set what the protocol believes a transaction costs to land. The warm or cold admin signs. The rails are a fixed inclusion fee, a per-signature fee, and a rate in lamports per requested cost unit. Relay crank payments are derived from them, so this is the one write that re-prices every crank when the network changes its fee model. A zero denominator prices cost units at nothing, which is the model that charges per signature alone. The last argument caps the compute-unit price a liquidation crank reimburses its keeper for, in micro-lamports per compute unit, and zero turns that reimbursement off. A market keeps the payments already written on its conditions account until its attach runs again through velocity-admin quoter set-market-clob.'
			)
	).action(
		async (
			inclusionLamports: string,
			signatureLamports: string,
			resourceFeeNumerator: string,
			resourceFeeDenominator: string,
			maxPriorityMicroLamportsPerCu: string,
			_flags,
			cmd: Command
		) => {
			const rails = {
				inclusionLamports: Number.parseInt(inclusionLamports, 10),
				signatureLamports: Number.parseInt(signatureLamports, 10),
				resourceFeeNumerator: Number.parseInt(resourceFeeNumerator, 10),
				resourceFeeDenominator: Number.parseInt(resourceFeeDenominator, 10),
				maxPriorityMicroLamportsPerCu: Number.parseInt(
					maxPriorityMicroLamportsPerCu,
					10
				),
			};
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getUpdateTransactionFeeRailsIx(rails);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees set-transaction-rails'
				);

				reportDispatch(
					`transaction_fee_rails = ${JSON.stringify(rails)}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command('set-referral-rate <percent>')
			.description(
				`Set the Standard referrer reward percentage on every active perp fee tier. Accelerated remains fixed at ${ACCELERATED_REFERRER_REWARD_PERCENT}%. Preserves all other fee parameters. Warm or cold admin.`
			)
	).action(async (percentArg: string, _flags, cmd: Command) => {
		if (!/^\d+$/.test(percentArg)) {
			throw new Error(`percent must be an integer, got "${percentArg}"`);
		}
		const percent = Number.parseInt(percentArg, 10);
		if (percent > 100) {
			throw new Error('percent must be between 0 and 100');
		}

		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const current = client.getStateAccount().perpFeeStructure;
			const feeStructure = {
				...current,
				feeTiers: current.feeTiers.map((tier) => ({ ...tier })),
			};
			let updatedTiers = 0;
			for (const tier of feeStructure.feeTiers) {
				if (tier.feeNumerator > 0) {
					tier.referrerRewardNumerator = percent;
					updatedTiers++;
				}
			}
			if (updatedTiers === 0) {
				throw new Error('perp fee structure has no active tiers');
			}

			const multisigPda = opts.multisig
				? new PublicKey(opts.multisig)
				: undefined;
			const ix = await client.getUpdatePerpFeeStructureIx(
				feeStructure,
				resolveAdminAuthority(provider, multisigPda)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				multisigPda,
				'velocity-admin fees set-referral-rate'
			);
			reportDispatch(
				`Standard referral rate = ${percent}%, Accelerated remains ${ACCELERATED_REFERRER_REWARD_PERCENT}% across ${updatedTiers} active tiers`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('set-recipient <pubkey> <marketType>')
			.description(
				'Set the protocol fee recipient for one side (cold admin only): <marketType> is "perp" (quote-denominated perp fees) or "spot" (per-market lending/liquidation fees). Withdrawals pay the ATA of the configured key.'
			)
	).action(
		async (pubkey: string, marketTypeArg: string, _flags, cmd: Command) => {
			const marketType = parseMarketType(marketTypeArg);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getUpdateProtocolFeeRecipientIx(
					new PublicKey(pubkey),
					marketType
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees set-recipient'
				);
				reportDispatch(
					`protocol_fee_recipient_${marketTypeArg.toLowerCase()} = ${pubkey}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command('withdraw-perp <market> <amount>')
			.description(
				"Withdraw from a perp market protocol_fee_pool (quote tokens) to the recipient's associated token account (created if needed). Signer must hold the FeeWithdraw hot role. <amount> in token base units."
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getWithdrawProtocolFeesPerpIx(
				Number.parseInt(market, 10),
				new BN(amount)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees withdraw-perp'
			);
			reportDispatch(
				`perp-market[${market}] protocol fees ${amount} -> recipient ATA`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('withdraw-protocol-user <market> <amount>')
			.description(
				"Withdraw settled crank rewards from the protocol-owned User to the recipient's associated token account. That User's authority is the velocity signer PDA, and rewards accrue there in program-keeper crank mode. Settle the accrued perp quote to deposits first with the permissionless settle-pnl. The signer must hold the FeeWithdraw hot role. <market> is the SPOT market index. <amount> is in token base units."
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getWithdrawProtocolUserDepositIx(
				Number.parseInt(market, 10),
				new BN(amount)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees withdraw-protocol-user'
			);

			reportDispatch(
				`protocol user deposit (spot-market[${market}]) ${amount} -> recipient ATA`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('withdraw-spot <market> <amount>')
			.description(
				"Withdraw from a spot market protocol_fee_pool to the recipient's associated token account (created if needed). Signer must hold the FeeWithdraw hot role. <amount> in token base units."
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getWithdrawProtocolFeesSpotIx(
				Number.parseInt(market, 10),
				new BN(amount)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees withdraw-spot'
			);
			reportDispatch(
				`spot-market[${market}] protocol fees ${amount} -> recipient ATA`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('sweep <market>')
			.description(
				'Run the streaming fee sweep for a perp market (permissionless): materialize pending IF/protocol/AMM carveouts out of the pnl pool.'
			)
	).action(async (market: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getSweepPerpMarketFeesIx(
				Number.parseInt(market, 10)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees sweep'
			);
			reportDispatch(`perp-market[${market}] fee sweep`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('settle-revenue-share <market> [escrowAuthority]')
			.description(
				'Settle accrued builder and referrer revenue share for a perp market out of its pnl pool. Permissionless. It pays beneficiaries without the escrow owner settling pnl. Pass an escrow authority for one escrow, or --all to scan for and settle every escrow still owed on the market. `settle_expired_market_pools_to_revenue_pool` requires that before it delists a market.'
			)
			.option(
				'--all',
				'settle every escrow still owed on the market (scans all escrow accounts)'
			)
	).action(
		async (
			market: string,
			escrowAuthority: string | undefined,
			flags: { all?: boolean },
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			const marketIndex = Number.parseInt(market, 10);
			try {
				if (!flags.all && !escrowAuthority) {
					throw new Error('pass an <escrowAuthority> or --all');
				}

				const targets = new Map<string, RevenueShareEscrowAccount>();
				if (flags.all) {
					const escrowMap = new RevenueShareEscrowMap(client);
					await escrowMap.syncAll();
					for (const [
						authority,
						escrow,
					] of escrowMap.getEscrowsOwingRevenueShare(marketIndex)) {
						targets.set(authority, escrow);
					}
					if (targets.size === 0) {
						console.log(
							`perp-market[${market}] has no escrows owing revenue share`
						);
						return;
					}
					console.log(
						`perp-market[${market}] settling ${targets.size} escrow(s) owing revenue share`
					);
				} else {
					const authority = new PublicKey(escrowAuthority!);
					targets.set(
						authority.toBase58(),
						await client.fetchRevenueShareEscrowAccount(authority)
					);
				}

				let failed = 0;

				// The sweep needs both the User and the RevenueShare account of a beneficiary. It
				// skips the row when either is absent. The program also refuses to forfeit that
				// row, because it can still pay it. The row would then block the delist. Anyone can
				// create a RevenueShare account, so create it here.
				if (flags.all) {
					const beneficiaries = new Set<string>();
					for (const escrow of targets.values()) {
						for (const order of escrow.orders) {
							if (
								order.marketIndex !== marketIndex ||
								!isVariant(order.marketType, 'perp') ||
								order.feesAccrued.isZero()
							) {
								continue;
							}
							const beneficiary = isBuilderOrderReferral(order)
								? escrowHasReferrer(escrow)
									? escrow.referrer
									: undefined
								: escrow.approvedBuilders[order.builderIdx]?.authority;
							if (beneficiary) {
								beneficiaries.add(beneficiary.toBase58());
							}
						}
					}
					for (const beneficiary of beneficiaries) {
						const revenueSharePk = getRevenueShareAccountPublicKey(
							client.program.programId,
							new PublicKey(beneficiary)
						);
						if (
							(await client.connection.getAccountInfo(revenueSharePk)) !== null
						) {
							continue;
						}
						try {
							const ix = await client.getInitializeRevenueShareIx(
								new PublicKey(beneficiary)
							);
							const result = await sendOrPropose(
								provider,
								[ix],
								opts.multisig ? new PublicKey(opts.multisig) : undefined,
								'velocity-admin fees settle-revenue-share (init beneficiary)'
							);
							reportDispatch(
								`created RevenueShare for beneficiary ${beneficiary}`,
								result
							);
						} catch (e) {
							failed += 1;
							console.error(
								`failed to create RevenueShare for beneficiary ${beneficiary}: ${
									(e as Error).message
								}`
							);
						}
					}
				}

				// One transaction for each escrow. Each escrow carries its own beneficiary accounts,
				// and one transaction cannot hold them all. Report each failure and continue, so
				// that one bad escrow does not stop the others.
				for (const [authority, escrow] of targets) {
					try {
						const ix = await client.getSettleRevenueShareIx(
							new PublicKey(authority),
							escrow,
							marketIndex
						);
						const result = await sendOrPropose(
							provider,
							[ix],
							opts.multisig ? new PublicKey(opts.multisig) : undefined,
							'velocity-admin fees settle-revenue-share'
						);
						reportDispatch(
							`perp-market[${market}] revenue-share settle for ${authority}`,
							result
						);
					} catch (e) {
						failed += 1;
						console.error(
							`perp-market[${market}] revenue-share settle FAILED for ${authority}: ${
								(e as Error).message
							}`
						);
					}
				}

				// A row that still owes after the settle pass is one the program will not pay.
				// The beneficiary has no payout account, the pool is too small, or the row names
				// nobody. The delist needs a zero counter, so this pass forfeits those rows. The
				// program proves each reason again and rejects a payable row with
				// RevenueShareOrderNotForfeitable.
				//
				// This pass runs only for a closing market. On a live market the program refuses
				// every forfeit, so the pass would report failures after a successful settle pass.
				const marketStatus =
					client.getPerpMarketAccountOrThrow(marketIndex).status;
				const marketWindingDown =
					isVariant(marketStatus, 'settlement') ||
					isVariant(marketStatus, 'delisted');
				if (flags.all && marketWindingDown) {
					const escrowMap = new RevenueShareEscrowMap(client);
					await escrowMap.syncAll();
					const stragglers = escrowMap.getEscrowsOwingRevenueShare(marketIndex);
					for (const [authority, escrow] of stragglers) {
						for (const [orderIndex, order] of escrow.orders.entries()) {
							if (
								order.marketIndex !== marketIndex ||
								!isVariant(order.marketType, 'perp') ||
								order.feesAccrued.isZero()
							) {
								continue;
							}
							try {
								const ix = await client.getForfeitRevenueShareOrderIx(
									new PublicKey(authority),
									escrow,
									marketIndex,
									orderIndex
								);
								const result = await sendOrPropose(
									provider,
									[ix],
									opts.multisig ? new PublicKey(opts.multisig) : undefined,
									'velocity-admin fees forfeit-revenue-share-order'
								);
								reportDispatch(
									`perp-market[${market}] revenue-share forfeit for ${authority} order ${orderIndex}`,
									result
								);
							} catch (e) {
								failed += 1;
								console.error(
									`perp-market[${market}] revenue-share forfeit FAILED for ${authority} order ${orderIndex}: ${
										(e as Error).message
									}`
								);
							}
						}
					}
				}

				if (failed > 0) {
					throw new Error(
						`${failed} revenue-share operation(s) failed on perp-market[${market}]`
					);
				}
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command(
				'transfer-fee-pnl <feePoolMarket> <pnlPoolMarket> <amount> <direction>'
			)
			.description(
				'Transfer quote tokens between one perp market\'s protocol_fee_pool and another perp market\'s pnl_pool. <direction> is "fee-to-pnl" or "pnl-to-fee". <amount> in token base units.'
			)
	).action(
		async (
			feePoolMarket: string,
			pnlPoolMarket: string,
			amount: string,
			direction: string,
			_flags,
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getTransferFeeAndPnlPoolIx(
					Number.parseInt(feePoolMarket, 10),
					Number.parseInt(pnlPoolMarket, 10),
					new BN(amount),
					parseTransferDirection(direction)
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees transfer-fee-pnl'
				);
				reportDispatch(
					`perp-market[${feePoolMarket}].protocol_fee_pool ${direction} perp-market[${pnlPoolMarket}].pnl_pool: ${amount}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command('set-schedule <tier0bp> <tier1bp> <tier2bp> <tier3bp>')
			.description(
				'Rewrite the perp fee schedule in one instruction. Each argument is a taker fee in ' +
					'bps, and decimals are allowed, for the four live tiers Regular, VIP 1, VIP 2 and ' +
					'VIP 3. The 30-day volume thresholds of $5M, $80M and $200M are program constants. ' +
					'Unused tiers 4-9 mirror tier 3. The command fetches the current fee structure and ' +
					'patches only what is passed. The maker rebate, the referral fields and the amm and ' +
					'insurance-fund split stay unchanged unless the matching option is given. The warm ' +
					'or cold admin signs.'
			)
			.option(
				'--maker-rebate-bp <bp>',
				'maker rebate in bps on all tiers (e.g. 0.25)'
			)
			.option(
				'--referrer <pct>',
				'referrer reward, as a percent of the taker fee'
			)
			.option(
				'--referee <pct>',
				'referee discount, as a percent of the taker fee'
			)
			.option('--amm-split <pct>', 'amm share of the fee remainder, percent')
			.option(
				'--if-split <pct>',
				'insurance fund share of the fee remainder, percent'
			)
			.option(
				'--dry-run',
				'print the resulting structure and expected proposal rent/fees, send nothing',
				false
			)
	).action(
		async (
			tier0bp: string,
			tier1bp: string,
			tier2bp: string,
			tier3bp: string,
			_flags,
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const local = cmd.opts() as {
				makerRebateBp?: string;
				referrer?: string;
				referee?: string;
				ammSplit?: string;
				ifSplit?: string;
				dryRun: boolean;
			};
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const current = client.getStateAccount().perpFeeStructure;
				const feeStructure = {
					...current,
					feeTiers: current.feeTiers.map((tier) => ({ ...tier })),
				};
				const tiers = feeStructure.feeTiers;
				const bpToNumerator = (bp: string, denominator: number): number => {
					const value = Number(bp);
					if (!Number.isFinite(value) || value < 0) {
						throw new Error(`bad bps value "${bp}"`);
					}
					const numerator = (value * denominator) / 10000;
					if (!Number.isInteger(numerator)) {
						throw new Error(
							`${bp} bps does not land on an integer numerator over ${denominator}`
						);
					}
					return numerator;
				};
				const tierBps = [tier0bp, tier1bp, tier2bp, tier3bp];
				for (let i = 0; i < tiers.length; i++) {
					const bp = tierBps[Math.min(i, tierBps.length - 1)];
					tiers[i].feeNumerator = bpToNumerator(bp, tiers[i].feeDenominator);
					if (local.makerRebateBp !== undefined) {
						tiers[i].makerRebateNumerator = bpToNumerator(
							local.makerRebateBp,
							tiers[i].makerRebateDenominator
						);
					}

					// Only a tier with a nonzero referral numerator carries referral
					// fields. The zeroed tail stays as it is.
					if (
						local.referrer !== undefined &&
						tiers[i].referrerRewardNumerator > 0
					) {
						tiers[i].referrerRewardNumerator = Number.parseInt(
							local.referrer,
							10
						);
					}
					if (local.referee !== undefined && tiers[i].refereeFeeNumerator > 0) {
						tiers[i].refereeFeeNumerator = Number.parseInt(local.referee, 10);
					}
				}
				if (local.ammSplit !== undefined) {
					feeStructure.ammFeeNumerator = Number.parseInt(local.ammSplit, 10);
				}
				if (local.ifSplit !== undefined) {
					feeStructure.ifFeeNumerator = Number.parseInt(local.ifSplit, 10);
				}

				const label =
					`fee schedule: tiers ${tier0bp}/${tier1bp}/${tier2bp}/${tier3bp}bp` +
					(local.makerRebateBp ? ` makerRebate=${local.makerRebateBp}bp` : '') +
					(local.referrer ? ` referrer=${local.referrer}%` : '') +
					(local.referee ? ` referee=${local.referee}%` : '') +
					(local.ammSplit ? ` amm=${local.ammSplit}%` : '') +
					(local.ifSplit ? ` if=${local.ifSplit}%` : '');
				const multisigPda = opts.multisig
					? new PublicKey(opts.multisig)
					: undefined;
				const ix = await client.getUpdatePerpFeeStructureIx(
					feeStructure,
					resolveAdminAuthority(provider, multisigPda)
				);
				if (local.dryRun) {
					const bps = (numerator: number, denominator: number) =>
						`${+((numerator / denominator) * 10_000).toFixed(4)} bps`;
					ui.header('perp fee schedule', pc.dim('proposed'));
					ui.table(
						tierBps.map((_, i) => {
							const live = current.feeTiers[i];
							const next = tiers[i];
							const from = bps(live.feeNumerator, live.feeDenominator);
							const to = bps(next.feeNumerator, next.feeDenominator);
							return [
								pc.dim(`tier ${i}`),
								from === to
									? `taker ${pc.dim(to)}`
									: `taker ${ui.change(from, to)}`,
								pc.dim(
									`maker ${bps(
										next.makerRebateNumerator,
										next.makerRebateDenominator
									)} rebate`
								),
								pc.dim(
									`referrer ${next.referrerRewardNumerator}%, referee ${next.refereeFeeNumerator}%`
								),
								from === to ? pc.dim('unchanged') : '',
							];
						})
					);
					ui.kv(
						'split',
						pc.dim(
							`amm ${feeStructure.ammFeeNumerator}%, if ${feeStructure.ifFeeNumerator}%, protocol residual`
						)
					);
					ui.note('tiers 4-9 mirror the last tier');
					await reportDryRun(
						provider,
						[ix],
						opts.multisig ? new PublicKey(opts.multisig) : undefined
					);
					return;
				}
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees set-schedule'
				);
				reportDispatch(label, result);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command('set-split <ammFeeNumerator> <ifFeeNumerator>')
			.description(
				'Set the global trade-fee remainder split (percent, 0-100 each; protocol receives the residual). Fetches the current perp fee structure and updates only the two numerators.'
			)
	).action(
		async (
			ammFeeNumerator: string,
			ifFeeNumerator: string,
			_flags,
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const feeStructure = client.getStateAccount().perpFeeStructure;
				feeStructure.ammFeeNumerator = Number.parseInt(ammFeeNumerator, 10);
				feeStructure.ifFeeNumerator = Number.parseInt(ifFeeNumerator, 10);
				const ix = await client.getUpdatePerpFeeStructureIx(feeStructure);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees set-split'
				);
				reportDispatch(
					`fee split: amm=${ammFeeNumerator}% if=${ifFeeNumerator}% protocol=residual`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command('set-taker-addon <market> <tenthBps>')
			.description(
				"Set a perp market's additive taker-fee surcharge in tenth-bps. 15 adds 1.5bp and 0 adds nothing. The program applies it to the tier fee before feeAdjustment. The value is a surcharge only and its range is 0 to 100. Discounts go through set-promo-tier. The warm admin signs."
			)
	).action(async (market: string, tenthBps: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdatePerpMarketTakerFeeAddonIx(
				Number.parseInt(market, 10),
				Number.parseInt(tenthBps, 10)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees set-taker-addon'
			);
			reportDispatch(
				`perp market ${market} taker fee addon = ${tenthBps} tenth-bps`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('set-promo-tier <tier>')
			.description(
				'Set the promotional fee-tier floor. While it is set, every account gets at least this perp fee tier: 1 is VIP 1, 2 is VIP 2 and 3 is VIP 3. An account already above the floor keeps its tier. 0 disables the floor, and accounts return to their volume tier on their next fill. The warm admin signs.'
			)
	).action(async (tier: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdatePromoFeeTierIx(
				Number.parseInt(tier, 10)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees set-promo-tier'
			);
			reportDispatch(`promo fee tier = ${tier}`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('init-crank-treasury')
			.description(
				"Create the protocol's relay crank treasury, the single account every market's crank reservoir refills from. The warm or cold admin signs, and this runs once per deployment. The treasury is created inert. Price it with fees set-crank-treasury, then fund it by sending SOL to the address this prints. Markets refill themselves from it, so nobody has to watch a per-market balance."
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getInitializeCrankTreasuryIx();
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees init-crank-treasury'
			);

			reportDispatch(
				`crank treasury = ${getCrankTreasuryPublicKey(
					client.program.programId
				).toBase58()} (fund it by sending SOL to this address)`,

				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command(
				'set-crank-treasury <refillTargetCranks> <refillWatermarkCranks>'
			)
			.description(
				"Set the two levels a market's crank reservoir is held between. The warm or cold admin signs. Both levels count that market's most expensive crank rather than lamports, so one setting serves every market and a market whose cranks cost more carries a proportionally larger float. <refillWatermarkCranks> is the level at which a refill wakes. It must cover the refill's own round trip, because the reservoir keeps paying cranks while that refill lands. <refillTargetCranks> is how full the refill leaves the reservoir, and it must exceed the watermark. The target reaches every market at once. A new watermark reaches a market on its next quoter set-market-clob. What a refill pays is priced on the market it fills, through --crank-cu-refill."
			)
	).action(
		async (
			refillTargetCranks: string,
			refillWatermarkCranks: string,
			_flags,
			cmd: Command
		) => {
			const target = Number.parseInt(refillTargetCranks, 10);
			const watermark = Number.parseInt(refillWatermarkCranks, 10);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getUpdateCrankTreasuryIx(target, watermark);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees set-crank-treasury'
				);

				reportDispatch(
					`crank treasury wakes under ${watermark} cranks, fills to ${target}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		fees
			.command('withdraw-crank-treasury <lamports>')
			.description(
				'Move lamports from the crank treasury to the admin. The warm or cold admin signs. The balance never falls below the account rent, so nobody can close the treasury while markets still draw on it.'
			)
	).action(async (lamports: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getWithdrawCrankTreasuryIx(new BN(lamports));
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees withdraw-crank-treasury'
			);

			reportDispatch(
				`withdrew ${lamports} lamports from crank treasury`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		fees
			.command('sweep-crank-reservoir <marketIndex> <lamports>')
			.description(
				"Move lamports from a market's crank reservoir back to the treasury. The warm or cold admin signs. Lamports reach a reservoir through the refill crank and leave it as crank payments, so without this command they travel one way and a retired or over-provisioned market holds them forever. The balance never falls below the account rent, and a reservoir swept under its watermark refills itself."
			)
	).action(
		async (marketIndex: string, lamports: string, _flags, cmd: Command) => {
			const market = Number.parseInt(marketIndex, 10);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getSweepCrankReservoirIx(
					market,
					new BN(lamports)
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin fees sweep-crank-reservoir'
				);

				reportDispatch(
					`swept ${lamports} lamports from market ${market} reservoir`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);
}
