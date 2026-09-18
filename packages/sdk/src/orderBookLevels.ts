import { BN } from './isomorphic/anchor';
import { isVariant, PositionDirection } from './types';
import { PublicKey } from '@solana/web3.js';
import { standardizePrice } from './math/orders';

/**
 * Where a level's depth came from. Only `clob` is a resting, cancellable order with
 * a queue position; `propamm` is a maker-program quote at the size asked.
 */
type liquiditySource = 'vamm' | 'clob' | 'propamm';

/**
 * A single aggregated price level of an L2 order book: one price with the combined size of all
 * orders resting at (or grouped into) that price, broken down by originating liquidity source.
 */
export type L2Level = {
	/** Level price, PRICE_PRECISION (1e6). */
	price: BN;
	/** Total size resting at this price, BASE_PRECISION (1e9). */
	size: BN;
	/** Size contributed by each liquidity source (`'vamm'`, `'clob'`, `'propamm'`), BASE_PRECISION (1e9). Sources with no contribution are omitted rather than zero. */
	sources: { [key in liquiditySource]?: BN };
};

/** Aggregated (price, size) view of a market's book, as the dlob-server's `/l2` serves it. */
export type L2OrderBook = {
	/** Ask levels, ordered from best (lowest price) to worst. */
	asks: L2Level[];
	/** Bid levels, ordered from best (highest price) to worst. */
	bids: L2Level[];
	/** Slot the book was computed at, if the caller supplied one. */
	slot?: number;
};

/** A single unaggregated order in an L3 (order-by-order) book view, as the dlob-server's `/l3` serves it. */
export type L3Level = {
	/** Order's limit price, PRICE_PRECISION (1e6). */
	price: BN;
	/** Order's remaining (unfilled) size, BASE_PRECISION (1e9). */
	size: BN;
	/** Pubkey of the order's owning `User` account. */
	maker: PublicKey;
	/** The order's `orderId` (unique per maker, not globally). */
	orderId: number;
};

/** Unaggregated, order-by-order view of a market's resting liquidity, as the dlob-server's `/l3` serves it. A PropAMM quotes at a size rather than resting orders, so it has no rows here. */
export type L3OrderBook = {
	/** Individual resting ask orders, ordered from best (lowest price) to worst. */
	asks: L3Level[];
	/** Individual resting bid orders, ordered from best (highest price) to worst. */
	bids: L3Level[];
	/** Slot the book was computed at, if the caller supplied one. */
	slot?: number;
};

/**
 * Re-buckets an `L2OrderBook` onto a coarser price grid, summing size per bucket and
 * truncating each side to `depth` levels. Bids standardize down, asks up.
 * @param grouping price bucket size, PRICE_PRECISION (1e6), must be a multiple of the tick size.
 */
export function groupL2(
	l2: L2OrderBook,
	grouping: BN,
	depth: number
): L2OrderBook {
	return {
		bids: groupL2Levels(l2.bids, grouping, PositionDirection.LONG, depth),
		asks: groupL2Levels(l2.asks, grouping, PositionDirection.SHORT, depth),
		slot: l2.slot,
	};
}

function cloneL2Level(level: L2Level): L2Level {
	if (!level) return level;

	return {
		price: level.price,
		size: level.size,
		sources: { ...level.sources },
	};
}

function groupL2Levels(
	levels: L2Level[],
	grouping: BN,
	direction: PositionDirection,
	depth: number
): L2Level[] {
	const groupedLevels: L2Level[] = [];
	for (const level of levels) {
		const price = standardizePrice(level.price, grouping, direction);
		const size = level.size;
		if (
			groupedLevels.length > 0 &&
			groupedLevels[groupedLevels.length - 1].price.eq(price)
		) {
			// Clones things so we don't mutate the original
			const currentLevel = cloneL2Level(
				groupedLevels[groupedLevels.length - 1]
			);

			currentLevel.size = currentLevel.size.add(size);
			for (const [source, size] of Object.entries(level.sources) as [
				liquiditySource,
				BN,
			][]) {
				const existingSize = currentLevel.sources[source];
				if (existingSize) {
					currentLevel.sources[source] = existingSize.add(size);
				} else {
					currentLevel.sources[source] = size;
				}
			}

			groupedLevels[groupedLevels.length - 1] = currentLevel;
		} else {
			const groupedLevel = {
				price: price,
				size,
				sources: level.sources,
			};

			groupedLevels.push(groupedLevel);
		}

		if (groupedLevels.length === depth) {
			break;
		}
	}

	return groupedLevels;
}

