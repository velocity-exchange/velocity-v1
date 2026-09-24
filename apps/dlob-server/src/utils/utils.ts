import {
	BN,
	BigNum,
	VelocityClient,
	VelocityEnv,
	L2OrderBook,
	L3OrderBook,
	MarketType,
	OraclePriceData,
	PublicKey,
	decodeUser,
	isVariant,
	PositionDirection,
	ZERO,
	BASE_PRECISION,
	PRICE_PRECISION,
	calculateEstimatedEntryPriceWithL2,
	calculateBidAskPrice,
	AssetType,
	MainnetSpotMarkets,
	DevnetSpotMarkets,
	PERCENTAGE_PRECISION_EXP,
	isMajorPerpMarket,
} from '@velocity-exchange/sdk';
import { RedisClient } from '@velocity-exchange/common/clients';
import { logger } from './logger';
import { NextFunction, Request, Response } from 'express';
import FEATURE_FLAGS from './featureFlags';
import { Connection } from '@solana/web3.js';
import { MID_MAJOR_MARKETS } from './constants';
import { calculateSpreadBidAskMark } from '@velocity-exchange/common';

export const GROUPING_OPTIONS = [1, 10, 100, 500, 1000];
export const GROUPING_DEPENDENCIES = {
	1: null,
	10: 1,
	100: 10,
	500: 100,
	1000: 100,
};

export const l2WithBNToStrings = (l2: L2OrderBook): any => {
	for (const key of Object.keys(l2)) {
		for (const idx in l2[key]) {
			const level = l2[key][idx];
			const sources = level['sources'];
			for (const sourceKey of Object.keys(sources)) {
				sources[sourceKey] = sources[sourceKey].toString();
			}
			l2[key][idx] = {
				price: level.price.toString(),
				size: level.size.toString(),
				sources,
			};
		}
	}
	return l2;
};

export const l3WithBNToStrings = (l3: L3OrderBook): any => {
	for (const key of Object.keys(l3)) {
		for (const idx in l3[key]) {
			const level = l3[key][idx];
			l3[key][idx] = {
				price: level.price.toString(),
				size: level.size.toString(),
				maker: level.maker.toBase58(),
				orderId: level.orderId.toString(),
			};
		}
	}
	return l3;
};

export function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

export function parsePositiveIntArray(
	intArray: string,
	separator = ','
): number[] {
	return intArray
		.split(separator)
		.map((s) => s.trim())
		.map((s) => parseInt(s))
		.filter((n) => !isNaN(n) && n >= 0);
}

export const getOracleForMarket = (
	velocityClient: VelocityClient,
	marketType: MarketType,
	marketIndex: number,
	useMMOracleData = false
): number => {
	if (isVariant(marketType, 'spot')) {
		return velocityClient
			.getOracleDataForSpotMarket(marketIndex)
			.price.toNumber();
	} else if (isVariant(marketType, 'perp')) {
		return useMMOracleData
			? velocityClient
					.getMMOracleDataForPerpMarket(marketIndex)
					.price.toNumber()
			: velocityClient.getOracleDataForPerpMarket(marketIndex).price.toNumber();
	}
};

type SerializableOraclePriceData = {
	price: string;
	slot: string;
	confidence: string;
	hasSufficientNumberOfDataPoints: boolean;
	twap?: string;
	twapConfidence?: string;
	maxPrice?: string;
};

const getSerializableOraclePriceData = (
	oraclePriceData: OraclePriceData
): SerializableOraclePriceData => {
	return {
		price: oraclePriceData.price?.toString?.(),
		slot: oraclePriceData.slot?.toString?.(),
		confidence: oraclePriceData.confidence?.toString?.(),
		hasSufficientNumberOfDataPoints:
			oraclePriceData.hasSufficientNumberOfDataPoints,
		twap: oraclePriceData.twap?.toString?.(),
		twapConfidence: oraclePriceData.twapConfidence?.toString?.(),
		maxPrice: oraclePriceData.maxPrice?.toString?.(),
	};
};

