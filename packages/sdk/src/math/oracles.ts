import {
	HistoricalOracleData,
	MarketStats,
	OracleGuardRails,
	OracleSource,
	OracleValidity,
	PerpMarketAccount,
	SpotMarketAccount,
	isOneOfVariant,
	isVariant,
} from '../types';
import { OraclePriceData } from '../oracles/types';
import {
	BID_ASK_SPREAD_PRECISION,
	MARGIN_PRECISION,
	MM_ORACLE_MIN_WRITE_GAP,
	ONE,
	ZERO,
	FIVE_MINUTE,
	PERCENTAGE_PRECISION,
	FIVE,
	TEN,
} from '../constants/numericConstants';
import { assert } from '../assert/assert';
import { BN } from '../isomorphic/anchor';
import {
	Millis,
	SlotDurationMs,
	SLOT_DURATION_BASELINE,
	millisFromStoredUnits,
	millisToSlots,
} from './time';

/**
 * Computes a generic sanity band around the oracle price, sized by the gap between the
 * market's initial and maintenance margin ratios (a wider margin gap allows a wider band).
 * This is a coarse UI/client-side sanity check, not the exact on-chain price-band gate —
 * order and settlement price-divergence checks on-chain compare the 5-min oracle TWAP
 * spread via `isMarkOracleTooDivergent`/`isOracleTooDivergent` instead.
 * @param market Perp market whose `marginRatioInitial`/`marginRatioMaintenance` (MARGIN_PRECISION, 1e4) set the band width.
 * @param oraclePriceData Must provide `price`, PRICE_PRECISION (1e6).
 * @returns `[lowerBound, upperBound]`, both PRICE_PRECISION (1e6).
 */
export function oraclePriceBands(
	market: PerpMarketAccount,
	oraclePriceData: Pick<OraclePriceData, 'price'>
): [BN, BN] {
	const maxPercentDiff =
		market.marginRatioInitial - market.marginRatioMaintenance;
	const offset = oraclePriceData.price
		.mul(new BN(maxPercentDiff))
		.div(MARGIN_PRECISION);

	assert(offset.gte(ZERO));

	return [oraclePriceData.price.sub(offset), oraclePriceData.price.add(offset)];
}

/**
 * Returns the per-market multiplier applied to `confidenceIntervalMaxSize` when checking
 * oracle confidence-interval validity, mirroring `PerpMarket::get_max_confidence_interval_multiplier`.
 * Riskier contract tiers tolerate a wider oracle confidence interval before being flagged
 * invalid: 1x for tier A/B, 2x for tier C, 10x for Speculative, 50x for HighlySpeculative and Isolated.
 * @param market Perp market whose `contractTier` selects the multiplier.
 * @returns Unitless multiplier (dimensionless BN).
 */
export function getMaxConfidenceIntervalMultiplier(
	market: PerpMarketAccount
): BN {
	let maxConfidenceIntervalMultiplier;
	if (isVariant(market.contractTier, 'a')) {
		maxConfidenceIntervalMultiplier = new BN(1);
	} else if (isVariant(market.contractTier, 'b')) {
		maxConfidenceIntervalMultiplier = new BN(1);
	} else if (isVariant(market.contractTier, 'c')) {
		maxConfidenceIntervalMultiplier = new BN(2);
	} else if (isVariant(market.contractTier, 'speculative')) {
		maxConfidenceIntervalMultiplier = new BN(10);
	} else {
		maxConfidenceIntervalMultiplier = new BN(50);
	}
	return maxConfidenceIntervalMultiplier;
}

