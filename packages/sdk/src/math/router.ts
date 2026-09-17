/**
 * Router split. This is the TypeScript mirror of
 * `programs/velocity/src/math/router.rs`.
 *
 * The on-chain router combines every quoter's book into per-quoter allocations
 * for a taker of `direction` and `size`. A client must reproduce it exactly to
 * predict a fill. At each price the priority tiers fill in ascending order, the
 * vAMM first, then the CLOB, then the customs. Books that share a tier fill
 * pro rata.
 *
 * Divergence between this file and `math/router.rs` makes the SDK mispredict
 * fills, slippage and entry prices. When the Rust changes, change this file in
 * the same pull request.
 */

import { BN } from '@coral-xyz/anchor';
import { PositionDirection } from '../types';
import { isVariant } from '../types';
import {
	BASE_PRECISION,
	MARGIN_PRECISION,
	ZERO,
} from '../constants/numericConstants';

/** Levels read per book. The walk ignores the rest. Mirrors `MAX_LEVELS_PER_BOOK`. */
export const MAX_LEVELS_PER_BOOK = 128;

/** One level of a quoter's book. It is the size available at a price. */
export type RouterPriceLevel = {
	/** PRICE_PRECISION */
	price: BN;
	/** base precision */
	size: BN;
};

/** A quoter's book and its routing tier. A lower tier fills first at a price. */
export type RouterQuoterBook = {
	priority: number;
	/** Best price first: ascending for a long taker, descending for a short taker. */
	levels: RouterPriceLevel[];
	/**
	 * Depth the quoter says it holds at a better price than it quoted, and could
	 * not offer because the accounts of the user who owns it are not in the
	 * transaction. `quote_v0` returns it as `withheld`, one price level behind
	 * the ladder.
	 *
	 * Nobody can fill it, so it never belongs in `levels` and takes no part of
	 * the split. It reports that the transaction omitted a maker the book
	 * wanted. On a fill the taker did not sign, the program then checks whether
	 * the filler had room to carry that maker.
	 */
	withheld?: RouterPriceLevel;
};

/** What the split routes to one quoter. */
export type RouterAllocation = {
	/** Base routed to this quoter, a `stepSize` multiple. */
	base: BN;
	/** Quote notional at the quoted levels. Each level rounds toward the taker. */
	quote: BN;
	/**
	 * The sum of `price * base` over the levels this allocation was cut from,
	 * before the single division into quote units. It mirrors
	 * `QuoterAllocation::scaled_quote`.
	 *
	 * The program holds the execute leg to this scalar in
	 * `validate_allocated_notional`. A client that reproduces the route needs it
	 * to predict whether a fill is accepted. The split accrues it while it walks
	 * the ladder, so nothing downstream reads the levels again.
	 */
	scaledQuote: BN;
};

/** Default tiers by quoter type. They mirror `QuoterType::default_priority`. */
export const VAMM_PRIORITY = 0;
export const CLOB_PRIORITY = 10;
export const CUSTOM_PRIORITY = 20;

/** Floor `value` to a multiple of `step`. */
function floorToStep(value: BN, step: BN): BN {
	return value.sub(value.mod(step));
}

/**
 * The notional rounded toward the bound that is safe for the taker. It mirrors
 * `math::router::quote_notional`. A long taker pays at most the quote, so the
 * notional rounds up. A short taker receives at least the quote, so it rounds
 * down.
 */
function quoteNotional(direction: PositionDirection, price: BN, base: BN): BN {
	const exact = price.mul(base);
	if (isVariant(direction, 'long')) {
		return exact.add(BASE_PRECISION).subn(1).div(BASE_PRECISION);
	}
	return exact.div(BASE_PRECISION);
}

/**
 * One book's sanitized read cursor. It mirrors the Rust `Cursor`. It skips a
 * degenerate level, truncates the book at its first non-monotone level, and
 * stops at `MAX_LEVELS_PER_BOOK`. Truncation keeps an untrusted quoter to its
 * own book. Reported availability is quantized to `step`, because an allocation
 * must be a step multiple and a level's sub-step tail is dust nobody can fill.
 */
class Cursor {
	index = 0;
	consumed: BN = ZERO;

	constructor(
		readonly priority: number,
		readonly levels: RouterPriceLevel[],
		readonly step: BN
	) {}

