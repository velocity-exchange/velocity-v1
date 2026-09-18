import { BN } from '@coral-xyz/anchor';

/**
 * Strict positional-argument parsers. `Number`, `parseInt`, and `new BN` accept
 * malformed input silently, so every parser here demands a full decimal-integer match.
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
 * Parse a signed decimal integer argument. Rejects an empty string, whitespace,
 * hex, a float, an exponent, and any trailing text.
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
 */
export function parseBnArg(name: string, raw: string): BN {
	if (!UNSIGNED_INTEGER.test(raw)) {
		throw new Error(
			`${name} must be an unsigned decimal integer, got "${raw}"`
		);
	}
	return new BN(raw, 10);
}
