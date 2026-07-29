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
import { BASE_PRECISION, ZERO } from '../constants/numericConstants';

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
};

/** What the split routes to one quoter. */
export type RouterAllocation = {
	/** Base routed to this quoter, a `stepSize` multiple. */
	base: BN;
	/** Quote notional at the quoted levels — the bound execution is held to. */
	quote: BN;
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
	const allocations: RouterAllocation[] = books.map(() => ({
		base: ZERO,
		quote: ZERO,
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
