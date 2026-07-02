import { ZERO } from '../constants/numericConstants';
import { hasOpenOrders } from './position';
import { isVariant } from '../types';
import { User } from '../user';

export function isUserBankrupt(user: User): boolean {
	const userAccount = user.getUserAccountOrThrow();
	let hasLiability = false;
	for (const position of userAccount.spotPositions) {
		if (position.scaledBalance.gt(ZERO)) {
			if (isVariant(position.balanceType, 'deposit')) {
				return false;
			}
			if (isVariant(position.balanceType, 'borrow')) {
				hasLiability = true;
			}
		}
	}

	for (const position of userAccount.perpPositions) {
		// Isolated perp positions are handled by isIsolatedPositionBankrupt
		if (user.isPerpPositionIsolated(position)) {
			continue;
		}

		if (
			!position.baseAssetAmount.eq(ZERO) ||
			position.quoteAssetAmount.gt(ZERO) ||
			hasOpenOrders(position)
		) {
			return false;
		}

		if (position.quoteAssetAmount.lt(ZERO)) {
			hasLiability = true;
		}
	}

	return hasLiability;
}

export function isIsolatedPositionBankrupt(
	user: User,
	marketIndex: number
): boolean {
	const position = user.getPerpPositionOrThrow(marketIndex);

	if (position.isolatedPositionScaledBalance.gt(ZERO)) {
		return false;
	}

	return (
		position.baseAssetAmount.eq(ZERO) &&
		position.quoteAssetAmount.lt(ZERO) &&
		!hasOpenOrders(position)
	);
}
