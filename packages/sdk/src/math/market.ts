import { BN } from '../isomorphic/anchor';
import {
	PerpMarketAccount,
	PositionDirection,
	MarginCategory,
	SpotMarketAccount,
	SpotBalanceType,
	isVariant,
} from '../types';
import {
	calculatePrice,
	calculateUpdatedAMMSpreadReserves,
	calculateUpdatedAMM,
} from './amm';
import {
	calculateSizeDiscountAssetWeight,
	calculateSizePremiumLiabilityWeight,
} from './margin';
import { MMOraclePriceData, OraclePriceData } from '../oracles/types';
import {
	BASE_PRECISION,
	MARGIN_PRECISION,
	PRICE_TO_QUOTE_PRECISION,
	ZERO,
	QUOTE_SPOT_MARKET_INDEX,
	PRICE_PRECISION,
	PERCENTAGE_PRECISION,
	FUNDING_RATE_OFFSET_PERCENTAGE,
	TRIGGER_PRICE_LAST_FILL_MAX_AGE,
} from '../constants/numericConstants';
import { getTokenAmount } from './spotBalance';
import { assert } from '../assert/assert';
import { SlotDurationState } from './time';

/**
 * Calculates the perp market's current mark (mid) price from its raw (non-spread) AMM reserves,
 * after first repegging the AMM to the oracle price (`calculateUpdatedAMM`) if `mmOraclePriceData`
 * is provided.
 *
 * @param {PerpMarketAccount} market - The perp market account
 * @param {MMOraclePriceData} [mmOraclePriceData] - Current MM oracle price data; omit to price the
 *   AMM's stored reserves as-is without repegging
 * @return {BN} The mark price, PRICE_PRECISION (1e6)
 */
export function calculateReservePrice(
	market: PerpMarketAccount,
	mmOraclePriceData?: MMOraclePriceData
): BN {
	const newAmm = calculateUpdatedAMM(market.amm, mmOraclePriceData);
	return calculatePrice(
		newAmm.baseAssetReserve,
		newAmm.quoteAssetReserve,
		newAmm.pegMultiplier
	);
}

/**
 * Calculates the perp market's current bid price — the price a taker sells into — by repegging
 * the AMM to the oracle price and pricing the short-side spread reserves.
 *
 * @param {PerpMarketAccount} market - The perp market account
 * @param {MMOraclePriceData} [mmOraclePriceData] - Current MM oracle price data, used both to
 *   repeg the AMM and to compute the spread reserves
 * @param {BN} [latestSlot] - Current slot, used for reference-price-offset smoothing in the
 *   spread calculation
 * @return {BN} The bid price, PRICE_PRECISION (1e6)
 */
export function calculateBidPrice(
	market: PerpMarketAccount,
	mmOraclePriceData?: MMOraclePriceData,
	latestSlot?: BN,
	slotDurationState?: SlotDurationState
): BN {
	const { baseAssetReserve, quoteAssetReserve, newPeg } =
		calculateUpdatedAMMSpreadReserves(
			market.amm,
			market.marketStats,
			PositionDirection.SHORT,
			mmOraclePriceData,
			latestSlot,
			slotDurationState
		);

	return calculatePrice(baseAssetReserve, quoteAssetReserve, newPeg);
}

/**
 * Calculates the perp market's current ask price — the price a taker buys at — by repegging
 * the AMM to the oracle price and pricing the long-side spread reserves.
 *
 * @param {PerpMarketAccount} market - The perp market account
 * @param {MMOraclePriceData} [mmOraclePriceData] - Current MM oracle price data, used both to
 *   repeg the AMM and to compute the spread reserves
 * @param {BN} [latestSlot] - Current slot, used for reference-price-offset smoothing in the
 *   spread calculation
 * @return {BN} The ask price, PRICE_PRECISION (1e6)
 */
export function calculateAskPrice(
	market: PerpMarketAccount,
	mmOraclePriceData?: MMOraclePriceData,
	latestSlot?: BN,
	slotDurationState?: SlotDurationState
): BN {
	const { baseAssetReserve, quoteAssetReserve, newPeg } =
		calculateUpdatedAMMSpreadReserves(
			market.amm,
			market.marketStats,
			PositionDirection.LONG,
			mmOraclePriceData,
			latestSlot,
			slotDurationState
		);

	return calculatePrice(baseAssetReserve, quoteAssetReserve, newPeg);
}

