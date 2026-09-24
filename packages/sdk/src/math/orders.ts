import {
	isOneOfVariant,
	isVariant,
	PerpMarketAccount,
	AMM,
	MarketStats,
	Order,
	PositionDirection,
	MarketTypeStr,
	OrderBitFlag,
	OrderType,
	OracleValidity,
	PostOnlyParams,
	PerpOperation,
	StateAccount,
} from '../types';
import {
	ZERO,
	ONE,
	SPOT_MARKET_IMF_PRECISION,
	MARGIN_PRECISION,
} from '../constants/numericConstants';
import { BN } from '../isomorphic/anchor';
import { PublicKey } from '@solana/web3.js';
import { sha256 } from '@noble/hashes/sha256';
import { MMOraclePriceData, OraclePriceData } from '../oracles/types';
import {
	SlotDurationState,
	elapsedMillis,
	millis,
	slotAtOrAfterDuration,
} from './time';
import {
	calculateMaxBaseAssetAmountFillable,
	calculateMaxBaseAssetAmountToTrade,
	calculateUpdatedAMM,
} from './amm';
import { calculateSizePremiumLiabilityWeight } from './margin';
import { getOracleValidity } from './oracles';
import { isAmmDrawdownPause, isOperationPaused } from './exchangeStatus';

/** Rounds `baseAssetAmount` down to the nearest multiple of `stepSize` (always truncates toward zero — never rounds up), matching the on-chain order/fill step-size standardization. @param baseAssetAmount Amount to round, BASE_PRECISION (1e9). @param stepSize Market's order step size, BASE_PRECISION (1e9). @returns Amount rounded down to a `stepSize` multiple, BASE_PRECISION (1e9). */
export function standardizeBaseAssetAmount(
	baseAssetAmount: BN,
	stepSize: BN
): BN {
	const remainder = baseAssetAmount.mod(stepSize);
	return baseAssetAmount.sub(remainder);
}

/**
 * Rounds `price` to a multiple of `tickSize`, rounding in the direction that's conservative
 * for the order's side: down for a long (never overpay past the tick) and up for a short
 * (never undersell past the tick). Used across auction pricing and limit-price derivation so
 * every price the SDK produces already lines up with the market's `orderTickSize` before it
 * reaches the program, avoiding the on-chain tick-size rejection this standardization fix
 * addresses. A `tickSize <= 0` (unset/no constraint) or `price == 0` passes through
 * unchanged.
 * @param price Price to standardize, PRICE_PRECISION (1e6).
 * @param tickSize Market's order tick size, PRICE_PRECISION (1e6). Non-positive means "no tick constraint."
 * @param direction Order side; determines rounding direction.
 * @returns `price` rounded to the nearest tick in the conservative direction, PRICE_PRECISION (1e6).
 */
export function standardizePrice(
	price: BN,
	tickSize: BN,
	direction: PositionDirection
): BN {
	if (price.eq(ZERO)) {
		return price;
	}

	// A non-positive tick size means "no tick constraint" (e.g. unset markets);
	// on-chain markets always have tick_size >= 1, but guard against a zero
	// divisor rather than throwing.
	if (tickSize.lte(ZERO)) {
		return price;
	}

	const remainder = price.mod(tickSize);
	if (remainder.eq(ZERO)) {
		return price;
	}

	if (isVariant(direction, 'long')) {
		return price.sub(remainder);
	} else {
		return price.add(tickSize).sub(remainder);
	}
}

/**
 * Resolves an order's effective limit price at the current slot, standardized to
 * `tickSize`: the in-progress auction price while the auction hasn't completed, the
 * oracle-offset price for oracle-pegged orders, the order's fixed `price` if set, or
 * `fallbackPrice` (also standardized) for a market order with no price/offset/auction.
 * @param order Order to price.
 * @param oraclePriceData Oracle price source — use `MMOraclePriceData` for perp orders, `OraclePriceData` for spot.
 * @param slot Current slot, used to evaluate auction progress.
 * @param fallbackPrice Price to return for a market order with no auction/offset/fixed price (e.g. a mark or oracle price), PRICE_PRECISION (1e6).
 * @param tickSize Market's order tick size, PRICE_PRECISION (1e6). Defaults to `ONE` (no effective standardization).
 * @returns Limit price, PRICE_PRECISION (1e6); `undefined` if the order has no resolvable price and no `fallbackPrice` was given.
 */