export const getOracleDataForMarket = (
	velocityClient: VelocityClient,
	marketType: MarketType,
	marketIndex: number,
	useMMOracleData = false
): SerializableOraclePriceData => {
	if (isVariant(marketType, 'spot')) {
		return getSerializableOraclePriceData(
			velocityClient.getOracleDataForSpotMarket(marketIndex)
		);
	} else if (isVariant(marketType, 'perp')) {
		return getSerializableOraclePriceData(
			useMMOracleData
				? velocityClient.getMMOracleDataForPerpMarket(marketIndex)
				: velocityClient.getOracleDataForPerpMarket(marketIndex)
		);
	}
};

export const addOracletoResponse = (
	response: L2OrderBook | L3OrderBook,
	velocityClient: VelocityClient,
	marketType: MarketType,
	marketIndex: number
): void => {
	if (FEATURE_FLAGS.OLD_ORACLE_PRICE_IN_L2) {
		response['oracle'] = getOracleForMarket(
			velocityClient,
			marketType,
			marketIndex
		);
		if (response['oracle'] == 0) {
			logger.info(`oracle price is 0 for ${marketType}-${marketIndex}`);
		}
	}
	if (FEATURE_FLAGS.NEW_ORACLE_DATA_IN_L2) {
		response['oracleData'] = getOracleDataForMarket(
			velocityClient,
			marketType,
			marketIndex
		);
		if (!response['oracleData'].price) {
			logger.info(
				`oracle price is undefined or 0 for ${marketType}-${marketIndex}`
			);
		}
		response['mmOracleData'] = getOracleDataForMarket(
			velocityClient,
			marketType,
			marketIndex,
			true
		);
		if (!response['mmOracleData'].price && response['mmOracleData'].isActive) {
			logger.info(
				`mm oracle price is undefined or 0 for ${marketType}-${marketIndex}`
			);
		}
	}
};

export const addMarketSlotToResponse = (
	response: L2OrderBook | L3OrderBook,
	velocityClient: VelocityClient,
	marketType: MarketType,
	marketIndex: number
): void => {
	let marketSlot: number;
	if (isVariant(marketType, 'perp')) {
		marketSlot =
			velocityClient.accountSubscriber.getMarketAccountAndSlot(
				marketIndex
			).slot;
	} else {
		marketSlot =
			velocityClient.accountSubscriber.getSpotMarketAccountAndSlot(
				marketIndex
			).slot;
	}
	response['marketSlot'] = marketSlot;
};

export function aggregatePrices(entries, side, pricePrecision) {
	const isAsk = side === 'ask';
	const result = new Map();

	entries.forEach((entry) => {
		const price = parseFloat(entry.price);
		const data = {
			size: parseFloat(entry.size),
			sources: entry.sources || {},
		};

		let bucketPrice, displayPrice;
		if (isAsk) {
			displayPrice = Math.ceil(price / pricePrecision) * pricePrecision;
			bucketPrice = displayPrice;
		} else {
			displayPrice = Math.floor(price / pricePrecision) * pricePrecision;
			bucketPrice = displayPrice;
		}

		const bucketKey = Math.round(bucketPrice);

		if (!result.has(bucketKey)) {
			result.set(bucketKey, {
				size: 0,
				price: displayPrice,
				sources: {},
			});
		}

		const bucketData = result.get(bucketKey);
		bucketData.size += data.size;

		if (data.sources) {
			Object.entries(data.sources).forEach(
				([sourceKey, sourceSize]: [string, string]) => {
					if (!bucketData.sources[sourceKey]) {
						bucketData.sources[sourceKey] = 0;
					}
					bucketData.sources[sourceKey] += parseFloat(sourceSize);
				}
			);
		}
	});

	return Array.from(result.values());
}

const REDIS_WARN_THROTTLE_MS = 5_000;
const lastRedisWarnAt: Map<string, number> = new Map();

/**
 * A Redis write in the publish path is unawaited, so a rejection reaches
 * Node's unhandled-rejection handler. `RedisClient.set`/`setRaw` do not
 * await the underlying ioredis command, so a reconnect-time failure
 * surfaces only here. Logging is throttled per context, since these writes
 * run once per market per update.
 */
export function fireAndForgetRedis(
	write: Promise<unknown>,
	context: string
): void {
	Promise.resolve(write).catch((e) => {
		const now = Date.now();
		const last = lastRedisWarnAt.get(context) ?? 0;
		if (now - last < REDIS_WARN_THROTTLE_MS) {
			return;
		}
		lastRedisWarnAt.set(context, now);
		logger.warn(`Redis write failed (${context}): ${String(e)}`);
	});
}

