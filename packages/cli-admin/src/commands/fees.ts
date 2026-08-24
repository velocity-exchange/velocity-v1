import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import {
	escrowHasReferrer,
	getRevenueShareAccountPublicKey,
	isBuilderOrderReferral,
	isVariant,
	MarketType,
	RevenueShareEscrowAccount,
	RevenueShareEscrowMap,
	TransferFeeAndPnlPoolDirection,
	ACCELERATED_REFERRER_REWARD_PERCENT,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import {
	reportDispatch,
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
				'Settle accrued builder/referrer revenue share for a perp market out of its pnl pool (permissionless). Pays beneficiaries without the escrow owner having to settle pnl. Pass an escrow authority for one escrow, or --all to scan for and settle every escrow still owed on the market — which is what `settle_expired_market_pools_to_revenue_pool` requires before it will delist.'
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

				// A row that still owes after the settle pass is one that the program will not pay.
				// The beneficiary has no payout account, the pool is too small, or the row names
				// nobody. The delist needs a zero counter, so write those rows off here. The
				// program proves each reason again and rejects a payable row with
				// RevenueShareOrderNotForfeitable.
				//
				// Only for a closing market. On a live market the program refuses every forfeit, so
				// this pass would report failures after a successful settle pass.
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
				"Set a perp market's additive taker-fee surcharge in tenth-bps (15 = +1.5bp, 0 = none), applied to the tier fee before feeAdjustment. Surcharge only, range 0..100; discounts go through set-promo-tier. Warm admin."
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
				'Set the promotional fee-tier floor: every account gets at least this perp fee tier while set (1 = VIP 1, 2 = VIP 2; accounts already above keep their tier). 0 disables; accounts revert to volume tiers on their next fill. Warm admin.'
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
}