/**
 * Classifies an oracle reading's validity for `market`, mirroring `oracle_validity` in
 * `programs/velocity/src/math/oracle.rs`. Checks are evaluated in severity order and the
 * first failing check wins: non-positive price, too volatile vs the oracle TWAP
 * (`tooVolatileRatio`), confidence interval too wide (scaled by
 * `getMaxConfidenceIntervalMultiplier`), stale for margin use, insufficient oracle data
 * points, then stale for AMM use (low-risk or immediate, gated by the market's
 * `oracleLowRiskSlotDelayOverride`/`oracleSlotDelayOverride`). Returns `OracleValidity.Valid`
 * only if none of these trip. Callers typically gate on the returned enum via
 * `isOracleValidForAction`-style helpers rather than comparing directly.
 * @param market Perp market providing contract tier, oracle source, and stale-slot overrides.
 * @param oraclePriceData Oracle reading to validate (`price`/`confidence` PRICE_PRECISION 1e6, `slot`).
 * @param oracleGuardRails Protocol-wide validity thresholds (`state.oracleGuardRails`).
 * @param slot Current slot, used to compute oracle delay.
 * @param oracleStalenessBuffer Extra slots subtracted from the raw oracle delay before staleness checks (default 5) to absorb normal reporting lag.
 * @param isMmSourcedPrice Whether `oraclePriceData` carries an MM-oracle-sourced price. Only
 * affects the unset (`oracleSlotDelayOverride < 0`) immediate-fill threshold, which resolves to
 * `MM_ORACLE_MIN_WRITE_GAP` for an MM-sourced price and to zero for an exchange-sourced one,
 * mirroring `oracle_validity`'s `immediate_price_is_mm_sourced`.
 * @param slotDuration Current slot duration (`slotDurationFromState(state.slotDurationMs)`).
 * Staleness thresholds are wall-clock durations expressed in actual slots at this duration,
 * mirroring `oracle_validity`.
 * @returns The most severe `OracleValidity` classification that applies.
 */
export function getOracleValidity(
	market: PerpMarketAccount,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails,
	slot: BN,
	oracleStalenessBuffer = FIVE,
	isMmSourcedPrice = false,
	slotDuration: SlotDurationMs = SLOT_DURATION_BASELINE
): OracleValidity {
	const slots = (m: Millis) => millisToSlots(m, slotDuration);
	const isNonPositive = oraclePriceData.price.lte(ZERO);
	const isTooVolatile = BN.max(
		oraclePriceData.price,
		market.marketStats.historicalOracleData.lastOraclePriceTwap
	)
		.div(
			BN.max(
				ONE,
				BN.min(
					oraclePriceData.price,
					market.marketStats.historicalOracleData.lastOraclePriceTwap
				)
			)
		)
		.gt(oracleGuardRails.validity.tooVolatileRatio);

	const confPctOfPrice = oraclePriceData.confidence
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(oraclePriceData.price);
	const isConfTooLarge = confPctOfPrice.gt(
		oracleGuardRails.validity.confidenceIntervalMaxSize.mul(
			getMaxConfidenceIntervalMultiplier(market)
		)
	);

	const oracleDelay = slot.sub(oraclePriceData.slot).sub(oracleStalenessBuffer);

	// Mirrors `math::oracle::oracle_validity`. `0` is the explicit "never allow
	// immediate AMM fills" sentinel. A negative override means unset, and its
	// resolution is source-aware: MM_ORACLE_MIN_WRITE_GAP for an MM-oracle-sourced
	// price (the program will not accept MM-oracle writes closer together than
	// that, so a tighter threshold could never be satisfied) and the strict zero
	// threshold for an exchange-sourced price, which can be same-slot fresh.
	let isStaleForAmmImmediate = true;
	if (market.oracleSlotDelayOverride < 0) {
		isStaleForAmmImmediate = oracleDelay.gt(
			isMmSourcedPrice ? slots(MM_ORACLE_MIN_WRITE_GAP) : ZERO
		);
	} else if (market.oracleSlotDelayOverride != 0) {
		isStaleForAmmImmediate = oracleDelay.gt(
			slots(millisFromStoredUnits(market.oracleSlotDelayOverride))
		);
	}

	let isStaleForAmmLowRisk = false;
	if (market.oracleLowRiskSlotDelayOverride != 0) {
		isStaleForAmmLowRisk = oracleDelay.gt(
			slots(
				millisFromStoredUnits(
					Math.max(market.oracleLowRiskSlotDelayOverride, 0)
				)
			)
		);
	} else {
		isStaleForAmmLowRisk = oracleDelay.gt(
			slots(
				millisFromStoredUnits(oracleGuardRails.validity.slotsBeforeStaleForAmm)
			)
		);
	}

	let isStaleForMargin = oracleDelay.gt(
		slots(
			millisFromStoredUnits(oracleGuardRails.validity.slotsBeforeStaleForMargin)
		)
	);
	if (isVariant(market.oracleSource, 'pythLazerStableCoin')) {
		isStaleForMargin = oracleDelay.gt(
			slots(
				millisFromStoredUnits(
					oracleGuardRails.validity.slotsBeforeStaleForMargin
				)
			).muln(3)
		);
	}

	if (isNonPositive) {
		return OracleValidity.NonPositive;
	} else if (isTooVolatile) {
		return OracleValidity.TooVolatile;
	} else if (isConfTooLarge) {
		return OracleValidity.TooUncertain;
	} else if (isStaleForMargin) {
		return OracleValidity.StaleForMargin;
	} else if (!oraclePriceData.hasSufficientNumberOfDataPoints) {
		return OracleValidity.InsufficientDataPoints;
	} else if (isStaleForAmmLowRisk) {
		return OracleValidity.StaleForAMMLowRisk;
	} else if (isStaleForAmmImmediate) {
		return OracleValidity.isStaleForAmmImmediate;
	} else {
		return OracleValidity.Valid;
	}
}

