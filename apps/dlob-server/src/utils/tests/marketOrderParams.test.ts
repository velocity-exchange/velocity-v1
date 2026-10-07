import { describe, it, expect, jest, beforeEach } from '@jest/globals';
import {
	BN,
	PositionDirection,
	ZERO,
	PRICE_PRECISION,
	BASE_PRECISION,
} from '@velocity-exchange/sdk';
import { calculateDynamicSlippage } from '../utils';
import { quoteMarketOrder, worstPriceFromSlippage } from '../marketOrderParams';

const usd = (dollars: number) => new BN(dollars).mul(PRICE_PRECISION);

describe('worstPriceFromSlippage', () => {
	it('moves a long up and a short down by the tolerance', () => {
		expect(
			worstPriceFromSlippage(PositionDirection.LONG, usd(100), 1).toString()
		).toBe(usd(101).toString());
		expect(
			worstPriceFromSlippage(PositionDirection.SHORT, usd(100), 1).toString()
		).toBe(usd(99).toString());
	});

	it('clamps the tolerance to 0 and 99 percent', () => {
		expect(
			worstPriceFromSlippage(PositionDirection.LONG, usd(100), -5).toString()
		).toBe(usd(100).toString());
		expect(
			worstPriceFromSlippage(PositionDirection.SHORT, usd(100), 500).toString()
		).toBe(usd(1).toString());
	});
});

describe('quoteMarketOrder', () => {
	const velocityClient = {
		getMMOracleDataForPerpMarket: jest.fn(() => ({ price: usd(160) })),
		getOracleDataForSpotMarket: jest.fn(),
	} as any;
	const book = {
		bids: [{ price: '159950000', size: '1000000000' }],
		asks: [
			{ price: '160050000', size: '1000000000' },
			{ price: '160100000', size: '2000000000' },
		],
	};
	const fetchFromRedis = jest.fn(async (key: string) =>
		key === 'last_update_orderbook_perp_0' ? book : null
	) as any;
	const sources = {
		velocityClient,
		fetchFromRedis,
		selectMostRecentBySlot: jest.fn() as any,
	};
	const request = {
		marketIndex: 0,
		direction: 'long' as const,
		amount: BASE_PRECISION.muln(2).toString(),
		assetType: 'base' as const,
	};

	beforeEach(() => {
		jest.clearAllMocks();
	});

	it('names the worst price as the order price, the tolerance past the best ask', async () => {
		const quote = await quoteMarketOrder(
			{ ...request, slippageTolerance: 1 },
			sources
		);

		expect(fetchFromRedis).toHaveBeenCalledWith(
			'last_update_orderbook_perp_0',
			sources.selectMostRecentBySlot
		);
		expect(quote.params.orderType).toBe('market');
		expect(quote.params.price).toBe(
			worstPriceFromSlippage(
				PositionDirection.LONG,
				quote.estimatedPrices.bestPrice,
				1
			).toString()
		);
		expect(quote.params.baseAssetAmount).toBe(request.amount);
		expect(quote.params.maxTs).toBeNull();
		expect(quote.params.activationDelaySlots).toBeNull();
	});

	it('holds the worst price of an oracle order as an offset from the oracle', async () => {
		const quote = await quoteMarketOrder(
			{ ...request, slippageTolerance: 1, isOracleOrder: true },
			sources
		);

		const worst = worstPriceFromSlippage(
			PositionDirection.LONG,
			quote.estimatedPrices.bestPrice,
			1
		);
		expect(quote.params.orderType).toBe('oracle');
		expect(quote.params.price).toBe('0');
		expect(quote.params.oraclePriceOffset).toBe(
			worst.sub(quote.estimatedPrices.oraclePrice).toString()
		);
	});

	it('passes the activation delay through', async () => {
		const quote = await quoteMarketOrder(
			{ ...request, activationDelaySlots: 4 },
			sources
		);

		expect(quote.params.activationDelaySlots).toBe(4);
	});

	it('picks a dynamic tolerance that reaches the walk when none is named', async () => {
		const quote = await quoteMarketOrder(request, sources);

		const worst = new BN(quote.params.price);
		expect(quote.slippageTolerance).toBeGreaterThan(0);
		expect(worst.gte(quote.estimatedPrices.worstPrice)).toBe(true);
		expect(worst.gt(ZERO)).toBe(true);
	});
});

