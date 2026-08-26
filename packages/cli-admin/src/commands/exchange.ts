import { Command } from 'commander';
import { PublicKey } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	activeSlotDurationFromState,
	getIbrlFeatureGate,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, sendOrPropose } from '../lib/squads';

export function registerExchange(parent: Command): void {
	const ex = parent.command('exchange').description('Whole-protocol controls.');

	withGlobalOptions(
		ex
			.command('set-status <bitfield>')
			.description(
				'Set ExchangeStatus bitfield. 0=active. Bits: 1=depositPaused, 2=withdrawPaused, 4=ammPaused, 8=fillPaused, 16=liqPaused, 32=fundingPaused, 64=settlePnlPaused.'
			)
	).action(async (bitfield: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateExchangeStatusIx(
				Number.parseInt(bitfield, 10)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin exchange set-status'
			);
			reportDispatch(`exchange status = ${bitfield}`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		ex
			.command('sync-slot-duration <ms>')
			.description(
				'Synchronize one IBRL slot-duration transition (400 -> 350 -> 300 -> 250 -> 200) from its feature-gate account. Permissionless: the program validates the gate and derives the effective slot from the EpochSchedule itself; idempotent per gate. 0 on chain reads as 400.'
			)
	).action(async (ms: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			if (!/^\d+$/.test(ms.trim())) {
				throw new Error(
					`slot duration must be an integer number of ms, got "${ms}"`
				);
			}
			const newMs = Number.parseInt(ms.trim(), 10);
			const currentSlot = await provider.connection.getSlot();
			// The live value, resolved through the transition archive.
			const currentMs = activeSlotDurationFromState(
				client.getStateAccount(),
				new BN(currentSlot)
			);
			console.log(`slot duration ${currentMs}ms -> ${newMs}ms`);
			// Preview the effective slot the program will derive: first slot of the
			// epoch after the gate's activation epoch (mirrors Agave).
			const featureGate = getIbrlFeatureGate(newMs);
			if (featureGate) {
				const acct = await provider.connection.getAccountInfo(featureGate);
				if (acct && acct.data.length === 9 && acct.data[0] === 1) {
					const activation = Number(acct.data.readBigUInt64LE(1));
					const epochSchedule = await provider.connection.getEpochSchedule();
					const effective = epochSchedule.getFirstSlotInEpoch(
						epochSchedule.getEpoch(activation) + 1
					);
					const status =
						currentSlot >= effective
							? 'already effective'
							: `effective in ~${effective - currentSlot} slots`;
					console.log(
						`IBRL gate ${featureGate.toBase58()}: activation slot ${activation}, ` +
							`effective slot ${effective} (current ${currentSlot}, ${status})`
					);
				} else {
					console.log(
						`IBRL gate ${featureGate.toBase58()} is not activated yet — the program will reject this until Anza activates it`
					);
				}
			}
			const ix = await client.getSyncStateSlotDurationIx(newMs);
			// permissionless instruction with no admin signer: always send
			// directly, never through a multisig proposal (a proposal would
			// fail to execute and only adds delay)
			const result = await sendOrPropose(
				provider,
				[ix],
				undefined,
				'velocity-admin exchange sync-slot-duration'
			);
			reportDispatch(`slot duration sync = ${newMs}ms`, result);
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		ex
			.command('set-solvency-status <bitfield>')
			.description(
				'Set SolvencyStatus bitfield, gating internal solvency-repair ixs (resolve bankruptcy / pnl-deficit) independently of withdrawals. Cold admin only. 0=active. Bits: 1=solvencyRepairPaused.'
			)
	).action(async (bitfield: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const ix = await client.getUpdateSolvencyStatusIx(
				Number.parseInt(bitfield, 10)
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin exchange set-solvency-status'
			);
			reportDispatch(`solvency status = ${bitfield}`, result);
		} finally {
			await client.unsubscribe();
		}
	});
}
