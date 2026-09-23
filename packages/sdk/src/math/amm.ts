import { BN } from '../isomorphic/anchor';
import {
	AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO,
	PRICE_PRECISION,
	PEG_PRECISION,
	ZERO,
	BID_ASK_SPREAD_PRECISION,
	ONE,
	AMM_TO_QUOTE_PRECISION_RATIO,
	QUOTE_PRECISION,
	PRICE_DIV_PEG,
	PERCENTAGE_PRECISION,
	DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT,
	FUNDING_RATE_BUFFER_PRECISION,
	FUNDING_RATE_OFFSET_PERCENTAGE,
	FUNDING_RATE_OFFSET_DENOMINATOR,
	TWO,
	LAZER_CONF_FLOOR_PCT,
	REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT,
	SPREAD_CONF_DISCOUNT_DIVISOR,
} from '../constants/numericConstants';
import {
	AMM,
	MarketStats,
	PositionDirection,
	SwapDirection,
	PerpMarketAccount,
	isVariant,
} from '../types';
import { assert } from '../assert/assert';
import { squareRootBN, sigNum, clampBN } from './utils';
import { standardizeBaseAssetAmount } from './orders';

import { MMOraclePriceData, OraclePriceData } from '../oracles/types';
import {
	calculateRepegCost,
	calculateAdjustKCost,
	calculateBudgetedPeg,
} from './repeg';

import { calculateLiveOracleStd, getNewOracleConfPct } from './oracles';

/**
 * Solves for the `pegMultiplier` that would make the AMM's constant-product price equal
 * `targetPrice` at the current reserves, mirroring `calculate_peg_from_target_price` in
 * `vlp/amm/math/repeg.rs`. Rounds to the nearest peg unit and floors at 1.
 * @param targetPrice Desired price, PRICE_PRECISION (1e6).
 * @param baseAssetReserve AMM base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param quoteAssetReserve AMM quote asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @returns Peg multiplier that yields `targetPrice`, PEG_PRECISION (1e6), never below 1.
 */
export function calculatePegFromTargetPrice(
	targetPrice: BN,
	baseAssetReserve: BN,
	quoteAssetReserve: BN
): BN {
	return BN.max(
		targetPrice
			.mul(baseAssetReserve)
			.div(quoteAssetReserve)
			.add(PRICE_DIV_PEG.div(new BN(2)))
			.div(PRICE_DIV_PEG),
		ONE
	);
}

/**
 * Computes the oracle-implied target price/peg for a repeg and the quote budget available to
 * fund it, mirroring `calculate_optimal_peg_and_budget` in `vlp/amm/math/repeg.rs`. The
 * budget is the AMM's own retained equity (`max(0, totalFeeMinusDistributions)`) — there is
 * no separate protocol floor post-isolation. If that budget can't cover a direct repeg to
 * the oracle price, the target is pulled back to the edge of half the market's max spread
 * instead (a partial repeg that costs exactly the recomputed budget), and
 * `checkLowerBound` is set to `false` since that partial move is unconditionally affordable
 * by construction. `checkLowerBound` is also `false` when the budget is exactly zero (no
 * equity to spend at all — `calculateUpdatedAMM` uses this to know whether it must reject
 * the whole update or can proceed with the recomputed, always-affordable target).
 * @param amm AMM state (reserves, `pegMultiplier`, `totalFeeMinusDistributions`, `maxSpread`).
 * @param mmOraclePriceData Current MM oracle price data; `price` is the desired target.
 * @returns `[targetPrice, newPeg, budget, checkLowerBound]`: `targetPrice`/`newPeg` are
 *   PRICE_PRECISION (1e6) / PEG_PRECISION (1e6) respectively; `budget` is the quote amount
 *   available to spend, QUOTE_PRECISION (1e6); `checkLowerBound` tells the caller whether it
 *   must still verify the repeg doesn't push `totalFeeMinusDistributions` negative.
 */
export function calculateOptimalPegAndBudget(
	amm: AMM,
	mmOraclePriceData: Pick<MMOraclePriceData, 'price'>
): [BN, BN, BN, boolean] {
	const reservePriceBefore = calculatePrice(
		amm.baseAssetReserve,
		amm.quoteAssetReserve,
		amm.pegMultiplier
	);
	const targetPrice = mmOraclePriceData.price;
	const newPeg = calculatePegFromTargetPrice(
		targetPrice,
		amm.baseAssetReserve,
		amm.quoteAssetReserve
	);
	const prePegCost = calculateRepegCost(amm, newPeg);

	// no protocol floor post-isolation: tfmd contains only the AMM's own
	// equity and is fully spendable on the repeg
	const budget = BN.max(ZERO, amm.totalFeeMinusDistributions);

	let checkLowerBound = true;
	if (budget.lt(prePegCost)) {
		const halfMaxPriceSpread = new BN(amm.maxSpread)
			.div(new BN(2))
			.mul(targetPrice)
			.div(BID_ASK_SPREAD_PRECISION);

		let newTargetPrice: BN;
		let newOptimalPeg: BN;
		let newBudget: BN;
		const targetPriceGap = reservePriceBefore.sub(targetPrice);

		if (targetPriceGap.abs().gt(halfMaxPriceSpread)) {
			const markAdj = targetPriceGap.abs().sub(halfMaxPriceSpread);

			if (targetPriceGap.lt(new BN(0))) {
				newTargetPrice = reservePriceBefore.add(markAdj);
			} else {
				newTargetPrice = reservePriceBefore.sub(markAdj);
			}

			newOptimalPeg = calculatePegFromTargetPrice(
				newTargetPrice,
				amm.baseAssetReserve,
				amm.quoteAssetReserve
			);

			newBudget = calculateRepegCost(amm, newOptimalPeg);
			checkLowerBound = false;

			return [newTargetPrice, newOptimalPeg, newBudget, false];
		} else if (budget.eq(ZERO)) {
			// mirrors the program: budget = max(0, tfmd), so a zero budget
			// means the AMM has no equity to spend (no floor post-isolation)
			checkLowerBound = false;
		}
	}

	return [targetPrice, newPeg, budget, checkLowerBound];
}

/**
 * Determines the full curve update (repeg cost, K scale factor, new peg) for `amm` against
 * the current oracle price, mirroring the "use full budget peg" fallback path of `adjust_amm`
 * in `vlp/amm/math/repeg.rs`. Starts from `calculateOptimalPegAndBudget`'s target/budget; if
 * the direct repeg cost meets or exceeds that budget, shrinks `sqrtK` by 0.1% (999/1000) via
 * `calculateAdjustKCost` first to free up additional budget, then re-solves for the peg with
 * `calculateBudgetedPeg` using the combined budget.
 * @param amm AMM state to evaluate a curve update for.
 * @param mmOraclePriceData Current MM oracle price data.
 * @returns `[prePegCost, pKNumer, pKDenom, newPeg, checkLowerBound]`: `prePegCost` is the
 *   quote cost of the full update, QUOTE_PRECISION (1e6); `pKNumer`/`pKDenom` are the sqrtK
 *   scale factor (999/1000 if K was shrunk, else 1/1); `newPeg` is PEG_PRECISION (1e6);
 *   `checkLowerBound` is forwarded from `calculateOptimalPegAndBudget` and tells
 *   `calculateUpdatedAMM` whether it must still verify affordability against
 *   `totalFeeMinusDistributions`.
 */
export function calculateNewAmm(
	amm: AMM,
	mmOraclePriceData: Pick<MMOraclePriceData, 'price'>
): [BN, BN, BN, BN, boolean] {
	let pKNumer = new BN(1);
	let pKDenom = new BN(1);

	const [targetPrice, _newPeg, budget, checkLowerBound] =
		calculateOptimalPegAndBudget(amm, mmOraclePriceData);
	let prePegCost = calculateRepegCost(amm, _newPeg);
	let newPeg = _newPeg;

	if (prePegCost.gte(budget) && prePegCost.gt(ZERO)) {
		[pKNumer, pKDenom] = [new BN(999), new BN(1000)];
		const deficitMadeup = calculateAdjustKCost(amm, pKNumer, pKDenom);
		assert(deficitMadeup.lte(new BN(0)));
		prePegCost = budget.add(deficitMadeup.abs());
		const newAmm = Object.assign({}, amm);
		newAmm.baseAssetReserve = newAmm.baseAssetReserve.mul(pKNumer).div(pKDenom);
		newAmm.sqrtK = newAmm.sqrtK.mul(pKNumer).div(pKDenom);
		const invariant = newAmm.sqrtK.mul(newAmm.sqrtK);
		newAmm.quoteAssetReserve = invariant.div(newAmm.baseAssetReserve);
		const directionToClose = amm.baseAssetAmountWithAmm.gt(ZERO)
			? PositionDirection.SHORT
			: PositionDirection.LONG;

		const [newQuoteAssetReserve, _newBaseAssetReserve] =
			calculateAmmReservesAfterSwap(
				newAmm,
				'base',
				amm.baseAssetAmountWithAmm.abs(),
				getSwapDirection('base', directionToClose)
			);

		newAmm.terminalQuoteAssetReserve = newQuoteAssetReserve;
		newPeg = calculateBudgetedPeg(newAmm, prePegCost, targetPrice);
		prePegCost = calculateRepegCost(newAmm, newPeg);
	}

	return [prePegCost, pKNumer, pKDenom, newPeg, checkLowerBound];
}

/**
 * Returns a copy of `amm` with its curve (peg, reserves, sqrtK) repegged/updated to the
 * current oracle price, or `amm` unchanged if no update applies. Mirrors the program's
 * curve-update gating: a no-op if `curveUpdateIntensity == 0`, if `mmOraclePriceData` is
 * omitted, or if the oracle price is non-positive (mirrors
 * `is_oracle_valid_for_action(..., UpdateAMMCurve)` — only a non-positive price invalidates
 * the update here). **Affordability gate:** when `calculateNewAmm`'s `checkLowerBound` is
 * true and debiting the computed cost from `totalFeeMinusDistributions` would push it
 * negative, the update is rejected wholesale and `amm` is returned unchanged — the AMM will
 * never spend equity it doesn't have on a repeg. When the update proceeds, both
 * `totalFeeMinusDistributions` and `netRevenueSinceLastFunding` are debited by the repeg
 * cost.
 * @param amm AMM state to update.
 * @param mmOraclePriceData Current MM oracle price data; omit to skip the update entirely.
 * @returns Updated `AMM` (new object), or the original `amm` reference if no update applies or the affordability gate rejects it.
 */