/**
 * Calculates the signed spread between a price and the oracle price.
 *
 * @param {BN} price - A price, PRICE_PRECISION (1e6)
 * @param {OraclePriceData} oraclePriceData - Oracle price data, PRICE_PRECISION (1e6)
 * @return {BN} `price - oraclePriceData.price`, PRICE_PRECISION (1e6)
 */
export function calculateOracleSpread(
	price: BN,
	oraclePriceData: OraclePriceData
): BN {
	return price.sub(oraclePriceData.price);
}

/**
 * Calculates the effective margin ratio for a perp position of a given size, applying the
 * IMF size premium on top of the market's base initial/maintenance ratio. Returns 0 for markets
 * in `'Settlement'` status (no margin is required once a market is settling out).
 *
 * @param {PerpMarketAccount} market - The perp market account
 * @param {BN} size - The position's base asset amount (`abs()` semantics expected), BASE_PRECISION (1e9)
 * @param {MarginCategory} marginCategory - `'Initial'`, `'Maintenance'`, or `'Fill'`; throws for any other value.
 *   `'Fill'` uses `(marginRatioInitial + marginRatioMaintenance) / 2` (integer division), mirroring
 *   `PerpMarket::get_margin_ratio`.
 * @param {number} [customMarginRatio] - User's custom max margin ratio, `MARGIN_PRECISION` (1e4)
 *   units; only applied for `'Initial'`, where the looser (higher) of the computed ratio and
 *   this value is used
 * @return {number} The margin ratio, scaled by `MARGIN_PRECISION` (1e4, i.e. 10000 = 100%)
 */
export function calculateMarketMarginRatio(
	market: PerpMarketAccount,
	size: BN,
	marginCategory: MarginCategory,
	customMarginRatio = 0
): number {
	if (market.status === 'Settlement') return 0;

	let defaultMarginRatio: number;
	switch (marginCategory) {
		case 'Initial':
			defaultMarginRatio = market.marginRatioInitial;
			break;
		case 'Fill':
			// mirrors PerpMarket::get_margin_ratio's Fill branch: integer-divided average
			defaultMarginRatio = Math.floor(
				(market.marginRatioInitial + market.marginRatioMaintenance) / 2
			);
			break;
		case 'Maintenance':
			defaultMarginRatio = market.marginRatioMaintenance;
			break;
		default:
			throw new Error('Invalid margin category');
	}

	let marginRatio: number;

	const sizeAdjMarginRatio = calculateSizePremiumLiabilityWeight(
		size,
		new BN(market.imfFactor),
		new BN(defaultMarginRatio),
		MARGIN_PRECISION,
		true
	).toNumber();

	marginRatio = Math.max(defaultMarginRatio, sizeAdjMarginRatio);

	if (marginCategory === 'Initial') {
		marginRatio = Math.max(marginRatio, customMarginRatio);
	}

	return marginRatio;
}

/**
 * Calculates the asset weight applied to a perp position's unrealized (positive) PnL when it
 * counts toward collateral, mirroring `PerpMarket::get_unrealized_asset_weight`'s
 * `Initial`/`Maintenance` branches. Only call this for a positive `unrealizedPnl` — the on-chain
 * equivalent always weights a negative unrealized PnL at `SPOT_MARKET_WEIGHT_PRECISION` (100%,
 * i.e. it's not discounted since it's a liability, not an asset).
 *
 * `'Initial'` weighting applies two independent discounts: (1) if `calculateNetUserPnlImbalance`
 * (net user PnL less the pnl pool and a fifth of the fee pool) exceeds `unrealizedPnlMaxImbalance`,
 * the base weight is first scaled down by `unrealizedPnlMaxImbalance / netUnsettledPnl`; (2) the
 * IMF size-discount (`calculateSizeDiscountAssetWeight`) is then applied to the position's own
 * `unrealizedPnl` size. Two notes for exact parity with the on-chain `get_unrealized_asset_weight`:
 * (a) the Rust gate compares the *raw* `calculate_net_user_pnl` (no pool subtraction) against
 * `unrealized_pnl_max_imbalance`, whereas step (1) here nets out the pnl/fee pool first — a
 * looser (more forgiving) trigger condition; (b) the Rust size-discount rescales `unrealized_pnl`
 * by `AMM_TO_QUOTE_PRECISION_RATIO` (1e3) before step (2), whereas this passes `unrealizedPnl`
 * (QUOTE_PRECISION, 1e6) directly — `calculateSizeDiscountAssetWeight`'s `size` parameter is
 * otherwise documented as `AMM_RESERVE_PRECISION` (1e9) elsewhere in the SDK.
 *
 * @param {PerpMarketAccount} market - The perp market account
 * @param {SpotMarketAccount} quoteSpotMarket - The market's quote spot market account
 * @param {BN} unrealizedPnl - The position's unrealized PnL, expected positive, QUOTE_PRECISION (1e6)
 * @param {MarginCategory} marginCategory - `'Initial'`, `'Maintenance'`, or `'Fill'` (Fill is weighted identically to Initial)
 * @param {Pick<OraclePriceData, 'price'>} oraclePriceData - Oracle price, PRICE_PRECISION (1e6),
 *   used only for the imbalance check's `calculateNetUserPnlImbalance` call
 * @return {BN} The asset weight, scaled by `SPOT_MARKET_WEIGHT_PRECISION` (1e4, i.e. 10000 = 100%)
 */
