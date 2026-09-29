/**
 * allow-verbose: mirrors `programs/velocity/src/vlp/amm/router_adapter.rs::vamm_quote_levels`;
 * a divergence between the two is a bug, so change both together.
 *
 * The router turns the continuous curve into discrete price levels so the vAMM can be split
 * against the CLOB and PropAMM books. A client reproduces the ladder to predict a fill, because
 * the split consumes the ladder rather than the raw curve.
 *
 * Two properties carry over from the Rust and matter for prediction:
 *  - A rung's price is the exact per-unit cost of its own slice, from the same swap math the
 *    fill runs, spread reserves included, rounded against the taker. It is not the curve's
 *    marginal price.
 *  - A rival price within {@link LAST_LOOK_BAND} of the vAMM's top becomes a rung, priced at the
 *    rival's price; the vAMM wins the tie on tier priority. A client that ignores rival books
 *    under-estimates what the taker pays. A rung reprices only the last base before the curve
 *    reaches the rival's price `P`. It covers at most `min(D, total - reach(P))` base, where `D`
 *    is the rival depth not yet shaded, so a rival that the take does not reach shades nothing.
 */

import { BN } from '@coral-xyz/anchor';
import {
	AMM,
	MarketStats,
	PositionDirection,
	SwapDirection,
	isVariant,
} from '../types';
import { MMOraclePriceData } from '../oracles/types';
import {
	BASE_PRECISION,
	PERCENTAGE_PRECISION,
	ZERO,
} from '../constants/numericConstants';
import {
	calculateAmmAvailableLiquidity,
	calculateAmmReservesAfterSwap,
	calculateMaxBaseAssetAmountToTrade,
	calculateQuoteAssetAmountSwapped,
	calculateUpdatedAMMSpreadReserves,
} from './amm';
import { calculateBidAskPrice } from './amm';
import { standardizeBaseAssetAmount } from './orders';
import { RouterPriceLevel, RouterQuoterBook } from './router';

/** Ladder checkpoints per quote (rival rungs + equal-size filler). */
export const VAMM_QUOTE_CHECKPOINTS = 8;

/**
 * A rival price becomes a shading rung only within this fraction of the vAMM's top price
 * (PERCENTAGE_PRECISION, 5 percent), so a bad price from an approved quoter cannot inflate the book.
 */
export const LAST_LOOK_BAND = PERCENTAGE_PRECISION.divn(20);

/**
 * Quote notional the AMM charges for a swap of `base`, on the spread-adjusted
 * reserves. This is the TypeScript form of
 * `calculate_base_swap_output(...).quote_asset_amount`.
 */
function swapNotional(
	spreadReserves: {
		baseAssetReserve: BN;
		quoteAssetReserve: BN;
		sqrtK: BN;
		newPeg: BN;
	},
	base: BN,
	swapDirection: SwapDirection
): BN {
	const [newQuoteAssetReserve] = calculateAmmReservesAfterSwap(
		{
			baseAssetReserve: spreadReserves.baseAssetReserve,
			quoteAssetReserve: spreadReserves.quoteAssetReserve,
			sqrtK: spreadReserves.sqrtK,
			pegMultiplier: spreadReserves.newPeg,
		},
		'base',
		base,
		swapDirection
	);
	const reserveDelta = newQuoteAssetReserve
		.sub(spreadReserves.quoteAssetReserve)
		.abs();
	return calculateQuoteAssetAmountSwapped(
		reserveDelta,
		spreadReserves.newPeg,
		swapDirection
	);
}

/**
 * The vAMM's ladder for a taker of `direction` and `size`. The levels run best
 * first and cover the lesser of `size` and the available liquidity. Rival
 * prices inside the last-look band shade them, and `takerLimit` caps them.
 *
 * @param amm the market's AMM
 * @param marketStats the market's stats, which the spread reserves derive from
 * @param mmOraclePriceData the current MM oracle reading
 * @param direction the taker's direction
 * @param size the base the taker wants
 * @param stepSize the market's `orderStepSize`. Rung sizes are multiples of it
 * @param rivalBooks every other book the router already quoted
 * @param takerLimit the taker's effective limit price, if the taker set one
 */