/**
 * Simplified, AMM-fill-oriented validity check: `true` only if the oracle has sufficient
 * data points, is not stale (vs `slotsBeforeStaleForAmm`), has a positive price, isn't too
 * volatile vs the market's oracle TWAP, and its confidence interval isn't too wide. Unlike
 * `getOracleValidity` this does not distinguish "stale for margin" or "low risk" tiers — it
 * is a single valid/invalid gate specifically for whether the AMM may fill against this
 * price.
 * @param market Perp market providing the oracle TWAP and contract tier for the confidence multiplier.
 * @param oraclePriceData Oracle reading to validate (`price`/`confidence` PRICE_PRECISION 1e6).
 * @param oracleGuardRails Protocol-wide validity thresholds.
 * @param slot Current slot, used to compute oracle staleness.
 * @returns `true` if the oracle is valid for an AMM-only fill.
 */
export function isOracleValid(
	market: PerpMarketAccount,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails,
	slot: number,
	slotDuration: SlotDurationMs = SLOT_DURATION_BASELINE
): boolean {
	// checks if oracle is valid for an AMM only fill

	const stats = market.marketStats;
	const isOraclePriceNonPositive = oraclePriceData.price.lte(ZERO);
	const isOraclePriceTooVolatile =
		oraclePriceData.price
			.div(BN.max(ONE, stats.historicalOracleData.lastOraclePriceTwap))
			.gt(oracleGuardRails.validity.tooVolatileRatio) ||
		stats.historicalOracleData.lastOraclePriceTwap
			.div(BN.max(ONE, oraclePriceData.price))
			.gt(oracleGuardRails.validity.tooVolatileRatio);

	const maxConfidenceIntervalMultiplier =
		getMaxConfidenceIntervalMultiplier(market);
	const isConfidenceTooLarge = BN.max(ONE, oraclePriceData.confidence)
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(oraclePriceData.price)
		.gt(
			oracleGuardRails.validity.confidenceIntervalMaxSize.mul(
				maxConfidenceIntervalMultiplier
			)
		);

	const oracleIsStale = new BN(slot)
		.sub(oraclePriceData.slot)
		.gt(
			millisToSlots(
				millisFromStoredUnits(oracleGuardRails.validity.slotsBeforeStaleForAmm),
				slotDuration
			)
		);

	return !(
		!oraclePriceData.hasSufficientNumberOfDataPoints ||
		oracleIsStale ||
		isOraclePriceNonPositive ||
		isOraclePriceTooVolatile ||
		isConfidenceTooLarge
	);
}