export function calculateUnrealizedAssetWeight(
	market: PerpMarketAccount,
	quoteSpotMarket: SpotMarketAccount,
	unrealizedPnl: BN,
	marginCategory: MarginCategory,
	oraclePriceData: Pick<OraclePriceData, 'price'>
): BN {
	let assetWeight: BN;
	switch (marginCategory) {
		// mirrors get_unrealized_asset_weight: Fill is treated like Initial (same base
		// weight, same imbalance + size-discount adjustments).
		case 'Initial':
		case 'Fill':
			assetWeight = new BN(market.unrealizedPnlInitialAssetWeight);

			if (market.unrealizedPnlMaxImbalance.gt(ZERO)) {
				const netUnsettledPnl = calculateNetUserPnlImbalance(
					market,
					quoteSpotMarket,
					oraclePriceData
				);
				if (netUnsettledPnl.gt(market.unrealizedPnlMaxImbalance)) {
					assetWeight = assetWeight
						.mul(market.unrealizedPnlMaxImbalance)
						.div(netUnsettledPnl);
				}
			}

			assetWeight = calculateSizeDiscountAssetWeight(
				unrealizedPnl,
				new BN(market.unrealizedPnlImfFactor),
				assetWeight
			);
			break;
		case 'Maintenance':
			assetWeight = new BN(market.unrealizedPnlMaintenanceAssetWeight);
			break;
		default:
			throw new Error('Invalid margin category');
	}

	return assetWeight;
}

/**
 * Calculates the perp market's pnl pool balance — the quote tokens on hand to pay out settled
 * user profits before insurance fund draws are needed.
 *
 * @param {PerpMarketAccount} perpMarket - The perp market account
 * @param {SpotMarketAccount} spotMarket - The market's quote spot market account
 * @return {BN} The pnl pool token amount, scaled by `spotMarket.decimals` (quote decimals)
 */
export function calculateMarketAvailablePNL(
	perpMarket: PerpMarketAccount,
	spotMarket: SpotMarketAccount
): BN {
	return getTokenAmount(
		perpMarket.pnlPool.scaledBalance,
		spotMarket,
		SpotBalanceType.DEPOSIT
	);
}

/**
 * Calculates the maximum insurance the market could still draw to cover a PnL deficit: the
 * remaining `quoteMaxInsurance` allocation not yet claimed, plus the AMM's own fee pool (which is
 * drawn down before external insurance). `spotMarket` must be the quote spot market — asserts
 * otherwise.
 *
 * @param {PerpMarketAccount} perpMarket - The perp market account
 * @param {SpotMarketAccount} spotMarket - The quote spot market account (must have
 *   `marketIndex === QUOTE_SPOT_MARKET_INDEX`)
 * @return {BN} `quoteMaxInsurance - quoteSettledInsurance + ammFeePoolTokenAmount`, scaled by
 *   quote decimals
 */
export function calculateMarketMaxAvailableInsurance(
	perpMarket: PerpMarketAccount,
	spotMarket: SpotMarketAccount
): BN {
	assert(spotMarket.marketIndex == QUOTE_SPOT_MARKET_INDEX);

	// todo: insuranceFundAllocation technically not guaranteed to be in Insurance Fund
	const insuranceFundAllocation =
		perpMarket.insuranceClaim.quoteMaxInsurance.sub(
			perpMarket.insuranceClaim.quoteSettledInsurance
		);
	const ammFeePool = getTokenAmount(
		perpMarket.amm.feePool.scaledBalance,
		spotMarket,
		SpotBalanceType.DEPOSIT
	);
	return insuranceFundAllocation.add(ammFeePool);
}