export function calculateUpdatedAMM(
	amm: AMM,
	mmOraclePriceData?: Pick<MMOraclePriceData, 'price'>
): AMM {
	if (amm.curveUpdateIntensity == 0 || mmOraclePriceData === undefined) {
		return amm;
	}
	// mirrors is_oracle_valid_for_action(..., UpdateAMMCurve): only a
	// non-positive oracle price invalidates the curve update
	if (mmOraclePriceData.price.lte(ZERO)) {
		return amm;
	}
	const newAmm = Object.assign({}, amm);
	const [prepegCost, pKNumer, pKDenom, newPeg, checkLowerBound] =
		calculateNewAmm(amm, mmOraclePriceData);

	if (prepegCost.gt(ZERO)) {
		const newTotalFeeMinusDistributions =
			amm.totalFeeMinusDistributions.sub(prepegCost);
		if (checkLowerBound && newTotalFeeMinusDistributions.lt(ZERO)) {
			// affordability floor rejected the debit: passthrough, unchanged
			return amm;
		}
	}

	newAmm.baseAssetReserve = newAmm.baseAssetReserve.mul(pKNumer).div(pKDenom);
	newAmm.sqrtK = newAmm.sqrtK.mul(pKNumer).div(pKDenom);
	const invariant = newAmm.sqrtK.mul(newAmm.sqrtK);
	newAmm.quoteAssetReserve = invariant.div(newAmm.baseAssetReserve);
	newAmm.pegMultiplier = newPeg;

	const directionToClose = amm.baseAssetAmountWithAmm.gt(ZERO)
		? PositionDirection.SHORT
		: PositionDirection.LONG;

	const [newQuoteAssetReserve, _newBaseAssetReserve] =
		calculateAmmReservesAfterSwap(
			newAmm,
			'base',
			amm.baseAssetAmountWithAmm.abs(),
			getSwapDirection('base', directionToClose)
		);

	newAmm.terminalQuoteAssetReserve = newQuoteAssetReserve;

	newAmm.totalFeeMinusDistributions =
		newAmm.totalFeeMinusDistributions.sub(prepegCost);
	newAmm.netRevenueSinceLastFunding =
		newAmm.netRevenueSinceLastFunding.sub(prepegCost);
	return newAmm;
}

/**
 * Repegs `amm` to the current oracle price (`calculateUpdatedAMM`) and returns the
 * one-sided spread reserves (bid reserves for `short`, ask reserves for `long`) plus the
 * post-update `sqrtK`/peg — the reserves a trade closing/opening in `direction` would
 * actually execute against.
 * @param amm AMM state to update and derive spread reserves from.
 * @param marketStats Market stats needed for spread and reference-price-offset calculation.
 * @param direction Which side's spread reserves to return.
 * @param mmOraclePriceData Current MM oracle price data, forwarded to `calculateUpdatedAMM`.
 * @returns `baseAssetReserve`/`quoteAssetReserve` for the requested side (AMM_RESERVE_PRECISION, 1e9), and the post-update `sqrtK`/`newPeg` (AMM_RESERVE_PRECISION 1e9 / PEG_PRECISION 1e6).
 */
export function calculateUpdatedAMMSpreadReserves(
	amm: AMM,
	marketStats: MarketStats,
	direction: PositionDirection,
	mmOraclePriceData?: Pick<MMOraclePriceData, 'price' | 'confidence'>
): { baseAssetReserve: BN; quoteAssetReserve: BN; sqrtK: BN; newPeg: BN } {
	const newAmm = calculateUpdatedAMM(amm, mmOraclePriceData);
	const [shortReserves, longReserves] = calculateSpreadReserves(
		newAmm,
		marketStats,
		mmOraclePriceData
	);

	const dirReserves = isVariant(direction, 'long')
		? longReserves
		: shortReserves;

	const result = {
		baseAssetReserve: dirReserves.baseAssetReserve,
		quoteAssetReserve: dirReserves.quoteAssetReserve,
		sqrtK: newAmm.sqrtK,
		newPeg: newAmm.pegMultiplier,
	};

	return result;
}

/**
 * Returns the AMM's current bid and ask prices, computed from its spread reserves
 * (`calculateSpreadReserves`) after optionally repegging to the oracle price first.
 * @param amm AMM state to price.
 * @param marketStats Market stats needed for spread calculation.
 * @param mmOraclePriceData Current MM oracle price data; used both to repeg (if `withUpdate`) and to compute the spread.
 * @param withUpdate If true (default), repegs `amm` to the oracle price (`calculateUpdatedAMM`) before pricing; if false, prices the AMM's stored reserves as-is.
 * @returns `[bidPrice, askPrice]`, both PRICE_PRECISION (1e6).
 */
export function calculateBidAskPrice(
	amm: AMM,
	marketStats: MarketStats,
	mmOraclePriceData?: Pick<MMOraclePriceData, 'price' | 'confidence'>,
	withUpdate = true
): [BN, BN] {
	let newAmm: AMM;
	if (withUpdate) {
		newAmm = calculateUpdatedAMM(amm, mmOraclePriceData);
	} else {
		newAmm = amm;
	}

	const [bidReserves, askReserves] = calculateSpreadReserves(
		newAmm,
		marketStats,
		mmOraclePriceData
	);

	const askPrice = calculatePrice(
		askReserves.baseAssetReserve,
		askReserves.quoteAssetReserve,
		newAmm.pegMultiplier
	);

	const bidPrice = calculatePrice(
		bidReserves.baseAssetReserve,
		bidReserves.quoteAssetReserve,
		newAmm.pegMultiplier
	);

	return [bidPrice, askPrice];
}

/**
 * Computes the constant-product price implied by a pair of AMM reserves and a peg multiplier:
 * `quoteAssetReserves * pegMultiplier / baseAssetReserves`, converted to `PRICE_PRECISION`.
 * `baseAssetReserves` and `quoteAssetReserves` must be the same precision (typically both
 * `AMM_RESERVE_PRECISION`, 1e9) — this is a pure ratio, not tied to any specific reserve field.
 * @param baseAssetReserves Base reserve amount, same precision as `quoteAssetReserves`.
 * @param quoteAssetReserves Quote reserve amount, same precision as `baseAssetReserves`.
 * @param pegMultiplier Peg multiplier to scale the ratio by, PEG_PRECISION (1e6).
 * @returns Price, PRICE_PRECISION (1e6); zero if `baseAssetReserves` is zero.
 */
export function calculatePrice(
	baseAssetReserves: BN,
	quoteAssetReserves: BN,
	pegMultiplier: BN
): BN {
	if (baseAssetReserves.abs().lte(ZERO)) {
		return new BN(0);
	}

	return quoteAssetReserves
		.mul(PRICE_PRECISION)
		.mul(pegMultiplier)
		.div(PEG_PRECISION)
		.div(baseAssetReserves);
}

/** Which side of an AMM swap an amount is denominated in. */
export type AssetType = 'quote' | 'base';

/**
 * Calculates what the AMM's reserves would be after swapping a quote or base asset amount
 * against its constant-product curve (`sqrtK^2 = baseAssetReserve * quoteAssetReserve`). When
 * `inputAssetType` is `'quote'`, `swapAmount` is first converted from `QUOTE_PRECISION`-scale
 * quote units into the AMM's own quote-reserve precision via the peg multiplier before being
 * applied.
 * @param amm AMM state (`pegMultiplier`, `quoteAssetReserve`, `sqrtK`, `baseAssetReserve`).
 * @param inputAssetType Which side `swapAmount` is denominated in.
 * @param swapAmount Amount being swapped, QUOTE_PRECISION (1e6) if `inputAssetType` is `'quote'`, else AMM_RESERVE_PRECISION (1e9); must be non-negative.
 * @param swapDirection Whether `swapAmount` is added to or removed from the AMM's reserve on the input side.
 * @returns `[quoteAssetReserve, baseAssetReserve]` after the swap, both AMM_RESERVE_PRECISION (1e9).
 */
export function calculateAmmReservesAfterSwap(
	amm: Pick<
		AMM,
		'pegMultiplier' | 'quoteAssetReserve' | 'sqrtK' | 'baseAssetReserve'
	>,
	inputAssetType: AssetType,
	swapAmount: BN,
	swapDirection: SwapDirection
): [BN, BN] {
	assert(swapAmount.gte(ZERO), 'swapAmount must be greater than 0');

	let newQuoteAssetReserve;
	let newBaseAssetReserve;

	if (inputAssetType === 'quote') {
		swapAmount = swapAmount
			.mul(AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO)
			.div(amm.pegMultiplier);

		[newQuoteAssetReserve, newBaseAssetReserve] = calculateSwapOutput(
			amm.quoteAssetReserve,
			swapAmount,
			swapDirection,
			amm.sqrtK.mul(amm.sqrtK)
		);
	} else {
		[newBaseAssetReserve, newQuoteAssetReserve] = calculateSwapOutput(
			amm.baseAssetReserve,
			swapAmount,
			swapDirection,
			amm.sqrtK.mul(amm.sqrtK)
		);
	}

	return [newQuoteAssetReserve, newBaseAssetReserve];
}

/**
 * Calculates how much base asset the AMM could still absorb on each side before hitting its
 * configured reserve bounds — the AMM's own "open interest" available to bids/asks. Zeroes
 * out a side if its available room is less than half a step size (dust, not fillable), when
 * `stepSize` is provided.
 * @param baseAssetReserve AMM's current base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param minBaseAssetReserve AMM's minimum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param maxBaseAssetReserve AMM's maximum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param stepSize Optional order step size, AMM_RESERVE_PRECISION (1e9), used to zero out dust amounts.
 * @returns `[openBids, openAsks]`: `openBids` non-negative (room to absorb more longs), `openAsks` non-positive (room to absorb more shorts), both AMM_RESERVE_PRECISION (1e9).
 */
export function calculateMarketOpenBidAsk(
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN,
	stepSize?: BN
): [BN, BN] {
	// open orders
	let openAsks;
	if (minBaseAssetReserve.lt(baseAssetReserve)) {
		openAsks = baseAssetReserve.sub(minBaseAssetReserve).mul(new BN(-1));

		if (stepSize && openAsks.abs().div(TWO).lt(stepSize)) {
			openAsks = ZERO;
		}
	} else {
		openAsks = ZERO;
	}

	let openBids;
	if (maxBaseAssetReserve.gt(baseAssetReserve)) {
		openBids = maxBaseAssetReserve.sub(baseAssetReserve);

		if (stepSize && openBids.div(TWO).lt(stepSize)) {
			openBids = ZERO;
		}
	} else {
		openBids = ZERO;
	}

	return [openBids, openAsks];
}

