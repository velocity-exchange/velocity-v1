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
 * Determines whether a user's cross-margin book is bankrupt. This mirrors
 * `is_cross_margin_bankrupt` in `programs/velocity/src/math/bankruptcy.rs`. A user is
 * cross-margin bankrupt when the user holds no realizable spot deposits, holds at least one
 * spot borrow, and every non-isolated perp position is flat. A flat position has zero base,
 * no open orders, and no realizable positive quote. At least one of them carries a negative
 * quote, which is an unpaid perp liability. Isolated perp positions are skipped here.
 * `user.isPerpPositionIsolated` identifies them, and `isIsolatedPositionBankrupt` checks them
 * one at a time, because they resolve and settle apart from the cross-margin book.
 *
 * Two of the vetoes read value rather than rows. A keeper that gets either one wrong stalls
 * the bad-debt repair the program is willing to perform. The resolvers admit an estate
 * themselves, so the one thing between such an account and resolution is a caller that
 * decides to send the instruction.
 *
 * A deposit row vetoes only when it is worth at least one token. A full spot-market
 * socialization floors `cumulativeDepositInterest` at 1, which leaves every wiped depositor
 * holding a positive `scaledBalance` worth zero tokens. `liquidate_spot` rejects a zero token
 * amount, so such a row cannot be seized. Treating it as collateral blocks admission forever.
 *
 * A positive perp quote does not veto, and neither does its market's PnL pool. The resolvers
 * recover whatever that pool can pay into the estate's quote deposit and forfeit the rest to
 * the market's insurance tranche. A claim therefore cannot strand a resolvable loss in
 * another market, whatever the pool holds. A veto on the pool would stall the repair, and
 * trading fees flow into the pool, so any market participant could re-arm such a veto with a
 * trade.
 *
 * The net-quote gate then keeps an estate that is net solvent out of bankruptcy, however
 * unfundable its claims are. The sum of the quotes is exact here rather than an
 * approximation. Every position that reaches the gate has `baseAssetAmount == 0`, so its
 * whole value is its `quoteAssetAmount`. The gate does not bound the forfeit. The program
 * bounds the forfeit against the loss each resolver call covers, because a latched estate
 * reaches a resolver without passing this predicate again.
 *
 * @param user The `User` account wrapper to evaluate.
 * @returns `true` if the user's cross-margin collateral is exhausted and the user still owes
 *   a liability, which is a spot borrow or a negative perp quote balance. `false` otherwise.
 * @throws if a spot market named by a nonzero deposit row is not loaded on the client.
 *   `getSpotMarketAccountOrThrow` raises it. The token conversion cannot run without the
 *   market, and treating an unloaded market as "no assets" would over-report bankruptcy.
 *   `User.canBeLiquidated` throws on the same condition, so a keeper that screens users with
 *   both does not gain a new failure mode here.
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
 * Perp market indexes a bankruptcy resolver may write to beyond the market being resolved.
 * This mirrors `perp_markets_with_forfeitable_claims` in
 * `programs/velocity/src/math/bankruptcy.rs`.
 *
 * `resolvePerpBankruptcy` and `resolveSpotBankruptcy` forfeit the estate's unfundable positive
 * perp claims to their own markets' insurance tranches. The forfeit debits the user's claim and
 * credits that market's `pendingIfFee`. Both instructions therefore pass these markets
 * writable. A read-only account fails the program's `load_mut` and reverts the whole resolve.
 *
 * The filter does not test fundability. A claim the pool can pay when the transaction is built
 * may be unfundable by the time it lands, and the program recomputes fundability at execution.
 * @param userAccount The decoded user account of the estate being resolved.
 * @returns The market indexes to add to `writablePerpMarketIndexes`. The list can be empty.
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
