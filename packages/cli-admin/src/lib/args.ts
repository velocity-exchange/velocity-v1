import { BN } from '@coral-xyz/anchor';

/**
 * Strict positional-argument parsers.
 *
 * `Number(x)` and `Number.parseInt(x, 10)` both accept malformed input without
 * an error. `Number('')` and `Number(' ')` are `0`. `parseInt('0x10', 10)` is
 * `0`, `parseInt('1abc', 10)` is `1`, and `parseInt('5.9', 10)` is `5`. `new
 * BN('')` is `0`, and `new BN(' ')` never terminates. On an admin CLI a misread
 * argument targets the wrong market or writes the wrong value. Every parser
 * here demands a full decimal-integer match and throws on anything else.
 */

const INTEGER = /^[+-]?\d+$/;
const UNSIGNED_INTEGER = /^\+?\d+$/;

/** Check a parsed `number` argument against its inclusive bounds. */
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
 * Parse a signed decimal integer argument. It rejects an empty string,
 * whitespace, hex, a float, an exponent, and any trailing text.
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
 * Parse an unsigned decimal integer argument of any width into a `BN`. Use it
 * for a u64 or u128 amount that overflows `number`.
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
