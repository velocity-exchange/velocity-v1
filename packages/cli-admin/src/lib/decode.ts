import { PublicKey, TransactionInstruction } from '@solana/web3.js';
import { BorshInstructionCoder } from '@coral-xyz/anchor';
import pc from 'picocolors';
import * as ui from './ui';
import velocityIdl from '@velocity-exchange/sdk/src/idl/velocity.json';

/**
 * Shared decoding for admin instructions.
 *
 * `multisig inspect` decodes a proposal that already exists on chain. `--dry-run`
 * needs the same rendering before anything is proposed, so a wrong argument shows
 * up while it is still free to fix. An `initializePythLazerOracle` built with a
 * snake_case `feed_id` key serialized feedId 0, and nothing revealed that until
 * the proposal was already up. Both paths use these helpers.
 */

function formatValue(value: unknown): string {
	return ui.safe(renderValue(value));
}

function renderValue(value: unknown): string {
	if (value === null || value === undefined) {
		return String(value);
	}
	if (value instanceof PublicKey) {
		return value.toBase58();
	}
	if (Buffer.isBuffer(value)) {
		return `0x${value.toString('hex')}`;
	}
	if (typeof value === 'object') {
		const obj = value as Record<string, unknown>;
		// Anchor renders a unit enum variant as { variantName: {} }.
		const keys = Object.keys(obj);
		if (
			keys.length === 1 &&
			typeof obj[keys[0]] === 'object' &&
			obj[keys[0]] !== null &&
			Object.keys(obj[keys[0]] as object).length === 0
		) {
			return keys[0];
		}
		if (typeof (obj as { toString?: unknown }).toString === 'function') {
			const s = String(value);
			if (s !== '[object Object]') {
				return s;
			}
		}
		return JSON.stringify(value);
	}
	return String(value);
}

/** True for values that render as one line rather than being walked into. */
function isLeaf(value: unknown): boolean {
	if (value === null || value === undefined) {
		return true;
	}
	if (typeof value !== 'object') {
		return true;
	}
	if (value instanceof PublicKey || Buffer.isBuffer(value)) {
		return true;
	}
	// BN and friends: objects that stringify to something meaningful.
	return (
		!Array.isArray(value) &&
		String(value) !== '[object Object]' &&
		Object.keys(value).length > 0 &&
		formatValue(value) !== JSON.stringify(value)
	);
}

/** Flatten a decoded struct to `path: value` lines, deepest field last. */
function flatten(
	value: unknown,
	prefix = ''
): { path: string; value: string }[] {
	if (isLeaf(value)) {
		return [{ path: prefix || 'value', value: formatValue(value) }];
	}
	const out: { path: string; value: string }[] = [];
	const entries = Array.isArray(value)
		? value.map((v, i) => [String(i), v] as const)
		: Object.entries(value as Record<string, unknown>);
	for (const [key, child] of entries) {
		out.push(
			...flatten(child, prefix ? `${key}` : key).map((e) => ({
				path: prefix ? `${prefix}.${e.path}` : e.path,
				value: e.value,
			}))
		);
	}
	return out;
}
/**
 * The JSON IDL names fields in snake_case. The Anchor client uses camelCase, and
 * so do the payload files and `multisig inspect`. Render camelCase so the dry run
 * shows the names a payload has to use.
 */
function camel(name: string): string {
	return name.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
}

let coder: BorshInstructionCoder | undefined;
function velocityCoder(): BorshInstructionCoder {
	if (!coder) {
		coder = new BorshInstructionCoder(velocityIdl as never);
	}
	return coder;
}

const VELOCITY_PROGRAM_ID = new PublicKey(
	(velocityIdl as { address: string }).address
);

/**
 * Print each instruction as its decoded name, arguments and named accounts.
 * An instruction this program cannot decode prints as undecodable rather than
 * dropping out of the output.
 */
export function renderInstructions(
	instructions: TransactionInstruction[],
	raw = false
): void {
	instructions.forEach((ix, i) => {
		const step = pc.dim(`${i + 1}.`);
		const isVelocity = ix.programId.equals(VELOCITY_PROGRAM_ID);
		const decoded = isVelocity ? velocityCoder().decode(ix.data) : null;
		if (decoded) {
			ui.line(`${step} ${pc.dim('velocity')} ${pc.bold(camel(decoded.name))}`);
		} else {
			ui.line(
				`${step} ${pc.yellow('cannot decode')} ${pc.dim(
					isVelocity ? 'unknown discriminator' : ix.programId.toBase58()
				)}`
			);
		}

		// Print the account count on every instruction. The Squads executor
		// allocates a fixed buffer to reconstruct the inner transaction, and a
		// large multi-hop swap route overflows it. Routes with 83 and 85 accounts
		// failed with an access violation where a 49-account route executed fine.
		// That count is the only warning, and the instructions this decoder
		// cannot read are the ones most likely to carry it.
		const count = `${ix.keys.length} account${ix.keys.length === 1 ? '' : 's'}`;
		ui.line(
			`   ${pc.dim(count)}${
				ix.keys.length >= 60 ? pc.yellow('  large route, may overflow') : ''
			}`
		);

		const rows: string[][] = [];
		if (decoded) {
			const args = flatten(decoded.data);
			const shown = raw ? args : args.slice(0, 20);
			for (const { path, value } of shown) {
				rows.push([pc.dim(camel(path)), pc.bold(value)]);
			}
			if (args.length > shown.length) {
				rows.push([
					pc.dim(`+${args.length - shown.length} more fields`),
					pc.dim('--raw for all'),
				]);
			}
			const idlIx = (
				velocityIdl as {
					instructions: Array<{ name: string; accounts: { name: string }[] }>;
				}
			).instructions.find((e) => e.name === decoded.name);
			(idlIx?.accounts ?? []).forEach((acc, n) => {
				const key = ix.keys[n];
				if (!key) {
					return;
				}
				const marks = [
					key.isSigner ? pc.yellow('signer') : pc.dim('·'),
					key.isWritable ? pc.yellow('writable') : pc.dim('read-only'),
				].join(' ');
				rows.push([
					pc.dim(camel(acc.name)),
					`${marks}  ${key.pubkey.toBase58()}`,
				]);
			});
		}
		if (raw || !decoded) {
			rows.push([
				pc.dim(`raw data (${ix.data.length}B)`),
				pc.dim(ix.data.toString('hex')),
			]);
		}
		if (rows.length > 0) {
			ui.table(rows, '      ');
		}
	});
}

export { flatten };
