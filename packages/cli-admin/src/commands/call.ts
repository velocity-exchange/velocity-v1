import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey, TransactionInstruction } from '@solana/web3.js';
import * as fs from 'fs';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, reportDryRun, sendOrPropose } from '../lib/squads';

/**
 * Generic IDL-driven dispatcher.
 *
 * Escape hatch for any velocity instruction without a dedicated CLI wrapper.
 * Caller supplies the camelCase ix name and a JSON file specifying `args` and
 * `accounts` (both keyed by the names in the IDL). Numeric BN args use a
 * string; pubkey accounts use the base58 string. Example payload:
 *
 * {
 *   "args": { "withdrawGuardThreshold": "1000000000" },
 *   "accounts": {
 *     "spotMarket": "...",
 *     "state": "...",
 *     "adminAuthorityConfig": "...",
 *     "admin": "..."
 *   }
 * }
 *
 * No PDA derivation, no implicit accounts. The named subcommands cover the
 * common cases — reach for this only when the wrapper doesn't exist yet.
 */
interface IxPayload {
	args?: Record<string, unknown>;
	accounts?: Record<string, string>;
}

function buildIxFromPayload(
	client: any,
	ixName: string,
	payload: IxPayload
): TransactionInstruction {
	const idlIx = (client.program.idl as any).instructions.find(
		(i: any) => i.name === ixName
	);
	if (!idlIx) {
		throw new Error(`unknown instruction "${ixName}"`);
	}
	const args = (idlIx.args as Array<{ name: string; type: unknown }>).map((a) =>
		coerceArg(payload.args?.[a.name], a.type)
	);
	const accounts = Object.fromEntries(
		Object.entries(payload.accounts ?? {}).map(([k, v]) => [
			k,
			new PublicKey(v),
		])
	);
	return (client.program.instruction as any)[ixName](...args, { accounts });
}

export function registerCall(parent: Command): void {
	withGlobalOptions(
		parent
			.command('call <ixName> <payloadFile>')
			.description(
				'Generic IDL-driven dispatcher. <ixName> is camelCase per the IDL (e.g. updateWithdrawGuardThreshold). <payloadFile> is a JSON file with { args, accounts }.'
			)
	).action(
		async (ixName: string, payloadFile: string, _flags, cmd: Command) => {
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const payload = JSON.parse(
					fs.readFileSync(payloadFile, 'utf-8')
				) as IxPayload;
				const ix = buildIxFromPayload(client, ixName, payload);

				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					`velocity-admin call ${ixName}`
				);
				reportDispatch(`call ${ixName}`, result);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		parent
			.command('batch <payloadFile>')
			.description(
				'Batch several instructions into ONE transaction / vault proposal (one ' +
					'approval round, one timelock). <payloadFile> is a JSON file with ' +
					'{ instructions: [{ ix, args, accounts }, ...] } — each entry is the ' +
					'`call` payload shape plus the camelCase ix name. Same rules as `call`: ' +
					'no PDA derivation, no implicit accounts.'
			)
			.option(
				'--dry-run',
				'print the instructions and expected proposal rent/fees, send nothing',
				false
			)
	).action(async (payloadFile: string, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { dryRun: boolean };
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, false);
		try {
			const payload = JSON.parse(fs.readFileSync(payloadFile, 'utf-8')) as {
				instructions?: Array<IxPayload & { ix: string }>;
			};
			if (!payload.instructions || payload.instructions.length === 0) {
				throw new Error('payload has no instructions');
			}
			const ixs = payload.instructions.map((entry) =>
				buildIxFromPayload(client, entry.ix, entry)
			);
			const names = payload.instructions.map((e) => e.ix).join(', ');
			const label = `batch ${ixs.length} ix(s): ${names}`;
			const memo = `velocity-admin batch: ${names}`;
			if (local.dryRun) {
				console.log(label);
				await reportDryRun(
					provider,
					ixs,
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					0,
					[],
					memo
				);
				return;
			}
			const result = await sendOrPropose(
				provider,
				ixs,
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				`velocity-admin batch: ${names}`
			);
			reportDispatch(label, result);
		} finally {
			if ((client as any).isSubscribed) {
				await client.unsubscribe();
			}
		}
	});
}

/** Best-effort coerce a JSON value into the runtime type Anchor expects. */
function coerceArg(value: unknown, type: unknown): unknown {
	if (value === undefined || value === null) {
		return value;
	}
	if (typeof type === 'string') {
		if (type === 'pubkey' || type === 'publicKey') {
			return new PublicKey(value as string);
		}
		if (type.startsWith('u') || type.startsWith('i')) {
			// u8/i8 fits in number; everything wider is BN.
			if (type === 'u8' || type === 'i8' || type === 'u16' || type === 'i16') {
				return Number(value);
			}
			return new BN(value as string | number);
		}
		if (type === 'bool') {
			return Boolean(value);
		}
		if (type === 'string') {
			return String(value);
		}
	}
	// Pass-through for vec/option/struct/enum — caller must already shape these.
	return value;
}
