import { ZERO } from '../constants/numericConstants';
import { hasOpenOrders } from './position';
import { getTokenAmount } from './spotBalance';
import {
	isVariant,
	PerpPosition,
	PositionFlag,
	SpotBalanceType,
	UserAccount,
} from '../types';
import { User } from '../user';

/**
 * Economic (balance-derived) bankruptcy test for a single isolated perp position, shared by
 * {@link isIsolatedPositionBankrupt} and {@link hasIsolatedMarginBankrupt}. Mirrors the body of
 * `is_isolated_margin_bankrupt` in `programs/velocity/src/math/bankruptcy.rs`: the position is
 * bankrupt once its isolated collateral is fully drained (`isolatedPositionScaledBalance == 0`)
 * while it still has a flat base position, a negative quote balance (unpaid liability), and no
 * open orders. The caller is responsible for ensuring `position` is an isolated position.
 */
function isIsolatedPositionEconomicallyBankrupt(
	position: PerpPosition
): boolean {
	// defensive ?? ZERO matches user.ts's reads of this field (see its `//TODO remove ? later`)
	if ((position.isolatedPositionScaledBalance ?? ZERO).gt(ZERO)) {
		return false;
	}

	return (
		position.baseAssetAmount.eq(ZERO) &&
		position.quoteAssetAmount.lt(ZERO) &&
		!hasOpenOrders(position)
	);
}

/**
 * Determines whether a user's cross-margin book is bankrupt, mirroring
 * `is_cross_margin_bankrupt` in `programs/velocity/src/math/bankruptcy.rs`. Bankrupt means:
 * no realizable spot deposits, at least one spot borrow, and every non-isolated perp position
 * flat (zero base, no open orders, no realizable positive quote) with at least one carrying a
 * negative quote (unpaid liability). Isolated positions are skipped here and checked one at a
 * time by {@link isIsolatedPositionBankrupt}, since they resolve apart from the cross-margin book.
 * A deposit row vetoes only when worth at least one token. Socialization floors
 * `cumulativeDepositInterest` at 1, leaving a wiped depositor's positive `scaledBalance` worth
 * zero tokens; `liquidate_spot` rejects a zero amount, so such a row cannot veto admission.
 * A positive perp quote, and its market's PnL pool, do not veto: the resolvers recover what
 * the pool can pay and forfeit the rest to the market's insurance tranche, so a claim cannot
 * strand a loss elsewhere. The net-quote sum is exact, not an approximation, because every
 * position reaching this gate has `baseAssetAmount == 0`.
 * @param user The `User` account wrapper to evaluate.
 * @returns `true` if cross-margin collateral is exhausted and a liability remains.
 * @throws if a spot market named by a nonzero deposit row is not loaded on the client, via
 *   `getSpotMarketAccountOrThrow`, the same condition `User.canBeLiquidated` throws on.
 */
export function isUserBankrupt(user: User): boolean {
	const userAccount = user.getUserAccountOrThrow();
	let hasLiability = false;
	for (const position of userAccount.spotPositions) {
		if (position.scaledBalance.eq(ZERO)) {
			continue;
		}

		if (isVariant(position.balanceType, 'deposit')) {
			const spotMarket = user.velocityClient.getSpotMarketAccountOrThrow(
				position.marketIndex
			);
			const tokenAmount = getTokenAmount(
				position.scaledBalance,
				spotMarket,
				SpotBalanceType.DEPOSIT
			);
			if (tokenAmount.gt(ZERO)) {
				return false;
			}
		} else if (isVariant(position.balanceType, 'borrow')) {
			hasLiability = true;
		}
	}

	let netPerpQuote = ZERO;

	for (const position of userAccount.perpPositions) {
		// Isolated perp positions are handled by isIsolatedPositionBankrupt
		if (user.isPerpPositionIsolated(position)) {
			continue;
		}

		if (!position.baseAssetAmount.eq(ZERO) || hasOpenOrders(position)) {
			return false;
		}

		if (position.quoteAssetAmount.lt(ZERO)) {
			hasLiability = true;
		}

		netPerpQuote = netPerpQuote.add(position.quoteAssetAmount);
	}

	if (netPerpQuote.gt(ZERO)) {
		return false;
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
 * @throws if the user has no perp position for `marketIndex` (via `getPerpPositionOrThrow`),
 *   or if that position is not an isolated position — mirroring the program's
 *   `get_isolated_perp_position`, which errors `InvalidPerpPosition` on a non-isolated index.
 */
export function isIsolatedPositionBankrupt(
	user: User,
	marketIndex: number
): boolean {
	const position = user.getPerpPositionOrThrow(marketIndex);

	if (!user.isPerpPositionIsolated(position)) {
		throw new Error(
			`Perp position ${marketIndex} is not an isolated position (InvalidPerpPosition)`
		);
	}

	return isIsolatedPositionEconomicallyBankrupt(position);
}

/**
 * Determines whether the user holds any bankrupt isolated perp position, mirroring the isolated
 * half of the program's bankruptcy routing. On-chain, a user is routed to bankruptcy resolution
 * when `is_cross_margin_bankrupt` OR `has_isolated_margin_bankrupt` — and an isolated position
 * counts as bankrupt either because the program already set `PositionFlag::Bankrupt` on it
 * (`has_isolated_margin_bankrupt`, the status-flag view) or because it is economically bankrupt
 * and should enter bankruptcy (`is_isolated_margin_bankrupt`, the balance-derived view). A keeper
 * must catch both: `User.isBankrupt()` only reads the account-level `UserStatus.BANKRUPT` bit,
 * which `enter_isolated_margin_bankruptcy` never sets — so without this check an isolated-only
 * bankruptcy is invisible to `isUserBankrupt` (which deliberately skips isolated positions) and
 * to `User.isBankrupt()`, and would never be resolved.
 * @param user The `User` account wrapper to evaluate.
 * @returns `true` if any isolated perp position is flagged bankrupt on-chain or is economically
 *   bankrupt now; `false` otherwise.
 */
export function hasIsolatedMarginBankrupt(user: User): boolean {
	const userAccount = user.getUserAccountOrThrow();
	for (const position of userAccount.perpPositions) {
		if (!user.isPerpPositionIsolated(position)) {
			continue;
		}
		if (
			(position.positionFlag & PositionFlag.Bankruptcy) !== 0 ||
			isIsolatedPositionEconomicallyBankrupt(position)
		) {
			return true;
		}
	}
	return false;
}

/**
 * Perp markets a bankruptcy resolver may write to beyond the one being resolved, to add to
 * `writablePerpMarketIndexes`. Mirrors `perp_markets_with_forfeitable_claims`. Unfundable
 * positive claims forfeit to these markets' insurance tranches, so a read-only account fails
 * `load_mut`. Fundability is not tested here, and the program recomputes it at execution.
 */
export function getPerpMarketsWithForfeitableClaims(
	userAccount: UserAccount
): number[] {
	return userAccount.perpPositions
		.filter(
			(position) =>
				(position.positionFlag & PositionFlag.IsolatedPosition) === 0 &&
				position.quoteAssetAmount.gt(ZERO) &&
				position.baseAssetAmount.eq(ZERO) &&
				!hasOpenOrders(position)
		)
		.map((position) => position.marketIndex);
}
