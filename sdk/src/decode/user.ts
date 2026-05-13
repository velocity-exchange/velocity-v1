import { BN } from '../isomorphic/anchor';
import {
	MarketType,
	Order,
	OrderStatus,
	OrderTriggerCondition,
	OrderType,
	PerpPosition,
	PositionDirection,
	PositionFlag,
	SpotBalanceType,
	SpotPosition,
	UserAccount,
} from '../types';
import { PublicKey } from '@solana/web3.js';
import { ZERO } from '../constants/numericConstants';

// Layout constants for the dynamic-orders User account.
// account = [8B disc][User header (1296B)][orders_len * Order (96B each)]
const USER_DISC_LEN = 8;
const USER_FIXED_SIZE = 1296;
const ORDER_SIZE = 96;
// orders_len: u32 lives at offset 1292 inside the User header → account offset 1300.
const USER_ORDERS_LEN_OFFSET = USER_DISC_LEN + 1292;
// orders tail starts immediately after the User header.
const USER_ORDERS_TAIL_OFFSET = USER_DISC_LEN + USER_FIXED_SIZE;

function readUnsignedBigInt64LE(buffer: Buffer, offset: number): BN {
	return new BN(buffer.subarray(offset, offset + 8), 10, 'le');
}

function readSignedBigInt64LE(buffer: Buffer, offset: number): BN {
	const unsignedValue = new BN(buffer.subarray(offset, offset + 8), 10, 'le');
	if (unsignedValue.testn(63)) {
		const inverted = unsignedValue.notn(64).addn(1);
		return inverted.neg();
	} else {
		return unsignedValue;
	}
}

function decodeOrderAt(buffer: Buffer, offset: number): Order | null {
	// skip order if it's not open (status byte at offset+82)
	if (buffer.readUint8(offset + 82) !== 1) {
		return null;
	}

	const slot = readUnsignedBigInt64LE(buffer, offset);
	const price = readUnsignedBigInt64LE(buffer, offset + 8);
	const baseAssetAmount = readUnsignedBigInt64LE(buffer, offset + 16);
	const baseAssetAmountFilled = readUnsignedBigInt64LE(buffer, offset + 24);
	const quoteAssetAmountFilled = readUnsignedBigInt64LE(buffer, offset + 32);
	const triggerPrice = readUnsignedBigInt64LE(buffer, offset + 40);
	const auctionStartPrice = readSignedBigInt64LE(buffer, offset + 48);
	const auctionEndPrice = readSignedBigInt64LE(buffer, offset + 56);
	const maxTs = readSignedBigInt64LE(buffer, offset + 64);
	const oraclePriceOffset = buffer.readInt32LE(offset + 72);
	const orderId = buffer.readUInt32LE(offset + 76);
	const marketIndex = buffer.readUInt16LE(offset + 80);

	const orderStatusNum = buffer.readUInt8(offset + 82);
	let status: OrderStatus;
	if (orderStatusNum === 0) {
		status = OrderStatus.INIT;
	} else if (orderStatusNum === 1) {
		status = OrderStatus.OPEN;
	}

	const orderTypeNum = buffer.readUInt8(offset + 83);
	let orderType: OrderType;
	if (orderTypeNum === 0) {
		orderType = OrderType.MARKET;
	} else if (orderTypeNum === 1) {
		orderType = OrderType.LIMIT;
	} else if (orderTypeNum === 2) {
		orderType = OrderType.TRIGGER_MARKET;
	} else if (orderTypeNum === 3) {
		orderType = OrderType.TRIGGER_LIMIT;
	} else if (orderTypeNum === 4) {
		orderType = OrderType.ORACLE;
	}

	const marketTypeNum = buffer.readUInt8(offset + 84);
	const marketType: MarketType =
		marketTypeNum === 0 ? MarketType.SPOT : MarketType.PERP;

	const userOrderId = buffer.readUint8(offset + 85);

	const existingPositionDirectionNum = buffer.readUInt8(offset + 86);
	const existingPositionDirection: PositionDirection =
		existingPositionDirectionNum === 0
			? PositionDirection.LONG
			: PositionDirection.SHORT;

	const positionDirectionNum = buffer.readUInt8(offset + 87);
	const direction: PositionDirection =
		positionDirectionNum === 0
			? PositionDirection.LONG
			: PositionDirection.SHORT;

	const reduceOnly = buffer.readUInt8(offset + 88) === 1;
	const postOnly = buffer.readUInt8(offset + 89) === 1;
	const immediateOrCancel = buffer.readUInt8(offset + 90) === 1;

	const triggerConditionNum = buffer.readUInt8(offset + 91);
	let triggerCondition: OrderTriggerCondition;
	if (triggerConditionNum === 0) {
		triggerCondition = OrderTriggerCondition.ABOVE;
	} else if (triggerConditionNum === 1) {
		triggerCondition = OrderTriggerCondition.BELOW;
	} else if (triggerConditionNum === 2) {
		triggerCondition = OrderTriggerCondition.TRIGGERED_ABOVE;
	} else if (triggerConditionNum === 3) {
		triggerCondition = OrderTriggerCondition.TRIGGERED_BELOW;
	}

	const auctionDuration = buffer.readUInt8(offset + 92);
	const postedSlotTail = buffer.readUint8(offset + 93);
	const bitFlags = buffer.readUint8(offset + 94);
	// offset + 95: padding

	return {
		slot,
		price,
		baseAssetAmount,
		quoteAssetAmount: undefined,
		baseAssetAmountFilled,
		quoteAssetAmountFilled,
		triggerPrice,
		auctionStartPrice,
		auctionEndPrice,
		maxTs,
		oraclePriceOffset,
		orderId,
		marketIndex,
		status,
		orderType,
		marketType,
		userOrderId,
		existingPositionDirection,
		direction,
		reduceOnly,
		postOnly,
		immediateOrCancel,
		triggerCondition,
		auctionDuration,
		bitFlags,
		postedSlotTail,
	};
}