export function getLimitPrice<T extends MarketTypeStr>(
	order: Order,
	oraclePriceData: T extends 'spot' ? OraclePriceData : MMOraclePriceData,
	fallbackPrice?: BN,
	tickSize: BN = ONE
): BN | undefined {
	if (!order.oraclePriceOffset.eq(ZERO)) {
		const limitPrice = BN.max(
			oraclePriceData.price.add(order.oraclePriceOffset),
			tickSize
		);
		return standardizePrice(limitPrice, tickSize, order.direction);
	} else if (order.price.eq(ZERO)) {
		return fallbackPrice === undefined
			? undefined
			: standardizePrice(fallbackPrice, tickSize, order.direction);
	} else {
		return order.price;
	}
}

/** True if the order has any way to resolve a limit price right now: a fixed `price` or a nonzero oracle offset. */
export function hasLimitPrice(order: Order): boolean {
	return order.price.gt(ZERO) || !order.oraclePriceOffset.eq(ZERO);
}

/**
 * True if the AMM is currently a fillable liquidity source for `order` — either it's
 * expired (always fillable to clean up), or the AMM has fillable size at the order's limit
 * price AND is an allowed liquidity source right now (`isFallbackAvailableLiquiditySource`,
 * which gates on oracle validity and low-risk-for-AMM classification).
 * @param order Order to check.
 * @param market Perp market the order is on.
 * @param mmOraclePriceData Current MM oracle price data.
 * @param slot Current slot.
 * @param ts Current unix timestamp (seconds), used for expiry.
 * @param state Global state, providing oracle guard rails and paused-operations flags.
 * @returns `true` if the AMM may currently fill this order.
 */
export function isFillableByVAMM(
	order: Order,
	market: PerpMarketAccount,
	mmOraclePriceData: MMOraclePriceData,
	slot: number,
	ts: number,
	state: StateAccount
): boolean {
	return (
		(isFallbackAvailableLiquiditySource(
			order,
			mmOraclePriceData,
			slot,
			state,
			market
		) &&
			calculateBaseAssetAmountForAmmToFulfill(
				order,
				market,
				mmOraclePriceData
			).gt(ZERO)) ||
		isOrderExpired(order, ts)
	);
}

/**
 * True if filling `order` against the AMM is considered low-risk even when the MM oracle
 * isn't fully valid, approximating `Order::is_low_risk_for_amm` in
 * `programs/velocity/src/state/user.rs`. Always false for spot orders. True when the order
 * was placed at or before the MM oracle's slot (so it can't be exploiting oracle staleness),
 * during liquidation, or when the order carries the `SafeTriggerOrder` bit flag.
 * @param order Order to check.
 * @param mmOraclePriceData Current MM oracle price data, used for its `slot`.
 * @param isLiquidation Whether the fill is part of a liquidation (always low-risk if so).
 * @returns `true` if the order is low-risk for an AMM fill under a degraded oracle.
 */
export function isLowRiskForAmm(
	order: Order,
	mmOraclePriceData: MMOraclePriceData,
	isLiquidation?: boolean
): boolean {
	if (isVariant(order.marketType, 'spot')) {
		return false;
	}

	const orderOlderThanOracleDelay = new BN(order.slot).lte(
		mmOraclePriceData.slot
	);

	return (
		orderOlderThanOracleDelay ||
		isLiquidation ||
		(order.bitFlags & OrderBitFlag.SafeTriggerOrder) !== 0
	);
}

/**
 * Calculates how much of `order` the AMM can currently fill, capped by both the order's
 * limit price (via `calculateBaseAssetAmountToFillUpToLimitPrice`, standardized to
 * `market.orderTickSize`) and the AMM's own max fillable size
 * (`calculateMaxBaseAssetAmountFillable`). Returns zero for a not-yet-triggered
 * trigger order. Prices against `calculateUpdatedAMM` (i.e. the repegged/curve-updated AMM
 * state), not the raw stored reserves.
 * @param order Order to evaluate.
 * @param market Perp market the order is on.
 * @param mmOraclePriceData Current MM oracle price data.
 * @returns Fillable base asset amount, BASE_PRECISION (1e9).
 */
