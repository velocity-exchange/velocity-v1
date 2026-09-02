import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	ExchangeStatus,
	FeatureBitFlags,
	FeeStructure,
	FeeTier,
	HotRole,
	LpPoolFeatureBitFlags,
	SolvencyStatus,
	decodeName,
	getTokenAmount,
	SpotBalanceType,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';

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

function describeFeeTier(tier: FeeTier): string {
	return [
		`taker ${asBps(tier.feeNumerator, tier.feeDenominator)}`,
		`maker rebate ${asBps(
			tier.makerRebateNumerator,
			tier.makerRebateDenominator
		)}`,
		`referrer reward ${asPct(
			tier.referrerRewardNumerator,
			tier.referrerRewardDenominator
		)} of taker fee`,
		`referee discount ${asPct(
			tier.refereeFeeNumerator,
			tier.refereeFeeDenominator
		)}`,
	].join(' | ');
}

function printFillerReward(structure: FeeStructure): void {
	console.log(
		'  filler (keeper) reward, carved out of the taker fee:',
		`flat $${trimZeros(structure.flatFillerFee.toNumber() / 1_000_000)}`,
		`+ ${asPct(
			structure.fillerRewardStructure.rewardNumerator,
			structure.fillerRewardStructure.rewardDenominator
		)} variable component`,
		`(time-based lower bound $${trimZeros(
			structure.fillerRewardStructure.timeBasedRewardLowerBound.toNumber() /
				1_000_000
		)})`
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

/** Render any decoded account field: pubkeys as base58, BNs as decimals, arrays/structs inline. */
function formatField(value: unknown): string {
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
		return `[${value.map(formatField).join(', ')}]`;
	}
	if (typeof value === 'object') {
		const entries = Object.entries(value as Record<string, unknown>).map(
			([k, v]) => `${k}: ${formatField(v)}`
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
			console.log('cold admin:', state.coldAdmin.toBase58());
			console.log('warm admin:', state.warmAdmin.toBase58());
			console.log(
				'pause admin:',
				state.pauseAdmin.equals(PublicKey.default)
					? '(unset)'
					: state.pauseAdmin.toBase58()
			);
			for (const role of Object.values(HotRole)) {
				const key = `hot${role.charAt(0).toUpperCase()}${role.slice(
					1
				)}` as keyof typeof state;
				const value = state[key] as unknown as PublicKey | undefined;
				const display =
					value && !value.equals(PublicKey.default)
						? value.toBase58()
						: '(unset)';
				console.log(`hot.${role}:`, display);
			}
			const recipientPerp = state.protocolFeeRecipientPerp;
			console.log(
				'protocol fee recipient (perp):',
				recipientPerp.equals(PublicKey.default)
					? '(unset)'
					: recipientPerp.toBase58()
			);
			const recipientSpot = state.protocolFeeRecipientSpot;
			console.log(
				'protocol fee recipient (spot):',
				recipientSpot.equals(PublicKey.default)
					? '(unset)'
					: recipientSpot.toBase58()
			);
			console.log(
				'trade-fee split: amm =',
				`${state.perpFeeStructure.ammFeeNumerator}%,`,
				'if =',
				`${state.perpFeeStructure.ifFeeNumerator}%,`,
				'protocol = residual'
			);
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
			console.log('state account:', statePk.toBase58());
			const width = Math.max(
				...Object.keys(state as unknown as object).map((k) => k.length)
			);
			for (const [key, value] of Object.entries(
				state as unknown as Record<string, unknown>
			)) {
				console.log(
					`  ${key.padEnd(width)}  ${decoded[key] ?? formatField(value)}`
				);
			}
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
			// User.getUserFeeTier: taker's rolling 30-day volume picks tiers 0-2
			// (Regular / VIP 1 / VIP 2).
			const perpTierLabels = [
				['Regular', '30d vol <  $5M '],
				['VIP 1  ', '30d vol >= $5M '],
				['VIP 2  ', '30d vol >= $80M'],
			];
			console.log('trading fees — perp (per fill, tier by taker 30d volume):');
			perpTierLabels.forEach(([name, label], i) => {
				console.log(
					`  ${name} (tier ${i}, ${label}):`,
					describeFeeTier(state.perpFeeStructure.feeTiers[i])
				);
			});
			printFillerReward(state.perpFeeStructure);
			console.log(
				'  remainder split (after rebate/referral/filler): amm =',
				`${state.perpFeeStructure.ammFeeNumerator}%,`,
				'if =',
				`${state.perpFeeStructure.ifFeeNumerator}%,`,
				'protocol = residual'
			);
			if (state.promoFeeTier > 0) {
				const promoName =
					perpTierLabels[state.promoFeeTier]?.[0]?.trim() ??
					`tier ${state.promoFeeTier}`;
				console.log(
					`  PROMO ACTIVE: every account gets at least ${promoName} (tier ${state.promoFeeTier})`
				);
			}

			console.log('\ntrading fees — spot (per fill, all users pay tier 0):');
			console.log(`  ${describeFeeTier(state.spotFeeStructure.feeTiers[0])}`);
			printFillerReward(state.spotFeeStructure);

			console.log(
				'\nperp markets (liquidation fees are paid by the liquidatee; liquidator fee ramps up to min(3x base, maintenance margin) while unfilled):'
			);
			const perpMarkets = client
				.getPerpMarketAccounts()
				.sort((a, b) => a.marketIndex - b.marketIndex);
			for (const market of perpMarkets) {
				console.log(
					`  [${market.marketIndex}] ${decodeName(market.name)}:`,
					`fee adjustment ${describeFeeAdjustment(market.feeAdjustment)},`,
					`taker addon ${market.takerFeeAddonTenthBps / 10}bps |`,
					`liquidation: liquidator ${pct1e6(market.liquidatorFee)},`,
					`if ${pct1e6(market.ifLiquidationFee)},`,
					`protocol ${pct1e6(market.protocolLiquidationFee)}`
				);
			}

			console.log(
				'\nspot markets (interest carveouts are shares of deposit-interest gains, not extra user charges):'
			);
			const spotMarkets = client
				.getSpotMarketAccounts()
				.sort((a, b) => a.marketIndex - b.marketIndex);
			for (const market of spotMarkets) {
				console.log(
					`  [${market.marketIndex}] ${decodeName(market.name)}:`,
					`fee adjustment ${describeFeeAdjustment(market.feeAdjustment)} |`,
					`liquidation: liquidator ${pct1e6(market.liquidatorFee)},`,
					`if ${pct1e6(market.ifLiquidationFee)},`,
					`protocol ${pct1e6(market.protocolLiquidationFee)} |`,
					`interest carveout: if ${pct1e6(market.insuranceFund.ifFeeFactor)},`,
					`protocol ${pct1e6(market.protocolFeeFactor)}`
				);
			}
		} finally {
			await client.unsubscribe();
		}
	});
}
