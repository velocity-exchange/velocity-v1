import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import { getSpotMarketPublicKey } from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';

export function registerSpotMarket(parent: Command): void {
	const sm = parent
		.command('spot-market')
		.description('Spot market governance.');

	withGlobalOptions(
		sm
			.command('set-status <market> <status>')
			.description(
				'Status: Active | Paused | ReduceOnly | Settlement | Delisted | Initialized.'
			)
	).action(async (market: string, status: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const enumVariant: { [k: string]: Record<string, never> } = {
				[status.charAt(0).toLowerCase() + status.slice(1)]: {},
			};
			const ix = await client.getUpdateSpotMarketStatusIx(
				Number.parseInt(market, 10),
				enumVariant as never
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin spot-market set-status'
			);
			reportDispatch(`spot-market[${market}] status = ${status}`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		sm
			.command('set-guard-threshold <market> <threshold>')
			.description(
				'Per-market withdraw guard threshold (raw u64, in token base units). ' +
					'On-chain program rejects thresholds worth more than $10k notional ' +
					'at the current oracle price.'
			)
	).action(async (market: string, threshold: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const marketIndex = Number.parseInt(market, 10);
			// The ix requires the spot market's oracle so the program can
			// enforce the $10k notional cap on the threshold.
			const spotMarketPk = await getSpotMarketPublicKey(
				client.program.programId,
				marketIndex
			);
			const accountInfo = await provider.connection.getAccountInfo(
				spotMarketPk
			);
			if (!accountInfo) {
				throw new Error(`spot market ${marketIndex} not found on chain`);
			}
			const { oracle } = (
				client.program.account as any
			).spotMarket.coder.accounts.decodeUnchecked(
				'spotMarket',
				accountInfo.data
			);
			const ix = await client.getUpdateWithdrawGuardThresholdIx(
				marketIndex,
				new BN(threshold),
				oracle as PublicKey
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin spot-market set-guard-threshold'
			);
			reportDispatch(
				`spot-market[${market}] guard-threshold = ${threshold} (oracle ${(
					oracle as PublicKey
				).toBase58()})`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		sm
			.command('set-fee-factors <market> <ifFeeFactor> <protocolFeeFactor>')
			.description(
				'Lending deposit-interest carveouts (IF_FACTOR_PRECISION = 1e6; sum <= 1e6): ifFeeFactor -> staker-owned insurance fund, protocolFeeFactor -> withdrawable protocol fees. Lenders receive the rest.'
			)
	).action(
		async (
			market: string,
			ifFeeFactor: string,
			protocolFeeFactor: string,
			_flags,
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getUpdateSpotMarketIfFactorIx(
					Number.parseInt(market, 10),
					Number.parseInt(ifFeeFactor, 10),
					Number.parseInt(protocolFeeFactor, 10)
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin spot-market set-fee-factors'
				);
				reportDispatch(
					`spot-market[${market}] if_fee_factor = ${ifFeeFactor}, protocol_fee_factor = ${protocolFeeFactor}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);

	withGlobalOptions(
		sm
			.command('set-withdraw-breaker <market> <pct>')
			.description(
				'Per-market daily withdraw circuit-breaker size: the max fraction of ' +
					'the 24h deposit TWAP withdrawable per 24h window ' +
					'(PERCENTAGE_PRECISION = 1e6, e.g. 250000 = 25%). 0 => default 25%.'
			)
	).action(async (market: string, pct: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateSpotMarketWithdrawCircuitBreakerIx(
				Number.parseInt(market, 10),
				Number.parseInt(pct, 10)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin spot-market set-withdraw-breaker'
			);
			reportDispatch(
				`spot-market[${market}] withdraw_circuit_breaker_pct = ${pct}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		sm
			.command('set-deposit-cap <market> <threshold> <pctPerDay>')
			.description(
				'Per-market daily deposit cap. threshold (raw u64, token base units): ' +
					'no rate limit below it. pctPerDay (PERCENTAGE_PRECISION = 1e6): max ' +
					'fraction above the 24h deposit TWAP deposits may reach per 24h window. ' +
					'pctPerDay = 0 disables the cap.'
			)
	).action(
		async (
			market: string,
			threshold: string,
			pctPerDay: string,
			_flags,
			cmd: Command
		) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts);
			try {
				const ix = await client.getUpdateSpotMarketDepositCapIx(
					Number.parseInt(market, 10),
					new BN(threshold),
					Number.parseInt(pctPerDay, 10)
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin spot-market set-deposit-cap'
				);
				reportDispatch(
					`spot-market[${market}] deposit_guard_threshold = ${threshold}, max_deposit_pct_per_day = ${pctPerDay}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);
}
