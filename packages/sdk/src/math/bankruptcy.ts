import { ZERO } from '../constants/numericConstants';
import { hasOpenOrders } from './position';
import { isVariant } from '../types';
import { User } from '../user';

/**
 * Determines whether a user's cross-margin book is bankrupt, mirroring
 * `is_cross_margin_bankrupt` in `programs/velocity/src/math/bankruptcy.rs`. A user is
 * cross-margin bankrupt when they hold no spot deposits, at least one spot borrow, and
 * every non-isolated perp position is flat (zero base, non-positive quote, no open orders)
 * with at least one carrying negative quote (an unpaid perp liability). Isolated perp
 * positions (`user.isPerpPositionIsolated`) are skipped here — check those individually
 * with `isIsolatedPositionBankrupt` instead, since they resolve/settle independently of
 * the cross-margin book.
 * @param user The `User` account wrapper to evaluate.
 * @returns `true` if the user's cross-margin collateral is exhausted and they still owe a
 *   liability (spot borrow or negative perp quote balance); `false` otherwise.
 */
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

/**
 * Determines whether a specific isolated perp position is bankrupt, mirroring
 * `is_isolated_margin_bankrupt` in `programs/velocity/src/math/bankruptcy.rs`. Isolated
 * positions carry their own collateral pool (`isolatedPositionScaledBalance`, spot-balance
 * precision) separate from the user's cross-margin book, so bankruptcy is evaluated
 * per-market: the position is bankrupt once its isolated collateral is fully drained
 * (`isolatedPositionScaledBalance == 0`) while it still has a flat base position, a
 * negative quote balance (unpaid liability), and no open orders.
 * @param user The `User` account wrapper to evaluate.
 * @param marketIndex Perp market index of the isolated position to check.
 * @returns `true` if the isolated position's collateral is exhausted and it still owes a
 *   liability; `false` otherwise.
 * @throws if the user has no perp position for `marketIndex` (via `getPerpPositionOrThrow`).
 */
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