/**
 * Calculates the net unrealized + unsettled PnL owed to all users of a perp market at a given
 * oracle price, mirroring `calculate_net_user_pnl`: the AMM's net counterparty position valued
 * at `oraclePriceData.price`, plus the market's cost basis (`quoteAssetAmount +
 * netUnsettledFundingPnl`). This is the quantity the pnl pool + insurance fund must be able to
 * cover across all users.
 *
 * @param {PerpMarketAccount} perpMarket - The perp market account
 * @param {Pick<OraclePriceData, 'price'>} oraclePriceData - Oracle price, PRICE_PRECISION (1e6)
 *   (callers typically pass the live price or a TWAP depending on the check being performed)
 * @return {BN} Net user PnL, QUOTE_PRECISION (1e6); positive means users are net owed
 */
export function calculateNetUserPnl(
	perpMarket: PerpMarketAccount,
	oraclePriceData: Pick<OraclePriceData, 'price'>
): BN {
	const netUserPositionValue = perpMarket.amm.baseAssetAmountWithAmm
		.mul(oraclePriceData.price)
		.div(BASE_PRECISION)
		.div(PRICE_TO_QUOTE_PRECISION);

	const netUserCostBasis = perpMarket.quoteAssetAmount.add(
		perpMarket.netUnsettledFundingPnl
	);

	const netUserPnl = netUserPositionValue.add(netUserCostBasis);

	return netUserPnl;
}

/**
 * Calculates the `pendingIfFee` floor that a pnl-pool drain must leave in the pool. This mirrors
 * `PerpMarket::get_bankruptcy_if_floor`. The floor is `bankruptcyIfFloorPct` of the open interest
 * notional, valued at the oracle TWAP. It uses the TWAP, not the live price, so that no party can
 * move the floor. It returns zero when the floor is not set.
 *
 * @param {PerpMarketAccount} perpMarket - The perp market account
 * @return {BN} The floor, QUOTE_PRECISION (1e6)
 */
export function calculateBankruptcyIfFloor(perpMarket: PerpMarketAccount): BN {
	if (perpMarket.bankruptcyIfFloorPct === 0) {
		return ZERO;
	}

	const oraclePriceTwap = BN.max(
		perpMarket.marketStats.historicalOracleData.lastOraclePriceTwap,
		ZERO
	);
	const openInterest = BN.max(
		perpMarket.baseAssetAmountLong.abs(),
		perpMarket.baseAssetAmountShort.abs()
	);

	return openInterest
		.mul(oraclePriceTwap)
		.div(BASE_PRECISION)
		.muln(perpMarket.bankruptcyIfFloorPct)
		.div(PERCENTAGE_PRECISION);
}

/**
 * Calculates the pnl-pool tokens that a drain must leave in the pool to back the first-loss
 * insurance-fund tranche. This mirrors `PerpMarket::get_bankruptcy_if_tranche_reservation` and
 * equals `min(pendingIfFee, bankruptcyIfFloor)`. `resolvePerpBankruptcy` changes only the
 * `pendingIfFee` counter, so the tokens must stay in the pool. The final delist sweep passes
 * `force` and reserves nothing.
 *
 * @param {PerpMarketAccount} perpMarket - The perp market account
 * @param {boolean} force - Delisting sweep; waives the reservation
 * @return {BN} The reservation, QUOTE_PRECISION (1e6)
 */
export function calculateBankruptcyIfTrancheReservation(
	perpMarket: PerpMarketAccount,
	force = false
): BN {
	if (force) {
		return ZERO;
	}
	return BN.min(
		perpMarket.feeLedger.pendingIfFee,
		calculateBankruptcyIfFloor(perpMarket)
	);
}

