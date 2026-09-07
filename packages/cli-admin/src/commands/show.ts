import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	ExchangeStatus,
	FeatureBitFlags,
	FeeStructure,
	HotRole,
	LpPoolFeatureBitFlags,
	SolvencyStatus,
	decodeName,
	getTokenAmount,
	SpotBalanceType,
} from '@velocity-exchange/sdk';
import pc from 'picocolors';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import * as ui from '../lib/ui';

/** Render `numerator / denominator` as basis points (e.g. taker fee). */
function asBps(numerator: number, denominator: number): string {
	return `${trimZeros((numerator / denominator) * 10_000)} bps`;
}

/** Render `numerator / denominator` as a percentage (e.g. referrer share). */
function asPct(numerator: number, denominator: number): string {
	return `${trimZeros((numerator / denominator) * 100)}%`;
}

/** Render a 1e6-precision fraction (LIQUIDATION_FEE_PRECISION, IF_FACTOR_PRECISION) as a percentage. */
function pct1e6(value: number): string {
	return `${trimZeros((value / 1_000_000) * 100)}%`;
}

function trimZeros(n: number): string {
	return n.toFixed(4).replace(/\.?0+$/, '');
}

/** A perp/spot market's `feeAdjustment`: signed percent applied to the tier's taker fee and maker rebate. */
function describeFeeAdjustment(adjustment: number): string {
	if (adjustment === 0) {
		return 'none';
	}
	return `${adjustment > 0 ? '+' : ''}${adjustment}% of tier fee/rebate`;
}

function printFillerReward(structure: FeeStructure): void {
	ui.kv(
		'filler reward',
		pc.dim(
			`flat $${trimZeros(
				structure.flatFillerFee.toNumber() / 1_000_000
			)} + ${asPct(
				structure.fillerRewardStructure.rewardNumerator,
				structure.fillerRewardStructure.rewardDenominator
			)}, floor $${trimZeros(
				structure.fillerRewardStructure.timeBasedRewardLowerBound.toNumber() /
					1_000_000
			)}`
		)
	);
}

/** Render a bitmask as its raw value, binary form, and the names of the set bits. */
function describeBitmask(
	value: number,
	bits: Record<string, number>,
	zeroLabel = 'none set'
): string {
	const set = Object.entries(bits)
		.filter(([, bit]) => bit !== 0 && (value & bit) === bit)
		.map(([name]) => name);
	const names = set.length > 0 ? set.join(' | ') : zeroLabel;
	return `${value} (0b${value.toString(2).padStart(8, '0')}) = ${names}`;
}

/**
 * Render any decoded account field: pubkeys as base58, BNs as decimals,
 * arrays/structs inline. Sanitized: account fields are chain data and a
 * string field can carry terminal escapes.
 */
function formatField(value: unknown): string {
	return ui.safe(renderField(value));
}

function renderField(value: unknown): string {
	if (value === undefined || value === null) {
		return '(none)';
	}
	if (value instanceof PublicKey) {
		return value.equals(PublicKey.default) ? '(unset)' : value.toBase58();
	}
	if (BN.isBN(value)) {
		return value.toString();
	}
	if (Array.isArray(value)) {
		return `[${value.map(renderField).join(', ')}]`;
	}
	if (typeof value === 'object') {
		const entries = Object.entries(value as Record<string, unknown>).map(
			([k, v]) => `${k}: ${renderField(v)}`
		);
		return `{ ${entries.join(', ')} }`;
	}
	return String(value);
}

