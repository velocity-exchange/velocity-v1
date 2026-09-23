/**
 * `GET /marketOrderParams`: the `OrderParams` a UI signs for a perp market order. Its `price`
 * is its worst price, a reference moved by the slippage tolerance away from the taker, and the
 * unfilled rest rests on the book there. `activationDelaySlots` delays when the rest can
 * fill. `maxTs` ends the order.
 */
import {
	BN,
	BASE_PRECISION,
	PRICE_PRECISION,
	PositionDirection,
	VelocityClient,
	ZERO,
	isVariant,
	AssetType,
	MarketType,
} from '@velocity-exchange/sdk';
import { calculateSpreadBidAskMark } from '@velocity-exchange/common';
import { TakerFillVsOracleBpsRedisResult } from '../athena/repositories/fillQualityAnalytics';
import { logger } from './logger';
import {
	calculateDynamicSlippage,
	convertRawL2ToBN,
	fetchL2FromRedis,
	getEstimatedPricesWithL2,
	getVammSideQuoteWithMargin,
	stringToBN,
} from './utils';

/** Fill-quality data older than this is ignored. */
const MAX_FILL_QUALITY_AGE_MS = 10 * 60 * 1000;

/** The largest slippage tolerance accepted, in percent. */
const MAX_SLIPPAGE_TOLERANCE_PCT = 99;

/** The price the slippage tolerance is measured from. */
export type PriceReference = 'best' | 'mark' | 'oracle' | 'entry';

export type MarketOrderParamsRequest = {
	marketIndex: number;
	direction: 'long' | 'short';
	/** BASE_PRECISION for `assetType: 'base'`, QUOTE_PRECISION for `'quote'`. */
	amount: string;
	assetType: AssetType;
	reduceOnly?: boolean;
	/** Percent. Omitted means dynamic. */
	slippageTolerance?: number;
	/** Defaults to `'best'`, the best price on the order's side of the book. */
	priceReference?: PriceReference;
	/** Price the order as an offset from the oracle. */
	isOracleOrder?: boolean;
	/** `null` takes the book's default. Below it needs the flow-authority attestation. */
	activationDelaySlots?: number;
	userOrderId?: number;
	maxLeverageSelected?: boolean;
	maxLeverageOrderSize?: string;
};

export type EstimatedPrices = {
	oraclePrice: BN;
	bestPrice: BN;
	entryPrice: BN;
	worstPrice: BN;
	markPrice: BN;
	priceImpact: BN;
};

/** Where the market's price data is read from. */
export type PriceSources = {
	velocityClient: VelocityClient;
	fetchFromRedis: (
		key: string,
		selectionCriteria: (responses: any) => any
	) => Promise<any>;
	selectMostRecentBySlot: (responses: any[]) => any;
	fillQualityInfo?: TakerFillVsOracleBpsRedisResult;
	/** The live chain slot, for the vAMM quote and the MM-oracle validity. */
	currentSlot?: number;
};

export type MarketOrderQuote = {
	/** Fields named as the SDK's `OptionalOrderParams` names them, BNs as strings. */
	params: {
		orderType: 'market' | 'oracle';
		marketType: 'perp';
		marketIndex: number;
		direction: 'long' | 'short';
		baseAssetAmount: string;
		price: string;
		oraclePriceOffset: string | null;
		reduceOnly: boolean;
		userOrderId: number | null;
		activationDelaySlots: number | null;
		maxTs: null;
	};
	estimatedPrices: EstimatedPrices;
	/** Percent. */
	slippageTolerance: number;
};

const directionOf = (direction: 'long' | 'short') =>
	direction === 'long' ? PositionDirection.LONG : PositionDirection.SHORT;

/**
 * Move `reference` by `slippagePct` percent away from the taker: up for a long, down for
 * a short.
 */