/**
 * True when the live oracle price has diverged from the market's 5-minute oracle TWAP by
 * more than the configured threshold (with a 50% safety floor). Distinct from
 * `isMarkOracleTooDivergent`, which compares mark (reserve) price to the same TWAP instead
 * of the live oracle price to itself — this catches an oracle feed itself jumping abruptly.
 * @param marketStats Market stats providing `historicalOracleData.lastOraclePriceTwap5Min`, PRICE_PRECISION (1e6).
 * @param oraclePriceData Live oracle reading (`price`, PRICE_PRECISION 1e6).
 * @param oracleGuardRails Protocol-wide guard rails; uses `priceDivergence.oracleTwap5MinPercentDivergence`, PERCENTAGE_PRECISION (1e6).
 * @returns `true` if the oracle-vs-TWAP spread exceeds the divergence threshold.
 */
export function isOracleTooDivergent(
	marketStats: MarketStats,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails
): boolean {
	const oracleSpreadPct = oraclePriceData.price
		.sub(marketStats.historicalOracleData.lastOraclePriceTwap5Min)
		.mul(PERCENTAGE_PRECISION)
		.div(marketStats.historicalOracleData.lastOraclePriceTwap5Min);
	const maxDivergence = BN.max(
		oracleGuardRails.priceDivergence.oracleTwap5MinPercentDivergence,
		PERCENTAGE_PRECISION.div(new BN(2))
	);
	const tooDivergent = oracleSpreadPct.abs().gte(maxDivergence);
	return tooDivergent;
}

/**
 * True when `|priceSpreadPct|` exceeds the configured mark/oracle divergence threshold,
 * with a 10% safety floor. Mirrors `is_mark_oracle_too_divergent` in
 * `programs/velocity/src/math/oracle.rs` — a pure decision helper used both to block
 * funding-rate updates (`block_operation`) and to reject orders/settlement when the market
 * has moved too far from its 5-minute oracle TWAP (`validate_market_within_price_band`,
 * which calls this once with the mark-vs-TWAP spread and once with the oracle-vs-TWAP
 * spread, blocking on whichever is more divergent).
 * @param priceSpreadPct Mark (or oracle) price spread vs the 5-minute oracle TWAP, PERCENTAGE_PRECISION (1e6, signed).
 * @param oracleGuardRails Protocol-wide guard rails; uses `priceDivergence.markOraclePercentDivergence`, PERCENTAGE_PRECISION (1e6).
 * @returns `true` if the spread exceeds `max(markOraclePercentDivergence, 10%)`.
 */
export function isMarkOracleTooDivergent(
	priceSpreadPct: BN,
	oracleGuardRails: OracleGuardRails
): boolean {
	const maxDivergence = BN.max(
		oracleGuardRails.priceDivergence.markOraclePercentDivergence,
		PERCENTAGE_PRECISION.div(TEN)
	);
	return priceSpreadPct.abs().gt(maxDivergence);
}

/**
 * Projects the oracle TWAP forward to `now` without requiring an on-chain update,
 * time-weighting the stored TWAP against the live oracle price clamped to within 1/3 of the
 * current TWAP (so a single outlier tick can't swing the live estimate too far). Uses the
 * 5-minute TWAP field when `period` equals `FIVE_MINUTE`, otherwise the funding-period (hourly) TWAP field.
 * @param histOracleData Market's historical oracle data (TWAP fields, PRICE_PRECISION 1e6, and their last-update timestamp).
 * @param oraclePriceData Live oracle reading (`price`, PRICE_PRECISION 1e6).
 * @param now Current unix timestamp (seconds).
 * @param period TWAP window length in seconds — pass `FIVE_MINUTE` for the 5-minute TWAP, otherwise the funding period is assumed.
 * @returns Live-projected oracle TWAP, PRICE_PRECISION (1e6).
 */