export function calculateBaseAssetAmountForAmmToFulfill(
	order: Order,
	market: PerpMarketAccount,
	mmOraclePriceData: MMOraclePriceData
): BN {
	if (mustBeTriggered(order) && !isTriggered(order)) {
		return ZERO;
	}

	const limitPrice = getLimitPrice(
		order,
		mmOraclePriceData,
		undefined,
		market.orderTickSize
	);
	let baseAssetAmount;

	const updatedAMM = calculateUpdatedAMM(market.amm, mmOraclePriceData);
	if (limitPrice !== undefined) {
		baseAssetAmount = calculateBaseAssetAmountToFillUpToLimitPrice(
			order,
			updatedAMM,
			market.marketStats,
			market.orderStepSize,
			market.orderTickSize,
			limitPrice,
			mmOraclePriceData
		);
	} else {
		baseAssetAmount = order.baseAssetAmount.sub(order.baseAssetAmountFilled);
	}

	const maxBaseAssetAmount = calculateMaxBaseAssetAmountFillable(
		updatedAMM,
		market.orderStepSize,
		order.direction
	);

	return BN.min(maxBaseAssetAmount, baseAssetAmount);
}

/**
 * Calculates how much base asset the AMM can trade against `order` without crossing its
 * limit price, adjusting the limit by one tick in the order's favor (so the AMM never fills
 * exactly at the boundary) before asking `calculateMaxBaseAssetAmountToTrade` how much
 * inventory the AMM has at that price. Returns zero if the AMM would only trade in the
 * opposite direction from the order. Caps the result at the order's unfilled remainder.
 * @param order Order being filled.
 * @param amm AMM state to trade against.
 * @param marketStats Market stats needed to compute spread reserves.
 * @param orderStepSize Market's order step size, BASE_PRECISION (1e9), used to standardize the result.
 * @param orderTickSize Market's order tick size, PRICE_PRECISION (1e6), used to adjust the limit price by one tick.
 * @param limitPrice Order's limit price, PRICE_PRECISION (1e6).
 * @param mmOraclePriceData Current MM oracle price data.
 * @returns Fillable base asset amount up to the limit price, BASE_PRECISION (1e9).
 */
export function calculateBaseAssetAmountToFillUpToLimitPrice(
	order: Order,
	amm: AMM,
	marketStats: MarketStats,
	orderStepSize: BN,
	orderTickSize: BN,
	limitPrice: BN,
	mmOraclePriceData: Pick<MMOraclePriceData, 'price' | 'confidence'>
): BN {
	const adjustedLimitPrice = isVariant(order.direction, 'long')
		? limitPrice.sub(orderTickSize)
		: limitPrice.add(orderTickSize);

	const [maxAmountToTrade, direction] = calculateMaxBaseAssetAmountToTrade(
		amm,
		marketStats,
		adjustedLimitPrice,
		order.direction,
		mmOraclePriceData
	);

	const baseAssetAmount = standardizeBaseAssetAmount(
		maxAmountToTrade,
		orderStepSize
	);

	// Check that directions are the same
	const sameDirection = isSameDirection(direction, order.direction);
	if (!sameDirection) {
		return ZERO;
	}

	const baseAssetAmountUnfilled = order.baseAssetAmount.sub(
		order.baseAssetAmountFilled
	);
	return baseAssetAmount.gt(baseAssetAmountUnfilled)
		? baseAssetAmountUnfilled
		: baseAssetAmount;
}

function isSameDirection(
	firstDirection: PositionDirection,
	secondDirection: PositionDirection
): boolean {
	return (
		(isVariant(firstDirection, 'long') && isVariant(secondDirection, 'long')) ||
		(isVariant(firstDirection, 'short') && isVariant(secondDirection, 'short'))
	);
}

/**
 * True if `order.maxTs` has passed as of `ts`. Never true for trigger orders, non-`open`
 * orders, or orders with no expiry (`maxTs == 0`).
 * @param order Order to check.
 * @param ts Current unix timestamp (seconds).
 * @param enforceBuffer If true, extends `maxTs` by `bufferSeconds` before comparing, but only for limit orders (default false) — gives resting limit orders a grace period before being treated as expired.
 * @param bufferSeconds Grace period in seconds applied when `enforceBuffer` is true (default 15).
 * @returns `true` if the order has expired.
 */
export function isOrderExpired(
	order: Order,
	ts: number,
	enforceBuffer = false,
	bufferSeconds = 15
): boolean {
	if (
		mustBeTriggered(order) ||
		!isVariant(order.status, 'open') ||
		order.maxTs.eq(ZERO)
	) {
		return false;
	}

	let maxTs;
	if (enforceBuffer && isLimitOrder(order)) {
		maxTs = order.maxTs.addn(bufferSeconds);
	} else {
		maxTs = order.maxTs;
	}

	return new BN(ts).gt(maxTs);
}