/**
 * Takes in a req.query like: `{
 * 		marketName: 'SOL-PERP,BTC-PERP,ETH-PERP',
 * 		marketType: undefined,
 * 		marketIndices: undefined,
 * 		...
 * 	}` and returns a normalized object like:
 *
 * `[
 * 		{marketName: 'SOL-PERP', marketType: undefined, marketIndex: undefined,...},
 * 		{marketName: 'BTC-PERP', marketType: undefined, marketIndex: undefined,...},
 * 		{marketName: 'ETH-PERP', marketType: undefined, marketIndex: undefined,...}
 * ]`
 *
 * @param rawParams req.query object
 * @returns normalized query params for batch requests, or undefined if there is a mismatched length
 */
export const normalizeBatchQueryParams = (rawParams: {
	[key: string]: string | undefined;
}): Array<{ [key: string]: string | undefined }> => {
	const normedParams: Array<{ [key: string]: string | undefined }> = [];
	const parsedParams = {};

	// parse the query string into arrays
	for (const key of Object.keys(rawParams)) {
		const rawParam = rawParams[key];
		if (rawParam === undefined) {
			parsedParams[key] = [];
		} else {
			parsedParams[key] = rawParam.split(',') || [rawParam];
		}
	}

	// of all parsedParams, find the max length
	const maxLength = Math.max(
		...Object.values(parsedParams).map((param: Array<unknown>) => param.length)
	);

	// all params have to be either 0 length, or maxLength to be valid
	const values = Object.values(parsedParams);
	const validParams = values.every(
		(value: Array<unknown>) => value.length === 0 || value.length === maxLength
	);
	if (!validParams) {
		return undefined;
	}

	// merge all params into an array of objects
	// normalize all params to the same length, filling in undefineds
	for (let i = 0; i < maxLength; i++) {
		const newParam = {};
		for (const key of Object.keys(parsedParams)) {
			const parsedParam = parsedParams[key];
			newParam[key] =
				parsedParam.length === maxLength ? parsedParam[i] : undefined;
		}
		normedParams.push(newParam);
	}

	return normedParams;
};

export const validateWsSubscribeMsg = (
	msg: any,
	sdkConfig: any
): { valid: boolean; msg?: string } => {
	const maxPerpMarketIndex = Math.max(
		...sdkConfig.PERP_MARKETS.map((m) => m.marketIndex)
	);
	const maxSpotMarketIndex = Math.max(
		...sdkConfig.SPOT_MARKETS.map((m) => m.marketIndex)
	);

	if (msg['marketIndex'] < 0) {
		return { valid: false, msg: `Invalid marketIndex, must be >= 0` };
	}

	if (
		msg['marketType'].toLowerCase() == 'spot' &&
		parseInt(msg['marketIndex']) > maxSpotMarketIndex
	) {
		return {
			valid: false,
			msg: `Invalid marketIndex for marketType: ${msg['marketType']}`,
		};
	}

	if (
		msg['marketType'].toLowerCase() == 'perp' &&
		parseInt(msg['marketIndex']) > maxPerpMarketIndex
	) {
		return {
			valid: false,
			msg: `Invalid marketIndex for marketType: ${msg['marketType']}`,
		};
	}

	if (
		msg['marketType'].toLowerCase() != 'perp' &&
		msg['marketType'] != 'spot'
	) {
		return {
			valid: false,
			msg: `Invalid marketType: ${msg['marketType']}`,
		};
	}

	return { valid: true };
};

