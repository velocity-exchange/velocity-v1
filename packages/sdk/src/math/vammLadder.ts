/**
 * vAMM quote ladder — TypeScript mirror of
 * `programs/velocity/src/vlp/amm/router_adapter.rs::vamm_quote_levels`.
 *
 * The router turns the continuous curve into discrete price levels so the vAMM
 * can be split against the CLOB and PropAMM books. Clients must reproduce it to
 * predict a fill, because the ladder — not the raw curve — is what the split
 * consumes.
 *
 * Two properties carry over from the Rust and matter for prediction:
 *
 *  - A rung's price is the exact per-unit cost of its own slice, computed from
 *    the same swap math the fill runs (spread reserves included), rounded
 *    against the taker. It is NOT the curve's marginal price.
 *  - Rival prices within {@link LAST_LOOK_BAND} of the vAMM's top become rungs
 *    (last look): the slice of curve cheaper than a rival is quoted AT the
 *    rival's price and the vAMM wins the tie on tier priority. So a client that
 *    ignores rival books will under-estimate what the taker pays. A rung
 *    reprices no more base than the rivals at that price offer, so rival size
 *    matters as much as rival price.
 *
 * Divergence from `router_adapter.rs` is a bug. Change both together.
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
	calculateAmmReservesAfterSwap,
	calculateMarketOpenBidAsk,
	calculateMaxBaseAssetAmountToTrade,
	calculateQuoteAssetAmountSwapped,
	calculateUpdatedAMMSpreadReserves,
} from './amm';
import { calculateBidAskPrice } from './amm';
import { RouterPriceLevel, RouterQuoterBook } from './router';

/** Ladder checkpoints per quote (rival rungs + equal-size filler). */
export const VAMM_QUOTE_CHECKPOINTS = 8;

/**
 * Rival prices are honored as shading rungs only within this fraction of the
 * vAMM's top price (PERCENTAGE_PRECISION): 5%. Beyond it a garbage price from a
 * malicious-but-approved quoter can't inflate the vAMM's book.
 */
export const LAST_LOOK_BAND = PERCENTAGE_PRECISION.divn(20);

function floorToStep(value: BN, step: BN): BN {
	return value.sub(value.mod(step));
}

/**
 * Quote notional the AMM charges for a swap of `base`, on the spread-adjusted
 * reserves — the TS equivalent of `calculate_base_swap_output(...).quote_asset_amount`.
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
 * The vAMM's ladder for a taker of `direction`/`size`: best-first levels
 * covering `min(size, available liquidity)`, shaded toward `rivalBooks` inside
 * the last-look band, capped at `takerLimit`.
 *
 * @param amm the market's AMM
 * @param marketStats the market's stats (spread reserves derive from it)
 * @param mmOraclePriceData current MM oracle reading
 * @param direction taker direction
 * @param size base the taker wants
 * @param stepSize the market's `orderStepSize` — rung sizes are multiples of it
 * @param rivalBooks every other book the router already quoted (last look)
 * @param takerLimit the taker's effective limit price, if any
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

	const [openBids, openAsks] = calculateMarketOpenBidAsk(
		amm.baseAssetReserve,
		amm.minBaseAssetReserve,
		amm.maxBaseAssetReserve,
		step
	);
	const available = isLong ? openAsks.abs() : openBids.abs();
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
		if (!isVariant(tradeDirection, isLong ? 'long' : 'short')) {
			// `top` is the reserve price plus one spread, but the swap's first
			// marginal is higher. A limit above `top` can still sit below that
			// marginal, so the inversion trades the other way. No size fills
			// within the limit. Quote nothing rather than leaving `total`
			// uncapped and quoting past the limit.
			return [];
		}
		total = BN.min(total, reachable);
		if (total.lte(ZERO)) {
			return [];
		}
	}

	// Rival prices past our top but inside the band (and the taker's limit)
	// become shading rungs, best-first.
	const band = top.mul(LAST_LOOK_BAND).div(PERCENTAGE_PRECISION);
	const bandEdge = isLong ? top.add(band) : top.sub(band);
	const rungEdge = takerLimit
		? isLong
			? BN.min(bandEdge, takerLimit)
			: BN.max(bandEdge, takerLimit)
		: bandEdge;
	// The best VAMM_QUOTE_CHECKPOINTS rungs, best first, each carrying the
	// depth the rivals offer at its price. The Rust insert-sorts into a fixed
	// array of that length, so a price that never reaches the array loses its
	// depth as well; the walk below reproduces that bound exactly.
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
	// A rung shades only as far as the rivals at that price or better can
	// supply. `rivalDepth` is that running total. Past it the taker's
	// alternative is not this rung but a worse one, so the curve is priced
	// honestly there and a later rung shades it if one carries the depth.
	const checkpoints: [BN, BN | undefined][] = [];
	let rivalDepth = ZERO;
	for (const rung of rivalRungs) {
		rivalDepth = rivalDepth.add(rung.size);
		const [cumulative, tradeDirection] = calculateMaxBaseAssetAmountToTrade(
			amm,
			marketStats,
			rung.price,
			direction,
			mmOraclePriceData
		);
		if (!isVariant(tradeDirection, isLong ? 'long' : 'short')) {
			continue;
		}
		const capped = BN.min(BN.min(cumulative, total), rivalDepth);
		checkpoints.push([capped, rung.price]);
		if (capped.eq(total)) {
			break;
		}
	}

	// Beyond the last rival rung the curve is priced honestly: equal-size
	// checkpoints, priced below from the swap math.
	const covered = checkpoints.length
		? checkpoints[checkpoints.length - 1][0]
		: ZERO;
	if (covered.lt(total)) {
		const filler = Math.max(VAMM_QUOTE_CHECKPOINTS - checkpoints.length, 1);
		const chunk = BN.max(total.sub(covered).divn(filler), new BN(1));
		for (let k = 1; k <= filler; k++) {
			const cumulative = BN.min(covered.add(chunk.muln(k)), total);
			checkpoints.push([cumulative, undefined]);
			if (cumulative.eq(total)) {
				break;
			}
		}
	}

	const spreadReserves = calculateUpdatedAMMSpreadReserves(
		amm,
		marketStats,
		direction,
		mmOraclePriceData
	);

	// Emit step-aligned rungs priced off the swap math that will execute, with
	// a running bound so shading can't break monotonicity (the split truncates
	// a book at its first non-monotone level).
	const levels: RouterPriceLevel[] = [];
	let previous = ZERO;
	let previousNotional = ZERO;
	let bound: BN | undefined;
	for (const [rawCumulative, shade] of checkpoints) {
		const cumulative = floorToStep(rawCumulative, step);
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
