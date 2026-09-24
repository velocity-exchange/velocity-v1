import {
	HistoricalOracleData,
	MarketStats,
	OracleGuardRails,
	OracleSource,
	OracleValidity,
	PerpOperation,
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
	TEN,
} from '../constants/numericConstants';
import { assert } from '../assert/assert';
import { BN } from '../isomorphic/anchor';
import {
	Millis,
	SlotDurationState,
	STORED_UNIT_MS,
	activeSlotDurationFromState,
	elapsedMillisFromSlotDelta,
	millis,
	millisFromStoredUnits,
	millisToSlots,
	millisToSlotsCeil,
} from './time';
import { isOperationPaused } from './exchangeStatus';

/**
 * Allowance the SDK subtracts from a raw oracle delay. The program applies no such subtraction.
 * Held as wall-clock time, it converts to slots at the live slot duration, so it does not shrink as slots get faster.
 */
export const ORACLE_STALENESS_BUFFER = millis(5 * STORED_UNIT_MS);

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
 * Mirrors `oracle_validity` in `programs/velocity/src/math/oracle.rs`.
 * Checks run in severity order. The first failing check wins, else the result is `OracleValidity.Valid`.
 * @param market Perp market providing contract tier, oracle source, and stale-slot overrides.
 * @param oraclePriceData Oracle reading to validate (`price`/`confidence` PRICE_PRECISION 1e6, `slot`).
 * @param oracleGuardRails Protocol-wide validity thresholds (`state.oracleGuardRails`).
 * @param slot Current slot, used to compute oracle delay.
 * @param oracleStalenessBuffer Slots subtracted from the raw oracle delay before the staleness checks. Omit it for `ORACLE_STALENESS_BUFFER`, two seconds of wall clock, converted at the live slot duration.
 * @param isMmSourcedPrice Whether `oraclePriceData` carries an MM-oracle-sourced price. It
 * affects only the immediate-fill threshold for an unset `oracleSlotDelayOverride`, which is a
 * negative one. That threshold resolves to `MM_ORACLE_MIN_WRITE_GAP` for an MM-sourced price and
 * to zero for an exchange-sourced one. This mirrors `immediate_price_is_mm_sourced` in
 * `oracle_validity`.
 * @param slotDurationState The `State` account, or the slot duration fields of it. The oracle
 * age is integrated per slot duration regime and compared against the wall clock staleness
 * thresholds, as `oracle_validity` does.
 * @returns The most severe `OracleValidity` classification that applies.
 */