export const validateDlobQuery = (
	velocityClient: VelocityClient,
	velocityEnv: VelocityEnv,
	marketType?: string,
	marketIndex?: string,
	marketName?: string
): {
	normedMarketType?: MarketType;
	normedMarketIndex?: number;
	error?: string;
} => {
	let normedMarketType: MarketType = undefined;
	let normedMarketIndex: number = undefined;
	let normedMarketName: string = undefined;
	if (marketName === undefined) {
		if (marketIndex === undefined || marketType === undefined) {
			return {
				error:
					'Bad Request: (marketName) or (marketIndex and marketType) must be supplied',
			};
		}

		// validate marketType
		switch ((marketType as string).toLowerCase()) {
			case 'spot': {
				normedMarketType = MarketType.SPOT;
				normedMarketIndex = parseInt(marketIndex as string);
				const spotMarketIndicies = velocityClient
					.getSpotMarketAccounts()
					.map((mkt) => mkt.marketIndex);
				if (!spotMarketIndicies.includes(normedMarketIndex)) {
					return {
						error: 'Bad Request: invalid marketIndex',
					};
				}
				break;
			}
			case 'perp': {
				normedMarketType = MarketType.PERP;
				normedMarketIndex = parseInt(marketIndex as string);
				const perpMarketIndicies = velocityClient
					.getPerpMarketAccounts()
					.map((mkt) => mkt.marketIndex);
				if (!perpMarketIndicies.includes(normedMarketIndex)) {
					return {
						error: 'Bad Request: invalid marketIndex',
					};
				}
				break;
			}
			default:
				return {
					error: 'Bad Request: marketType must be either "spot" or "perp"',
				};
		}
	} else {
		// validate marketName
		normedMarketName = (marketName as string).toUpperCase();
		const derivedMarketInfo =
			velocityClient.getMarketIndexAndType(normedMarketName);
		if (!derivedMarketInfo) {
			return {
				error: 'Bad Request: unrecognized marketName',
			};
		}
		normedMarketType = derivedMarketInfo.marketType;
		normedMarketIndex = derivedMarketInfo.marketIndex;
	}

	return {
		normedMarketType,
		normedMarketIndex,
	};
};

export const getAccountFromId = async (
	userMapClient: RedisClient,
	topMakers: string[]
) => {
	return Promise.all(
		topMakers.map(async (userAccountPubKey) => {
			const userAccountEncoded = await userMapClient.getRaw(userAccountPubKey);
			if (userAccountEncoded) {
				return {
					userAccountPubKey,
					account: decodeUser(
						Buffer.from(userAccountEncoded.split('::')[1], 'base64')
					),
				};
			}
			return {
				userAccountPubKey,
				account: null,
			};
		})
	).then((results) => results.filter((user) => !!user));
};

export const getRawAccountFromId = async (
	userMapClient: RedisClient,
	topMakers: string[],
	connection: Connection
): Promise<
	{
		userAccountPubKey: string;
		accountBase64: string;
	}[]
> => {
	return Promise.all(
		topMakers.map(async (userAccountPubKey) => {
			const userAccountEncoded = await userMapClient.getRaw(userAccountPubKey);
			if (userAccountEncoded) {
				return {
					userAccountPubKey,
					accountBase64: userAccountEncoded.split('::')[1],
				};
			} else {
				// user is not in the userMap, try to fetch from the connection
				const account = await connection.getAccountInfo(
					new PublicKey(userAccountPubKey)
				);
				if (account) {
					return {
						userAccountPubKey,
						accountBase64: account.data.toString('base64'),
					};
				}
			}

			return {
				userAccountPubKey,
				accountBase64: null,
			};
		})
	).then((results) => results.filter((user) => !!user));
};

export function errorHandler(
	err: Error,
	_req: Request,
	res: Response,
	_next: NextFunction
): void {
	logger.error(`errorHandler, message: ${err.message}, stack: ${err.stack}`);
	if (!res.headersSent) {
		res.status(500).send('Internal error');
	}
}

export type SubscriberLookup = {
	[marketIndex: number]: {
		tickSize?: BN;
	};
};

export const selectMostRecentBySlot = (
	responses: any[]
): {
	slot: number;
	[key: string]: any;
} => {
	const parsedResponses = responses
		.map((response) => {
			try {
				return JSON.parse(response);
			} catch {
				return null;
			}
		})
		.filter((parsed) => parsed && typeof parsed.slot === 'number');
	return parsedResponses.reduce((mostRecent, current) => {
		return !mostRecent || current.slot > mostRecent.slot ? current : mostRecent;
	}, null);
};

/**
 * Parse boolean values from string query parameters
 * @param value - string value from query parameter
 * @returns boolean | undefined - true for 'true'/'1', false for other values, undefined if input is undefined
 */
export const parseBoolean = (
	value: string | undefined
): boolean | undefined => {
	if (value === undefined) return undefined;
	return value === 'true' || value === '1';
};

/**
 * Safely parse numeric values from string query parameters
 * @param value - string value from query parameter
 * @returns number | undefined - parsed number or undefined if invalid/empty
 */