/**
 * Calculates the part of the pnl pool of a perp market that can pay accrued builder and referrer
 * fees. This mirrors the reserve that `sweep_completed_revenue_share_for_market` applies. The
 * result is `pnlPoolTokens - max(netUserPnl, 0) - bankruptcyIfTrancheReservation`, and never less
 * than zero.
 *
 * The sweep pays a row only when its `feesAccrued` fits in this amount. A keeper can therefore
 * check whether a `settleRevenueShare` call will pay before it sends the call. The sweep does not
 * reserve `pendingRevenueShare`, because it pays that claim.
 *
 * In settlement the reserve uses the `expiryPrice` of the market, not the live price. Expired
 * positions settle at `expiryPrice`. A lower live price on a net-long market would make the
 * reserve too small.
 *
 * @param {PerpMarketAccount} perpMarket - The perp market account
 * @param {SpotMarketAccount} spotMarket - The quote spot market account
 * @param {Pick<OraclePriceData, 'price'>} oraclePriceData - Live oracle price, PRICE_PRECISION (1e6);
 *   ignored while the market is in settlement
 * @return {BN} Tokens available to pay revenue share, QUOTE_PRECISION (1e6)
 */
export function calculateRevenueShareSweepAvailable(
	perpMarket: PerpMarketAccount,
	spotMarket: SpotMarketAccount,
	oraclePriceData: Pick<OraclePriceData, 'price'>
): BN {
	const priceToUse = isVariant(perpMarket.status, 'settlement')
		? perpMarket.expiryPrice
		: oraclePriceData.price;

	const reserved = BN.max(
		calculateNetUserPnl(perpMarket, { price: priceToUse }),
		ZERO
	).add(calculateBankruptcyIfTrancheReservation(perpMarket));

	const pnlPoolTokens = calculateMarketAvailablePNL(perpMarket, spotMarket);

	return BN.max(pnlPoolTokens.sub(reserved), ZERO);
}

/**
 * Calculates how far `calculateNetUserPnl` exceeds the funds already on hand to pay it out (the
 * pnl pool, plus by default a 20% slice of the AMM fee pool as a conservative haircut on funds
 * not yet swept into the pnl pool). A positive result means the market is short of pnl-pool
 * funds by that amount; a negative result means the pnl pool has surplus.
 *
 * @param {PerpMarketAccount} perpMarket - The perp market account
 * @param {SpotMarketAccount} spotMarket - The market's quote spot market account
 * @param {Pick<OraclePriceData, 'price'>} oraclePriceData - Oracle price, PRICE_PRECISION (1e6),
 *   passed through to `calculateNetUserPnl`
 * @param {boolean} [applyFeePoolDiscount] - When true (default), only 1/5 of the AMM fee pool
 *   counts toward available funds; when false, the full fee pool counts
 * @return {BN} `netUserPnl - (pnlPool + feePoolContribution)`, QUOTE_PRECISION (1e6)
 */
export function calculateNetUserPnlImbalance(
	perpMarket: PerpMarketAccount,
	spotMarket: SpotMarketAccount,
	oraclePriceData: Pick<OraclePriceData, 'price'>,
	applyFeePoolDiscount = true
): BN {
	const netUserPnl = calculateNetUserPnl(perpMarket, oraclePriceData);

	const pnlPool = getTokenAmount(
		perpMarket.pnlPool.scaledBalance,
		spotMarket,
		SpotBalanceType.DEPOSIT
	);
	let feePool = getTokenAmount(
		perpMarket.amm.feePool.scaledBalance,
		spotMarket,
		SpotBalanceType.DEPOSIT
	);
	if (applyFeePoolDiscount) {
		feePool = feePool.div(new BN(5));
	}

	const imbalance = netUserPnl.sub(pnlPool.add(feePool));

	return imbalance;
}

/**
 * Calculates the price used to evaluate trigger (stop/take-profit) orders for a perp market,
 * mirroring the Rust `get_trigger_price`. When `useMedianPrice` is true, the trigger price is the
 * median of three candidates — the last fill price (or oracle price if there's been no fill or the
 * last fill is older than `TRIGGER_PRICE_LAST_FILL_MAX_AGE`), the oracle price adjusted by the
 * implied funding basis, and the oracle price adjusted by the 5min mark/oracle TWAP basis — then
 * clamped to within a contract-tier-dependent band around the raw oracle price (tier A/B: 20bps,
 * tier C: 100bps, others: 250bps) via `clampTriggerPrice`. This
 * resists a single manipulated print (last fill or a momentary oracle/mark divergence) from
 * triggering orders it shouldn't. When `useMedianPrice` is false, the raw oracle price is used
 * directly with no smoothing.
 *
 * @param {PerpMarketAccount} market - The perp market account
 * @param {BN} oraclePrice - Current oracle price, PRICE_PRECISION (1e6); its absolute value is
 *   used throughout
 * @param {BN} now - Current unix timestamp, seconds; used to prorate the implied funding basis
 *   over the time remaining until the next funding update
 * @param {boolean} useMedianPrice - Whether to apply the median-of-three + clamp smoothing, or
 *   use the raw oracle price directly
 * @returns {BN} The trigger price, PRICE_PRECISION (1e6)
 */
