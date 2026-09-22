import { PositionDirection } from '../types';
import { BN } from '../isomorphic/anchor';
import { DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION } from '../constants/numericConstants';
import { isVariant } from '../types';

/**
 * The worst price to stamp on an order, from the price its sender named.
 *
 * Mirrors the program's `math::worst_price::derive_worst_price`. A named price
 * is the order's cap, and the sender chooses how far from the oracle it sits.
 * A sender that names no price takes `DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION`
 * of the oracle as the cap.
 */
export function deriveWorstPrice(
	oraclePrice: BN,
	direction: PositionDirection,
	namedPrice: BN
): BN {
	if (namedPrice.gtn(0)) {
		return namedPrice;
	}

	const slippage = oraclePrice.div(DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION);
	const bound = isVariant(direction, 'long')
		? oraclePrice.add(slippage)
		: oraclePrice.sub(slippage);

	return BN.max(bound, new BN(0));
}