/**
 * Method to merge bids or asks by price
 */
const mergeByPrice = (bidsOrAsks: L2Level[]) => {
	const merged = new Map<string, L2Level>();
	for (const level of bidsOrAsks) {
		const key = level.price.toString();
		const existing = merged.get(key);
		if (existing) {
			existing.size = existing.size.add(level.size);
			for (const [source, size] of Object.entries(level.sources) as [
				liquiditySource,
				BN,
			][]) {
				const existingSize = existing.sources[source];
				if (existingSize) {
					existing.sources[source] = existingSize.add(size);
				} else {
					existing.sources[source] = size;
				}
			}
		} else {
			merged.set(key, cloneL2Level(level));
		}
	}

	return Array.from(merged.values());
};

/**
 * The purpose of this function is uncross the L2 orderbook by modifying the bid/ask price at the top of the book
 * This will make the liquidity look worse but more intuitive (users familiar with clob get confused w temporarily
 * crossing book)
 *
 * Things to note about how it works:
 * - it will not uncross the user's liquidity
 * - it does the uncrossing by "shifting" the crossing liquidity to the nearest uncrossed levels. Thus the output liquidity maintains the same total size.
 *
 * No-ops (returns `bids`/`asks` unchanged) if either side is empty, or if the top of book is
 * already uncrossed (`bids[0].price < asks[0].price`).
 *
 * @param bids bid levels, PRICE_PRECISION (1e6) prices, best (highest) first
 * @param asks ask levels, PRICE_PRECISION (1e6) prices, best (lowest) first
 * @param oraclePrice current oracle price, PRICE_PRECISION (1e6)
 * @param oracleTwap5Min 5-minute oracle price TWAP, PRICE_PRECISION (1e6)
 * @param markTwap5Min 5-minute mark price TWAP, PRICE_PRECISION (1e6); `markTwap5Min - oracleTwap5Min` estimates the market's premium/discount to oracle, used as the reference point crossing liquidity is shifted around
 * @param grouping minimum price gap to enforce between the shifted bid/ask, PRICE_PRECISION (1e6)
 * @param userBids set of bid price strings (`BN.toString()`) belonging to the requesting user, which are left untouched rather than shifted
 * @param userAsks set of ask price strings (`BN.toString()`) belonging to the requesting user, which are left untouched rather than shifted
 * @returns new `bids`/`asks` arrays with crossing levels shifted apart by at least `grouping`; total size per side is preserved
 */