/**
 * Measures how skewed the AMM's net inventory is relative to the thinner of its two
 * remaining liquidity sides, as a fraction: `|baseAssetAmountWithAmm| / minSideLiquidity`,
 * capped at 100%. Feeds `calculateInventoryScale`'s spread widening — a fuller inventory
 * relative to available liquidity widens the paying side's spread more.
 * @param baseAssetAmountWithAmm AMM's net inventory, AMM_RESERVE_PRECISION (1e9, signed).
 * @param baseAssetReserve AMM's current base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param minBaseAssetReserve AMM's minimum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param maxBaseAssetReserve AMM's maximum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @returns Inventory-to-min-side-liquidity ratio, PERCENTAGE_PRECISION (1e6), capped at 100%.
 */
export function calculateInventoryLiquidityRatio(
	baseAssetAmountWithAmm: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN
): BN {
	// inventory skew
	const [openBids, openAsks] = calculateMarketOpenBidAsk(
		baseAssetReserve,
		minBaseAssetReserve,
		maxBaseAssetReserve
	);

	const minSideLiquidity = BN.min(openBids.abs(), openAsks.abs());

	const inventoryScaleBN = BN.min(
		baseAssetAmountWithAmm
			.mul(PERCENTAGE_PRECISION)
			.div(BN.max(minSideLiquidity, ONE))
			.abs(),
		PERCENTAGE_PRECISION
	);
	return inventoryScaleBN;
}

/**
 * Same shape as `calculateInventoryLiquidityRatio` but normalizes by the *average* of the
 * two liquidity sides rather than the thinner side, used specifically as the liquidity
 * fraction input to `calculateReferencePriceOffset` (whose offset should react to overall
 * inventory pressure, not just the constraining side).
 * @param baseAssetAmountWithAmm AMM's net inventory, AMM_RESERVE_PRECISION (1e9, signed).
 * @param baseAssetReserve AMM's current base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param minBaseAssetReserve AMM's minimum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param maxBaseAssetReserve AMM's maximum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @returns Inventory-to-average-side-liquidity ratio, PERCENTAGE_PRECISION (1e6), capped at 100%.
 */
export function calculateInventoryLiquidityRatioForReferencePriceOffset(
	baseAssetAmountWithAmm: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN
): BN {
	// inventory skew
	const [openBids, openAsks] = calculateMarketOpenBidAsk(
		baseAssetReserve,
		minBaseAssetReserve,
		maxBaseAssetReserve
	);

	const avgSideLiquidity = openBids.abs().add(openAsks.abs()).div(TWO);

	const inventoryScaleBN = BN.min(
		baseAssetAmountWithAmm
			.mul(PERCENTAGE_PRECISION)
			.div(BN.max(avgSideLiquidity, ONE))
			.abs(),
		PERCENTAGE_PRECISION
	);
	return inventoryScaleBN;
}

/**
 * Multiplier applied to the paying side's spread based on inventory skew
 * (`calculateInventoryLiquidityRatio`), scaled so the multiplier never exceeds the greater of
 * a fixed 10x cap or the ratio between the market's max spread and the current directional
 * spread. Returns `1` (no scaling) for a flat AMM.
 * @param baseAssetAmountWithAmm AMM's net inventory, AMM_RESERVE_PRECISION (1e9, signed).
 * @param baseAssetReserve AMM's current base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param minBaseAssetReserve AMM's minimum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param maxBaseAssetReserve AMM's maximum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param directionalSpread The spread (in `BID_ASK_SPREAD_PRECISION` bps-like units) on the inventory's own side, before this scale is applied.
 * @param maxSpread Market's configured max spread, `BID_ASK_SPREAD_PRECISION`-scaled units.
 * @returns Plain multiplier (not BN) to apply to the directional spread, `>= 1`.
 */
export function calculateInventoryScale(
	baseAssetAmountWithAmm: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN,
	directionalSpread: number,
	maxSpread: number
): number {
	if (baseAssetAmountWithAmm.eq(ZERO)) {
		return 1;
	}

	const MAX_BID_ASK_INVENTORY_SKEW_FACTOR = BID_ASK_SPREAD_PRECISION.mul(
		new BN(10)
	);

	const inventoryScaleBN = calculateInventoryLiquidityRatio(
		baseAssetAmountWithAmm,
		baseAssetReserve,
		minBaseAssetReserve,
		maxBaseAssetReserve
	);

	const inventoryScaleMaxBN = BN.max(
		MAX_BID_ASK_INVENTORY_SKEW_FACTOR,
		new BN(maxSpread)
			.mul(BID_ASK_SPREAD_PRECISION)
			.div(new BN(Math.max(directionalSpread, 1)))
	);

	const inventoryScaleCapped =
		BN.min(
			inventoryScaleMaxBN,
			BID_ASK_SPREAD_PRECISION.add(
				inventoryScaleMaxBN.mul(inventoryScaleBN).div(PERCENTAGE_PRECISION)
			)
		).toNumber() / BID_ASK_SPREAD_PRECISION.toNumber();

	return inventoryScaleCapped;
}

/**
 * Calculates the AMM's reference-price offset, mirroring the program's
 * `calculate_reference_price_offset`: a shift applied to both bid and ask that makes the
 * inventory-reducing side cheaper and the inventory-increasing side pricier.
 *
 *   offset = sign(q) * maxOffsetPct * min(1, |liquidityFraction| / 10%)
 *
 * The size depends only on inventory: it grows linearly to `maxOffsetPct` at
 * `REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT` of open liquidity. The market premium only gates
 * it: three clamped mark/oracle premium estimates (5-minute, 1-hour, and a 24h-funding-implied
 * premium net of the `FUNDING_RATE_OFFSET_DENOMINATOR` baseline) are averaged, and the offset
 * applies only when that average is nonzero with the same sign as the inventory. Returns zero
 * if there's no funding history or no inventory skew.
 * @param reservePrice Current AMM reserve price, PRICE_PRECISION (1e6).
 * @param last24hAvgFundingRate Market's 24h average funding rate, FUNDING_RATE_PRECISION-buffer-scaled (divided internally by `FUNDING_RATE_BUFFER_PRECISION`).
 * @param liquidityFraction Signed inventory liquidity fraction (see `calculateInventoryLiquidityRatioForReferencePriceOffset`, sign-adjusted for inventory direction), PERCENTAGE_PRECISION (1e6).
 * @param oracleTwapFast 5-minute oracle TWAP, PRICE_PRECISION (1e6).
 * @param markTwapFast 5-minute mark TWAP, PRICE_PRECISION (1e6).
 * @param oracleTwapSlow 1-hour oracle TWAP, PRICE_PRECISION (1e6).
 * @param markTwapSlow 1-hour mark TWAP, PRICE_PRECISION (1e6).
 * @param maxOffsetPct Maximum allowed offset, PERCENTAGE_PRECISION (1e6) fraction-of-price units — both the intermediate premium clamps and the final result are bounded by this.
 * @returns Reference price offset, PERCENTAGE_PRECISION (1e6, signed), clamped to `±maxOffsetPct`.
 */
export function calculateReferencePriceOffset(
	reservePrice: BN,
	last24hAvgFundingRate: BN,
	liquidityFraction: BN,
	oracleTwapFast: BN,
	markTwapFast: BN,
	oracleTwapSlow: BN,
	markTwapSlow: BN,
	maxOffsetPct: number
): BN {
	if (last24hAvgFundingRate.eq(ZERO) || liquidityFraction.eq(ZERO)) {
		return ZERO;
	}

	const maxOffsetInPrice = new BN(maxOffsetPct)
		.mul(reservePrice)
		.div(PERCENTAGE_PRECISION);

	// Calculate quote denominated market premium
	const markPremiumMinute = clampBN(
		markTwapFast.sub(oracleTwapFast),
		maxOffsetInPrice.mul(new BN(-1)),
		maxOffsetInPrice
	);

	const markPremiumHour = clampBN(
		markTwapSlow.sub(oracleTwapSlow),
		maxOffsetInPrice.mul(new BN(-1)),
		maxOffsetInPrice
	);

	// Convert last24hAvgFundingRate to quote denominated premium
	const markPremiumDay = clampBN(
		last24hAvgFundingRate
			.div(FUNDING_RATE_BUFFER_PRECISION)
			.mul(new BN(24))
			.sub(oracleTwapSlow.abs().div(FUNDING_RATE_OFFSET_DENOMINATOR)),
		maxOffsetInPrice.mul(new BN(-1)),
		maxOffsetInPrice
	);

	// Take average clamped premium as the price-based offset
	const markPremiumAvg = markPremiumMinute
		.add(markPremiumHour)
		.add(markPremiumDay)
		.div(new BN(3));

	const markPremiumAvgPct = markPremiumAvg
		.mul(PRICE_PRECISION)
		.div(reservePrice);

	// Only apply when inventory is consistent with recent and 24h market premium
	if (!sigNum(liquidityFraction).eq(sigNum(markPremiumAvgPct))) {
		return ZERO;
	}

	// Size from inventory alone: linear up to REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT,
	// maxOffsetPct beyond it.
	return new BN(maxOffsetPct)
		.mul(
			BN.min(liquidityFraction.abs(), REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT)
		)
		.div(REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT)
		.mul(sigNum(liquidityFraction));
}

/**
 * Estimates how "levered" the AMM's own net position is relative to its retained equity —
 * the gap between the AMM's local (reserve-price-valued) exposure and its actual net
 * inventory value, divided by `totalFeeMinusDistributions`. Used to scale up the spread on
 * the inventory side when the AMM is thinly capitalized relative to its exposure (see
 * `calculateSpreadBN`'s `effectiveLeverageCapped` term).
 * @param baseSpread Market's configured base spread (unused directly here beyond being part of the caller's contract; kept for parity with the on-chain signature).
 * @param quoteAssetReserve AMM quote asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param terminalQuoteAssetReserve AMM terminal (post-close) quote asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param pegMultiplier AMM peg multiplier, PEG_PRECISION (1e6).
 * @param netBaseAssetAmount AMM's net inventory, AMM_RESERVE_PRECISION (1e9, signed).
 * @param reservePrice Current AMM reserve price, PRICE_PRECISION (1e6).
 * @param totalFeeMinusDistributions AMM's retained equity, QUOTE_PRECISION (1e6).
 * @returns Plain (unitless) effective leverage ratio, floored at 0.
 */