export function decodeUser(buffer: Buffer): UserAccount {
	let offset = 8;
	const authority = new PublicKey(buffer.slice(offset, offset + 32));
	offset += 32;
	const delegate = new PublicKey(buffer.slice(offset, offset + 32));
	offset += 32;
	const name = [];
	for (let i = 0; i < 32; i++) {
		name.push(buffer.readUint8(offset + i));
	}
	offset += 32;

	const spotPositions: SpotPosition[] = [];
	for (let i = 0; i < 8; i++) {
		const scaledBalance = readUnsignedBigInt64LE(buffer, offset);
		const openOrders = buffer.readUInt8(offset + 35);
		if (scaledBalance.eq(ZERO) && openOrders === 0) {
			offset += 40;
			continue;
		}

		offset += 8;
		const openBids = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		const openAsks = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		const cumulativeDeposits = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		const marketIndex = buffer.readUInt16LE(offset);
		offset += 2;
		const balanceTypeNum = buffer.readUInt8(offset);
		let balanceType: SpotBalanceType;
		if (balanceTypeNum === 0) {
			balanceType = SpotBalanceType.DEPOSIT;
		} else {
			balanceType = SpotBalanceType.BORROW;
		}
		offset += 6;
		spotPositions.push({
			scaledBalance,
			openBids,
			openAsks,
			cumulativeDeposits,
			marketIndex,
			balanceType,
			openOrders,
		});
	}

	const perpPositions: PerpPosition[] = [];
	for (let i = 0; i < 8; i++) {
		const baseAssetAmount = readSignedBigInt64LE(buffer, offset + 8);
		const quoteAssetAmount = readSignedBigInt64LE(buffer, offset + 16);
		const lpShares = readUnsignedBigInt64LE(buffer, offset + 64);
		const isolatedPositionScaledBalance = readUnsignedBigInt64LE(
			buffer,
			offset + 72
		);
		const openOrders = buffer.readUInt8(offset + 94);
		const positionFlag = buffer.readUInt8(offset + 95);

		if (
			baseAssetAmount.eq(ZERO) &&
			openOrders === 0 &&
			quoteAssetAmount.eq(ZERO) &&
			lpShares.eq(ZERO) &&
			isolatedPositionScaledBalance.eq(ZERO) &&
			!(
				(positionFlag &
					(PositionFlag.BeingLiquidated | PositionFlag.Bankruptcy)) >
				0
			)
		) {
			offset += 96;
			continue;
		}

		const lastCumulativeFundingRate = readSignedBigInt64LE(buffer, offset);
		offset += 24;
		const quoteBreakEvenAmount = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		const quoteEntryAmount = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		const openBids = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		const openAsks = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		const settledPnl = readSignedBigInt64LE(buffer, offset);
		offset += 24;
		const lastQuoteAssetAmountPerLp = readSignedBigInt64LE(buffer, offset);
		offset += 8;
		offset += 2; // skip padding[u8; 2]
		const maxMarginRatio = buffer.readUInt16LE(offset); // offset+90
		offset += 2;
		const marketIndex = buffer.readUInt16LE(offset); // offset+92
		offset += 4; // advance past marketIndex(2) + openOrders(1) + positionFlag(1)
		perpPositions.push({
			lastCumulativeFundingRate,
			baseAssetAmount,
			quoteAssetAmount,
			quoteBreakEvenAmount,
			quoteEntryAmount,
			openBids,
			openAsks,
			settledPnl,
			lpShares,
			remainderBaseAssetAmount: 0,
			lastQuoteAssetAmountPerLp,
			marketIndex,
			openOrders,
			maxMarginRatio,
			positionFlag,
			isolatedPositionScaledBalance,
		});
	}

	// Post-orders header fields used to follow the embedded `[Order; 32]`.
	// They now live immediately after `perp_positions`, and `orders_len` plus the
	// orders tail come at the end.
	const lastAddPerpLpSharesTs = readSignedBigInt64LE(buffer, offset);
	offset += 8;

	const totalDeposits = readUnsignedBigInt64LE(buffer, offset);
	offset += 8;

	const totalWithdraws = readUnsignedBigInt64LE(buffer, offset);
	offset += 8;

	const totalSocialLoss = readUnsignedBigInt64LE(buffer, offset);
	offset += 8;

	const settledPerpPnl = readSignedBigInt64LE(buffer, offset);
	offset += 8;

	const cumulativeSpotFees = readSignedBigInt64LE(buffer, offset);
	offset += 8;

	const cumulativePerpFunding = readSignedBigInt64LE(buffer, offset);
	offset += 8;

	const liquidationMarginFreed = readUnsignedBigInt64LE(buffer, offset);
	offset += 8;

	const lastActiveSlot = readUnsignedBigInt64LE(buffer, offset);
	offset += 8;

	const nextOrderId = buffer.readUInt32LE(offset);
	offset += 4;

	const maxMarginRatio = buffer.readUInt32LE(offset);
	offset += 4;

	const nextLiquidationId = buffer.readUInt16LE(offset);
	offset += 2;

	const subAccountId = buffer.readUInt16LE(offset);
	offset += 2;

	const status = buffer.readUInt8(offset);
	offset += 1;

	const isMarginTradingEnabled = buffer.readUInt8(offset) === 1;
	offset += 1;

	const idle = buffer.readUInt8(offset) === 1;
	offset += 1;

	const openOrders = buffer.readUInt8(offset);
	offset += 1;

	const hasOpenOrder = buffer.readUInt8(offset) === 1;
	offset += 1;

	const openAuctions = buffer.readUInt8(offset);
	offset += 1;

	const hasOpenAuction = buffer.readUInt8(offset) === 1;
	offset += 1;

	offset += 1; // marginMode (removed)

	const poolId = buffer.readUint8(offset);
	offset += 1;
	offset += 3; // padding1

	const lastFuelBonusUpdateTs = buffer.readUint32LE(offset);
	offset += 4;
	const specialUserStatus = buffer.readUInt8(offset);
	offset += 1;
	offset += 7; // padding (was 11; orders_len reclaimed 4 bytes)

	const ordersLen = buffer.readUInt32LE(offset);
	offset = USER_ORDERS_TAIL_OFFSET;

	const orders: Order[] = [];
	const cap = Math.floor((buffer.length - USER_ORDERS_TAIL_OFFSET) / ORDER_SIZE);
	const effectiveLen = Math.min(ordersLen, cap);
	for (let i = 0; i < effectiveLen; i++) {
		const o = decodeOrderAt(buffer, offset);
		offset += ORDER_SIZE;
		if (o !== null) {
			orders.push(o);
		}
	}

	return {
		authority,
		delegate,
		name,
		spotPositions,
		perpPositions,
		orders,
		lastAddPerpLpSharesTs,
		totalDeposits,
		totalWithdraws,
		totalSocialLoss,
		settledPerpPnl,
		cumulativeSpotFees,
		cumulativePerpFunding,
		liquidationMarginFreed,
		lastActiveSlot,
		nextOrderId,
		maxMarginRatio,
		nextLiquidationId,
		subAccountId,
		status,
		isMarginTradingEnabled,
		idle,
		openOrders,
		hasOpenOrder,
		openAuctions,
		hasOpenAuction,
		poolId,
		lastFuelBonusUpdateTs,
		specialUserStatus,
	};
}
