import { ContractTier, PositionDirection } from '../types';
import { BN } from '../isomorphic/anchor';
import { isVariant } from '../types';

/**
 * The oracle over the slippage an unnamed price takes on a tier. Mirrors the
 * program's `math::worst_price::unnamed_price_slippage_divisor`.
 */
export function unnamedPriceSlippageDivisor(contractTier: ContractTier): BN {
	if (isVariant(contractTier, 'a')) {
		return new BN(50);
	}

	if (isVariant(contractTier, 'b') || isVariant(contractTier, 'c')) {
		return new BN(20);
	}

	if (isVariant(contractTier, 'speculative')) {
		return new BN(10);
	}

	return new BN(5);
}

/**
 * The worst price to stamp on an order, from the price its sender named.
 * Mirrors the program's `math::worst_price::derive_worst_price`. A named
 * price is the cap. No named price takes the tier's slippage from the oracle.
 */
export function deriveWorstPrice(
	oraclePrice: BN,
	contractTier: ContractTier,
	direction: PositionDirection,
	namedPrice: BN
): BN {
	if (namedPrice.gtn(0)) {
		return namedPrice;
	}

	const slippage = oraclePrice.div(unnamedPriceSlippageDivisor(contractTier));
	const bound = isVariant(direction, 'long')
		? oraclePrice.add(slippage)
		: oraclePrice.sub(slippage);

	return BN.max(bound, new BN(0));
}