/**
 * Last slot a signed-message (swift) order may still be placed on chain.
 * Mirrors the program's `signed_msg_max_slot`. A resting limit's message slot
 * is itself the deadline. Any other order gets `SIGNED_MSG_FILL_WINDOW_MS`
 * past it, integrated across slot duration transitions.
 */
export function signedMsgOrderMaxSlot(
	state: SlotDurationState,
	orderSlot: BN,
	isRestingLimit: boolean
): BN {
	if (isRestingLimit) {
		return orderSlot;
	}

	return slotAtOrAfterDuration(
		state,
		orderSlot,
		millis(SIGNED_MSG_FILL_WINDOW_MS)
	);
}

/**
 * How long a keeper may take to land a signed message. Mirrors the program's
 * `SIGNED_MSG_FILL_WINDOW`. The order's worst price was measured against the
 * oracle at signing, so a message landing later no longer describes the
 * market the signer agreed to.
 */
export const SIGNED_MSG_FILL_WINDOW_MS = 30_000;

/**
 * Mirrors `place_signed_msg_taker_order`'s `order_slot > clock.slot` gate.
 * Compares BNs: real slot numbers exceed `BN.gtn`/`BN.lten`'s 26-bit limit.
 */
export function signedMsgOrderSlotReached(
	orderSlot: BN,
	currentSlot: number
): boolean {
	return orderSlot.lte(new BN(currentSlot));
}

/**
 * Lead before `place_signed_msg_taker_order` refuses an early resting-limit
 * placement. Mirrors `max_resting_limit_lead`. The UI stamps about 14s ahead.
 */
export const SIGNED_MSG_RESTING_LIMIT_MAX_LEAD_MS = 30_000;

/**
 * True if a signed-message (swift) order rests from placement. The program
 * treats such an order's message slot as a placement deadline.
 */
export function isRestingSignedMsgLimitOrder(orderType: OrderType): boolean {
	return isVariant(orderType, 'limit');
}

/**
 * Mirrors `place_signed_msg_taker_order`'s slot gates via
 * `signedMsgOrderSlotReached` and `isRestingSignedMsgLimitOrder`.
 */
export function signedMsgOrderPlaceable(
	state: SlotDurationState,
	order: {
		slot: BN;
		orderType: OrderType;
	},
	currentSlot: number
): boolean {
	if (signedMsgOrderSlotReached(order.slot, currentSlot)) {
		return true;
	}
	if (!isRestingSignedMsgLimitOrder(order.orderType)) {
		return false;
	}
	return elapsedMillis(state, new BN(currentSlot), order.slot).lten(
		SIGNED_MSG_RESTING_LIMIT_MAX_LEAD_MS
	);
}

/**
 * Why `place_signed_msg_taker_order` refuses this entry order, or `undefined`
 * when it admits it. Mirrors `validate_entry_order_type`. A post-only entry
 * cannot take, and a trigger entry has no slot to wait in.
 */
export function signedMsgEntryOrderRefusal(params: {
	orderType: OrderType;
	postOnly: PostOnlyParams;
}): string | undefined {
	if (!isVariant(params.postOnly, 'none')) {
		return 'a signed-message entry cannot be post-only';
	}

	if (isOneOfVariant(params.orderType, ['triggerMarket', 'triggerLimit'])) {
		return 'a signed-message entry cannot be a trigger order';
	}

	return undefined;
}

/** True if `order.orderType` is `market`, `triggerMarket`, or `oracle`. */
export function isMarketOrder(order: Order): boolean {
	return isOneOfVariant(order.orderType, ['market', 'triggerMarket', 'oracle']);
}

/** True if `order.orderType` is `limit` or `triggerLimit`. */
export function isLimitOrder(order: Order): boolean {
	return isOneOfVariant(order.orderType, ['limit', 'triggerLimit']);
}

/** True if the order requires a trigger condition to fire before it becomes fillable (`triggerMarket`/`triggerLimit`). */
export function mustBeTriggered(order: Order): boolean {
	return isOneOfVariant(order.orderType, ['triggerMarket', 'triggerLimit']);
}

/** True if a trigger order's condition has already fired (`triggeredAbove`/`triggeredBelow`). */
export function isTriggered(order: Order): boolean {
	return isOneOfVariant(order.triggerCondition, [
		'triggeredAbove',
		'triggeredBelow',
	]);
}

/** True if the order rests on the book. A limit order rests from placement; nothing else does. */
export function isRestingLimitOrder(order: Order): boolean {
	return isLimitOrder(order);
}