export function getOracleValidity(
	market: PerpMarketAccount,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails,
	slot: BN,
	oracleStalenessBuffer?: BN,
	isMmSourcedPrice = false,
	slotDurationState: SlotDurationState = {}
): OracleValidity {
	const stalenessBuffer =
		oracleStalenessBuffer ??
		millisToSlots(
			ORACLE_STALENESS_BUFFER,
			activeSlotDurationFromState(slotDurationState, slot)
		);
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

	// The buffered oracle delay as wall clock age, integrated per slot duration
	// regime. This mirrors `oracle_age` in `oracle_validity`.
	const oracleAge = elapsedMillisFromSlotDelta(
		slotDurationState,
		BN.max(slot.sub(oraclePriceData.slot).sub(stalenessBuffer), ZERO),
		slot
	);

	// This mirrors `math::oracle::oracle_validity`. An override of 0 is the
	// explicit sentinel for "never allow immediate AMM fills". A negative
	// override means unset, and what it resolves to depends on the price
	// source. An MM-oracle-sourced price resolves to MM_ORACLE_MIN_WRITE_GAP.
	// The program refuses MM-oracle writes closer together than that, so a
	// tighter threshold could never be satisfied. An exchange-sourced price
	// resolves to the strict zero threshold, because it can be same-slot fresh.
	let isStaleForAmmImmediate = true;
	if (market.oracleSlotDelayOverride < 0) {
		const unsetThreshold = isMmSourcedPrice
			? elapsedMillisFromSlotDelta(
					slotDurationState,
					millisToSlotsCeil(
						MM_ORACLE_MIN_WRITE_GAP,
						activeSlotDurationFromState(slotDurationState, slot)
					),
					slot
			  )
			: (ZERO as Millis);
		isStaleForAmmImmediate = oracleAge.gt(unsetThreshold);
	} else if (market.oracleSlotDelayOverride != 0) {
		isStaleForAmmImmediate = oracleAge.gt(
			millisFromStoredUnits(market.oracleSlotDelayOverride)
		);
	}

	let isStaleForAmmLowRisk = false;
	if (market.oracleLowRiskSlotDelayOverride != 0) {
		isStaleForAmmLowRisk = oracleAge.gt(
			millisFromStoredUnits(Math.max(market.oracleLowRiskSlotDelayOverride, 0))
		);
	} else {
		isStaleForAmmLowRisk = oracleAge.gt(
			millisFromStoredUnits(oracleGuardRails.validity.slotsBeforeStaleForAmm)
		);
	}

	let isStaleForMargin = oracleAge.gt(
		millisFromStoredUnits(oracleGuardRails.validity.slotsBeforeStaleForMargin)
	);
	if (isVariant(market.oracleSource, 'pythLazerStableCoin')) {
		isStaleForMargin = oracleAge.gt(
			millisFromStoredUnits(
				oracleGuardRails.validity.slotsBeforeStaleForMargin
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
 * Simplified, AMM-fill-oriented validity check: true only if the oracle has data, is not stale, has a positive price, is not too volatile, and its confidence interval is not too wide.
 * Unlike `getOracleValidity` this does not distinguish severity tiers. It is a single valid/invalid gate for whether the AMM may fill against this price.
 * @param market Perp market providing the oracle TWAP and contract tier for the confidence multiplier.
 * @param oraclePriceData Oracle reading to validate (`price`/`confidence` PRICE_PRECISION 1e6).
 * @param oracleGuardRails Protocol-wide validity thresholds.
 * @param slot Current slot, used to compute oracle staleness.
 * @param slotDurationState The `State` account, or the slot duration fields of it. The staleness windows convert to actual slots through the live slot duration.
 * @returns `true` if the oracle is valid for an AMM-only fill.
 */
export function isOracleValid(
	market: PerpMarketAccount,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails,
	slot: number,
	slotDurationState: SlotDurationState = {}
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

	const oracleIsStale = elapsedMillisFromSlotDelta(
		slotDurationState,
		BN.max(new BN(slot).sub(oraclePriceData.slot), ZERO),
		new BN(slot)
	).gt(millisFromStoredUnits(oracleGuardRails.validity.slotsBeforeStaleForAmm));

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
 * Predicts whether the program blocks a funding update, mirroring `math::oracle::block_operation`.
 * Funding tolerates a stale or data-thin reading but rejects a non-positive, too volatile, or too uncertain price, plus mark-to-TWAP divergence, a paused market, or an AMM stale beyond 40 percent of the funding period.
 */
export function blockOperation(
	market: PerpMarketAccount,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails,
	reservePrice: BN,
	currentSlot: BN,
	slotDurationState: SlotDurationState = {}
): boolean {
	const validity = getOracleValidity(
		market,
		oraclePriceData,
		oracleGuardRails,
		currentSlot,
		ZERO,
		false,
		slotDurationState
	);
	const oracleInvalidForFunding =
		validity === OracleValidity.NonPositive ||
		validity === OracleValidity.TooVolatile ||
		validity === OracleValidity.TooUncertain;

	const oracleTwap5Min =
		market.marketStats.historicalOracleData.lastOraclePriceTwap5Min;
	const markSpreadPct = reservePrice
		.sub(oracleTwap5Min)
		.mul(BID_ASK_SPREAD_PRECISION)
		.div(reservePrice);
	const markTooDivergent = isMarkOracleTooDivergent(
		markSpreadPct,
		oracleGuardRails
	);

	const slotsSinceAmmUpdate = BN.max(
		currentSlot.sub(market.amm.lastUpdateSlot),
		ZERO
	);
	const ammStaleMs = elapsedMillisFromSlotDelta(
		slotDurationState,
		slotsSinceAmmUpdate,
		currentSlot
	);
	const staleLimitMs = market.marketStats.fundingPeriod.muln(400);

	return (
		ammStaleMs.gt(staleLimitMs) ||
		oracleInvalidForFunding ||
		markTooDivergent ||
		isOperationPaused(market.pausedOperations, PerpOperation.UPDATE_FUNDING)
	);
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
 * validity. Mirrors `SpotMarket::get_max_confidence_interval_multiplier`: 1 for
 * Collateral and Protected, 5 for Cross, 50 for Isolated and Unlisted.
 * @param spotMarket The spot market whose `assetTier` selects the multiplier.
 * @returns The multiplier, as a unitless BN.
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
 * Mirrors the spot-market parameters of `oracle_validity` in `programs/velocity/src/math/oracle.rs`. A stablecoin source tolerates three times the margin staleness.
 * Only the four severities `MarginCalc` reads are distinguished. A reading that passes all four reports `Valid`.
 * @param spotMarket The spot market that provides the oracle TWAP, the asset tier and the oracle source.
 * @param oraclePriceData The oracle reading to validate. `price` and `confidence` are in PRICE_PRECISION 1e6.
 * @param oracleGuardRails Protocol-wide validity thresholds, from `state.oracleGuardRails`.
 * @param slot The current slot, which the oracle delay is measured against.
 * @param oracleStalenessBuffer Slots subtracted from the raw oracle delay. Omit it for `ORACLE_STALENESS_BUFFER`, two seconds of wall clock, converted at the live slot duration.
 * @param slotDurationState The `State` account, or the slot duration fields of it. The oracle age is integrated per slot duration regime.
 * @returns The most severe `OracleValidity` that `MarginCalc` reads and that applies.
 */
export function getSpotOracleValidity(
	spotMarket: SpotMarketAccount,
	oraclePriceData: OraclePriceData,
	oracleGuardRails: OracleGuardRails,
	slot: BN,
	oracleStalenessBuffer?: BN,
	slotDurationState: SlotDurationState = {}
): OracleValidity {
	const stalenessBuffer =
		oracleStalenessBuffer ??
		millisToSlots(
			ORACLE_STALENESS_BUFFER,
			activeSlotDurationFromState(slotDurationState, slot)
		);
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

	const oracleAge = elapsedMillisFromSlotDelta(
		slotDurationState,
		BN.max(slot.sub(oraclePriceData.slot).sub(stalenessBuffer), ZERO),
		slot
	);
	let staleThreshold = millisFromStoredUnits(
		oracleGuardRails.validity.slotsBeforeStaleForMargin
	);
	if (isVariant(spotMarket.oracleSource, 'pythLazerStableCoin')) {
		staleThreshold = staleThreshold.muln(3) as Millis;
	}
	if (oracleAge.gt(staleThreshold)) {
		return OracleValidity.StaleForMargin;
	}

	return OracleValidity.Valid;
}

/**
 * True when `validity` is acceptable for `VelocityAction::MarginCalc`, mirroring `is_oracle_valid_for_action`.
 * Rejects `NonPositive`, `TooVolatile`, `TooUncertain` and `StaleForMargin`. Every other classification passes.
 */
export function isOracleValidForMarginCalc(validity: OracleValidity): boolean {
	return !(
		validity === OracleValidity.NonPositive ||
		validity === OracleValidity.TooVolatile ||
		validity === OracleValidity.TooUncertain ||
		validity === OracleValidity.StaleForMargin
	);
}
