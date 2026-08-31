/**
 * Router split — TypeScript mirror of `programs/velocity/src/math/router.rs`.
 *
 * The on-chain router combines every quoter's book into per-quoter allocations
 * for a taker of `direction`/`size`. Clients have to reproduce it exactly to
 * predict a fill: at each price, priority tiers fill in ascending order (vAMM,
 * then CLOB, then customs), pro rata within a tier.
 *
 * Divergence between this file and `math/router.rs` is a bug — it makes the
 * SDK mispredict fills, slippage, and entry prices. When the Rust changes,
 * change this in the same PR.
 */

import { BN } from '@coral-xyz/anchor';
import { PositionDirection } from '../types';
import { isVariant } from '../types';
import {
	BASE_PRECISION,
	MARGIN_PRECISION,
	ZERO,
} from '../constants/numericConstants';

/** Levels processed per book; anything past this is ignored (Rust: `MAX_LEVELS_PER_BOOK`). */
export const MAX_LEVELS_PER_BOOK = 128;

/** One level of a quoter's book: size available at a price. */
export type RouterPriceLevel = {
	/** PRICE_PRECISION */
	price: BN;
	/** base precision */
	size: BN;
};

/** A quoter's book plus its routing tier (lower fills first at a price). */
export type RouterQuoterBook = {
	priority: number;
	/** Best price first: ascending for a long taker, descending for a short taker. */
	levels: RouterPriceLevel[];
	/**
	 * Depth the quoter says it holds at a better price than it quoted, and
	 * could not offer because the accounts of the user who owns it are not in
	 * the transaction. `quote_v0` returns it as `withheld`, one price level
	 * behind the ladder.
	 *
	 * Not fillable, so it never belongs in `levels`, and it takes no part of the
	 * split. It reports that the transaction omitted a maker the book wanted. On
	 * a fill the taker did not sign, the program then checks whether the filler
	 * had room to carry that maker.
	 */
	withheld?: RouterPriceLevel;
};

/** What the split routes to one quoter. */
export type RouterAllocation = {
	/** Base routed to this quoter, a `stepSize` multiple. */
	base: BN;
	/** Quote notional at the quoted levels, rounded taker-conservatively per level. */
	quote: BN;
	/**
	 * `Σ price · base` over the levels this allocation was cut from, before the single division into
	 * quote units — mirroring `QuoterAllocation::scaled_quote`.
	 *
	 * This is the scalar the program holds the execute leg to (`validate_allocated_notional`), so a
	 * client reproducing the route needs it to predict whether a fill will be accepted. Accrued while
	 * the split already walks the ladder, so nothing downstream reads the levels again.
	 */
	scaledQuote: BN;
};

/** Default tiers by quoter type, mirroring `QuoterType::default_priority`. */
export const VAMM_PRIORITY = 0;
export const CLOB_PRIORITY = 10;
export const CUSTOM_PRIORITY = 20;

/** Floor `value` to a multiple of `step`. */
function floorToStep(value: BN, step: BN): BN {
	return value.sub(value.mod(step));
}

/**
 * Rounded toward the taker-conservative bound, mirroring
 * `math::router::quote_notional`: a long taker is bounded above (pays at most
 * the quote) so the notional ceils; a short taker is bounded below (receives
 * at least the quote) so it floors.
 */
function quoteNotional(direction: PositionDirection, price: BN, base: BN): BN {
	const exact = price.mul(base);
	if (isVariant(direction, 'long')) {
		return exact.add(BASE_PRECISION).subn(1).div(BASE_PRECISION);
	}
	return exact.div(BASE_PRECISION);
}