export const parseNumber = (value: string | undefined): number | undefined => {
	if (!value) return undefined;
	const parsed = parseFloat(value);
	return isNaN(parsed) ? undefined : parsed;
};

/**
 * Convert string to BN
 * @param value - string value to convert
 * @returns BN
 */
export const stringToBN = (value: string): BN => {
	if (!value) return ZERO;
	return new BN(value);
};

/**
 * Convert raw Redis L2 data (with string prices/sizes) to proper L2OrderBook format (with BN values)
 * @param rawL2 - Raw L2 data from Redis with string values
 * @returns L2OrderBook with proper BN values
 */
export const convertRawL2ToBN = (rawL2: any): L2OrderBook => {
	const convertLevel = (level: any) => ({
		...level,
		price: new BN(level.price),
		size: new BN(level.size),
	});

	return {
		...rawL2,
		bids: rawL2.bids?.map(convertLevel) || [],
		asks: rawL2.asks?.map(convertLevel) || [],
	};
};

/**
 * Fetch L2 orderbook data from Redis
 * @param fetchFromRedis - Redis fetch function
 * @param selectMostRecentBySlot - Slot selection function
 * @param marketType - MarketType enum (spot or perp)
 * @param marketIndex - Market index number
 * @returns Promise<any> - Raw L2 data from Redis or null if not found
 */
export const fetchL2FromRedis = async (
	fetchFromRedis: (
		key: string,
		selectionCriteria: (responses: any) => any
	) => Promise<any>,
	selectMostRecentBySlot: (responses: any[]) => any,
	marketType: MarketType,
	marketIndex: number
): Promise<any> => {
	const isSpot = isVariant(marketType, 'spot');
	const marketTypeStr = isSpot ? 'spot' : 'perp';

	return await fetchFromRedis(
		`last_update_orderbook_${marketTypeStr}_${marketIndex}`,
		selectMostRecentBySlot
	);
};

/**
 * The vAMM quote for the side of the book a taker order executes against
 * (ask for longs, bid for shorts), computed from the perp market account's
 * own AMM state (curve projection + cached spread state), with a safety
 * margin on top.
 *
 * On vAMM-dominated books the L2-walk `worstPrice` undershoots what the
 * program quotes at fill time, because `AmmQuoter::setup` re-projects the
 * curve and recomputes spreads against the fill-slot oracle. A worst price
 * set from the unfloored walk can sit just short of that quote and never
 * fill against the vAMM.
 *
 * `marginPct` (percent, `DYNAMIC_VAMM_QUOTE_MARGIN` env, default 0.15)
 * covers the model-vs-fill-slot drift. Returns `undefined` when the market
 * account or oracle is unavailable (callers skip the floor).
 */
export const getVammSideQuoteWithMargin = (
	velocityClient: VelocityClient,
	marketIndex: number,
	direction: PositionDirection,
	// The live chain slot, for the staged slot-duration switch. It falls back to
	// the MM oracle publication slot when the caller has no slot.
	currentSlot?: number
): BN | undefined => {
	try {
		const perpMarket = velocityClient.getPerpMarketAccount?.(marketIndex);
		if (!perpMarket) {
			return undefined;
		}
		const mmOracle = velocityClient.getMMOracleDataForPerpMarket(
			marketIndex,
			currentSlot
		);
		if (!mmOracle?.price || mmOracle.price.isZero()) {
			return undefined;
		}
		const nowSlot =
			currentSlot !== undefined ? new BN(currentSlot) : mmOracle.slot;
		const [vammBid, vammAsk] = calculateBidAskPrice(
			perpMarket.amm,
			perpMarket.marketStats,
			mmOracle,
			true,
			nowSlot,
			velocityClient.getStateAccount()
		);
		const marginPct = parseFloat(
			process.env.DYNAMIC_VAMM_QUOTE_MARGIN || '0.15'
		);
		// percent -> 1e6 fraction, applied away from the taker's favor
		const marginScaled = Math.round(marginPct * 10_000);
		const isLong = isVariant(direction, 'long');
		const quote = isLong ? vammAsk : vammBid;
		if (!quote || quote.isZero()) {
			return undefined;
		}
		return isLong
			? quote.muln(1_000_000 + marginScaled).divn(1_000_000)
			: quote.muln(1_000_000 - marginScaled).divn(1_000_000);
	} catch (error) {
		logger.warn(
			`Failed to compute vAMM side quote for market ${marketIndex}: ${error}`
		);
		return undefined;
	}
};

