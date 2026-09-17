import { Command } from 'commander';
import { CrankCostUnitsV0 } from '@velocity-exchange/sdk';

/**
 * Cost units to assume for a crank nobody has measured yet.
 *
 * The value is a ceiling on purpose. A keeper is paid the payment derived from
 * it to run the crank. A figure that is too low makes the crank cost the keeper
 * more than it earns, and nobody runs it. A figure that is too high makes the
 * reservoir overpay. Only the first outcome stops a market working, so the
 * default errs toward the second.
 *
 * The value is about what the widest fill this protocol measures requests. That
 * is a three-source router fill, plus the signature, the write locks and the
 * loaded-accounts limit that come with it. A real crank costs far less. Measure
 * each crank and re-run the attach with the real figures. The turner reports
 * the compute a crank burns, and the rest of the sum comes from the transaction
 * the turner assembles.
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

/**
 * Add `--crank-cu` and one override per crank to a command.
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