	peek(direction: PositionDirection): [BN, BN] | undefined {
		const cap = Math.min(this.levels.length, MAX_LEVELS_PER_BOOK);
		while (this.index < cap) {
			const level = this.levels[this.index];
			const available = BN.max(level.size.sub(this.consumed), ZERO);
			const usable = floorToStep(available, this.step);
			const degenerate = level.price.lte(ZERO) || usable.eq(ZERO);
			if (this.index > 0) {
				const prev = this.levels[this.index - 1].price;
				const outOfOrder = isVariant(direction, 'long')
					? level.price.lt(prev)
					: level.price.gt(prev);
				if (outOfOrder) {
					this.index = this.levels.length;
					return undefined;
				}
			}
			if (degenerate) {
				this.index += 1;
				this.consumed = ZERO;
				continue;
			}
			return [level.price, usable];
		}
		return undefined;
	}

	consume(amount: BN): void {
		this.consumed = this.consumed.add(amount);
	}
}

/**
 * Split `takerSize` across the books in `stepSize` quanta. It mirrors
 * `split_across_quoters`. It returns one allocation per book, in the order
 * given. The allocated base sums to `min(takerSize, usable depth)`, floored to
 * the step.
 *
 * A book's `withheld` report takes no part of the division. The taker asked to
 * trade, so the size goes to the sources that can fill it.
 *
 * @param direction taker direction
 * @param takerSize base the taker wants filled
 * @param books one per quoter, best price first
 * @param stepSize the market's `orderStepSize`
 */
export function splitAcrossQuoters(
	direction: PositionDirection,
	takerSize: BN,
	books: RouterQuoterBook[],
	stepSize: BN
): RouterAllocation[] {
	if (books.length === 0) {
		throw new Error('router split needs at least one book');
	}
	const step = stepSize.gt(ZERO) ? stepSize : new BN(1);
	const cursors = books.map(
		(book) => new Cursor(book.priority, book.levels, step)
	);
	const allocations: RouterAllocation[] = cursors.map(() => ({
		base: ZERO,
		quote: ZERO,
		scaledQuote: ZERO,
	}));
	let remaining = takerSize;

	const take = (
		i: number,
		price: BN,
		amount: BN,
		tops: ([BN, BN] | undefined)[]
	) => {
		cursors[i].consume(amount);
		const top = tops[i];
		tops[i] =
			top && top[1].gt(amount) ? [top[0], top[1].sub(amount)] : undefined;
		allocations[i].base = allocations[i].base.add(amount);
		allocations[i].quote = allocations[i].quote.add(
			quoteNotional(direction, price, amount)
		);
		allocations[i].scaledQuote = allocations[i].scaledQuote.add(
			price.mul(amount)
		);
	};

	while (remaining.gt(ZERO)) {
		// One peek per book per price round. Consumption updates the cached top
		// in place rather than walking the levels again.
		const tops: ([BN, BN] | undefined)[] = cursors.map((c) =>
			c.peek(direction)
		);
		const live = tops.filter((t): t is [BN, BN] => t !== undefined);
		if (live.length === 0) {
			break;
		}
		const price = live.reduce(
			(best, [p]) =>
				isVariant(direction, 'long') ? BN.min(best, p) : BN.max(best, p),
			live[0][0]
		);

		// Priority tiers quoting this price, ascending. A tier fills pro rata.
		const tiers = Array.from(
			new Set(
				cursors
					.map((cursor, i) => {
						const top = tops[i];
						return top && top[0].eq(price) ? cursor.priority : undefined;
					})
					.filter((t): t is number => t !== undefined)
			)
		).sort((a, b) => a - b);

		const availableAt = (i: number, tier: number): BN | undefined => {
			const top = tops[i];
			if (cursors[i].priority !== tier || !top || !top[0].eq(price)) {
				return undefined;
			}
			return top[1];
		};

		for (const tier of tiers) {
			if (remaining.eq(ZERO)) {
				break;
			}
			let total = ZERO;
			for (let i = 0; i < cursors.length; i++) {
				const available = availableAt(i, tier);
				if (available) {
					total = total.add(available);
				}
			}
			if (total.eq(ZERO)) {
				continue;
			}
			const demand = floorToStep(BN.min(remaining, total), step);
			if (demand.eq(ZERO)) {
				// Nothing step-sized left to give at this tier.
				remaining = ZERO;
				break;
			}
			let given = ZERO;
			for (let i = 0; i < cursors.length; i++) {
				const available = availableAt(i, tier);
				if (!available) {
					continue;
				}
				const share = floorToStep(demand.mul(available).div(total), step);
				if (share.eq(ZERO)) {
					continue;
				}
				take(i, price, share, tops);
				given = given.add(share);
			}
			// Floor-division dust: hand it to the first quoter with spare depth.
			let dust = demand.sub(given);
			for (let i = 0; i < cursors.length && dust.gt(ZERO); i++) {
				const available = availableAt(i, tier);
				if (!available) {
					continue;
				}
				const amount = floorToStep(BN.min(dust, available), step);
				if (amount.eq(ZERO)) {
					continue;
				}
				take(i, price, amount, tops);
				dust = dust.sub(amount);
			}
			remaining = remaining.sub(demand);
		}
	}

	return allocations;
}

