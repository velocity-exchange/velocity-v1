import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import {
	FeeStructure,
	FeeTier,
	HotRole,
	decodeName,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient } from '../lib/provider';

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
			// User.getUserFeeTier: taker's rolling 30-day volume picks tiers 0-5.
			const perpTierLabels = [
				'30d vol <  $2M ',
				'30d vol >= $2M ',
				'30d vol >= $10M',
				'30d vol >= $20M',
				'30d vol >= $80M',
				'30d vol >= $200M',
			];
			console.log('trading fees — perp (per fill, tier by taker 30d volume):');
			perpTierLabels.forEach((label, i) => {
				console.log(
					`  tier ${i} (${label}):`,
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
					`fee adjustment ${describeFeeAdjustment(market.feeAdjustment)} |`,
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
