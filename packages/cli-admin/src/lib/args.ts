import { BN } from '@coral-xyz/anchor';

/**
 * Strict positional-argument parsers.
 *
 * `Number(x)` and `Number.parseInt(x, 10)` both silently accept junk on an
 * admin CLI where a misread argument targets the wrong market or writes the
 * wrong value: `Number('')` and `Number(' ')` are `0`, `parseInt('0x10', 10)`
 * is `0`, `parseInt('1abc', 10)` is `1`, `parseInt('5.9', 10)` is `5`. `new
 * BN('')` is `0` and `new BN(' ')` never terminates. Everything here demands a
 * full decimal-integer match and throws on anything else.
 */

const INTEGER = /^[+-]?\d+$/;
const UNSIGNED_INTEGER = /^\+?\d+$/;

/** Largest exactly-representable integer bound for a `number`-typed argument. */
function assertRange(
	name: string,
	raw: string,
	value: number,
	min: number,
	max: number
): number {
	if (value < min || value > max) {
		throw new Error(
			`${name} must be an integer in [${min}, ${max}], got "${raw}"`
		);
	}
	return value;
}

/**
 * Parse a signed decimal integer argument, rejecting empty strings, whitespace,
 * hex, floats, exponents, and trailing garbage.
 * @param name - Argument name, used in the error message.
 * @param raw - Raw argv string.
 * @param min - Inclusive lower bound.
 * @param max - Inclusive upper bound.
 */
export function parseIntArg(
	name: string,
	raw: string,
	min: number,
	max: number
): number {
	if (!INTEGER.test(raw)) {
		throw new Error(`${name} must be a decimal integer, got "${raw}"`);
	}
	const value = Number(raw);
	if (!Number.isSafeInteger(value)) {
		throw new Error(`${name} is out of safe integer range, got "${raw}"`);
	}
	return assertRange(name, raw, value, min, max);
}

/** Parse a `<market>` positional into a u16 perp market index. */
export function parseMarketIndex(raw: string): number {
	return parseIntArg('market', raw, 0, 65535);
}

/**
 * Parse a `<markets>` positional: one index, a comma-separated list (`0,1,4`),
 * or `all`. Returns `'all'` for the caller to expand against onchain state.
 * Duplicates are rejected, since they would write the same market twice.
 */
export function parseMarketList(raw: string): number[] | 'all' {
	if (raw === 'all') {
		return 'all';
	}
	const indexes = raw.split(',').map(parseMarketIndex);
	if (new Set(indexes).size !== indexes.length) {
		throw new Error(`markets must not repeat an index, got "${raw}"`);
	}
	return indexes;
}

/**
 * Parse an unsigned decimal integer argument of arbitrary width into a `BN`
 * (for u64/u128 amounts that overflow `number`).
 * @param name - Argument name, used in the error message.
 * @param raw - Raw argv string.
 */
export function parseBnArg(name: string, raw: string): BN {
	if (!UNSIGNED_INTEGER.test(raw)) {
		throw new Error(
			`${name} must be an unsigned decimal integer, got "${raw}"`
		);
	}
	return new BN(raw, 10);
}