export const worstPriceFromSlippage = (
	direction: PositionDirection,
	reference: BN,
	slippagePct: number
): BN => {
	const pct = Math.min(Math.max(slippagePct, 0), MAX_SLIPPAGE_TOLERANCE_PCT);
	const shift = new BN(Math.round(pct * PRICE_PRECISION.toNumber())).divn(100);
	const factor = isVariant(direction, 'long')
		? PRICE_PRECISION.add(shift)
		: PRICE_PRECISION.sub(shift);

	return reference.mul(factor).div(PRICE_PRECISION);
};

/**
 * On a crossed book, shift the estimate toward where takers have been filling, by the
 * fill-quality offset from the oracle. The mark moves to the adjusted oracle when that
 * is closer to the fills or when the mark is against the taker.
 */
const applyFillQualityAdjustment = (
	prices: EstimatedPrices,
	direction: PositionDirection,
	oraclePrice: BN,
	fillQualityInfo: TakerFillVsOracleBpsRedisResult
): void => {
	if (
		Date.now() - (fillQualityInfo.updatedAtTs || 0) >
		MAX_FILL_QUALITY_AGE_MS
	) {
		return;
	}

	const bpsStr = isVariant(direction, 'long')
		? fillQualityInfo.takerBuyBpsFromOracle?.all
		: fillQualityInfo.takerSellBpsFromOracle?.all;
	const fillQualityBps = Math.round(parseFloat(bpsStr ?? '') * 100);
	if (isNaN(fillQualityBps)) {
		return;
	}

	const adjustment = oraclePrice.muln(fillQualityBps).divn(10000 * 100);
	const adjustedOracle = oraclePrice.add(adjustment);
	const markVsOracle = prices.markPrice.sub(oraclePrice);
	const markFavorsTaker = isVariant(direction, 'long')
		? markVsOracle.lt(ZERO)
		: markVsOracle.gt(ZERO);
	const oracleCloser = oraclePrice
		.sub(adjustedOracle)
		.abs()
		.lt(prices.markPrice.sub(adjustedOracle).abs());
	if (oracleCloser || !markFavorsTaker) {
		prices.markPrice = adjustedOracle;
	}

	prices.oraclePrice = adjustedOracle;
	prices.bestPrice = prices.bestPrice.add(adjustment);
	prices.entryPrice = prices.entryPrice.add(adjustment);
	prices.worstPrice = prices.worstPrice.add(adjustment);
};

/**
 * Floor a long's worst price at the vAMM ask, or cap a short's at the vAMM bid, when the
 * book's makers inside that quote cannot cover the order.
 */
const floorWorstPriceAtVammQuote = (
	prices: EstimatedPrices,
	sources: PriceSources,
	marketIndex: number,
	direction: PositionDirection,
	baseAmount: BN,
	redisL2: any
): void => {
	const vammQuote = getVammSideQuoteWithMargin(
		sources.velocityClient,
		marketIndex,
		direction,
		sources.currentSlot
	);
	if (!vammQuote) {
		return;
	}

	const isLong = isVariant(direction, 'long');
	const l2 = redisL2 ? convertRawL2ToBN(redisL2) : { bids: [], asks: [] };
	let makerDepthInsideQuote = ZERO;
	for (const level of (isLong ? l2.asks : l2.bids) ?? []) {
		const insideQuote = isLong
			? level.price.lte(vammQuote)
			: level.price.gte(vammQuote);
		if (!insideQuote) {
			break;
		}

		const vammSize = level.sources?.vamm ? new BN(level.sources.vamm) : ZERO;
		makerDepthInsideQuote = makerDepthInsideQuote.add(
			BN.max(level.size.sub(vammSize), ZERO)
		);
	}

	if (makerDepthInsideQuote.lt(baseAmount)) {
		prices.worstPrice = isLong
			? BN.max(prices.worstPrice, vammQuote)
			: BN.min(prices.worstPrice, vammQuote);
	}
};

const baseAmountOf = (
	request: MarketOrderParamsRequest,
	entryPrice: BN
): BN => {
	if (request.maxLeverageSelected && request.maxLeverageOrderSize) {
		return stringToBN(request.maxLeverageOrderSize);
	}

	const amount = stringToBN(request.amount);
	return request.assetType === 'base'
		? amount
		: amount.mul(BASE_PRECISION).div(entryPrice);
};

