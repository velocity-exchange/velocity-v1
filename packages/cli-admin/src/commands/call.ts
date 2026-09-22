import { Command } from 'commander';
import { BN } from '@coral-xyz/anchor';
import { PublicKey, TransactionInstruction } from '@solana/web3.js';
import * as fs from 'fs';
import * as path from 'path';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import {
	InstructionGroup,
	reportDispatch,
	reportDryRun,
	sendOrPropose,
	sendOrProposeBatch,
} from '../lib/squads';

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
	const idlArgs = idlIx.args as Array<{ name: string; type: unknown }>;

	// Validate the payload against the IDL before building. Anchor serializes a
	// missing numeric arg as 0, so a typo or a snake_case key ships a silent
	// zero. The Anchor client exposes IDL fields in camelCase, so
	// `initializePythLazerOracle` written with `feed_id` instead of `feedId`
	// proposed feed 0. This checks both directions, so neither a missing key
	// nor an unrecognised one reaches the chain.
	const supplied = Object.keys(payload.args ?? {});
	const known = new Set(idlArgs.map((a) => a.name));
	const unknownKeys = supplied.filter((k) => !known.has(k));
	if (unknownKeys.length > 0) {
		throw new Error(
			`${ixName}: unknown arg(s) ${unknownKeys.join(', ')}. ` +
				`Expected camelCase names: ${idlArgs.map((a) => a.name).join(', ')}`
		);
	}
	const missing = idlArgs
		.filter((a) => !isOptionType(a.type))
		.filter((a) => payload.args?.[a.name] === undefined)
		.map((a) => a.name);
	if (missing.length > 0) {
		throw new Error(
			`${ixName}: missing required arg(s) ${missing.join(', ')}. ` +
				`Expected camelCase names: ${idlArgs.map((a) => a.name).join(', ')}`
		);
	}

	const args = idlArgs.map((a) =>
		coerceArg(payload.args?.[a.name], a.type, client.program.idl)
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
			.command('propose-batch <payloadFiles...>')
			.description(
				'Propose several payload files as ONE Squads batch: one proposal, one ' +
					'approval round, one timelock, N inner transactions executed in order. ' +
					'Each file becomes one inner transaction, in the order given, and only ' +
					'each file has to fit the 1232-byte transaction limit, the total does ' +
					'not. Files use the `batch` payload shape, { instructions: [...] }. ' +
					'Use this instead of several `batch` calls when the actions belong to ' +
					'one change, such as listing a market. Requires --multisig.'
			)
	).action(async (payloadFiles: string[], _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, false);
		try {
			if (!opts.multisig) {
				throw new Error(
					'propose-batch builds a Squads batch, so it needs --multisig ' +
						'(or a profile that sets one). For a direct send use `batch`.'
				);
			}
			const groups: InstructionGroup[] = payloadFiles.map((file) => {
				const payload = JSON.parse(fs.readFileSync(file, 'utf-8')) as {
					instructions?: Array<IxPayload & { ix: string }>;
				};
				if (!payload.instructions || payload.instructions.length === 0) {
					throw new Error(`${file}: payload has no instructions`);
				}
				return {
					label: `${path.basename(file)} (${payload.instructions
						.map((e) => e.ix)
						.join(', ')})`,
					instructions: payload.instructions.map((entry) =>
						buildIxFromPayload(client, entry.ix, entry)
					),
				};
			});
			const total = groups.reduce((a, g) => a + g.instructions.length, 0);
			const label = `batch ${groups.length} transaction(s), ${total} ix(s)`;
			const result = await sendOrProposeBatch(
				provider,
				groups,
				new PublicKey(opts.multisig),
				`velocity-admin propose-batch: ${groups.length} tx, ${total} ix`
			);
			reportDispatch(label, result);
		} finally {
			if ((client as any).isSubscribed) {
				await client.unsubscribe();
			}
		}
	});

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
/** An `{ option: T }` arg may be absent. Every other arg may not. */
function isOptionType(type: unknown): boolean {
	return (
		typeof type === 'object' && type !== null && 'option' in (type as object)
	);
}

/**
 * Resolve a `{ defined: ... }` IDL type to its struct definition, if it is one.
 *
 * Anchor lowercases the first letter of type names when it loads an IDL, and
 * spells the reference as either a bare string or `{ name }` depending on
 * version, so both are tried.
 */
function resolveStruct(type: unknown, idl: any): any | undefined {
	const defined = (type as any)?.defined;
	if (!defined) {
		return undefined;
	}
	const name: string =
		typeof defined === 'string' ? defined : defined?.name ?? '';
	if (!name) {
		return undefined;
	}
	const alt = name.charAt(0).toLowerCase() + name.slice(1);
	const def = idl?.types?.find((t: any) => t.name === name || t.name === alt);
	return def?.type?.fields ? def : undefined;
}

function coerceArg(value: unknown, type: unknown, idl?: any): unknown {
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
	// A struct argument is coerced field by field against its IDL definition.
	// Without this a `u64` inside a params struct stays a JSON string and the
	// borsh encoder fails with `src.toArrayLike is not a function`, which says
	// nothing about which field was wrong.
	const struct = resolveStruct(type, idl);
	if (struct && value && typeof value === 'object') {
		const src = value as Record<string, unknown>;
		const out: Record<string, unknown> = {};
		for (const field of struct.type.fields) {
			out[field.name] = coerceArg(src[field.name], field.type, idl);
		}
		return out;
	}
	// Pass-through for vec/option/enum: caller must already shape these.
	return value;
}