export function calculateLiveOracleTwap(
	histOracleData: HistoricalOracleData,
	oraclePriceData: Pick<OraclePriceData, 'price'>,
	now: BN,
	period: BN
): BN {
	let oracleTwap = undefined;
	if (period.eq(FIVE_MINUTE)) {
		oracleTwap = histOracleData.lastOraclePriceTwap5Min;
	} else {
		//todo: assumes its fundingPeriod (1hr)
		// period = amm.fundingPeriod;
		oracleTwap = histOracleData.lastOraclePriceTwap;
	}

	const sinceLastUpdate = BN.max(
		ONE,
		now.sub(histOracleData.lastOraclePriceTwapTs)
	);
	const sinceStart = BN.max(ZERO, period.sub(sinceLastUpdate));

	const clampRange = oracleTwap.div(new BN(3));

	const clampedOraclePrice = BN.min(
		oracleTwap.add(clampRange),
		BN.max(oraclePriceData.price, oracleTwap.sub(clampRange))
	);

	const newOracleTwap = oracleTwap
		.mul(sinceStart)
		.add(clampedOraclePrice.mul(sinceLastUpdate))
		.div(sinceStart.add(sinceLastUpdate));

	return newOracleTwap;
}

/**
 * Live-projected oracle price standard deviation, combining the live oracle price's
 * deviation from the freshly-projected 1hr and 5min TWAPs with the decayed stored
 * `marketStats.oracleStd`. Feeds `calculateVolSpreadBN`'s volatility-based spread component.
 * @param marketStats Market stats providing `historicalOracleData`, `fundingPeriod`, and the stored `oracleStd`.
 * @param oraclePriceData Live oracle reading (`price`, PRICE_PRECISION 1e6).
 * @param now Current unix timestamp (seconds).
 * @returns Live oracle price standard deviation, PRICE_PRECISION (1e6).
 */
export function calculateLiveOracleStd(
	marketStats: MarketStats,
	oraclePriceData: Pick<OraclePriceData, 'price'>,
	now: BN
): BN {
	const sinceLastUpdate = BN.max(
		ONE,
		now.sub(marketStats.historicalOracleData.lastOraclePriceTwapTs)
	);
	const sinceStart = BN.max(
		ZERO,
		marketStats.fundingPeriod.sub(sinceLastUpdate)
	);

	const liveOracleTwap = calculateLiveOracleTwap(
		marketStats.historicalOracleData,
		oraclePriceData,
		now,
		marketStats.fundingPeriod
	);

	const liveOracleTwap5MIN = calculateLiveOracleTwap(
		marketStats.historicalOracleData,
		oraclePriceData,
		now,
		FIVE_MINUTE
	);

	const priceDeltaVsTwap = BN.max(
		oraclePriceData.price.sub(liveOracleTwap).abs(),
		oraclePriceData.price.sub(liveOracleTwap5MIN).abs()
	);

	const oracleStd = priceDeltaVsTwap.add(
		marketStats.oracleStd.mul(sinceStart).div(sinceStart.add(sinceLastUpdate))
	);

	return oracleStd;
}

/**
 * Live-projected oracle confidence interval as a fraction of `reservePrice`, floored by a
 * decaying lower bound derived from the market's last stored confidence (so confidence
 * can't be understated immediately after a stale update — it decays back down over ~20
 * seconds). Feeds the volatility-spread and quote calculations that need a current
 * confidence estimate without waiting for the next on-chain refresh.
 * @param marketStats Market stats providing `lastOracleConfPct` and `historicalOracleData`'s last-update timestamp.
 * @param oraclePriceData Live oracle reading; uses `confidence`, PRICE_PRECISION (1e6).
 * @param reservePrice AMM reserve (mark) price used to express confidence as a fraction, PRICE_PRECISION (1e6).
 * @param now Current unix timestamp (seconds).
 * @returns Oracle confidence as a fraction of price, BID_ASK_SPREAD_PRECISION (1e6).
 */