export function calculateEffectiveLeverage(
	baseSpread: number,
	quoteAssetReserve: BN,
	terminalQuoteAssetReserve: BN,
	pegMultiplier: BN,
	netBaseAssetAmount: BN,
	reservePrice: BN,
	totalFeeMinusDistributions: BN
): number {
	// vAMM skew
	const netBaseAssetValue = quoteAssetReserve
		.sub(terminalQuoteAssetReserve)
		.mul(pegMultiplier)
		.div(AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO);

	const localBaseAssetValue = netBaseAssetAmount
		.mul(reservePrice)
		.div(AMM_TO_QUOTE_PRECISION_RATIO.mul(PRICE_PRECISION));

	const effectiveGap = Math.max(
		0,
		localBaseAssetValue.sub(netBaseAssetValue).toNumber()
	);

	const effectiveLeverage =
		effectiveGap / (Math.max(0, totalFeeMinusDistributions.toNumber()) + 1) +
		1 / QUOTE_PRECISION.toNumber();

	return effectiveLeverage;
}

/**
 * Confidence contribution to the per-side volatility spread, mirroring the program's
 * `calculate_spread_conf_component`:
 *
 *   c = min(conf, conf / 20 + max(0, conf - LAZER_CONF_FLOOR_PCT))
 *
 * Lazer confidence never reports below the 20bp floor, so at the floor `c` is 1bp. Above it
 * every bp of real source disagreement adds about a bp, until `c` meets `conf` at 4% and follows
 * it from there. Continuous everywhere, with no threshold.
 * @param confidencePct Oracle confidence as a fraction of price, PERCENTAGE_PRECISION (1e6).
 * @returns The confidence contribution, PERCENTAGE_PRECISION (1e6).
 */
export function calculateSpreadConfComponent(confidencePct: BN): BN {
	return BN.min(
		confidencePct,
		confidencePct
			.div(SPREAD_CONF_DISCOUNT_DIVISOR)
			.add(BN.max(ZERO, confidencePct.sub(LAZER_CONF_FLOOR_PCT)))
	);
}

/**
 * Computes the volatility-driven component of the AMM's bid/ask spread, before inventory,
 * leverage, revenue-retreat, or funding-bias adjustments are layered on in `calculateSpreadBN`.
 * Blends the recent mark/oracle standard deviation (`markStd`, `oracleStd`) with oracle
 * confidence, then scales each side independently by that side's recent fill intensity
 * relative to 24h volume (a side that's been trading heavily gets a wider spread on that side).
 * Pyth Lazer confidence is floored at 20bp when posted, so the confidence counts at 1/20 weight
 * plus its excess over that floor at full weight (see `calculateSpreadConfComponent`), and the
 * vol base uses that discounted value rather than the raw confidence.
 * @param lastOracleConfPct Oracle confidence interval as a fraction of price, PERCENTAGE_PRECISION (1e6).
 * @param reservePrice Current AMM reserve price, PRICE_PRECISION (1e6).
 * @param markStd Recent mark price standard deviation, PRICE_PRECISION (1e6).
 * @param oracleStd Recent oracle price standard deviation, PRICE_PRECISION (1e6).
 * @param longIntensity Recent long-side fill volume intensity, BASE_PRECISION (1e9) or QUOTE_PRECISION depending on caller; only used relative to `volume24H`.
 * @param shortIntensity Recent short-side fill volume intensity, same units as `longIntensity`.
 * @param volume24H Trailing 24h volume, same units as `longIntensity`/`shortIntensity`.
 * @returns `[longVolSpread, shortVolSpread]`, both PERCENTAGE_PRECISION (1e6) fraction-of-price units.
 */
export function calculateVolSpreadBN(
	lastOracleConfPct: BN,
	reservePrice: BN,
	markStd: BN,
	oracleStd: BN,
	longIntensity: BN,
	shortIntensity: BN,
	volume24H: BN
): [BN, BN] {
	const marketAvgStdPct = markStd
		.add(oracleStd)
		.mul(PERCENTAGE_PRECISION)
		.div(reservePrice)
		.div(new BN(4));
	const confComponent = calculateSpreadConfComponent(lastOracleConfPct);
	const volSpread = BN.max(confComponent, marketAvgStdPct.div(new BN(2)));

	const clampMin = PERCENTAGE_PRECISION.div(new BN(100));
	const clampMax = PERCENTAGE_PRECISION;

	const longVolSpreadFactor = clampBN(
		longIntensity.mul(PERCENTAGE_PRECISION).div(BN.max(ONE, volume24H)),
		clampMin,
		clampMax
	);
	const shortVolSpreadFactor = clampBN(
		shortIntensity.mul(PERCENTAGE_PRECISION).div(BN.max(ONE, volume24H)),
		clampMin,
		clampMax
	);

	const longVolSpread = BN.max(
		confComponent,
		volSpread.mul(longVolSpreadFactor).div(PERCENTAGE_PRECISION)
	);
	const shortVolSpread = BN.max(
		confComponent,
		volSpread.mul(shortVolSpreadFactor).div(PERCENTAGE_PRECISION)
	);

	return [longVolSpread, shortVolSpread];
}

/**
 * Funding bias β(f) (BID_ASK_SPREAD_PRECISION): bounded multiplier for the
 * paying-side spread while the vAMM is paying funding. Mirrors the program's
 * `calculate_spread_funding_bias_scale`.
 *
 *   ρ(f) = clamp(|f| / f_ref, 0, 1),  f_ref = FUNDING_RATE_OFFSET_PERCENTAGE
 *   β(f) = 1 + s * ρ(f),              s = fundingBiasSensitivity / 100
 *
 * f = 24h avg funding rate normalized to a daily fraction of the oracle twap
 * captured at the last funding update. The vAMM pays when f * q < 0
 * (q = baseAssetAmountWithAmm). Returns 1x when the vAMM receives funding or
 * s = 0.
 */
export function calculateSpreadFundingBiasScale(
	baseAssetAmountWithAmm: BN,
	last24HAvgFundingRate: BN,
	lastFundingOracleTwap: BN,
	fundingBiasSensitivity: number
): number {
	const one = BID_ASK_SPREAD_PRECISION.toNumber();
	if (fundingBiasSensitivity === 0 || lastFundingOracleTwap.lte(ZERO)) {
		return one;
	}

	// f: daily funding rate as a fraction of price, FUNDING_RATE_PRECISION
	const fNorm = last24HAvgFundingRate
		.mul(PRICE_PRECISION)
		.div(lastFundingOracleTwap)
		.muln(24);

	// f * q >= 0: vAMM receives (or rate/inventory is zero), β = 1
	if (fNorm.isZero() || baseAssetAmountWithAmm.isZero()) {
		return one;
	}
	if (fNorm.isNeg() === baseAssetAmountWithAmm.isNeg()) {
		return one;
	}

	// ρ = clamp(|f| / f_ref, 0, 1), PERCENTAGE_PRECISION
	const ramp = BN.min(
		fNorm.abs().mul(PERCENTAGE_PRECISION).div(FUNDING_RATE_OFFSET_PERCENTAGE),
		PERCENTAGE_PRECISION
	).toNumber();

	// β = 1 + s * ρ
	return one + Math.floor((fundingBiasSensitivity * ramp) / 100);
}

/**
 * Full intermediate breakdown of `calculateSpreadBN`'s pipeline, returned instead of the plain
 * `[longSpread, shortSpread]` tuple when `returnTerms` is `true` — useful for debugging/UI
 * display of how each stage (volatility, peg-adjustment floor, inventory scale, effective
 * leverage, revenue retreat, funding bias, max-spread clamp) contributed to the final spread.
 * All numeric fields are plain numbers in `BID_ASK_SPREAD_PRECISION`/`PERCENTAGE_PRECISION`
 * (1e6) fraction-of-price units except `effectiveLeverage`/`effectiveLeverageCapped`
 * (unitless ratios) and `inventorySpreadScale`/`fundingBiasScale` (unitless multipliers,
 * `fundingBiasScale` additionally pre-scaled by `BID_ASK_SPREAD_PRECISION`).
 */
export interface SpreadTerms {
	longVolSpread: number;
	shortVolSpread: number;
	longSpreadwPS: number;
	shortSpreadwPS: number;
	maxTargetSpread: number;
	inventorySpreadScale: number;
	longSpreadwInvScale: number;
	shortSpreadwInvScale: number;
	effectiveLeverage: number;
	effectiveLeverageCapped: number;
	longSpreadwEL: number;
	shortSpreadwEL: number;
	revenueRetreatAmount: number;
	halfRevenueRetreatAmount: number;
	longSpreadwRevRetreat: number;
	shortSpreadwRevRetreat: number;
	fundingBiasScale: number;
	longSpreadwFundingBias: number;
	shortSpreadwFundingBias: number;
	longSpreadwOffsetShrink: number;
	shortSpreadwOffsetShrink: number;
	totalSpread: number;
	longSpread: number;
	shortSpread: number;
}