export function uncrossL2(
	bids: L2Level[],
	asks: L2Level[],
	oraclePrice: BN,
	oracleTwap5Min: BN,
	markTwap5Min: BN,
	grouping: BN,
	userBids: Set<string>,
	userAsks: Set<string>
): { bids: L2Level[]; asks: L2Level[] } {
	// If there are no bids or asks, there is nothing to center
	if (bids.length === 0 || asks.length === 0) {
		return { bids, asks };
	}

	// If the top of the book is already centered, there is nothing to do
	if (bids[0].price.lt(asks[0].price)) {
		return { bids, asks };
	}

	const newBids: L2Level[] = [];
	const newAsks: L2Level[] = [];

	const updateLevels = (newPrice: BN, oldLevel: L2Level, levels: L2Level[]) => {
		if (levels.length > 0 && levels[levels.length - 1].price.eq(newPrice)) {
			levels[levels.length - 1].size = levels[levels.length - 1].size.add(
				oldLevel.size
			);

			for (const [source, size] of Object.entries(oldLevel.sources) as [
				liquiditySource,
				BN,
			][]) {
				const existingSize = levels[levels.length - 1].sources[source];
				if (existingSize) {
					levels[levels.length - 1].sources = {
						...levels[levels.length - 1].sources,
						[source]: existingSize.add(size),
					};
				} else {
					levels[levels.length - 1].sources[source] = size;
				}
			}
		} else {
			levels.push({
				price: newPrice,
				size: oldLevel.size,
				sources: oldLevel.sources,
			});
		}
	};

	// This is the best estimate of the premium in the market vs oracle to filter crossing around
	const referencePrice = oraclePrice.add(markTwap5Min.sub(oracleTwap5Min));

	let bidIndex = 0;
	let askIndex = 0;
	let maxBid: BN | undefined;
	let minAsk: BN | undefined;

	const getPriceAndSetBound = (newPrice: BN, direction: PositionDirection) => {
		if (isVariant(direction, 'long')) {
			maxBid = maxBid ? BN.min(maxBid, newPrice) : newPrice;
			return maxBid;
		} else {
			minAsk = minAsk ? BN.max(minAsk, newPrice) : newPrice;
			return minAsk;
		}
	};

	while (bidIndex < bids.length || askIndex < asks.length) {
		const nextBid = cloneL2Level(bids[bidIndex]);
		const nextAsk = cloneL2Level(asks[askIndex]);

		if (!nextBid) {
			newAsks.push(nextAsk);
			askIndex++;
			continue;
		}

		if (!nextAsk) {
			newBids.push(nextBid);
			bidIndex++;
			continue;
		}

		if (userBids.has(nextBid.price.toString())) {
			newBids.push(nextBid);
			bidIndex++;
			continue;
		}

		if (userAsks.has(nextAsk.price.toString())) {
			newAsks.push(nextAsk);
			askIndex++;
			continue;
		}

		if (nextBid.price.gte(nextAsk.price)) {
			if (
				nextBid.price.gt(referencePrice) &&
				nextAsk.price.gt(referencePrice)
			) {
				let newBidPrice = nextAsk.price.sub(grouping);
				newBidPrice = getPriceAndSetBound(newBidPrice, PositionDirection.LONG);
				updateLevels(newBidPrice, nextBid, newBids);
				bidIndex++;
			} else if (
				nextAsk.price.lt(referencePrice) &&
				nextBid.price.lt(referencePrice)
			) {
				let newAskPrice = nextBid.price.add(grouping);
				newAskPrice = getPriceAndSetBound(newAskPrice, PositionDirection.SHORT);
				updateLevels(newAskPrice, nextAsk, newAsks);
				askIndex++;
			} else {
				let newBidPrice = referencePrice.sub(grouping);
				let newAskPrice = referencePrice.add(grouping);

				newBidPrice = getPriceAndSetBound(newBidPrice, PositionDirection.LONG);
				newAskPrice = getPriceAndSetBound(newAskPrice, PositionDirection.SHORT);

				updateLevels(newBidPrice, nextBid, newBids);
				updateLevels(newAskPrice, nextAsk, newAsks);
				bidIndex++;
				askIndex++;
			}
		} else {
			if (minAsk && nextAsk.price.lte(minAsk)) {
				const newAskPrice = getPriceAndSetBound(
					nextAsk.price,
					PositionDirection.SHORT
				);

				updateLevels(newAskPrice, nextAsk, newAsks);
			} else {
				newAsks.push(nextAsk);
			}

			askIndex++;

			if (maxBid && nextBid.price.gte(maxBid)) {
				const newBidPrice = getPriceAndSetBound(
					nextBid.price,
					PositionDirection.LONG
				);

				updateLevels(newBidPrice, nextBid, newBids);
			} else {
				newBids.push(nextBid);
			}

			bidIndex++;
		}
	}

	newBids.sort((a, b) => b.price.cmp(a.price));
	newAsks.sort((a, b) => a.price.cmp(b.price));

	const finalNewBids = mergeByPrice(newBids);
	const finalNewAsks = mergeByPrice(newAsks);

	return {
		bids: finalNewBids,
		asks: finalNewAsks,
	};
}
