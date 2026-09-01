import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import { BANKRUPTCY_IF_FLOOR_DISABLED } from '@velocity-exchange/sdk';
import { parseBnArg, parseIntArg, parseMarketIndex } from '../lib/args';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import {
	reportDispatch,
	resolveAdminAuthority,
	sendOrPropose,
} from '../lib/squads';

export function registerPerpMarket(parent: Command): void {
	const pm = parent
		.command('perp-market')
		.description('Perp market governance.');

	withGlobalOptions(
		pm
			.command('set-status <market> <status>')
			.description(
				'Status: Active | Paused | ReduceOnly | Settlement | Delisted | Initialized.'
			)
	).action(async (market: string, status: string, _flags, cmd: Command) => {
		const marketIndex = parseMarketIndex(market);
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const enumVariant: { [k: string]: Record<string, never> } = {
				[status.charAt(0).toLowerCase() + status.slice(1)]: {},
			};
			const ix = await client.getUpdatePerpMarketStatusIx(
				marketIndex,
				enumVariant as never
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin perp-market set-status'
			);
			reportDispatch(`perp-market[${marketIndex}] status = ${status}`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		pm
			.command('set-fee-buffer <market> <amount>')
			.description(
				'Pnl-pool retention buffer the streaming fee sweep leaves above live user claims for the IF/AMM-provision drains; the protocol drain is buffer-exempt (raw u64, QUOTE_PRECISION).'
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const marketIndex = parseMarketIndex(market);
		const amountValue = parseBnArg('amount', amount);
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdatePerpMarketFeePoolBufferTargetIx(
				marketIndex,
				amountValue
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin perp-market set-fee-buffer'
			);
			reportDispatch(
				`perp-market[${marketIndex}] fee buffer = ${amountValue.toString()}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		pm
			.command('set-bankruptcy-if-floor <market> <pct>')
			.description(
				'Fraction of OI notional (at the oracle TWAP) the fee sweep leaves in pending_if_fee as a standing bankruptcy first-loss tranche (u32, PERCENTAGE_PRECISION: 1000000 = 100%, 1000 = 10 bps). 0 selects the 10 bps default; pass "disabled" to turn the floor off. A latched bankruptcy freezes the sweep either way.'
			)
	).action(async (market: string, pct: string, _flags, cmd: Command) => {
		const marketIndex = parseMarketIndex(market);
		const value =
			pct === 'disabled'
				? BANKRUPTCY_IF_FLOOR_DISABLED
				: parseIntArg('pct', pct, 0, 1000000);
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdatePerpMarketBankruptcyIfFloorPctIx(
				marketIndex,
				value
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin perp-market set-bankruptcy-if-floor'
			);
			reportDispatch(
				`perp-market[${marketIndex}] bankruptcy_if_floor_pct = ${value}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		pm
			.command(
				'set-spread-adjustment <market> <spreadAdjustment> <inventorySpreadAdjustment>'
			)
			.description(
				'Set the vAMM final-spread and inventory-spread percentage adjustments. Both values must be integers in [-100, 100]; -100 removes the component, 0 leaves it unchanged, and 100 doubles it. Negative values must follow a `--` separator so they are not parsed as flags. Requires VammQuoteManagement, warm, or cold.'
			)
	).action(
		async (
			market: string,
			spreadAdjustment: string,
			inventorySpreadAdjustment: string,
			_flags,
			cmd: Command
		) => {
			const marketIndex = parseMarketIndex(market);
			const spread = parseIntArg(
				'spreadAdjustment',
				spreadAdjustment,
				-100,
				100
			);
			const inventorySpread = parseIntArg(
				'inventorySpreadAdjustment',
				inventorySpreadAdjustment,
				-100,
				100
			);

			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const multisigPda = opts.multisig
					? new PublicKey(opts.multisig)
					: undefined;
				const ix = await client.getUpdatePerpMarketAmmSpreadAdjustmentIx(
					marketIndex,
					spread,
					inventorySpread,
					// referencePriceOffset is ignored onchain: amm.reference_price_offset
					// is a per-crank output, not an admin-set value.
					0,
					resolveAdminAuthority(provider, multisigPda)
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					multisigPda,
					'velocity-admin perp-market set-spread-adjustment'
				);
				reportDispatch(
					`perp-market[${marketIndex}] amm_spread_adjustment = ${spread}, amm_inventory_spread_adjustment = ${inventorySpread}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		pm
			.command('set-funding-bias-sensitivity <market> <sensitivity>')
			.description(
				'Funding bias sensitivity (u8, hundredths): paying-side spread widens up to 1 + sensitivity/100 while the vAMM pays funding. 50 => up to 1.5x, 0 disables.'
			)
	).action(
		async (market: string, sensitivity: string, _flags, cmd: Command) => {
			const marketIndex = parseMarketIndex(market);
			const value = parseIntArg('sensitivity', sensitivity, 0, 255);

			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const multisigPda = opts.multisig
					? new PublicKey(opts.multisig)
					: undefined;
				const ix = await client.getUpdatePerpMarketFundingBiasSensitivityIx(
					marketIndex,
					value,
					resolveAdminAuthority(provider, multisigPda)
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					multisigPda,
					'velocity-admin perp-market set-funding-bias-sensitivity'
				);
				reportDispatch(
					`perp-market[${marketIndex}] funding_bias_sensitivity = ${value}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		pm
			.command('set-funding-dead-zone <market> <threshold> <slope>')
			.description(
				'Funding dead zone: threshold (u32, bps) is the noise band where the premium stays zero; slope (u32, PERCENTAGE_PRECISION, 1000000 = 1.0x) is the ramp applied to the spread past the band. 5 / 1000000 reproduces the launch defaults.'
			)
	).action(
		async (
			market: string,
			threshold: string,
			slope: string,
			_flags,
			cmd: Command
		) => {
			const marketIndex = parseMarketIndex(market);
			const thresholdValue = parseIntArg('threshold', threshold, 0, 9999);
			const slopeValue = parseIntArg('slope', slope, 1, 0xffffffff);

			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getUpdatePerpMarketFundingDeadZoneIx(
					marketIndex,
					thresholdValue,
					slopeValue
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin perp-market set-funding-dead-zone'
				);
				reportDispatch(
					`perp-market[${marketIndex}] funding_clamp_threshold = ${thresholdValue}, funding_ramp_slope = ${slopeValue}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		pm
			.command('set-oracle-slot-delay <market> <slots>')
			.description(
				'oracle_slot_delay_override (i8): max oracle age before the oracle is "stale for amm immediate". >0 = explicit threshold; 0 = never allow immediate AMM fills; <0 = unset, source-aware fallback (the init default). Set a positive value (e.g. 5) below the low-risk guard rail (10). Stored in legacy 400ms units and converted to actual slots on chain as slot time drops.'
			)
	).action(async (market: string, slots: string, _flags, cmd: Command) => {
		const marketIndex = parseMarketIndex(market);
		const value = parseIntArg('slots', slots, -128, 127);
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdatePerpMarketOracleSlotDelayOverrideIx(
				marketIndex,
				value
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin perp-market set-oracle-slot-delay'
			);
			reportDispatch(
				`perp-market[${marketIndex}] oracle_slot_delay_override = ${value}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});
}