/**
 * Whether velocity routes a `quote_v0` response at all. It mirrors
 * `validate_quoted_levels`. Every price and every size must be nonzero. Prices
 * must run best-first for the taker's direction, non-strictly. Equal
 * consecutive prices are legal, because a ladder's rungs come from distinct
 * offsets that can round to the same tick.
 *
 * On chain this rejects the fill rather than truncating the book. A client that
 * predicts a fill must apply it to any book it publishes or consumes.
 */
export function areQuotedLevelsValid(
	direction: PositionDirection,
	levels: RouterPriceLevel[]
): boolean {
	let previous: BN | undefined;
	for (const level of levels) {
		if (level.price.lte(ZERO) || level.size.lte(ZERO)) {
			return false;
		}
		if (previous) {
			const ordered = isVariant(direction, 'long')
				? level.price.gte(previous)
				: level.price.lte(previous);
			if (!ordered) {
				return false;
			}
		}
		previous = level.price;
	}
	return true;
}

/**
 * The prices a quoter committed to for the best-priced `base` units of the book
 * it quoted. An execute of that size is held to them. It mirrors
 * `QuotedPrefix`.
 */
export type RouterQuotedPrefix = {
	/** The sum of `price * base` over the prefix, before the division by `BASE_PRECISION`. */
	scaledQuote: BN;
	/** The prefix's first price, which is the best one for the taker. */
	bestPrice: BN;
	/** The prefix's last price. It is the worst price any unit in it was quoted at. */
	worstPrice: BN;
};

/**
 * Walk `levels` best-first for `base` units and price them at the quoted
 * levels. It mirrors `quoted_prefix`. It applies the same step quantization the
 * split does, so the prefix is the one the split allocated from.
 *
 * It returns `undefined` when the levels cannot cover `base`. On chain that is
 * a quoter filling more than it quoted, and it fails with `QuoterOverfilled`.
 */
export function quotedPrefix(
	levels: RouterPriceLevel[],
	stepSize: BN,
	base: BN
): RouterQuotedPrefix | undefined {
	const step = stepSize.gt(ZERO) ? stepSize : new BN(1);
	let remaining = base;
	let scaledQuote = ZERO;
	let bestPrice: BN | undefined;
	let worstPrice = ZERO;
	for (const level of levels.slice(0, MAX_LEVELS_PER_BOOK)) {
		if (remaining.eq(ZERO)) {
			break;
		}
		const usable = floorToStep(level.size, step);
		if (usable.eq(ZERO)) {
			continue;
		}
		const take = BN.min(remaining, usable);
		scaledQuote = scaledQuote.add(level.price.mul(take));
		bestPrice = bestPrice ?? level.price;
		worstPrice = level.price;
		remaining = remaining.sub(take);
	}
	if (!remaining.eq(ZERO)) {
		return undefined;
	}
	return { scaledQuote, bestPrice: bestPrice ?? ZERO, worstPrice };
}

/**
 * Whether `quote` lies in `[lo, hi]` after both bounds divide by
 * `BASE_PRECISION`. Each bound admits the one rounding step the division cannot
 * avoid, plus `slack` further quote units. It mirrors `notional_within`.
 */
function notionalWithin(lo: BN, hi: BN, quote: BN, slack: BN): boolean {
	const floor = BN.max(lo.div(BASE_PRECISION).sub(slack), ZERO);
	const ceil = hi.add(BASE_PRECISION).subn(1).div(BASE_PRECISION).add(slack);
	return quote.gte(floor) && quote.lte(ceil);
}