/**
 * Computes the AMM's directional (long/short) bid-ask spread, mirroring `calculate_spread` in
 * `vlp/amm/math/amm_spread.rs`. Pipeline: start from `calculateVolSpreadBN`'s volatility
 * spread, widen whichever side is on the far side of the oracle-vs-reserve price gap
 * (`lastOracleReservePriceSpreadPct`), apply inventory skew scaling
 * (`calculateInventoryScale`), scale by effective leverage when the AMM has positive retained
 * equity (or a flat 10x when it doesn't), add a revenue-retreat widening when
 * `netRevenueSinceLastFunding` is below the default retreat threshold, apply the funding-bias
 * multiplier (`calculateSpreadFundingBiasScale`) to the paying side, apply the market's manual
 * `ammInventorySpreadAdjustment` (%, shrink if negative/grow if positive), then clamp
 * `longSpread + shortSpread` to `maxTargetSpread` (proportionally rebalancing whichever side is
 * larger). Pass `returnTerms: true` to get the full `SpreadTerms` breakdown instead of just the
 * final tuple.
 * @param baseSpread Market's configured base spread floor (each side gets at least half), BID_ASK_SPREAD_PRECISION (1e6).
 * @param lastOracleReservePriceSpreadPct Signed reserve-price-vs-oracle gap, BID_ASK_SPREAD_PRECISION (1e6); widens the side the reserve price is away from the oracle.
 * @param lastOracleConfPct Oracle confidence interval as a fraction of price, PERCENTAGE_PRECISION (1e6).
 * @param maxSpread Market's configured max total spread, BID_ASK_SPREAD_PRECISION (1e6).
 * @param quoteAssetReserve AMM quote asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param terminalQuoteAssetReserve AMM terminal (post-close) quote asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param pegMultiplier AMM peg multiplier, PEG_PRECISION (1e6).
 * @param baseAssetAmountWithAmm AMM's net inventory, AMM_RESERVE_PRECISION (1e9, signed).
 * @param reservePrice Current AMM reserve price, PRICE_PRECISION (1e6).
 * @param totalFeeMinusDistributions AMM's retained equity, QUOTE_PRECISION (1e6).
 * @param netRevenueSinceLastFunding Net revenue accrued since the last funding update, QUOTE_PRECISION (1e6, signed); below `DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT` triggers a spread widening.
 * @param baseAssetReserve AMM's current base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param minBaseAssetReserve AMM's minimum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param maxBaseAssetReserve AMM's maximum allowed base asset reserve, AMM_RESERVE_PRECISION (1e9).
 * @param markStd Recent mark price standard deviation, PRICE_PRECISION (1e6).
 * @param oracleStd Recent oracle price standard deviation, PRICE_PRECISION (1e6).
 * @param longIntensity Recent long-side fill volume intensity (see `calculateVolSpreadBN`).
 * @param shortIntensity Recent short-side fill volume intensity (see `calculateVolSpreadBN`).
 * @param volume24H Trailing 24h volume, same units as `longIntensity`/`shortIntensity`.
 * @param ammInventorySpreadAdjustment Market's manual spread adjustment, percent (-100..100); negative shrinks, positive grows.
 * @param last24HAvgFundingRate Market's 24h average funding rate, forwarded to `calculateSpreadFundingBiasScale`; defaults to zero (no funding bias).
 * @param lastFundingOracleTwap Oracle TWAP captured at the last funding update, forwarded to `calculateSpreadFundingBiasScale`; defaults to zero.
 * @param fundingBiasSensitivity Market's funding-bias sensitivity setting (0-100); defaults to 0 (disabled).
 * @param returnTerms When omitted/`false`, returns the `[longSpread, shortSpread]` tuple; when `true`, returns the full `SpreadTerms` breakdown instead.
 * @returns `[longSpread, shortSpread]`, both BID_ASK_SPREAD_PRECISION (1e6) fraction-of-price units.
 */
export function calculateSpreadBN(
	baseSpread: number,
	lastOracleReservePriceSpreadPct: BN,
	lastOracleConfPct: BN,
	maxSpread: number,
	quoteAssetReserve: BN,
	terminalQuoteAssetReserve: BN,
	pegMultiplier: BN,
	baseAssetAmountWithAmm: BN,
	reservePrice: BN,
	totalFeeMinusDistributions: BN,
	netRevenueSinceLastFunding: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN,
	markStd: BN,
	oracleStd: BN,
	longIntensity: BN,
	shortIntensity: BN,
	volume24H: BN,
	ammInventorySpreadAdjustment: number,
	last24HAvgFundingRate?: BN,
	lastFundingOracleTwap?: BN,
	fundingBiasSensitivity?: number,
	returnTerms?: false
): [number, number];
/**
 * Same computation as the tuple-returning `calculateSpreadBN` overload, but with `returnTerms`
 * forced to `true` so it returns the full `SpreadTerms` breakdown of every pipeline stage
 * instead of just the final `[longSpread, shortSpread]`.
 * @returns The full `SpreadTerms` breakdown, BID_ASK_SPREAD_PRECISION/PERCENTAGE_PRECISION (1e6) units per field (see `SpreadTerms`).
 */
export function calculateSpreadBN(
	baseSpread: number,
	lastOracleReservePriceSpreadPct: BN,
	lastOracleConfPct: BN,
	maxSpread: number,
	quoteAssetReserve: BN,
	terminalQuoteAssetReserve: BN,
	pegMultiplier: BN,
	baseAssetAmountWithAmm: BN,
	reservePrice: BN,
	totalFeeMinusDistributions: BN,
	netRevenueSinceLastFunding: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN,
	markStd: BN,
	oracleStd: BN,
	longIntensity: BN,
	shortIntensity: BN,
	volume24H: BN,
	ammInventorySpreadAdjustment: number,
	last24HAvgFundingRate: BN,
	lastFundingOracleTwap: BN,
	fundingBiasSensitivity: number,
	returnTerms: true
): SpreadTerms;
export function calculateSpreadBN(
	baseSpread: number,
	lastOracleReservePriceSpreadPct: BN,
	lastOracleConfPct: BN,
	maxSpread: number,
	quoteAssetReserve: BN,
	terminalQuoteAssetReserve: BN,
	pegMultiplier: BN,
	baseAssetAmountWithAmm: BN,
	reservePrice: BN,
	totalFeeMinusDistributions: BN,
	netRevenueSinceLastFunding: BN,
	baseAssetReserve: BN,
	minBaseAssetReserve: BN,
	maxBaseAssetReserve: BN,
	markStd: BN,
	oracleStd: BN,
	longIntensity: BN,
	shortIntensity: BN,
	volume24H: BN,
	ammInventorySpreadAdjustment: number,
	last24HAvgFundingRate: BN = ZERO,
	lastFundingOracleTwap: BN = ZERO,
	fundingBiasSensitivity = 0,
	returnTerms = false
): [number, number] | SpreadTerms {
	assert(Number.isInteger(baseSpread));
	assert(Number.isInteger(maxSpread));

	const spreadTerms = {
		longVolSpread: 0,
		shortVolSpread: 0,
		longSpreadwPS: 0,
		shortSpreadwPS: 0,
		maxTargetSpread: 0,
		inventorySpreadScale: 0,
		longSpreadwInvScale: 0,
		shortSpreadwInvScale: 0,
		effectiveLeverage: 0,
		effectiveLeverageCapped: 0,
		longSpreadwEL: 0,
		shortSpreadwEL: 0,
		revenueRetreatAmount: 0,
		halfRevenueRetreatAmount: 0,
		longSpreadwRevRetreat: 0,
		shortSpreadwRevRetreat: 0,
		fundingBiasScale: 0,
		longSpreadwFundingBias: 0,
		shortSpreadwFundingBias: 0,
		longSpreadwOffsetShrink: 0,
		shortSpreadwOffsetShrink: 0,
		totalSpread: 0,
		longSpread: 0,
		shortSpread: 0,
	};

	const [longVolSpread, shortVolSpread] = calculateVolSpreadBN(
		lastOracleConfPct,
		reservePrice,
		markStd,
		oracleStd,
		longIntensity,
		shortIntensity,
		volume24H
	);

	spreadTerms.longVolSpread = longVolSpread.toNumber();
	spreadTerms.shortVolSpread = shortVolSpread.toNumber();

	const precision = BID_ASK_SPREAD_PRECISION.toNumber();
	// Keep the raw divergence for the divergence requirement below, but bound the
	// value the quote math consumes (as the program does).
	const rawDivergence = lastOracleReservePriceSpreadPct.toNumber();
	const divergence = Math.min(Math.max(rawDivergence, -precision), precision);

	// 1: max(base/2, v) per side. base_spread / 2 is integer division on chain.
	const halfBaseSpread = Math.floor(baseSpread / 2);
	const floorLong = Math.max(halfBaseSpread, longVolSpread.toNumber());
	const floorShort = Math.max(halfBaseSpread, shortVolSpread.toNumber());
	let longSpread = floorLong;
	let shortSpread = floorShort;
	// directional widening that discourages inventory-growing flow, per side
	let steeringLong = 0;
	let steeringShort = 0;

	// w_max: dynamic ceiling; divergence and vol can raise it above maxSpread.
	const maxSpreadBaseline = Math.max(
		Math.abs(divergence),
		Math.min(
			Math.max(
				lastOracleConfPct.muln(2).toNumber(),
				BN.max(markStd, oracleStd)
					.mul(PERCENTAGE_PRECISION)
					.div(reservePrice)
					.toNumber()
			),
			precision
		)
	);
	const maxTargetSpread: number = Math.floor(
		Math.max(maxSpread, maxSpreadBaseline)
	);
	spreadTerms.maxTargetSpread = maxTargetSpread;

	// 2: oracle retreat: the side facing the divergence floors at |gap| + v.
	if (divergence < 0) {
		longSpread = Math.max(
			longSpread,
			Math.abs(divergence) + longVolSpread.toNumber()
		);
	} else if (divergence > 0) {
		shortSpread = Math.max(
			shortSpread,
			Math.abs(divergence) + shortVolSpread.toNumber()
		);
	}
	spreadTerms.longSpreadwPS = longSpread;
	spreadTerms.shortSpreadwPS = shortSpread;

	// The side whose fills grow the pool's net position.
	const loadedLong = baseAssetAmountWithAmm.gt(ZERO);
	const loadedShort = baseAssetAmountWithAmm.lt(ZERO);
	// Scale the loaded side by `factor` (a plain multiplier) the way the program
	// does, `spread * factor_int / PRECISION` in integers, and record the
	// positive change as steering.
	const scaleLoaded = (factor: number) => {
		const factorInt = Math.round(factor * precision);
		if (loadedLong) {
			const before = longSpread;
			longSpread = Math.floor((longSpread * factorInt) / precision);
			steeringLong += Math.max(0, longSpread - before);
		} else if (loadedShort) {
			const before = shortSpread;
			shortSpread = Math.floor((shortSpread * factorInt) / precision);
			steeringShort += Math.max(0, shortSpread - before);
		}
	};

	// 3: x sigma(q): inventory scale, loaded side only.
	const inventorySpreadScale = calculateInventoryScale(
		baseAssetAmountWithAmm,
		baseAssetReserve,
		minBaseAssetReserve,
		maxBaseAssetReserve,
		loadedLong ? longSpread : shortSpread,
		maxTargetSpread
	);
	scaleLoaded(inventorySpreadScale);
	spreadTerms.inventorySpreadScale = inventorySpreadScale;
	spreadTerms.longSpreadwInvScale = longSpread;
	spreadTerms.shortSpreadwInvScale = shortSpread;

	// 4: x lambda(q): leverage scale vs fee cushion, loaded side only; both
	// sides x 10 when the fee cushion is empty.
	const MAX_SPREAD_SCALE = 10;
	const feeCushionEmpty = totalFeeMinusDistributions.lte(ZERO);
	if (feeCushionEmpty) {
		longSpread *= MAX_SPREAD_SCALE;
		shortSpread *= MAX_SPREAD_SCALE;
	} else {
		const effectiveLeverage = calculateEffectiveLeverage(
			baseSpread,
			quoteAssetReserve,
			terminalQuoteAssetReserve,
			pegMultiplier,
			baseAssetAmountWithAmm,
			reservePrice,
			totalFeeMinusDistributions
		);
		spreadTerms.effectiveLeverage = effectiveLeverage;
		const spreadScale = Math.min(MAX_SPREAD_SCALE, 1 + effectiveLeverage);
		spreadTerms.effectiveLeverageCapped = spreadScale;
		scaleLoaded(spreadScale);
	}
	spreadTerms.longSpreadwEL = longSpread;
	spreadTerms.shortSpreadwEL = shortSpread;

	// 5: + r: revenue retreat, full on the loaded side, half on the other.
	if (
		netRevenueSinceLastFunding.lt(
			DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT
		)
	) {
		const maxRetreat = Math.floor(maxTargetSpread / 10);
		let revenueRetreatAmount = maxRetreat;
		if (
			netRevenueSinceLastFunding.gte(
				DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT.mul(new BN(1000))
			)
		) {
			revenueRetreatAmount = Math.min(
				maxRetreat,
				Math.floor(
					(baseSpread * netRevenueSinceLastFunding.abs().toNumber()) /
						DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT.abs().toNumber()
				)
			);
		}

		const halfRevenueRetreatAmount = Math.floor(revenueRetreatAmount / 2);
		const directionalRetreat = revenueRetreatAmount - halfRevenueRetreatAmount;

		spreadTerms.revenueRetreatAmount = revenueRetreatAmount;
		spreadTerms.halfRevenueRetreatAmount = halfRevenueRetreatAmount;

		if (loadedLong) {
			longSpread += revenueRetreatAmount;
			shortSpread += halfRevenueRetreatAmount;
			steeringLong += directionalRetreat;
		} else if (loadedShort) {
			longSpread += halfRevenueRetreatAmount;
			shortSpread += revenueRetreatAmount;
			steeringShort += directionalRetreat;
		} else {
			longSpread += halfRevenueRetreatAmount;
			shortSpread += halfRevenueRetreatAmount;
		}
	}
	spreadTerms.longSpreadwRevRetreat = longSpread;
	spreadTerms.shortSpreadwRevRetreat = shortSpread;

	// 6: x beta(f): funding bias on the loaded side while the vAMM pays funding.
	const fundingBiasScale = calculateSpreadFundingBiasScale(
		baseAssetAmountWithAmm,
		last24HAvgFundingRate,
		lastFundingOracleTwap,
		fundingBiasSensitivity
	);
	if (fundingBiasScale > precision) {
		scaleLoaded(fundingBiasScale / precision);
	}
	spreadTerms.fundingBiasScale = fundingBiasScale;
	spreadTerms.longSpreadwFundingBias = longSpread;
	spreadTerms.shortSpreadwFundingBias = shortSpread;

	// 7: x tilt gain: ammInventorySpreadAdjustment, floored at max(base/2, v).
	if (ammInventorySpreadAdjustment < 0) {
		const adjustment = Math.abs(ammInventorySpreadAdjustment);
		longSpread = Math.max(
			floorLong,
			Math.max(1, longSpread - Math.floor((longSpread * adjustment) / 100))
		);
		shortSpread = Math.max(
			floorShort,
			Math.max(1, shortSpread - Math.floor((shortSpread * adjustment) / 100))
		);
	} else if (ammInventorySpreadAdjustment > 0) {
		const adjustment = ammInventorySpreadAdjustment;
		longSpread = Math.max(
			floorLong,
			Math.max(1, longSpread + Math.ceil((longSpread * adjustment) / 100))
		);
		shortSpread = Math.max(
			floorShort,
			Math.max(1, shortSpread + Math.ceil((shortSpread * adjustment) / 100))
		);
	}

	// 8: cap at the dynamic ceiling. With an empty fee cushion the program scales
	// proportionally; otherwise it caps by safety priority: divergence, then the
	// base/vol floor, then steering, then the remaining padding.
	const totalSpread = longSpread + shortSpread;
	let capped: [number, number];
	if (feeCushionEmpty) {
		capped = capToMaxSpread(longSpread, shortSpread, maxTargetSpread);
	} else {
		capped = capSpreadOrdered(
			[longSpread, shortSpread],
			[
				rawDivergence < 0 ? Math.abs(rawDivergence) : 0,
				rawDivergence > 0 ? rawDivergence : 0,
			],
			[floorLong, floorShort],
			[steeringLong, steeringShort],
			maxTargetSpread
		);
	}
	[longSpread, shortSpread] = capped;

	spreadTerms.totalSpread = totalSpread;
	spreadTerms.longSpread = longSpread;
	spreadTerms.shortSpread = shortSpread;
	if (returnTerms) {
		return spreadTerms;
	}
	return [longSpread, shortSpread];
}

