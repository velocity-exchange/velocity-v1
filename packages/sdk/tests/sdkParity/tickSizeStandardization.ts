import { expect } from 'chai';

import {
	BN,
	MarketType,
	Order,
	OrderStatus,
	OrderType,
	OrderTriggerCondition,
	PositionDirection,
	MMOraclePriceData,
	ZERO,
	standardizePrice,
	getLimitPrice,
	hasBuilder,
	OrderBitFlag,
} from '../../src';

// Minimal Order factory mirroring the on-chain layout used by the math paths.
function makeOrder(overrides: Partial<Order>): Order {
	return {
		status: OrderStatus.OPEN,
		orderType: OrderType.LIMIT,
		marketType: MarketType.PERP,
		slot: new BN(1),
		orderId: 1,
		userOrderId: 0,
		marketIndex: 0,
		price: ZERO,
		baseAssetAmount: new BN(1),
		baseAssetAmountFilled: ZERO,
		quoteAssetAmountFilled: ZERO,
		direction: PositionDirection.LONG,
		reduceOnly: false,
		triggerPrice: ZERO,
		triggerCondition: OrderTriggerCondition.ABOVE,
		existingPositionDirection: PositionDirection.LONG,
		postOnly: false,
		immediateOrCancel: false,
		oraclePriceOffset: ZERO,
		unusedAuctionDuration: 0,
		clobNodeIndex: ZERO,
		clobOrderId: ZERO,
		maxTs: ZERO,
		bitFlags: 0,
		postedSlotTail: 0,
		padding: [0, 0, 0, 0],
		...overrides,
	} as Order;
}

function mmOracle(price: number, slot: number): MMOraclePriceData {
	return {
		price: new BN(price),
		slot: new BN(slot),
		confidence: new BN(1),
		hasSufficientNumberOfDataPoints: true,
		isMMOracleActive: true,
	};
}

describe('tick size standardization parity', () => {
	const TICK = new BN(10);

	describe('standardizePrice mirrors program standardize_price', () => {
		it('long floors to the tick below', () => {
			expect(
				standardizePrice(new BN(127), TICK, PositionDirection.LONG).toString()
			).to.equal('120');
		});

		it('short ceils to the tick above', () => {
			expect(
				standardizePrice(new BN(127), TICK, PositionDirection.SHORT).toString()
			).to.equal('130');
		});

		it('leaves on-tick prices untouched (both directions)', () => {
			expect(
				standardizePrice(new BN(130), TICK, PositionDirection.LONG).toString()
			).to.equal('130');
			expect(
				standardizePrice(new BN(130), TICK, PositionDirection.SHORT).toString()
			).to.equal('130');
		});

		it('returns zero unchanged', () => {
			expect(
				standardizePrice(ZERO, TICK, PositionDirection.SHORT).toString()
			).to.equal('0');
		});
	});

	describe('getLimitPrice threads tick size', () => {
		it('standardizes the oracle-offset limit price and floors at tickSize', () => {
			const longOrder = makeOrder({
				orderType: OrderType.LIMIT,
				direction: PositionDirection.LONG,
				oraclePriceOffset: new BN(27),
			});

			expect(
				getLimitPrice(longOrder, mmOracle(1000, 4), undefined, TICK)!.toString()
			).to.equal('1020');

			const shortOrder = makeOrder({
				orderType: OrderType.LIMIT,
				direction: PositionDirection.SHORT,
				oraclePriceOffset: new BN(27),
			});

			expect(
				getLimitPrice(
					shortOrder,
					mmOracle(1000, 4),
					undefined,
					TICK
				)!.toString()
			).to.equal('1030');
		});

		it('oracle-offset limit floors at tickSize when the sum underflows', () => {
			const order = makeOrder({
				orderType: OrderType.LIMIT,
				direction: PositionDirection.LONG,
				oraclePriceOffset: new BN(-100),
			});

			expect(
				getLimitPrice(order, mmOracle(5, 4), undefined, TICK)!.toString()
			).to.equal('10');
		});

		it('returns a raw fixed limit price unchanged (matches program)', () => {
			const order = makeOrder({
				orderType: OrderType.LIMIT,
				direction: PositionDirection.LONG,
				price: new BN(123),
			});

			expect(
				getLimitPrice(order, mmOracle(1000, 4), undefined, TICK)!.toString()
			).to.equal('123');
		});

		it('standardizes a fallback price when order price is zero', () => {
			const order = makeOrder({
				orderType: OrderType.LIMIT,
				direction: PositionDirection.SHORT,
				price: ZERO,
				oraclePriceOffset: ZERO,
			});

			expect(
				getLimitPrice(order, mmOracle(1000, 4), new BN(127), TICK)!.toString()
			).to.equal('130');
		});
	});

	describe('hasBuilder uses the shared OrderBitFlag enum', () => {
		it('detects the HasBuilder flag', () => {
			const order = makeOrder({ bitFlags: OrderBitFlag.HasBuilder });
			expect(hasBuilder(order)).to.equal(true);
		});
		it('is false without the flag', () => {
			const order = makeOrder({ bitFlags: OrderBitFlag.SignedMessage });
			expect(hasBuilder(order)).to.equal(false);
		});
	});
});