export function getTriggerPrice(
	market: PerpMarketAccount,
	oraclePrice: BN,
	now: BN,
	useMedianPrice: boolean
): BN {
	if (!useMedianPrice) {
		return oraclePrice.abs();
	}

	// Leg A: last trade price, only while fresh. `lastTradeTs` is stamped by
	// the same fill path that writes `lastFillPrice`.
	const lastFillPrice = market.lastFillPrice;
	const lastFillIsFresh = now
		.sub(market.marketStats.lastTradeTs)
		.lte(TRIGGER_PRICE_LAST_FILL_MAX_AGE);

	// Leg C: oracle + (mark_twap_5min - oracle_twap_5min)
	const markPrice5minTwap = market.marketStats.lastMarkPriceTwap5Min;
	const lastOraclePriceTwap5min =
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min;
	const basis5min = markPrice5minTwap.sub(lastOraclePriceTwap5min);

	const oraclePlusBasis5min = oraclePrice.add(basis5min);

	// Leg B: oracle + decayed funding basis
	const lastFundingBasis = getLastFundingBasis(market, oraclePrice, now);
	const oraclePlusFundingBasis = oraclePrice.add(lastFundingBasis);

	// No fill yet or last fill is stale: oracle price stands in for Leg A
	const prices = [
		lastFillPrice.gt(ZERO) && lastFillIsFresh ? lastFillPrice : oraclePrice,
		oraclePlusFundingBasis,
		oraclePlusBasis5min,
	].sort((a, b) => a.cmp(b));
	const medianPrice = prices[1];

	return clampTriggerPrice(market, oraclePrice.abs(), medianPrice);
}

/**
 * Calculates the last funding basis for trigger price calculation
 * Implements the same logic as the Rust get_last_funding_basis function
 */
function getLastFundingBasis(
	market: PerpMarketAccount,
	oraclePrice: BN,
	now: BN
): BN {
	if (market.marketStats.lastFundingOracleTwap.gt(ZERO)) {
		const lastFundingRate = market.lastFundingRate
			.mul(PRICE_PRECISION)
			.div(market.marketStats.lastFundingOracleTwap)
			.muln(24);
		const lastFundingRatePreAdj = lastFundingRate.sub(
			FUNDING_RATE_OFFSET_PERCENTAGE
		);
		const timeSinceFundingUpdate = BN.min(
			BN.max(now.sub(market.lastFundingRateTs), ZERO),
			market.marketStats.fundingPeriod
		);
		const lastFundingBasis = oraclePrice
			.mul(lastFundingRatePreAdj)
			.div(PERCENTAGE_PRECISION)
			.mul(market.marketStats.fundingPeriod.sub(timeSinceFundingUpdate))
			.div(market.marketStats.fundingPeriod)
			.div(new BN(1000)); // FUNDING_RATE_BUFFER
		return lastFundingBasis;
	} else {
		return ZERO;
	}
}

/**
 * Clamps trigger price based on contract tier
 * Implements the same logic as the Rust clamp_trigger_price function
 */
function clampTriggerPrice(
	market: PerpMarketAccount,
	oraclePrice: BN,
	medianPrice: BN
): BN {
	let clampDivisor: BN;
	const tier = market.contractTier;
	if (isVariant(tier, 'a') || isVariant(tier, 'b')) {
		clampDivisor = new BN(500); // oracle / 500 = 20 bps
	} else if (isVariant(tier, 'c')) {
		clampDivisor = new BN(100); // oracle / 100 = 100 bps
	} else {
		clampDivisor = new BN(40); // oracle / 40 = 250 bps
	}
	const maxOracleDiff = oraclePrice.div(clampDivisor);
	return BN.min(
		BN.max(medianPrice, oraclePrice.sub(maxOracleDiff)),
		oraclePrice.add(maxOracleDiff)
	);
}