export function getNewOracleConfPct(
	marketStats: MarketStats,
	oraclePriceData: Pick<OraclePriceData, 'confidence'>,
	reservePrice: BN,
	now: BN
): BN {
	const confInterval = oraclePriceData.confidence || ZERO;

	const sinceLastUpdate = BN.max(
		ZERO,
		now.sub(marketStats.historicalOracleData.lastOraclePriceTwapTs)
	);
	let lowerBoundConfPct = marketStats.lastOracleConfPct;
	if (sinceLastUpdate.gt(ZERO)) {
		const lowerBoundConfDivisor = BN.max(
			new BN(21).sub(sinceLastUpdate),
			new BN(5)
		);
		lowerBoundConfPct = marketStats.lastOracleConfPct.sub(
			marketStats.lastOracleConfPct.div(lowerBoundConfDivisor)
		);
	}
	const confIntervalPct = confInterval
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(reservePrice);

	const confIntervalPctResult = BN.max(confIntervalPct, lowerBoundConfPct);

	return confIntervalPctResult;
}

/**
 * Returns the scale factor to convert a price quoted under `firstOracleSource` into the
 * equivalent price under `secondOracleSource`, for the Pyth Lazer "scaled" variants
 * (`pythLazer1K`/`pythLazer1M` report a price 1,000x/1,000,000x smaller than `pythLazer` for
 * high-priced assets). Returns `{1, 1}` (no conversion) for any other source pair.
 * @param firstOracleSource Oracle source the input price is denominated in.
 * @param secondOracleSource Oracle source to convert the price into.
 * @returns `{ numerator, denominator }` such that `price * numerator / denominator` converts between sources.
 * @throws if either source is a removed Pyth-pull variant (`pythPull`, `pyth1KPull`, `pyth1MPull`, `pythStableCoinPull`).
 */
export function getMultipleBetweenOracleSources(
	firstOracleSource: OracleSource,
	secondOracleSource: OracleSource
): { numerator: BN; denominator: BN } {
	if (
		isOneOfVariant(firstOracleSource, [
			'pythPull',
			'pyth1KPull',
			'pyth1MPull',
			'pythStableCoinPull',
		]) ||
		isOneOfVariant(secondOracleSource, [
			'pythPull',
			'pyth1KPull',
			'pyth1MPull',
			'pythStableCoinPull',
		])
	) {
		throw new Error('Pyth pull oracle support has been removed from the SDK');
	}

	if (
		isVariant(firstOracleSource, 'pythLazer') &&
		isVariant(secondOracleSource, 'pythLazer1M')
	) {
		return { numerator: new BN(1000000), denominator: new BN(1) };
	}

	if (
		isVariant(firstOracleSource, 'pythLazer') &&
		isVariant(secondOracleSource, 'pythLazer1K')
	) {
		return { numerator: new BN(1000), denominator: new BN(1) };
	}

	if (
		isVariant(firstOracleSource, 'pythLazer1M') &&
		isVariant(secondOracleSource, 'pythLazer')
	) {
		return { numerator: new BN(1), denominator: new BN(1000000) };
	}

	if (
		isVariant(firstOracleSource, 'pythLazer1K') &&
		isVariant(secondOracleSource, 'pythLazer')
	) {
		return { numerator: new BN(1), denominator: new BN(1000) };
	}

	return { numerator: new BN(1), denominator: new BN(1) };
}

/**
 * Per-market multiplier applied to `confidenceIntervalMaxSize` for spot oracle
 * validity, mirroring `SpotMarket::get_max_confidence_interval_multiplier`:
 * 1x for Collateral/Protected, 5x for Cross, 50x for Isolated/Unlisted.
 * @param spotMarket Spot market whose `assetTier` selects the multiplier.
 * @returns Unitless multiplier (dimensionless BN).
 */