/**
 * Suggests a slippage tolerance for a quote, as a percentage. It adds a tier-based floor to
 * the book's observed spread. It widens the result to cover the distance to the worst fill
 * price when the order size is known. It then scales the result by a tier multiplier and
 * clamps it to the configured minimum and maximum. The environment tunes every tier constant
 * through `DYNAMIC_BASE_SLIPPAGE_*`, `DYNAMIC_SLIPPAGE_MULTIPLIER_*`, `DYNAMIC_SLIPPAGE_MIN`
 * and `DYNAMIC_SLIPPAGE_MAX`.
 *
 * @param marketIndex the market being quoted. `isMajorPerpMarket` tiers a perp market
 * @param marketType `'perp'` or `'spot'`. Only a perp market is tiered
 * @param velocityClient the client that reads the oracle price data
 * @param l2Formatted the L2 book the spread component is measured from
 * @param startPrice the best available price for the order
 * @param worstPrice the worst price the order would reach, for the size-adjusted component
 * @returns the slippage tolerance as a percentage
 */
export const calculateDynamicSlippage = (
	marketIndex: number,
	marketType: string,
	velocityClient: VelocityClient,
	l2Formatted: L2OrderBook,
	startPrice: BN,
	worstPrice: BN
): number => {
	const isPerp = marketType.toLowerCase() === 'perp';
	const isMajor = isPerp && isMajorPerpMarket(marketIndex);
	const isMidMajor = isPerp && MID_MAJOR_MARKETS.includes(marketIndex);

	const baseSlippage = isMajor
		? parseFloat(process.env.DYNAMIC_BASE_SLIPPAGE_MAJOR || '0') // 0% default
		: isMidMajor
		? parseFloat(process.env.DYNAMIC_BASE_SLIPPAGE_MID_MAJOR || '0.25') // 0.25% default
		: parseFloat(process.env.DYNAMIC_BASE_SLIPPAGE_NON_MAJOR || '0.5'); // 0.5% default

	// Calculate spread using L2 data
	let spreadBaseSlippage = 0.0005; // 0.05% fallback spread
	try {
		// Get oracle data
		const oracleData = isPerp
			? velocityClient.getMMOracleDataForPerpMarket(marketIndex)
			: velocityClient.getOracleDataForSpotMarket(marketIndex);

		// Get oracle price
		const oraclePrice = new BN(oracleData?.price || 0).mul(PRICE_PRECISION);

		// Calculate actual spread
		const spreadInfo = calculateSpreadBidAskMark(l2Formatted, oraclePrice);

		const spreadPctNum = BigNum.from(
			spreadInfo.spreadPct,
			PERCENTAGE_PRECISION_EXP
		)?.toNum();

		if (spreadInfo?.spreadPct) {
			spreadBaseSlippage = spreadPctNum * 0.9;

			// If the L2 is crossed (best bid > best ask), cap the spread contribution
			const bestBid = spreadInfo.bestBidPrice;
			const bestAsk = spreadInfo.bestAskPrice;
			const isCrossed = !!(bestBid && bestAsk && bestBid.gt(bestAsk));

			if (isCrossed) {
				// Always cap the spread component tightly when crossed, default to 0.1%
				const defaultCrossCap = 0.1;
				const crossCap =
					parseFloat(
						process.env.DYNAMIC_CROSS_SPREAD_CAP || defaultCrossCap.toString()
					) ?? defaultCrossCap;
				spreadBaseSlippage = Math.min(spreadBaseSlippage, crossCap);
			}
		}
	} catch (error) {
		console.warn('Failed to calculate spread, using fallback:', error);
	}

	let dynamicSlippage = baseSlippage + spreadBaseSlippage;

	// use halfway to worst price as size adjusted slippage
	if (startPrice && worstPrice) {
		const sizeAdjustedSlippage =
			(startPrice.sub(worstPrice).abs().toNumber() /
				startPrice.toNumber() /
				2) *
			100;

		dynamicSlippage = Math.max(dynamicSlippage, sizeAdjustedSlippage);
	}

	// The order's worst price (start × (1 + slippage)) must reach the walk's
	// worst fill, or the order stops short of the vAMM and rests unfilled.
	if (isPerp && startPrice && worstPrice && !startPrice.isZero()) {
		const fullDistancePct =
			(startPrice.sub(worstPrice).abs().toNumber() / startPrice.toNumber()) *
			100;
		const worstPriceMarginPct = parseFloat(
			process.env.DYNAMIC_SLIPPAGE_WORST_PRICE_MARGIN || '0.1'
		);
		dynamicSlippage = Math.max(
			dynamicSlippage,
			fullDistancePct + worstPriceMarginPct
		);
	}

	// Apply multiplier from env var
	const multiplier = isMajor
		? parseFloat(process.env.DYNAMIC_SLIPPAGE_MULTIPLIER_MAJOR || '1.1')
		: isMidMajor
		? parseFloat(process.env.DYNAMIC_SLIPPAGE_MULTIPLIER_MID_MAJOR || '1.25')
		: parseFloat(process.env.DYNAMIC_SLIPPAGE_MULTIPLIER_NON_MAJOR || '1.5');
	dynamicSlippage = dynamicSlippage * multiplier;

	// Enforce minimum and maximum limits from env vars
	const minSlippage = parseFloat(process.env.DYNAMIC_SLIPPAGE_MIN || '0.035'); // 0.035% minimum
	const maxSlippage = parseFloat(process.env.DYNAMIC_SLIPPAGE_MAX || '5'); // 5% maximum

	return Math.min(Math.max(dynamicSlippage, minSlippage), maxSlippage);
};