/**
 * Proportionally squeezes a two-sided spread whose total exceeds `maxSpread`, mirroring the
 * program's `cap_to_max_spread`: the larger side is scaled to `maxSpread / total` with a ceiling
 * division and the smaller side takes the remainder.
 */
function capToMaxSpread(
	longSpread: number,
	shortSpread: number,
	maxSpread: number
): [number, number] {
	const total = longSpread + shortSpread;
	if (total <= maxSpread) {
		return [longSpread, shortSpread];
	}
	if (longSpread > shortSpread) {
		const long = Math.ceil((longSpread * maxSpread) / total);
		return [long, maxSpread - long];
	}
	const short = Math.ceil((shortSpread * maxSpread) / total);
	return [maxSpread - short, short];
}

/**
 * Caps a spread pair by safety priority, mirroring the program's
 * `SpreadComponents::from_raw(..).cap_total_ordered(..)`. The raw pair is split into
 * divergence, base/vol floor, steering and residual padding (each claiming what remains after
 * the one before), and when the total is over `maxTotal` each layer gets room in that order; a
 * layer that only partly fits is squeezed within itself and later layers get nothing.
 */
function capSpreadOrdered(
	raw: [number, number],
	divergenceRequired: [number, number],
	floorRequired: [number, number],
	steeringAdded: [number, number],
	maxTotal: number
): [number, number] {
	if (raw[0] + raw[1] <= maxTotal) {
		return raw;
	}
	const layers: [number, number][] = [];
	let remainingRaw: [number, number] = [raw[0], raw[1]];
	for (const required of [divergenceRequired, floorRequired, steeringAdded]) {
		const layer: [number, number] = [
			Math.min(remainingRaw[0], required[0]),
			Math.min(remainingRaw[1], required[1]),
		];
		layers.push(layer);
		remainingRaw = [remainingRaw[0] - layer[0], remainingRaw[1] - layer[1]];
	}
	layers.push(remainingRaw);

	const result: [number, number] = [0, 0];
	let remaining = maxTotal;
	for (const layer of layers) {
		if (remaining === 0) {
			break;
		}
		const allocated = capToMaxSpread(layer[0], layer[1], remaining);
		result[0] += allocated[0];
		result[1] += allocated[1];
		remaining -= allocated[0] + allocated[1];
	}
	return result;
}

/**
 * Applies the market's manual `ammSpreadAdjustment` (%, grow if positive / shrink if
 * negative, floored at 1) to an already-computed spread pair. Mirrors the tail of
 * `update_spreads` in `vlp/amm/math/spread.rs`, which applies it to the frozen-curve
 * branch as well as the dynamic one, and rounds as integer math (ceil when growing,
 * floor when shrinking).
 * @param amm AMM state holding `ammSpreadAdjustment`.
 * @param longSpread Long-side spread before adjustment.
 * @param shortSpread Short-side spread before adjustment.
 * @returns `[longSpread, shortSpread]` after adjustment.
 */
function applyAmmSpreadAdjustment(
	amm: AMM,
	longSpread: number,
	shortSpread: number
): [number, number] {
	if (amm.ammSpreadAdjustment > 0) {
		const grow = (spread: number) =>
			Math.max(spread + Math.ceil((spread * amm.ammSpreadAdjustment) / 100), 1);
		return [grow(longSpread), grow(shortSpread)];
	}
	if (amm.ammSpreadAdjustment < 0) {
		const shrink = (spread: number) =>
			Math.max(
				spread - Math.floor((spread * -amm.ammSpreadAdjustment) / 100),
				1
			);
		return [shrink(longSpread), shrink(shortSpread)];
	}
	return [longSpread, shortSpread];
}

/**
 * Convenience wrapper around `calculateSpreadBN` that derives its lower-level inputs
 * (reserve price, oracle-vs-reserve spread, live oracle std, and confidence interval) from
 * `amm`/`marketStats`/`oraclePriceData` directly, then applies the market's manual
 * `ammSpreadAdjustment` (%, shrink if negative/grow if positive, floored at 1) on top.
 * Mirrors `update_spreads` in `vlp/amm/math/spread.rs`: the dynamic spread is computed
 * whenever `curveUpdateIntensity` is nonzero, and only a zero `curveUpdateIntensity`
 * falls back to `[baseSpread/2, baseSpread/2]` (truncated).
 * @param amm AMM state to price the spread for.
 * @param marketStats Market stats needed for volatility/funding-bias inputs.
 * @param oraclePriceData Current oracle price data; required unless `curveUpdateIntensity` is zero.
 * @param now Current unix timestamp (seconds); defaults to wall-clock time if omitted.
 * @param reservePrice Current AMM reserve price, PRICE_PRECISION (1e6); computed from `amm`'s reserves if omitted.
 * @throws if `oraclePriceData` is omitted while `curveUpdateIntensity` is nonzero.
 * @returns `[longSpread, shortSpread]`, both BID_ASK_SPREAD_PRECISION (1e6) fraction-of-price units.
 */