/** Estimate the order's prices and size against the published book. */
export const estimateMarketOrder = async (
	request: MarketOrderParamsRequest,
	sources: PriceSources
): Promise<{ prices: EstimatedPrices; baseAmount: BN; redisL2: any }> => {
	const direction = directionOf(request.direction);
	const redisL2 = await fetchL2FromRedis(
		sources.fetchFromRedis,
		sources.selectMostRecentBySlot,
		MarketType.PERP,
		request.marketIndex
	);
	const prices = await getEstimatedPricesWithL2(
		sources.velocityClient,
		MarketType.PERP,
		request.marketIndex,
		direction,
		stringToBN(request.amount),
		request.assetType,
		redisL2
	);

	if (sources.fillQualityInfo && redisL2) {
		const oraclePrice =
			sources.velocityClient.getMMOracleDataForPerpMarket(
				request.marketIndex,
				sources.currentSlot
			).price ?? ZERO;
		const spread = calculateSpreadBidAskMark(
			convertRawL2ToBN(redisL2),
			oraclePrice
		);
		const crossed =
			spread.bestBidPrice &&
			spread.bestAskPrice &&
			spread.bestBidPrice.gte(spread.bestAskPrice);
		if (crossed) {
			applyFillQualityAdjustment(
				prices,
				direction,
				oraclePrice,
				sources.fillQualityInfo
			);
		}
	}

	const baseAmount = baseAmountOf(request, prices.entryPrice);
	floorWorstPriceAtVammQuote(
		prices,
		sources,
		request.marketIndex,
		direction,
		baseAmount,
		redisL2
	);

	return { prices, baseAmount, redisL2 };
};

const referencePrice = (
	prices: EstimatedPrices,
	reference: PriceReference
): BN =>
	({
		best: prices.bestPrice,
		mark: prices.markPrice,
		oracle: prices.oraclePrice,
		entry: prices.entryPrice,
	}[reference]);

/** Quote the `OrderParams` for a perp market order. */
export const quoteMarketOrder = async (
	request: MarketOrderParamsRequest,
	sources: PriceSources
): Promise<MarketOrderQuote> => {
	const direction = directionOf(request.direction);
	const { prices, baseAmount, redisL2 } = await estimateMarketOrder(
		request,
		sources
	);
	const reference = referencePrice(prices, request.priceReference ?? 'best');

	const slippageTolerance =
		request.slippageTolerance ??
		calculateDynamicSlippage(
			request.marketIndex,
			'perp',
			sources.velocityClient,
			redisL2 ? convertRawL2ToBN(redisL2) : { bids: [], asks: [] },
			reference,
			prices.worstPrice
		);
	const worstPrice = worstPriceFromSlippage(
		direction,
		reference,
		slippageTolerance
	);

	// An oracle order holds its worst price as an offset, which follows the oracle
	// until the order fills.
	const isOracleOrder = !!request.isOracleOrder && !prices.oraclePrice.isZero();
	logger.info(
		JSON.stringify({
			event: 'market_order_params_quoted',
			marketIndex: request.marketIndex,
			direction: request.direction,
			slippageTolerance,
			worstPrice: worstPrice.toString(),
			entryPrice: prices.entryPrice.toString(),
			walkWorstPrice: prices.worstPrice.toString(),
		})
	);

	return {
		params: {
			orderType: isOracleOrder ? 'oracle' : 'market',
			marketType: 'perp',
			marketIndex: request.marketIndex,
			direction: request.direction,
			baseAssetAmount: baseAmount.toString(),
			price: isOracleOrder ? '0' : worstPrice.toString(),
			oraclePriceOffset: isOracleOrder
				? worstPrice.sub(prices.oraclePrice).toString()
				: null,
			reduceOnly: request.reduceOnly ?? false,
			userOrderId: request.userOrderId ?? null,
			activationDelaySlots: request.activationDelaySlots ?? null,
			maxTs: null,
		},
		estimatedPrices: prices,
		slippageTolerance,
	};
};
