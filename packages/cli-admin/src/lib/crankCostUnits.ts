import { Command } from 'commander';
import { CrankCostUnitsV0 } from '@velocity-exchange/sdk';

/**
 * Cost units to assume for a crank nobody has measured yet.
 *
 * Deliberately a ceiling. The payment derived from it is what a keeper is paid
 * to run the crank, so too low means the crank costs the keeper more than it
 * earns and nobody runs it; too high means the reservoir overpays. Only one of
 * those stops a market working, so the default errs the other way.
 *
 * Roughly what the widest fill this protocol measures requests: a three-source
 * router fill, plus the signature, write locks and loaded-accounts limit that
 * come with it. A real crank is far below it. Measure each one — the turner
 * reports the compute a crank burns, and the rest of the sum falls out of the
 * transaction it assembles — and re-run the attach with the real figures.
 */
export const CRANK_COST_UNITS_CEILING = 250_000;

/** Per-crank flag names, in the order the on-chain struct declares them. */
const CRANKS = [
	['removal', 'evict_worst / remove_expired'],
	['cross', 'crank_cross_match'],
	['taker-origin-cross', 'crank_taker_origin_cross'],
	['trigger', 'trigger_order / trigger_clob_order'],
	['liquidation', 'liquidate_perp_with_fill'],
	['force-cancel', 'force_cancel_clob_orders'],
] as const;

/**
 * Add `--crank-cu` and its six per-crank overrides to a command.
 *
 * @param command - the command to extend.
 * @returns the same command, for chaining.
 */
export function withCrankCostUnitOptions(command: Command): Command {
	command.option(
		'--crank-cu <n>',
		`cost units every crank requests, unless overridden below. A crank's keeper payment is derived from this and State.transactionFeeRails, so measure it rather than taking the default ceiling`,
		String(CRANK_COST_UNITS_CEILING)
	);
	for (const [flag, ixs] of CRANKS) {
		command.option(`--crank-cu-${flag} <n>`, `cost units for ${ixs}`);
	}
	return command;
}

/** camelCase key on the parsed flags object for a `--crank-cu-<flag>` option. */
function flagKey(flag: string): string {
	const camel = flag.replace(/-([a-z])/g, (_, c: string) => c.toUpperCase());
	return `crankCu${camel[0].toUpperCase()}${camel.slice(1)}`;
}

/**
 * Read the cost units the flags describe.
 *
 * @param flags - the command's parsed options.
 * @returns one cost-unit figure per crank, ready to pass to
 * `updatePerpMarketClobQuoter`.
 * @throws if any figure is not a positive integer — a zero payment is a crank
 * no turner takes, and the program refuses it.
 */
export function readCrankCostUnits(
	flags: Record<string, string | undefined>
): CrankCostUnitsV0 {
	const shared = parse('--crank-cu', flags.crankCu);
	const read = (flag: string) => {
		const raw = flags[flagKey(flag)];
		return raw === undefined ? shared : parse(`--crank-cu-${flag}`, raw);
	};
	return {
		removal: read('removal'),
		cross: read('cross'),
		takerOriginCross: read('taker-origin-cross'),
		trigger: read('trigger'),
		liquidation: read('liquidation'),
		forceCancel: read('force-cancel'),
	};
}

function parse(flag: string, raw: string | undefined): number {
	const value = Number.parseInt(raw ?? '', 10);
	if (!Number.isInteger(value) || value <= 0) {
		throw new Error(`${flag} must be a positive integer, got ${raw}`);
	}
	return value;
}