export function getSpotMaxConfidenceIntervalMultiplier(
	spotMarket: SpotMarketAccount
): BN {
	if (
		isVariant(spotMarket.assetTier, 'collateral') ||
		isVariant(spotMarket.assetTier, 'protected')
	) {
		return new BN(1);
	}
	if (isVariant(spotMarket.assetTier, 'cross')) {
		return new BN(5);
	}
	return new BN(50);
}

/**
 * Classifies a spot oracle reading's validity for the checks `MarginCalc`
 * cares about, mirroring the spot-market parameterization of
 * `oracle_validity` in `programs/velocity/src/math/oracle.rs` (twap from the
 * spot market's `historicalOracleData`, confidence multiplier from the asset
 * tier, stablecoin sources tolerate 3x margin staleness). Only the four
 * `MarginCalc`-relevant severities are distinguished; readings that pass all
 * four report `Valid`.
 * @param spotMarket Spot market providing the oracle TWAP, asset tier and oracle source.
 * @param oraclePriceData Oracle reading to validate (`price`/`confidence` PRICE_PRECISION 1e6, `slot`).
 * @param oracleGuardRails Protocol-wide validity thresholds (`state.oracleGuardRails`).
 * @param slot Current slot, used to compute oracle delay.
 * @param oracleStalenessBuffer Extra slots subtracted from the raw oracle delay (default 5).
 * @returns The most severe `MarginCalc`-relevant `OracleValidity` that applies.
 */
export function getSpotOracleValidity(
	spotMarket: SpotMarketAccount,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails,
	slot: BN,
	oracleStalenessBuffer = FIVE,
	slotDuration: SlotDurationMs = SLOT_DURATION_BASELINE
): OracleValidity {
	if (oraclePriceData.price.lte(ZERO)) {
		return OracleValidity.NonPositive;
	}

	const twap = spotMarket.historicalOracleData.lastOraclePriceTwap;
	const isTooVolatile = BN.max(oraclePriceData.price, twap)
		.div(BN.max(ONE, BN.min(oraclePriceData.price, twap)))
		.gt(oracleGuardRails.validity.tooVolatileRatio);
	if (isTooVolatile) {
		return OracleValidity.TooVolatile;
	}

	const confPctOfPrice = oraclePriceData.confidence
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(oraclePriceData.price);
	const isConfTooLarge = confPctOfPrice.gt(
		oracleGuardRails.validity.confidenceIntervalMaxSize.mul(
			getSpotMaxConfidenceIntervalMultiplier(spotMarket)
		)
	);
	if (isConfTooLarge) {
		return OracleValidity.TooUncertain;
	}

	const oracleDelay = slot.sub(oraclePriceData.slot).sub(oracleStalenessBuffer);
	let staleSlots = millisToSlots(
		millisFromStoredUnits(oracleGuardRails.validity.slotsBeforeStaleForMargin),
		slotDuration
	);
	if (isVariant(spotMarket.oracleSource, 'pythLazerStableCoin')) {
		staleSlots = staleSlots.muln(3);
	}
	if (oracleDelay.gt(staleSlots)) {
		return OracleValidity.StaleForMargin;
	}

	return OracleValidity.Valid;
}

/**
 * True when `validity` is acceptable for `VelocityAction::MarginCalc`,
 * mirroring `is_oracle_valid_for_action`: rejects `NonPositive`,
 * `TooVolatile`, `TooUncertain`, and `StaleForMargin`; every other
 * classification passes.
 * @param validity Classification from `getOracleValidity`/`getSpotOracleValidity`.
 * @returns Whether the equity-floor metric may trust this oracle.
 */
export function isOracleValidForMarginCalc(validity: OracleValidity): boolean {
	return !(
		validity === OracleValidity.NonPositive ||
		validity === OracleValidity.TooVolatile ||
		validity === OracleValidity.TooUncertain ||
		validity === OracleValidity.StaleForMargin
	);
}