describe('calculateDynamicSlippage - crossed book handling', () => {
	const mockVelocityClient = {
		getMMOracleDataForPerpMarket: jest.fn(),
		getOracleDataForSpotMarket: jest.fn(),
	} as any;

	beforeEach(() => {
		jest.clearAllMocks();
	});

	it('caps spread contribution when crossed (default cap mode)', () => {
		// Set env to deterministic values
		process.env.DYNAMIC_BASE_SLIPPAGE_MAJOR = '0';
		process.env.DYNAMIC_SLIPPAGE_MULTIPLIER_MAJOR = '1';
		process.env.DYNAMIC_SLIPPAGE_MIN = '0';
		process.env.DYNAMIC_SLIPPAGE_MAX = '100';
		delete process.env.DYNAMIC_CROSS_SPREAD_MODE; // default 'cap'
		process.env.DYNAMIC_CROSS_SPREAD_CAP = '0.1'; // 0.1%

		// Oracle 100
		mockVelocityClient.getMMOracleDataForPerpMarket.mockReturnValue({
			price: new BN(100).mul(PRICE_PRECISION),
		});

		const l2Crossed = {
			bids: [{ price: new BN(101).mul(PRICE_PRECISION), size: new BN(1) }],
			asks: [{ price: new BN(99).mul(PRICE_PRECISION), size: new BN(1) }],
		} as any;

		const startPrice = new BN(100).mul(PRICE_PRECISION);
		const worstPrice = new BN(100).mul(PRICE_PRECISION);

		const slip = calculateDynamicSlippage(
			0, // major perp
			'perp',
			mockVelocityClient,
			l2Crossed,
			startPrice,
			worstPrice
		);

		// Should be capped at 0.1% given our env
		expect(slip).toBeLessThanOrEqual(0.1);
		expect(slip).toBeGreaterThanOrEqual(0);
	});

	it('normal (non-crossed) book produces spread-based slippage > crossed-capped', () => {
		process.env.DYNAMIC_BASE_SLIPPAGE_MAJOR = '0';
		process.env.DYNAMIC_SLIPPAGE_MULTIPLIER_MAJOR = '1';
		process.env.DYNAMIC_SLIPPAGE_MIN = '0';
		process.env.DYNAMIC_SLIPPAGE_MAX = '100';
		delete process.env.DYNAMIC_CROSS_SPREAD_MODE; // default cap
		process.env.DYNAMIC_CROSS_SPREAD_CAP = '0.1';

		mockVelocityClient.getMMOracleDataForPerpMarket.mockReturnValue({
			price: new BN(100).mul(PRICE_PRECISION),
		});

		const l2Normal = {
			bids: [{ price: new BN(99).mul(PRICE_PRECISION), size: new BN(1) }],
			asks: [{ price: new BN(101).mul(PRICE_PRECISION), size: new BN(1) }],
		} as any;

		const startPrice = new BN(100).mul(PRICE_PRECISION);
		const worstPrice = new BN(100).mul(PRICE_PRECISION);

		const slipNormal = calculateDynamicSlippage(
			0,
			'perp',
			mockVelocityClient,
			l2Normal,
			startPrice,
			worstPrice
		);

		const l2Crossed = {
			bids: [{ price: new BN(101).mul(PRICE_PRECISION), size: new BN(1) }],
			asks: [{ price: new BN(99).mul(PRICE_PRECISION), size: new BN(1) }],
		} as any;

		const slipCrossed = calculateDynamicSlippage(
			0,
			'perp',
			mockVelocityClient,
			l2Crossed,
			startPrice,
			worstPrice
		);

		// Non-crossed spread should be >= crossed (which is capped)
		expect(slipNormal).toBeGreaterThanOrEqual(slipCrossed);
	});
});

describe('calculateDynamicSlippage - the worst price reaches the walk', () => {
	const mockVelocityClient = {
		getMMOracleDataForPerpMarket: jest.fn(),
		getOracleDataForSpotMarket: jest.fn(),
	} as any;

	beforeEach(() => {
		jest.clearAllMocks();
	});

	it('floors slippage at the full start→worst distance plus a margin', () => {
		// A tolerance of half the distance leaves the worst price short of the
		// walk, and on a vAMM-only book the order then never fills.
		process.env.DYNAMIC_BASE_SLIPPAGE_MAJOR = '0';
		process.env.DYNAMIC_SLIPPAGE_MULTIPLIER_MAJOR = '1';
		process.env.DYNAMIC_SLIPPAGE_MIN = '0';
		process.env.DYNAMIC_SLIPPAGE_MAX = '100';
		delete process.env.DYNAMIC_SLIPPAGE_WORST_PRICE_MARGIN; // default 0.1%

		mockVelocityClient.getMMOracleDataForPerpMarket.mockReturnValue({
			price: new BN(100).mul(PRICE_PRECISION),
		});

		// Tight book so the spread term stays negligible
		const l2Tight = {
			bids: [
				{
					price: new BN(9_999).mul(PRICE_PRECISION).divn(100),
					size: new BN(1),
				},
			],
			asks: [
				{
					price: new BN(10_001).mul(PRICE_PRECISION).divn(100),
					size: new BN(1),
				},
			],
		} as any;

		// worst is 1% past start, as a vAMM-floored worst on a wide-spread
		// market can be.
		const startPrice = new BN(100).mul(PRICE_PRECISION);
		const worstPrice = new BN(101).mul(PRICE_PRECISION);

		const slip = calculateDynamicSlippage(
			0, // major perp
			'perp',
			mockVelocityClient,
			l2Tight,
			startPrice,
			worstPrice
		);

		// full 1% distance + 0.1% margin
		expect(slip).toBeGreaterThanOrEqual(1.1);
	});
});
