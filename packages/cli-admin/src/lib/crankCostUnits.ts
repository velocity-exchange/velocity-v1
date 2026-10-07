import { Command } from 'commander';
import { CrankCostUnitsV0 } from '@velocity-exchange/sdk';

/**
 * Ceiling cost units for a crank nobody has measured yet. Too low starves the
 * keeper payment, so the default errs high. It is sized to a three-source router fill.
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
	['refill', 'refill_crank_reservoir (paid by the crank treasury)'],
] as const;

/** Adds `--crank-cu` and one override per crank to a command. */
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
 * Reads the cost units the flags describe.
 * @returns one cost-unit figure per crank, ready to pass to `updatePerpMarketClobQuoter`.
 * @throws if any figure is not a positive integer. A zero payment is a crank
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
		refill: read('refill'),
	};
}

function parse(flag: string, raw: string | undefined): number {
	const value = Number.parseInt(raw ?? '', 10);
	if (!Number.isInteger(value) || value <= 0) {
		throw new Error(`${flag} must be a positive integer, got ${raw}`);
	}

	return value;
}