/**
 * Get L2 orderbook data and calculate estimated prices using pre-fetched L2 data
 * @param velocityClient - VelocityClient instance
 * @param marketType - MarketType enum
 * @param marketIndex - Market index number
 * @param direction - Position direction
 * @param amount - Amount as BN (could be base or quote amount)
 * @param assetType - Whether amount is 'base' or 'quote'
 * @param redisL2 - Pre-fetched L2 data from Redis
 * @returns Price data object with oracle, best, entry, worst, and mark prices
 */
export const getEstimatedPricesWithL2 = async (
	velocityClient: VelocityClient,
	marketType: MarketType,
	marketIndex: number,
	direction: PositionDirection,
	amount: BN,
	assetType: AssetType,
	redisL2: any
): Promise<{
	oraclePrice: BN;
	bestPrice: BN;
	entryPrice: BN;
	worstPrice: BN;
	markPrice: BN;
	priceImpact: BN;
}> => {
	const isSpot = isVariant(marketType, 'spot');

	let l2Formatted: L2OrderBook;
	if (redisL2) {
		l2Formatted = convertRawL2ToBN(redisL2);
	} else {
		l2Formatted = {
			bids: [],
			asks: [],
		};
	}

	const oracleData = isSpot
		? velocityClient.getOracleDataForSpotMarket(marketIndex)
		: velocityClient.getMMOracleDataForPerpMarket(marketIndex);

	// Get oracle price
	const oraclePrice = oracleData.price ?? ZERO;

	const spreadInfo = calculateSpreadBidAskMark(l2Formatted, oraclePrice);

	const markPrice = spreadInfo?.markPrice ?? oraclePrice;

	// If we have L2 data, calculate estimated prices
	if (l2Formatted.bids?.length > 0 || l2Formatted.asks?.length > 0) {
		try {
			const basePrecision = !isSpot
				? BASE_PRECISION
				: process.env.ENV === 'mainnet-beta'
				? MainnetSpotMarkets[marketIndex].precision
				: DevnetSpotMarkets[marketIndex].precision;

			const priceEstimate = calculateEstimatedEntryPriceWithL2(
				assetType,
				amount,
				direction,
				basePrecision,
				l2Formatted as L2OrderBook
			);

			return {
				oraclePrice,
				bestPrice: priceEstimate.bestPrice,
				entryPrice: priceEstimate.entryPrice,
				worstPrice: priceEstimate.worstPrice,
				markPrice,
				priceImpact: priceEstimate.priceImpact,
			};
		} catch (error) {
			// If calculation fails, fallback to oracle prices
			console.warn('Price calculation failed, using oracle fallback:', error);
		}
	}

	// Fallback to oracle prices if no L2 data or calculation fails
	return {
		oraclePrice,
		bestPrice: oraclePrice,
		entryPrice: oraclePrice,
		worstPrice: oraclePrice,
		markPrice,
		priceImpact: ZERO,
	};
};