export function registerShow(parent: Command): void {
	const show = parent
		.command('show')
		.description('Read-only inspectors for the live admin authority state.');

	withGlobalOptions(
		show
			.command('config')
			.description(
				'Print the cold admin, warm admin, and every hot-role pubkey from State.'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const client = await buildAdminClient(opts);
		try {
			const state = client.getStateAccount();
			const key = (value: PublicKey) =>
				value.equals(PublicKey.default)
					? pc.dim('(unset)')
					: pc.dim(value.toBase58());

			ui.header('admin authorities');
			ui.table([
				[pc.bold('cold'), pc.dim(state.coldAdmin.toBase58())],
				[pc.bold('warm'), pc.dim(state.warmAdmin.toBase58())],
				[pc.bold('pause'), key(state.pauseAdmin)],
			]);

			ui.header('hot roles');
			ui.table(
				Object.values(HotRole).map((role) => {
					const field = `hot${role.charAt(0).toUpperCase()}${role.slice(
						1
					)}` as keyof typeof state;
					const value = state[field] as unknown as PublicKey | undefined;
					return [pc.bold(role), value ? key(value) : pc.dim('(unset)')];
				})
			);

			ui.header('protocol fees');
			ui.table([
				[pc.bold('recipient perp'), key(state.protocolFeeRecipientPerp)],
				[pc.bold('recipient spot'), key(state.protocolFeeRecipientSpot)],
				[
					pc.bold('split'),
					pc.dim(
						`amm ${state.perpFeeStructure.ammFeeNumerator}%, if ` +
							`${state.perpFeeStructure.ifFeeNumerator}%, protocol residual`
					),
				],
			]);
			console.log('');
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		show
			.command('state')
			.description(
				'Dump every field of the singleton State account, with the exchange-status, feature, LP-pool feature, and solvency bitmasks decoded to bit names.'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const client = await buildAdminClient(opts);
		try {
			const state = client.getStateAccount();
			const decoded: Record<string, string> = {
				exchangeStatus: describeBitmask(
					state.exchangeStatus,
					ExchangeStatus as unknown as Record<string, number>,
					'ACTIVE'
				),
				featureBitFlags: describeBitmask(
					state.featureBitFlags,
					FeatureBitFlags as unknown as Record<string, number>
				),
				lpPoolFeatureBitFlags: describeBitmask(
					state.lpPoolFeatureBitFlags,
					LpPoolFeatureBitFlags as unknown as Record<string, number>
				),
				solvencyStatus: describeBitmask(
					state.solvencyStatus,
					SolvencyStatus as unknown as Record<string, number>,
					'ACTIVE'
				),
			};

			const statePk = await client.getStatePublicKey();
			ui.header('State', pc.dim(statePk.toBase58()));
			ui.table(
				Object.entries(state as unknown as Record<string, unknown>).map(
					([field, value]) => [
						pc.dim(field),
						decoded[field] ? pc.bold(decoded[field]) : formatField(value),
					]
				)
			);
			console.log('');
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		show
			.command('perp-markets [market]')
			.description(
				"Print every live perp market's risk and quoting params: OI cap, margins, " +
					'imf, liquidation fees, spreads, jit/curve intensities, funding clamp, ' +
					'insurance claim, and the fee/pnl pool balances (the vAMM capital view). ' +
					'Pass a market index to show just one.'
			)
	).action(async (market: string | undefined, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const client = await buildAdminClient(opts);
		try {
			const markets = (client as any).getPerpMarketAccounts() as any[];
			markets.sort((a, b) => a.marketIndex - b.marketIndex);
			const filter =
				market !== undefined ? Number.parseInt(market, 10) : undefined;
			for (const m of markets) {
				if (filter !== undefined && m.marketIndex !== filter) {
					continue;
				}
				const quoteSpot = (client as any).getQuoteSpotMarketAccount();
				const q = (bn: any) => trimZeros(Number(bn.toString()) / 1_000_000);
				const feePool = getTokenAmount(
					m.amm.feePool.scaledBalance,
					quoteSpot,
					SpotBalanceType.DEPOSIT
				);
				const pnlPool = getTokenAmount(
					m.pnlPool.scaledBalance,
					quoteSpot,
					SpotBalanceType.DEPOSIT
				);
				// AMM reserve price: (quote/base) * peg. Good enough for display.
				const price =
					(Number(m.amm.quoteAssetReserve.toString()) /
						Number(m.amm.baseAssetReserve.toString())) *
					(Number(m.amm.pegMultiplier.toString()) / 1e6);
				const maxOiBase = Number(m.maxOpenInterest.toString()) / 1e9;
				console.log(
					`${decodeName(m.name)} (perp ${m.marketIndex}, ${JSON.stringify(
						m.status
					)})`
				);
				console.log(
					`  oracle: ${m.oracle.toBase58()} reservePrice=$${trimZeros(price)}`
				);
				console.log(
					`  max OI: ${trimZeros(maxOiBase)} base (~$${trimZeros(
						maxOiBase * price
					)})  quote_max_insurance: $${q(m.insuranceClaim.quoteMaxInsurance)}`
				);
				console.log(
					`  margins: init ${m.marginRatioInitial / 100}% maint ${
						m.marginRatioMaintenance / 100
					}%  imf: ${m.imfFactor}  liq fees: ${pct1e6(
						m.liquidatorFee
					)}/${pct1e6(m.ifLiquidationFee)} (liquidator/IF)`
				);
				console.log(
					`  spreads: base ${m.amm.baseSpread / 100}bp max ${
						m.amm.maxSpread / 100
					}bp  jit: ${m.amm.ammJitIntensity}  curve intensity: ${
						m.amm.curveUpdateIntensity
					}`
				);
				console.log(
					`  funding: clamp ${m.fundingClampThreshold}bp slope ${
						Number(m.fundingRampSlope) / 1e6
					}x`
				);
				console.log(
					`  pools: fee $${trimZeros(Number(feePool) / 1e6)} pnl $${trimZeros(
						Number(pnlPool) / 1e6
					)}  sqrt_k ${trimZeros(Number(m.amm.sqrtK.toString()) / 1e9)}`
				);
			}
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		show
			.command('spot-markets [market]')
			.description(
				"Print every live spot market's lending and collateral params: deposit cap " +
					'and headroom, scale start, weights, rate curve, withdraw guard, and the ' +
					'vault + insurance fund balances. Pass a market index to show just one.'
			)
	).action(async (market: string | undefined, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const markets = (client as any).getSpotMarketAccounts() as any[];
			markets.sort((a, b) => a.marketIndex - b.marketIndex);
			const filter =
				market !== undefined ? Number.parseInt(market, 10) : undefined;
			for (const m of markets) {
				if (filter !== undefined && m.marketIndex !== filter) {
					continue;
				}
				const div = 10 ** m.decimals;
				const deposits =
					Number(getTokenAmount(m.depositBalance, m, SpotBalanceType.DEPOSIT)) /
					div;
				const borrows =
					Number(getTokenAmount(m.borrowBalance, m, SpotBalanceType.BORROW)) /
					div;
				const cap = Number(m.maxTokenDeposits.toString()) / div;
				const ifBal = await provider.connection
					.getTokenAccountBalance(m.insuranceFund.vault)
					.then((r) => r.value.uiAmountString)
					.catch(() => 'n/a');
				console.log(
					`${decodeName(m.name)} (spot ${m.marketIndex}, ${JSON.stringify(
						m.status
					)}) mint=${m.mint.toBase58()} decimals=${m.decimals}`
				);
				console.log(
					`  deposits: ${trimZeros(deposits)} / cap ${
						cap === 0 ? 'uncapped' : trimZeros(cap)
					}${
						cap > 0 ? ` (headroom ${trimZeros(cap - deposits)})` : ''
					}  borrows: ${trimZeros(borrows)}`
				);
				console.log(
					`  weights: asset ${m.initialAssetWeight / 100}/${
						m.maintenanceAssetWeight / 100
					}% liability ${m.initialLiabilityWeight / 100}/${
						m.maintenanceLiabilityWeight / 100
					}%  imf: ${m.imfFactor}  scale start: $${trimZeros(
						Number(m.scaleInitialAssetWeightStart.toString()) / 1e6
					)}`
				);
				console.log(
					`  rates: optimal ${pct1e6(m.optimalUtilization)} util @ ${pct1e6(
						m.optimalBorrowRate
					)} APR, max ${pct1e6(m.maxBorrowRate)}  withdraw guard: ${trimZeros(
						Number(m.withdrawGuardThreshold.toString()) / div
					)}`
				);
				console.log(
					`  liq fees: ${pct1e6(m.liquidatorFee)}/${pct1e6(
						m.ifLiquidationFee
					)}  IF vault: ${ifBal}`
				);
			}
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		show
			.command('fees')
			.description(
				'Print every fee a user can pay and when: perp/spot trading fee tiers, filler reward, the trade-fee split, and per-market fee adjustments + liquidation fees.'
			)
	).action(async (_flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const client = await buildAdminClient(opts);
		try {
			const state = client.getStateAccount();

			// Breakpoints mirror determine_perp_fee_tier (math/fees.rs) /
			// User.getUserFeeTier: taker's rolling 30-day volume picks tiers 0-3
			// (Regular / VIP 1 / VIP 2 / VIP 3).
			const perpTierLabels = [
				['Regular', '30d vol <  $5M  '],
				['VIP 1  ', '30d vol >= $5M  '],
				['VIP 2  ', '30d vol >= $80M '],
				['VIP 3  ', '30d vol >= $200M'],
			];
			ui.header(
				'perp trading fees',
				state.promoFeeTier > 0
					? pc.yellow(
							`promo floor: ${
								perpTierLabels[state.promoFeeTier]?.[0]?.trim() ??
								`tier ${state.promoFeeTier}`
							}`
					  )
					: pc.dim('tier by taker 30d volume')
			);
			ui.table(
				perpTierLabels.map(([name, label], i) => {
					const tier = state.perpFeeStructure.feeTiers[i];
					const promoted = state.promoFeeTier > 0 && i < state.promoFeeTier;
					return [
						promoted ? pc.dim(name.trim()) : pc.bold(name.trim()),
						pc.dim(label.trim()),
						`taker ${
							promoted
								? pc.dim(asBps(tier.feeNumerator, tier.feeDenominator))
								: pc.bold(asBps(tier.feeNumerator, tier.feeDenominator))
						}`,
						pc.dim(
							`maker ${asBps(
								tier.makerRebateNumerator,
								tier.makerRebateDenominator
							)} rebate`
						),
						pc.dim(
							`referrer ${asPct(
								tier.referrerRewardNumerator,
								tier.referrerRewardDenominator
							)}, referee ${asPct(
								tier.refereeFeeNumerator,
								tier.refereeFeeDenominator
							)}`
						),
						promoted ? pc.dim('below floor') : '',
					];
				})
			);
			printFillerReward(state.perpFeeStructure);
			ui.kv(
				'split',
				pc.dim(
					`amm ${state.perpFeeStructure.ammFeeNumerator}%, ` +
						`if ${state.perpFeeStructure.ifFeeNumerator}%, protocol residual`
				)
			);

			ui.header('spot trading fees', pc.dim('all users pay tier 0'));
			const spotTier = state.spotFeeStructure.feeTiers[0];
			ui.table([
				[
					pc.bold('tier 0'),
					`taker ${pc.bold(
						asBps(spotTier.feeNumerator, spotTier.feeDenominator)
					)}`,
					pc.dim(
						`maker ${asBps(
							spotTier.makerRebateNumerator,
							spotTier.makerRebateDenominator
						)} rebate`
					),
					pc.dim(
						`referrer ${asPct(
							spotTier.referrerRewardNumerator,
							spotTier.referrerRewardDenominator
						)}, referee ${asPct(
							spotTier.refereeFeeNumerator,
							spotTier.refereeFeeDenominator
						)}`
					),
				],
			]);
			printFillerReward(state.spotFeeStructure);

			ui.header(
				'perp markets',
				pc.dim('liquidation fees paid by the liquidatee')
			);
			ui.table(
				client
					.getPerpMarketAccounts()
					.sort((a, b) => a.marketIndex - b.marketIndex)
					.map((market) => [
						pc.dim(`[${market.marketIndex}]`),
						pc.bold(ui.safe(decodeName(market.name))),
						pc.dim(
							`fee adj ${describeFeeAdjustment(market.feeAdjustment)}, addon ${
								market.takerFeeAddonTenthBps / 10
							}bps`
						),
						pc.dim(
							`liq: liquidator ${pct1e6(market.liquidatorFee)}, if ${pct1e6(
								market.ifLiquidationFee
							)}, protocol ${pct1e6(market.protocolLiquidationFee)}`
						),
					])
			);
			ui.note('liquidator fee ramps to min(3x base, maintenance margin)');

			ui.header(
				'spot markets',
				pc.dim('carveouts are shares of deposit interest')
			);
			ui.table(
				client
					.getSpotMarketAccounts()
					.sort((a, b) => a.marketIndex - b.marketIndex)
					.map((market) => [
						pc.dim(`[${market.marketIndex}]`),
						pc.bold(ui.safe(decodeName(market.name))),
						pc.dim(`fee adj ${describeFeeAdjustment(market.feeAdjustment)}`),
						pc.dim(
							`liq: liquidator ${pct1e6(market.liquidatorFee)}, if ${pct1e6(
								market.ifLiquidationFee
							)}, protocol ${pct1e6(market.protocolLiquidationFee)}`
						),
						pc.dim(
							`carveout: if ${pct1e6(
								market.insuranceFund.ifFeeFactor
							)}, protocol ${pct1e6(market.protocolFeeFactor)}`
						),
					])
			);
			console.log('');
		} finally {
			await client.unsubscribe();
		}
	});
}
