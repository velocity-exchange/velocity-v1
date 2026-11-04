import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import {
	PRICE_PRECISION,
	decodeName,
	getSpotMarketPublicKey,
} from '@velocity-exchange/sdk';
import { parseIntArg, parseMarketIndex } from '../lib/args';
import { DEFAULT_WITHDRAW_BREAKER_BPS, breakerState } from '../lib/breaker';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';
import * as ui from '../lib/ui';
import { resolveAuthority } from '../lib/userOps';

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
			// All spot-market governance ixs are warm-gated: the admin signer
			// must be the key that executes — the multisig's vault PDA when
			// proposing, else the local keypair (never state.coldAdmin blindly).
			const ix = await client.getUpdateSpotMarketStatusIx(
				Number.parseInt(market, 10),
				enumVariant as never,
				resolveAuthority(opts)
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
				oracle as PublicKey,
				resolveAuthority(opts)
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
			.command('set-scale-initial-asset-weight-start <market> <start>')
			.description(
				'Deposit-notional threshold (QUOTE_PRECISION, 1e6) above which initialAssetWeight scales down. 0 disables. Maintenance weight is unaffected.'
			)
	).action(async (market: string, start: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateSpotMarketScaleInitialAssetWeightStartIx(
				Number.parseInt(market, 10),
				new BN(start),
				resolveAuthority(opts)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin spot-market set-scale-initial-asset-weight-start'
			);
			reportDispatch(
				`spot-market[${market}] scale_initial_asset_weight_start = ${start}`,
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
					Number.parseInt(protocolFeeFactor, 10),
					resolveAuthority(opts)
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
			.command('set-withdraw-breaker <changes...>')
			.description(
				'Set the daily withdraw circuit breaker on one or more spot markets in ONE transaction ' +
					'or proposal. Each <change> is <market>:<bps>, the max fraction of the 24h deposit TWAP ' +
					'withdrawable per 24h window (10000 = 100%, 2500 = 25%, 0 = the 25% default). Warm ' +
					"admin may only set up to 25%. Prints each market's withdraw headroom and Grafana " +
					'breaker consumption before and after, using the TWAP the next transaction will see.'
			)
	).action(async (changes: string[], _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const parsed = changes.map((arg) => {
			const match = /^(\d+):(\d+)$/.exec(arg.trim());
			if (!match) {
				throw new Error(`change "${arg}" must be <market>:<bps>`);
			}
			return {
				marketIndex: parseMarketIndex(match[1]),
				bps: parseIntArg('bps', match[2], 0, 10000),
			};
		});
		const markets = parsed.map((c) => c.marketIndex);
		if (new Set(markets).size !== markets.length) {
			throw new Error(`each market may appear once, got ${markets.join(', ')}`);
		}
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const now = Math.floor(Date.now() / 1000);
			const pct = (bps: number) =>
				`${((bps || DEFAULT_WITHDRAW_BREAKER_BPS) / 100).toFixed(0)}%${
					bps === 0 ? ' (0)' : ''
				}`;
			const rows = [['market', 'breaker', 'headroom', 'usd', 'consumed']];
			const ixs = [];
			for (const { marketIndex, bps } of parsed) {
				const market = client.getSpotMarketAccount(marketIndex);
				if (!market) {
					throw new Error(`spot market ${marketIndex} not found`);
				}
				const price =
					client.getOracleDataForSpotMarket(marketIndex).price.toNumber() /
					PRICE_PRECISION.toNumber();
				const before = breakerState(market, now);
				const after = breakerState(market, now, bps);
				const usd = (x: number) =>
					`$${Math.round(x * price).toLocaleString('en-US')}`;
				rows.push([
					`${marketIndex} ${decodeName(market.name)}`,
					ui.change(pct(market.withdrawCircuitBreakerBps), pct(bps)),
					ui.change(before.headroom.toFixed(4), after.headroom.toFixed(4)),
					ui.change(usd(before.headroom), usd(after.headroom)),
					ui.change(
						`${before.consumedPct.toFixed(1)}%`,
						`${after.consumedPct.toFixed(1)}%`
					),
				]);
				ixs.push(
					await client.getUpdateSpotMarketWithdrawCircuitBreakerIx(
						marketIndex,
						bps
					)
				);
			}
			ui.header('withdraw breakers');
			ui.table(rows);
			ui.note(
				'headroom is what users can withdraw before the breaker blocks; negative means blocked now'
			);
			const result = await sendOrPropose(
				provider,
				ixs,
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				`velocity-admin spot-market set-withdraw-breaker ${changes.join(' ')}`
			);
			reportDispatch(
				`spot-market withdraw_circuit_breaker_bps ${changes.join(' ')}`,
				result
			);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		sm
			.command('set-max-token-deposits <market> <amount>')
			.description(
				'Hard deposit cap for a spot market. <amount> is raw u64 token base units ' +
					'(mint decimals); 0 = uncapped. Deposits that would push total market ' +
					'deposits above it are rejected. Warm/cold admin. Mirror the change in ' +
					'deploy-scripts/params/relaunch-spot-markets.json (max_token_deposits).'
			)
	).action(async (market: string, amount: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateSpotMarketMaxTokenDepositsIx(
				Number.parseInt(market, 10),
				new BN(amount)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin spot-market set-max-token-deposits'
			);
			reportDispatch(
				`spot-market[${market}] max_token_deposits = ${amount}`,
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
					'no rate limit below it. pctPerDay (basis points, 10000 = 100%): max ' +
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
					`spot-market[${market}] deposit_guard_threshold = ${threshold}, max_deposit_bps_per_day = ${pctPerDay}`,
					result
				);
			} finally {
				await client.unsubscribe();
			}
		}
	);
}