/** True if the order was submitted via the signed-message (swift/off-chain relay) path (`OrderBitFlag.SignedMessage`). */
export function isSignedMsgOrder(order: Order): boolean {
	return (order.bitFlags & OrderBitFlag.SignedMessage) !== 0;
}

/** True if the order carries a builder-fee attribution (`OrderBitFlag.HasBuilder`) — the associated builder is entitled to a fee cut on fill. */
export function hasBuilder(order: Order): boolean {
	return (order.bitFlags & OrderBitFlag.HasBuilder) !== 0;
}

/**
 * Resolves the effective base asset amount for a reduce-only order: caps it so the order
 * can't flip the position through zero (a reduce-only long can close at most the existing
 * short, and vice versa). Non-reduce-only orders pass through `order.baseAssetAmount`
 * unchanged.
 * @param order Order to resolve.
 * @param existingBaseAssetAmount Current position size before this order fills, BASE_PRECISION (1e9, signed).
 * @returns Effective base asset amount, BASE_PRECISION (1e9).
 */
export function calculateOrderBaseAssetAmount(
	order: Order,
	existingBaseAssetAmount: BN
): BN {
	if (!order.reduceOnly) {
		return order.baseAssetAmount;
	}

	if (isVariant(order.direction, 'long')) {
		return BN.min(
			BN.min(existingBaseAssetAmount, ZERO).abs(),
			order.baseAssetAmount
		);
	} else {
		return BN.min(BN.max(existingBaseAssetAmount, ZERO), order.baseAssetAmount);
	}
}

// ---------- inverse ----------
/**
 * Inverts `calculateSizePremiumLiabilityWeight` via binary search: given a target margin ratio
 * (liability weight), finds the largest position `size` whose size-premium-adjusted liability
 * weight is still `<= target`. Used to size down an order/position to stay under a margin-ratio
 * target as size grows (the on-chain weight increases with `sqrt(size)` via `imfFactor`).
 * @param target Target (max acceptable) liability weight, MARGIN_PRECISION (1e4).
 * @param imfFactor Market's initial-margin-fraction scaling factor, SPOT_MARKET_IMF_PRECISION-scaled.
 * @param liabilityWeight Market's base (zero-size) liability weight, MARGIN_PRECISION (1e4).
 * @param market Perp market providing `maxOpenInterest` as a final cap on the result.
 * @returns Max size, AMM_RESERVE_PRECISION (1e9), capped at `market.maxOpenInterest` (a zero `maxOpenInterest` means uncapped, per on-chain convention); `null` if `target < liabilityWeight` (impossible) or `imfFactor` is zero (weight is size-invariant, so no size bounds it).
 */
export function maxSizeForTargetLiabilityWeightBN(
	target: BN,
	imfFactor: BN,
	liabilityWeight: BN,
	market: PerpMarketAccount
): BN | null {
	if (target.lt(liabilityWeight)) return null;
	if (imfFactor.isZero()) return null;

	const base = liabilityWeight.muln(4).divn(5);

	const denom = new BN(100_000)
		.mul(SPOT_MARKET_IMF_PRECISION)
		.div(MARGIN_PRECISION);
	if (denom.isZero())
		throw new Error('denom=0: bad precision/spotImfPrecision');

	const allowedInc = target.gt(base) ? target.sub(base) : ZERO;

	const maxSqrt = allowedInc.mul(denom).div(imfFactor);

	if (maxSqrt.lte(ZERO)) {
		const fitsZero = calculateSizePremiumLiabilityWeight(
			ZERO,
			imfFactor,
			liabilityWeight,
			MARGIN_PRECISION
		).lte(target);
		return fitsZero ? ZERO : null;
	}

	let hi = maxSqrt.mul(maxSqrt).sub(ONE).divn(10);
	if (hi.isNeg()) hi = ZERO;

	let lo = ZERO;
	while (lo.lt(hi)) {
		const mid = lo.add(hi).add(ONE).divn(2); // upper mid to prevent infinite loop
		if (
			calculateSizePremiumLiabilityWeight(
				mid,
				imfFactor,
				liabilityWeight,
				MARGIN_PRECISION
			).lte(target)
		) {
			lo = mid;
		} else {
			hi = mid.sub(ONE);
		}
	}

	// cap at max OI. A maxOpenInterest of 0 means no configured cap (unlimited),
	// matching the on-chain convention — do not treat it as a hard cap of 0.
	const maxOpenInterest = market.maxOpenInterest;
	if (!maxOpenInterest.isZero() && lo.gt(maxOpenInterest)) {
		return maxOpenInterest;
	}

	return lo;
}

