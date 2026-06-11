import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';

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
			.command('set-recipient <pubkey>')
			.description(
				'Set State.protocol_fee_recipient (cold admin only). Withdrawals can only pay token accounts owned by this key.'
			)
	).action(async (pubkey: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateProtocolFeeRecipientIx(
				new PublicKey(pubkey)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin fees set-recipient'
			);
			reportDispatch(`protocol_fee_recipient = ${pubkey}`, result);
		} finally {
			await client.unsubscribe();
		}
	});

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
}