export function calculateSpread(
	amm: AMM,
	marketStats: MarketStats,
	oraclePriceData?: Pick<OraclePriceData, 'price' | 'confidence'>,
	now?: BN,
	reservePrice?: BN
): [number, number] {
	// On chain the dynamic spread is computed whenever curveUpdateIntensity > 0; a
	// baseSpread of 0 does NOT disable it, it only lowers the floor that the vol
	// spread is maxed against. Short-circuiting on baseSpread == 0 made the SDK
	// report a zero-width vAMM spread on markets configured with baseSpread 0,
	// while the program was quoting an inventory-skewed spread of >10%.
	if (!reservePrice) {
		reservePrice = calculatePrice(
			amm.baseAssetReserve,
			amm.quoteAssetReserve,
			amm.pegMultiplier
		);
	}

	if (amm.curveUpdateIntensity == 0) {
		// integer division on chain: `base_spread.safe_div(2)`. A market with
		// curveUpdateIntensity 0 never repegs and quotes off its curve alone, so
		// neither the oracle retreat nor the oracle guard applies.
		const halfBaseSpread = Math.floor(amm.baseSpread / 2);
		return applyAmmSpreadAdjustment(amm, halfBaseSpread, halfBaseSpread);
	}

	if (!oraclePriceData) {
		throw new Error(
			'calculateSpread: oraclePriceData is required when curveUpdateIntensity is nonzero'
		);
	}

	const targetPrice = oraclePriceData?.price || reservePrice;
	const targetMarkSpreadPct = reservePrice
		.sub(targetPrice)
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(reservePrice);

	now = now || new BN(new Date().getTime() / 1000); //todo
	const liveOracleStd = calculateLiveOracleStd(
		marketStats,
		oraclePriceData,
		now
	);
	const confIntervalPct = getNewOracleConfPct(
		marketStats,
		oraclePriceData,
		reservePrice,
		now
	);

	const spreads = calculateSpreadBN(
		amm.baseSpread,
		targetMarkSpreadPct,
		confIntervalPct,
		amm.maxSpread,
		amm.quoteAssetReserve,
		amm.terminalQuoteAssetReserve,
		amm.pegMultiplier,
		amm.baseAssetAmountWithAmm,
		reservePrice,
		amm.totalFeeMinusDistributions,
		amm.netRevenueSinceLastFunding,
		amm.baseAssetReserve,
		amm.minBaseAssetReserve,
		amm.maxBaseAssetReserve,
		marketStats.markStd,
		liveOracleStd,
		marketStats.longIntensityVolume,
		marketStats.shortIntensityVolume,
		marketStats.volume24H,
		amm.ammInventorySpreadAdjustment,
		marketStats.last24HAvgFundingRate,
		marketStats.lastFundingOracleTwap,
		amm.fundingBiasSensitivity
	);
	const [longSpread, shortSpread] = applyAmmSpreadAdjustment(
		amm,
		spreads[0],
		spreads[1]
	);
	return applyOracleGuard(
		longSpread,
		shortSpread,
		calculateReferencePriceOffsetForAmm(amm, marketStats, reservePrice),
		reservePrice,
		oraclePriceData.price
	);
}

/**
 * Keeps the final quote on the correct side of the oracle, mirroring the program's
 * `apply_oracle_guard`: the bid at or below the oracle and the ask at or above it, so the AMM
 * never quotes a price the market can immediately arbitrage against it. Only ever widens a
 * spread.
 *
 * A side quotes at `reservePrice * (1 + s / 2)^2` for its signed composite spread `s`. With
 * `r = sqrt(oracle / reservePrice)`, the bid needs `short >= offset + 2 * (1 - r)` and the ask
 * needs `long >= 2 * (r - 1) - offset`; `r` is rounded down for the bid and up for the ask, and
 * a binding requirement gets one extra unit for reserve rounding.
 * @param longSpread Long (ask) spread, BID_ASK_SPREAD_PRECISION (1e6).
 * @param shortSpread Short (bid) spread, BID_ASK_SPREAD_PRECISION (1e6).
 * @param referencePriceOffset Reference price offset applied to both quotes, BID_ASK_SPREAD_PRECISION (1e6, signed).
 * @param reservePrice AMM reserve price, PRICE_PRECISION (1e6).
 * @param oraclePrice Oracle price the quotes must not cross, PRICE_PRECISION (1e6).
 * @returns `[longSpread, shortSpread]` after the guard.
 */
export function applyOracleGuard(
	longSpread: number,
	shortSpread: number,
	referencePriceOffset: number,
	reservePrice: BN,
	oraclePrice: BN
): [number, number] {
	if (oraclePrice.lte(ZERO) || reservePrice.lte(ZERO)) {
		return [longSpread, shortSpread];
	}

	const numerator = oraclePrice
		.mul(BID_ASK_SPREAD_PRECISION)
		.mul(BID_ASK_SPREAD_PRECISION);
	const scaledRatio = numerator.div(reservePrice);
	const ratioIsExact = scaledRatio.mul(reservePrice).eq(numerator);
	const rFloor = squareRootBN(scaledRatio);
	const rIsExact = ratioIsExact && rFloor.mul(rFloor).eq(scaledRatio);
	const rCeil = rIsExact ? rFloor : rFloor.add(ONE);

	const precision = BID_ASK_SPREAD_PRECISION;
	const offset = new BN(referencePriceOffset);
	const withMargin = (requirement: BN) =>
		requirement.gt(ZERO) ? requirement.add(ONE) : requirement;
	const minShort = withMargin(offset.add(precision.sub(rFloor).muln(2)));
	const minLong = withMargin(rCeil.sub(precision).muln(2).sub(offset));

	let long = longSpread;
	let short = shortSpread;
	if (minShort.gtn(short)) {
		short = Math.max(
			short,
			Math.min(minShort.toNumber(), precision.toNumber() - long)
		);
	}
	if (minLong.gtn(long)) {
		long = Math.max(
			long,
			Math.min(minLong.toNumber(), precision.toNumber() - short)
		);
	}
	return [long, short];
}

/**
 * Computes the AMM's reference price offset from its state, mirroring the offset step of the
 * program's `compute_quote_state`: zero unless `curveUpdateIntensity > 100`, otherwise
 * `calculateReferencePriceOffset` on the inventory's share of average open liquidity after the
 * configured deadband.
 * @param amm AMM state.
 * @param marketStats Market stats holding the premium inputs.
 * @param reservePrice AMM reserve price, PRICE_PRECISION (1e6).
 * @returns Reference price offset, BID_ASK_SPREAD_PRECISION (1e6, signed).
 */
export function calculateReferencePriceOffsetForAmm(
	amm: AMM,
	marketStats: MarketStats,
	reservePrice: BN
): number {
	if (amm.curveUpdateIntensity <= 100) {
		return 0;
	}

	// always allow 10 bps of price offset, up to a half of the market's max_spread
	let maxOffset: number;
	if (amm.curveUpdateIntensity >= 200) {
		maxOffset = Math.max(Math.floor(amm.maxSpread / 2), 10_000);
	} else {
		maxOffset = Math.min(
			Math.floor(amm.maxSpread / 2),
			(PERCENTAGE_PRECISION.toNumber() / 10000) *
				(amm.curveUpdateIntensity - 100)
		);
	}

	const liquidityFraction =
		calculateInventoryLiquidityRatioForReferencePriceOffset(
			amm.baseAssetAmountWithAmm,
			amm.baseAssetReserve,
			amm.minBaseAssetReserve,
			amm.maxBaseAssetReserve
		);
	const liquidityFractionSigned = liquidityFraction.mul(
		sigNum(amm.baseAssetAmountWithAmm)
	);

	let liquidityFractionAfterDeadband = liquidityFractionSigned;
	const deadbandPct = amm.referencePriceOffsetDeadbandPct
		? PERCENTAGE_PRECISION.mul(
				new BN(amm.referencePriceOffsetDeadbandPct as number)
		  ).divn(100)
		: ZERO;
	if (!liquidityFractionAfterDeadband.eq(ZERO) && deadbandPct.gt(ZERO)) {
		const abs = liquidityFractionAfterDeadband.abs();
		if (abs.lte(deadbandPct)) {
			liquidityFractionAfterDeadband = ZERO;
		} else {
			liquidityFractionAfterDeadband = liquidityFractionAfterDeadband.sub(
				deadbandPct.mul(sigNum(liquidityFractionAfterDeadband))
			);
		}
	}

	return calculateReferencePriceOffset(
		reservePrice,
		marketStats.last24HAvgFundingRate,
		liquidityFractionAfterDeadband,
		marketStats.historicalOracleData.lastOraclePriceTwap5Min,
		marketStats.lastMarkPriceTwap5Min,
		marketStats.historicalOracleData.lastOraclePriceTwap,
		marketStats.lastMarkPriceTwap,
		maxOffset
	).toNumber();
}

/**
 * Computes the AMM's one-sided bid and ask reserves — the reserves a long (ask side) or short
 * (bid side) trade would actually execute against — mirroring the program's
 * `refresh_cached_spread_reserves`. The spreads come from `calculateSpread` (which already
 * applies the oracle guard), and both sides are shifted by the reference price offset
 * (`calculateReferencePriceOffsetForAmm`, active only when `curveUpdateIntensity > 100`).
 * @param amm AMM state to derive spread reserves for.
 * @param marketStats Market stats needed for spread and reference-price-offset calculation.
 * @param mmOraclePriceData Current MM oracle price data, forwarded to `calculateSpread`.
 * @param now Current unix timestamp (seconds), forwarded to `calculateSpread`.
 * @returns `[bidReserves, askReserves]`, each `{ baseAssetReserve, quoteAssetReserve }` in AMM_RESERVE_PRECISION (1e9).
 */
export function calculateSpreadReserves(
	amm: AMM,
	marketStats: MarketStats,
	mmOraclePriceData?: Pick<MMOraclePriceData, 'price' | 'confidence'>,
	now?: BN
) {
	// The signed composite spread s moves the quote reserve by exactly
	// quote * s / (2 * BID_ASK_SPREAD_PRECISION); the base reserve follows from
	// k = sqrtK^2. Mirrors `compute_spread_reserves_for_direction`.
	function calculateSpreadReserve(
		spread: number,
		amm: AMM
	): {
		baseAssetReserve: BN;
		quoteAssetReserve: BN;
	} {
		// BN division truncates toward zero, like the program's i128 division.
		const quoteAssetReserveDelta = amm.quoteAssetReserve
			.mul(new BN(spread))
			.div(BID_ASK_SPREAD_PRECISION.muln(2));
		const quoteAssetReserve = amm.quoteAssetReserve.add(quoteAssetReserveDelta);
		const baseAssetReserve = amm.sqrtK.mul(amm.sqrtK).div(quoteAssetReserve);
		return {
			baseAssetReserve,
			quoteAssetReserve,
		};
	}

	const reservePrice = calculatePrice(
		amm.baseAssetReserve,
		amm.quoteAssetReserve,
		amm.pegMultiplier
	);

	const referencePriceOffset = calculateReferencePriceOffsetForAmm(
		amm,
		marketStats,
		reservePrice
	);

	const [longSpread, shortSpread] = calculateSpread(
		amm,
		marketStats,
		mmOraclePriceData,
		now,
		reservePrice
	);

	let askReserves = calculateSpreadReserve(
		longSpread + referencePriceOffset,
		amm
	);
	let bidReserves = calculateSpreadReserve(
		-shortSpread + referencePriceOffset,
		amm
	);

	// With no reference offset the program clamps asks to at least and bids to
	// at most the reserve price.
	if (referencePriceOffset === 0) {
		askReserves = {
			baseAssetReserve: BN.min(
				askReserves.baseAssetReserve,
				amm.baseAssetReserve
			),
			quoteAssetReserve: BN.max(
				askReserves.quoteAssetReserve,
				amm.quoteAssetReserve
			),
		};
		bidReserves = {
			baseAssetReserve: BN.max(
				bidReserves.baseAssetReserve,
				amm.baseAssetReserve
			),
			quoteAssetReserve: BN.min(
				bidReserves.quoteAssetReserve,
				amm.quoteAssetReserve
			),
		};
	}

	return [bidReserves, askReserves];
}