/**
 * Whether an external quoter's executed `(base, quote)` is inside the quote it
 * gave in the same transaction. This check decides whether a router fill lands.
 *
 * The upper bound is the notional of the best-priced `base` units of the
 * allocation. That is what every unit at the price quoted for that unit means
 * for a partial fill. The lower bound is every unit at the prefix's best price.
 * Without it a response could pay its makers nothing. Rounding is admitted
 * exactly once, for the single division into quote units. A quoter whose
 * encoder divides per fill must carry the remainder across them.
 *
 * The program's `validate_executed_notional` is stricter than this band. It
 * requires `quote` to equal `prefix.scaledQuote` divided by `BASE_PRECISION`,
 * so this band accepts a fill the program rejects.
 */
export function isExecutedNotionalInQuote(
	prefix: RouterQuotedPrefix,
	base: BN,
	quote: BN
): boolean {
	const atBest = prefix.bestPrice.mul(base);
	return notionalWithin(
		BN.min(atBest, prefix.scaledQuote),
		BN.max(atBest, prefix.scaledQuote),
		quote,
		ZERO
	);
}

/**
 * The same band applied to one balance change. It mirrors
 * `validate_change_notional`. Every unit of a change must price inside the
 * quoted prefix's range. The aggregate bound alone would let a quoter overpay
 * one maker out of another maker's pocket.
 *
 * `orders` is how many of the quoter's own orders the change merges. For a CLOB
 * that is `completedOrderIds.length + 1`. A merged record cannot be exact even
 * when the response total is, so each merged order admits one quote unit of
 * slack. The program also caps `orders` at `MAX_LEVELS_PER_BOOK`, which this
 * function does not.
 */
export function isChangeNotionalInQuote(
	prefix: RouterQuotedPrefix,
	base: BN,
	quote: BN,
	orders: BN
): boolean {
	const atBest = prefix.bestPrice.mul(base);
	const atWorst = prefix.worstPrice.mul(base);
	return notionalWithin(
		BN.min(atBest, atWorst),
		BN.max(atBest, atWorst),
		quote,
		orders
	);
}

/**
 * How far from oracle a fill on a quoter entry may price, in MARGIN_PRECISION
 * units. It mirrors `QuoterConfigV0::oracle_band`.
 *
 * The market's `marginRatioInitial` is the ceiling. A maker's declaration can
 * only bring the band in, which is what makes the declaration safe to take from
 * the maker rather than from the admin.
 */
export function quoterOracleBand(
	maxOracleDeviationBps: number,
	marketMarginRatioInitial: number
): number {
	return maxOracleDeviationBps === 0
		? marketMarginRatioInitial
		: Math.min(maxOracleDeviationBps, marketMarginRatioInitial);
}

/**
 * Whether a maker filled at `price` sits outside `band` of `oraclePrice`. It
 * mirrors `limit_price_breaches_maker_oracle_price_bands`, which every external
 * quoter leg is held to.
 *
 * The bound is one-sided, in the maker's favour. A maker buying below oracle or
 * selling above it never breaches. Only the direction that moves value off the
 * maker is bounded, and that is the direction a compromised quoter moves it.
 *
 * `makerDirection` is the maker's side, so it is the opposite of the taker's.
 */
export function makerPriceBreachesOracleBand(
	price: BN,
	makerDirection: PositionDirection,
	oraclePrice: BN,
	band: number
): boolean {
	const oracle = oraclePrice.abs();
	if (oracle.isZero()) {
		return false;
	}
	const diff = isVariant(makerDirection, 'long')
		? price.sub(oracle)
		: oracle.sub(price);
	if (diff.lte(ZERO)) {
		return false;
	}
	return diff.mul(MARGIN_PRECISION).div(oracle).gten(band);
}

/**
 * Whether a book's report of `base` filled or removed for a maker is inside
 * what velocity reserved for that maker. It mirrors the
 * `PerpPosition::reserved_open_base` bound every external unwind is held to.
 *
 * `openBids` and `openAsks` come from the maker's `PerpPosition`. Placement
 * writes them under the owner's signature, so a quoter cannot inflate them. A
 * report above them fails the fill on chain.
 */
export function isReportWithinReservation(
	openBids: BN,
	openAsks: BN,
	makerDirection: PositionDirection,
	base: BN
): boolean {
	const reserved = isVariant(makerDirection, 'long')
		? BN.max(openBids, ZERO)
		: BN.min(openAsks, ZERO).abs();
	return base.lte(reserved);
}