/**
 * Width of a route digest, sized so every subset of a market's carry limit
 * over its registered entries still fits as that set grows.
 */
export const ROUTE_DIGEST_LEN = 8;

/**
 * Digest of a signed route: `QuoterV0` entries reduced to the bytes a
 * `SignedMsgOrderId` holds. Mirrors the program's
 * `state::order_params::route_digest`. Sorted and deduped first, so the
 * same route always digests the same way. An empty route digests to zero
 * bytes. A real route never does, so one equality check tells the two apart.
 * @param route The taker's signed quoter entries, empty or absent if unrouted.
 * @returns The digest bytes, as `SignedMsgOrderId.routeDigest` holds them.
 */
export function getRouteDigest(route?: PublicKey[] | null): number[] {
	if (!route || route.length === 0) {
		return new Array(ROUTE_DIGEST_LEN).fill(0);
	}

	const keys = route
		.map((key) => key.toBytes())
		.sort(Buffer.compare as (a: Uint8Array, b: Uint8Array) => number);
	const deduped = keys.filter(
		(key, index) => index === 0 || Buffer.compare(keys[index - 1], key) !== 0
	);
	const digest = Array.from(
		sha256(Buffer.concat(deduped)).slice(0, ROUTE_DIGEST_LEN)
	);

	// Never collide with "no route": a real route must be distinguishable from
	// an absent one.
	if (digest.every((byte) => byte === 0)) {
		digest[0] = 1;
	}

	return digest;
}

export function isFallbackAvailableLiquiditySource(
	order: Order,
	mmOraclePriceData: MMOraclePriceData,
	slot: number,
	state: StateAccount,
	market: PerpMarketAccount,
	isLiquidation?: boolean
): boolean {
	if (isOperationPaused(market.pausedOperations, PerpOperation.AMM_FILL)) {
		return false;
	}

	if (isAmmDrawdownPause(market)) {
		return false;
	}

	// MM-oracle volatility gate (M15): mirrors `amm_fill_gates_ok`'s
	// `mm_oracle_not_too_volatile`. We already use safe MM oracle data, but the AMM isn't
	// available if we *could* have used the MM oracle yet fell back due to a >1% price diff —
	// early volatility protection. Only applies when the MM oracle is enabled and at least as
	// recent as the exchange oracle; skipped when those flags weren't populated.
	if (
		mmOraclePriceData.isMMOracleEnabled &&
		mmOraclePriceData.isMMOracleAsRecent &&
		mmOraclePriceData.isMMExchangeDiffBpsHigh
	) {
		return false;
	}

	const oracleValidity = getOracleValidity(
		market!,
		{
			price: mmOraclePriceData.price,
			slot: mmOraclePriceData.slot,
			confidence: mmOraclePriceData.confidence,
			hasSufficientNumberOfDataPoints:
				mmOraclePriceData.hasSufficientNumberOfDataPoints,
		},
		state.oracleGuardRails,
		new BN(slot),
		undefined,
		mmOraclePriceData.isMMSourcedPrice ?? false,
		state
	);
	if (oracleValidity <= OracleValidity.StaleForAMMLowRisk) {
		return false;
	}

	if (oracleValidity == OracleValidity.Valid) {
		return true;
	}

	const isOrderLowRiskForAmm = isLowRiskForAmm(
		order,
		mmOraclePriceData,
		isLiquidation
	);

	if (!isOrderLowRiskForAmm) {
		return false;
	} else {
		return true;
	}
}

/**
 * Dispatches to the correct in-progress auction price for `order` based on its order type:
 * fixed-price auction (`getAuctionPriceForFixedAuction`) for market/triggerLimit/plain-limit
 * orders, or oracle-offset auction (`getAuctionPriceForOracleOffsetAuction`) for
 * oracle-pegged limit/oracle/oracle-triggered-market orders. The result is always
 * standardized to `tickSize`.
 * @param order Order whose auction price to compute.
 * @param slot Current slot.
 * @param oraclePrice Use `MMOraclePriceData` source for perp orders, `OraclePriceData` for spot; PRICE_PRECISION (1e6).
 * @param tickSize Market's order tick size, PRICE_PRECISION (1e6). Defaults to `ONE` (no effective standardization).
 * @returns Auction price at the current slot, PRICE_PRECISION (1e6).
 * @throws if `order.orderType` doesn't match any known auction pricing path.
 */