/**
 * One book's sanitized read cursor. Mirrors the Rust `Cursor`: it skips
 * degenerate levels, truncates the book at its first non-monotone level (an
 * untrusted quoter only truncates its own book), caps at
 * `MAX_LEVELS_PER_BOOK`, and reports availability quantized to `step` —
 * allocations must be step multiples, so a level's sub-step tail is
 * unfillable dust.
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
 * Split `takerSize` across the books in `stepSize` quanta — the mirror of
 * `split_across_quoters`. Returns one allocation per book, in the order given;
 * allocated base sums to `min(takerSize, usable depth)` floored to the step.
 *
 * @param direction taker direction
 * @param takerSize base the taker wants filled
 * @param books one per quoter, best price first
 * @param stepSize the market's `orderStepSize`
 *
 * A book's `withheld` report takes no part of the division. The taker asked to
 * trade, so the size goes to the sources that can fill it.
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
		// One peek per book per price round; consumption updates the cached top
		// in place rather than re-walking levels.
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

		// Priority tiers quoting this price, ascending; pro rata within a tier.
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
 * Whether a `quote_v0` response is one velocity will route at all — the mirror
 * of `validate_quoted_levels`. Every price and size must be nonzero, and
 * prices must run best-first for the taker's direction, non-strictly (equal
 * consecutive prices are legal: a ladder's rungs come from distinct offsets
 * that can round to the same tick).
 *
 * On-chain this rejects the fill rather than truncating the book, so a client
 * predicting a fill must apply it to any book it publishes or consumes.
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
 * it quoted — what an execute of that size is held to. Mirrors `QuotedPrefix`.
 */
export type RouterQuotedPrefix = {
	/** `Σ price × base` over the prefix, *before* dividing by `BASE_PRECISION`. */
	scaledQuote: BN;
	/** The prefix's first (taker-favourable) price. */
	bestPrice: BN;
	/** The prefix's last price — the worst any unit in it was quoted at. */
	worstPrice: BN;
};

/**
 * Walk `levels` best-first for `base` units and price them at the quoted
 * levels — the mirror of `quoted_prefix`. Applies the same step quantization
 * the split does, so the prefix is the one the split allocated from.
 *
 * Returns `undefined` when the levels can't cover `base`, which on-chain is a
 * quoter filling more than it quoted (`QuoterOverfilled`).
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

/** `quote` lies in `[lo, hi]` after dividing both by `BASE_PRECISION`, with the
 * one unavoidable rounding step admitted at each end plus `slack` further quote
 * units either side. Mirrors `notional_within`. */
function notionalWithin(lo: BN, hi: BN, quote: BN, slack: BN): boolean {
	const floor = BN.max(lo.div(BASE_PRECISION).sub(slack), ZERO);
	const ceil = hi.add(BASE_PRECISION).subn(1).div(BASE_PRECISION).add(slack);
	return quote.gte(floor) && quote.lte(ceil);
}

/**
 * Whether an external quoter's executed `(base, quote)` is inside the quote it
 * gave in the same transaction — the mirror of `validate_executed_notional`,
 * and the check that decides whether a router fill lands at all.
 *
 * The upper bound is the notional of the best-priced `base` units of the
 * allocation (what "every unit at the price quoted for that unit" means for a
 * partial fill); the lower bound is every unit at the prefix's best price
 * (without it a response could pay its makers nothing). Rounding is admitted
 * exactly once, for the single division into quote units — a quoter whose
 * encoder divides per fill must carry the remainder across them.
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
 * The same band applied to one balance change — the mirror of
 * `validate_change_notional`. Every unit of a change must be priced inside the
 * quoted prefix's range; the aggregate bound alone would let a quoter overpay
 * one maker out of another's pocket.
 *
 * `orders` is how many of the quoter's own orders the change merges (for a
 * CLOB, `completedOrderIds.length + 1`): a merged record can't be exact even
 * when the response total is, so one quote unit of slack per merged order is
 * admitted.
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
 * units — the mirror of `QuoterV0::oracle_band`.
 *
 * The market's `marginRatioInitial` is the ceiling. A maker's declaration can
 * only bring it in, which is what makes the declaration safe to take from the
 * maker rather than from the admin.
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
 * Whether a maker filled at `price` sits inside `band` of `oraclePrice` — the
 * mirror of `limit_price_breaches_maker_oracle_price_bands`, which every
 * external quoter leg is held to.
 *
 * One-sided, in the maker's favour: a maker buying below oracle or selling
 * above it is never breaching. Only the direction that moves value off the
 * maker is bounded, which is the direction a compromised quoter moves it.
 *
 * `makerDirection` is the maker's side, so the opposite of the taker's.
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
 * what velocity reserved for them — the mirror of the
 * `PerpPosition::reserved_open_base` bound every external unwind is held to.
 *
 * `openBids` / `openAsks` come straight off the maker's `PerpPosition`. They
 * are written at placement under the owner's signature, so a quoter cannot
 * inflate them; a report above them fails the fill on-chain.
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