/**
 * Applies the constant-product invariant (`invariant = k^2 = inputReserve * outputReserve`) to
 * a single reserve swap; agnostic to whether the input side is quote or base. Both reserve
 * arguments and the result must share the same precision as `invariant`'s square root
 * (typically `AMM_RESERVE_PRECISION`, 1e9).
 * @param inputAssetReserve Current reserve on the input side, same precision as `invariant`'s square root.
 * @param swapAmount Amount being added to or removed from `inputAssetReserve`.
 * @param swapDirection Whether `swapAmount` is added to or removed from the input reserve.
 * @param invariant Constant-product invariant (`sqrtK^2`), same precision as `inputAssetReserve` squared.
 * @returns `[newInputAssetReserve, newOutputAssetReserve]`, both same precision as `inputAssetReserve`.
 */
export function calculateSwapOutput(
	inputAssetReserve: BN,
	swapAmount: BN,
	swapDirection: SwapDirection,
	invariant: BN
): [BN, BN] {
	let newInputAssetReserve;
	if (swapDirection === SwapDirection.ADD) {
		newInputAssetReserve = inputAssetReserve.add(swapAmount);
	} else {
		newInputAssetReserve = inputAssetReserve.sub(swapAmount);
	}
	const newOutputAssetReserve = invariant.div(newInputAssetReserve);
	return [newInputAssetReserve, newOutputAssetReserve];
}

/**
 * Maps a desired position direction and the asset side being specified into the AMM
 * reserve-swap direction (`ADD`/`REMOVE`) needed to execute it: opening a long by specifying
 * base, or a short by specifying quote, removes that reserve from the AMM; every other
 * combination adds to it.
 * @param inputAssetType Which side (`'quote'` or `'base'`) the trade amount is denominated in.
 * @param positionDirection Direction of the position being opened/closed.
 * @returns `SwapDirection.ADD` or `SwapDirection.REMOVE` for `calculateAmmReservesAfterSwap`/`calculateSwapOutput`.
 */
export function getSwapDirection(
	inputAssetType: AssetType,
	positionDirection: PositionDirection
): SwapDirection {
	if (isVariant(positionDirection, 'long') && inputAssetType === 'base') {
		return SwapDirection.REMOVE;
	}

	if (isVariant(positionDirection, 'short') && inputAssetType === 'quote') {
		return SwapDirection.REMOVE;
	}

	return SwapDirection.ADD;
}

/**
 * Computes the AMM's "terminal price" — the constant-product price that would result if the
 * AMM's entire net inventory (`baseAssetAmountWithAmm`) were closed out against itself in one
 * swap. Used as a floor/ceiling reference distinct from the current spot reserve price, since
 * it reflects where the curve would settle once open interest unwinds.
 * @param market Perp market whose AMM to compute the terminal price for.
 * @returns Terminal price, PRICE_PRECISION (1e6).
 */
export function calculateTerminalPrice(market: PerpMarketAccount) {
	const directionToClose = market.amm.baseAssetAmountWithAmm.gt(ZERO)
		? PositionDirection.SHORT
		: PositionDirection.LONG;

	const [newQuoteAssetReserve, newBaseAssetReserve] =
		calculateAmmReservesAfterSwap(
			market.amm,
			'base',
			market.amm.baseAssetAmountWithAmm.abs(),
			getSwapDirection('base', directionToClose)
		);

	const terminalPrice = newQuoteAssetReserve
		.mul(PRICE_PRECISION)
		.mul(market.amm.pegMultiplier)
		.div(PEG_PRECISION)
		.div(newBaseAssetReserve);

	return terminalPrice;
}

/**
 * Solves for how much base asset the AMM could absorb, trading against its `direction`-side
 * spread reserves (`calculateSpreadReserves`), before its constant-product price would cross
 * `limit_price` — i.e. the AMM-side fill size available up to a resting limit order's price.
 * The returned direction is the side the AMM would be trading (opposite what a taker matching
 * against it would take): `SHORT` if the AMM's reserves must shrink to reach `limit_price`
 * (limit price above current), `LONG` if they must grow (limit price below current).
 * @param amm AMM state to solve against.
 * @param marketStats Market stats needed to derive spread reserves.
 * @param limit_price Limit price the AMM may trade up to, PRICE_PRECISION (1e6).
 * @param direction Which side's spread reserves to start from (see `calculateSpreadReserves`).
 * @param mmOraclePriceData Current MM oracle price data, forwarded to `calculateSpreadReserves`.
 * @param now Current unix timestamp (seconds), forwarded to `calculateSpreadReserves`.
 * @returns `[baseAssetAmount, direction]`: `baseAssetAmount` is AMM_RESERVE_PRECISION (1e9), zero if the trade size would round to nothing.
 */
export function calculateMaxBaseAssetAmountToTrade(
	amm: AMM,
	marketStats: MarketStats,
	limit_price: BN,
	direction: PositionDirection,
	mmOraclePriceData: Pick<MMOraclePriceData, 'price' | 'confidence'>,
	now?: BN
): [BN, PositionDirection] {
	const invariant = amm.sqrtK.mul(amm.sqrtK);

	const newBaseAssetReserveSquared = invariant
		.mul(PRICE_PRECISION)
		.mul(amm.pegMultiplier)
		.div(limit_price)
		.div(PEG_PRECISION);

	const newBaseAssetReserve = squareRootBN(newBaseAssetReserveSquared);
	const [shortSpreadReserves, longSpreadReserves] = calculateSpreadReserves(
		amm,
		marketStats,
		mmOraclePriceData,
		now
	);

	const baseAssetReserveBefore: BN = isVariant(direction, 'long')
		? longSpreadReserves.baseAssetReserve
		: shortSpreadReserves.baseAssetReserve;

	if (newBaseAssetReserve.gt(baseAssetReserveBefore)) {
		return [
			newBaseAssetReserve.sub(baseAssetReserveBefore),
			PositionDirection.SHORT,
		];
	} else if (newBaseAssetReserve.lt(baseAssetReserveBefore)) {
		return [
			baseAssetReserveBefore.sub(newBaseAssetReserve),
			PositionDirection.LONG,
		];
	} else {
		console.log('tradeSize Too Small');
		return [new BN(0), PositionDirection.LONG];
	}
}

/**
 * Converts a quote-asset AMM reserve amount into the actual quote asset amount swapped
 * (applying the peg multiplier), rounding by 1 in the direction that favors the AMM when
 * `swapDirection` is `remove` (quote leaving the AMM), mirroring the on-chain rounding used to
 * avoid ever giving out a fraction of a unit more than intended.
 * @param quoteAssetReserves Quote reserve delta from a swap, AMM_RESERVE_PRECISION (1e9).
 * @param pegMultiplier AMM peg multiplier, PEG_PRECISION (1e6).
 * @param swapDirection Whether quote is being added to or removed from the AMM.
 * @returns Quote asset amount actually swapped, QUOTE_PRECISION (1e6).
 */
export function calculateQuoteAssetAmountSwapped(
	quoteAssetReserves: BN,
	pegMultiplier: BN,
	swapDirection: SwapDirection
): BN {
	if (isVariant(swapDirection, 'remove')) {
		quoteAssetReserves = quoteAssetReserves.add(ONE);
	}

	let quoteAssetAmount = quoteAssetReserves
		.mul(pegMultiplier)
		.div(AMM_TIMES_PEG_TO_QUOTE_PRECISION_RATIO);

	if (isVariant(swapDirection, 'remove')) {
		quoteAssetAmount = quoteAssetAmount.add(ONE);
	}

	return quoteAssetAmount;
}

/**
 * Caps how much base asset the AMM is willing to fill in one instruction: the smaller of
 * `amm.maxFillReserveFraction`'s share of the current base reserve and the room remaining to
 * the AMM's min/max reserve bound on the taker's side, then rounded down to `orderStepSize`.
 * This is a per-fill risk limit distinct from `calculateMaxBaseAssetAmountToTrade` (which sizes
 * against a limit price) — it bounds how much of the AMM's own liquidity can move at once
 * regardless of price.
 * @param amm AMM state (`baseAssetReserve`, `minBaseAssetReserve`, `maxBaseAssetReserve`, `maxFillReserveFraction`).
 * @param orderStepSize Order step size to standardize the result to, BASE_PRECISION (1e9).
 * @param orderDirection Direction of the order being filled against the AMM.
 * @returns Max fillable base asset amount, BASE_PRECISION (1e9), standardized to `orderStepSize`.
 */
export function calculateMaxBaseAssetAmountFillable(
	amm: AMM,
	orderStepSize: BN,
	orderDirection: PositionDirection
): BN {
	const maxFillSize = amm.baseAssetReserve.div(
		new BN(amm.maxFillReserveFraction)
	);
	let maxBaseAssetAmountOnSide: BN;
	if (isVariant(orderDirection, 'long')) {
		maxBaseAssetAmountOnSide = BN.max(
			ZERO,
			amm.baseAssetReserve.sub(amm.minBaseAssetReserve)
		);
	} else {
		maxBaseAssetAmountOnSide = BN.max(
			ZERO,
			amm.maxBaseAssetReserve.sub(amm.baseAssetReserve)
		);
	}

	return standardizeBaseAssetAmount(
		BN.min(maxFillSize, maxBaseAssetAmountOnSide),
		orderStepSize
	);
}