export function vammQuoteLevels(
	amm: AMM,
	marketStats: MarketStats,
	mmOraclePriceData: Pick<MMOraclePriceData, 'price' | 'confidence'>,
	direction: PositionDirection,
	size: BN,
	stepSize: BN,
	rivalBooks: RouterQuoterBook[] = [],
	takerLimit?: BN
): RouterPriceLevel[] {
	const isLong = isVariant(direction, 'long');
	const step = stepSize.gt(ZERO) ? stepSize : new BN(1);
	const swapDirection = isLong ? SwapDirection.REMOVE : SwapDirection.ADD;

	// The per-fill reserve throttle, not the room to the hard reserve bound.
	// `vamm_quote_levels` caps its ladder at this, so a client that used the
	// wider figure would quote depth the program refuses.
	const available = calculateAmmAvailableLiquidity(amm, direction, step);
	let total = BN.min(size, available);
	if (total.lte(ZERO)) {
		return [];
	}

	const [bid, ask] = calculateBidAskPrice(amm, marketStats, mmOraclePriceData);
	const top = isLong ? ask : bid;

	if (takerLimit) {
		const crossedAtTop = isLong ? takerLimit.lt(top) : takerLimit.gt(top);
		if (crossedAtTop) {
			return [];
		}

		const [reachable, tradeDirection] = calculateMaxBaseAssetAmountToTrade(
			amm,
			marketStats,
			takerLimit,
			direction,
			mmOraclePriceData
		);

		// The reach runs on the same reserves as `top` but rounds differently.
		// Its direction is the exact test that some base fills within the limit.
		if (!isVariant(tradeDirection, isLong ? 'long' : 'short')) {
			return [];
		}

		total = BN.min(total, reachable);
		if (total.lte(ZERO)) {
			return [];
		}
	}

	// A rival price past the vAMM's top becomes a shading rung when it stays
	// inside the band and inside the taker's limit. Rungs are best first.
	const band = top.mul(LAST_LOOK_BAND).div(PERCENTAGE_PRECISION);
	const bandEdge = isLong ? top.add(band) : top.sub(band);
	const rungEdge = takerLimit
		? isLong
			? BN.min(bandEdge, takerLimit)
			: BN.max(bandEdge, takerLimit)
		: bandEdge;
	// The best VAMM_QUOTE_CHECKPOINTS rungs, best first. Each one carries the
	// depth the rivals offer at its price. The Rust insert-sorts into a fixed
	// array of that length, so a price that never reaches the array loses its
	// depth too. The walk below reproduces that bound.
	const rivalRungs: RouterPriceLevel[] = [];
	const ranksBefore = (a: BN, b: BN) => (isLong ? a.lt(b) : a.gt(b));
	for (const book of rivalBooks) {
		for (const level of book.levels) {
			const inBand = isLong
				? level.price.gt(top) && level.price.lte(rungEdge)
				: level.price.lt(top) &&
				  level.price.gte(rungEdge) &&
				  level.price.gt(ZERO);
			if (!inBand || level.size.lte(ZERO)) {
				continue;
			}

			let at = 0;
			while (
				at < rivalRungs.length &&
				ranksBefore(rivalRungs[at].price, level.price)
			) {
				at++;
			}

			if (at < rivalRungs.length && rivalRungs[at].price.eq(level.price)) {
				// Two rivals at one price offer that price for both their sizes.
				rivalRungs[at] = {
					price: rivalRungs[at].price,
					size: rivalRungs[at].size.add(level.size),
				};

				continue;
			}
			if (at >= VAMM_QUOTE_CHECKPOINTS) {
				continue;
			}

			rivalRungs.splice(at, 0, { price: level.price, size: level.size });
			if (rivalRungs.length > VAMM_QUOTE_CHECKPOINTS) {
				rivalRungs.length = VAMM_QUOTE_CHECKPOINTS;
			}
		}
	}

	// Checkpoints: [cumulative base, shading price if this is a rival rung].
	//
	// A rung shades the last base before the curve reaches its price, floored
	// to the step. The honest slice before the rung is cheaper and the
	// slice after it is dearer, so the book stays monotone. Equal-size
	// checkpoints price the rest of the curve honestly. The emit loop skips a
	// checkpoint that does not advance the ladder.
	const chunk = BN.max(total.divn(VAMM_QUOTE_CHECKPOINTS), new BN(1));
	const gridPoint = (k: number) =>
		k === VAMM_QUOTE_CHECKPOINTS ? total : BN.min(total, chunk.muln(k));

	const checkpoints: [BN, BN | undefined][] = [];
	let gridIndex = 1;
	let rivalDepth = ZERO;
	let shadedBase = ZERO;
	let covered = ZERO;
	for (const rung of rivalRungs) {
		rivalDepth = rivalDepth.add(rung.size);
		const [reach, tradeDirection] = calculateMaxBaseAssetAmountToTrade(
			amm,
			marketStats,
			rung.price,
			direction,
			mmOraclePriceData
		);

		if (!isVariant(tradeDirection, isLong ? 'long' : 'short')) {
			continue;
		}

		// The rivals at this price trade only the take past `reach`, so the
		// rung shades no more base than that.
		const shadeEnd = standardizeBaseAssetAmount(BN.min(reach, total), step);
		const shadeBudget = standardizeBaseAssetAmount(
			BN.min(rivalDepth.sub(shadedBase), BN.max(total.sub(reach), ZERO)),
			step
		);
		const shadeStart = BN.max(BN.max(shadeEnd.sub(shadeBudget), ZERO), covered);
		if (shadeStart.gte(shadeEnd)) {
			continue;
		}

		while (
			gridIndex <= VAMM_QUOTE_CHECKPOINTS &&
			gridPoint(gridIndex).lt(shadeStart)
		) {
			checkpoints.push([gridPoint(gridIndex), undefined]);
			gridIndex++;
		}

		while (
			gridIndex <= VAMM_QUOTE_CHECKPOINTS &&
			gridPoint(gridIndex).lte(shadeEnd)
		) {
			gridIndex++;
		}

		checkpoints.push([shadeStart, undefined]);
		checkpoints.push([shadeEnd, rung.price]);
		shadedBase = shadedBase.add(shadeEnd.sub(shadeStart));
		covered = shadeEnd;
	}

	for (; gridIndex <= VAMM_QUOTE_CHECKPOINTS; gridIndex++) {
		checkpoints.push([gridPoint(gridIndex), undefined]);
	}

	const spreadReserves = calculateUpdatedAMMSpreadReserves(
		amm,
		marketStats,
		direction,
		mmOraclePriceData
	);

	// Emit step-aligned rungs priced off the swap math that executes the fill.
	// A running bound keeps the book monotone where rounding puts a slice past
	// the rung before it. The split truncates a book at its first non-monotone
	// level.
	const levels: RouterPriceLevel[] = [];
	let previous = ZERO;
	let previousNotional = ZERO;
	let bound: BN | undefined;
	for (const [rawCumulative, shade] of checkpoints) {
		const cumulative = standardizeBaseAssetAmount(rawCumulative, step);
		if (cumulative.lte(previous)) {
			continue;
		}

		const levelSize = cumulative.sub(previous);
		const notional = BN.max(
			swapNotional(spreadReserves, cumulative, swapDirection),
			previousNotional
		);
		const sliceNotional = notional.sub(previousNotional);
		const exact = sliceNotional.mul(BASE_PRECISION);
		const honest = isLong
			? exact.add(levelSize).subn(1).div(levelSize)
			: exact.div(levelSize);
		let price = honest;
		if (isLong) {
			if (shade && shade.gt(price)) price = shade;
			if (bound && bound.gt(price)) price = bound;
		} else {
			if (shade && shade.lt(price)) price = shade;
			if (bound && bound.lt(price)) price = bound;
		}
		if (price.lte(ZERO)) {
			break;
		}

		bound = price;
		levels.push({ price, size: levelSize });
		previous = cumulative;
		previousNotional = notional;
	}

	return levels;
}
