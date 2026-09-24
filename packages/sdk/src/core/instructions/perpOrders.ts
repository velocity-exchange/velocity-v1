import type {
	AccountMeta,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';
import type { VelocityProgram } from '../../config';
import type {
	ClobAccounts,
	PlaceAndTakeOrderSuccessCondition,
} from '../../types';

/**
 * Builds a `placeTriggerOrdersV1` instruction, arming trigger orders in the user's own order slots. A slot holds one unfired conditional, so every entry must be a `TriggerMarket` or a `TriggerLimit` on a perp market, or the program returns `OrderTypeNotConditional`.
 * A live order rests on the book instead, via `buildPlaceAndTakePerpOrderInstruction` or `buildPlaceAndMakePerpOrderInstruction`. One margin check covers the whole batch, letting a stop loss and a take profit arrive together.
 * @param args.orderParams - the triggers to arm; `baseAssetAmount` is BASE_PRECISION (1e9), `price`/`triggerPrice` are PRICE_PRECISION (1e6).
 * @param args.remainingAccounts - oracle/market `AccountMeta[]` for each `marketIndex`, plus the placing user's `RevenueShareEscrow` account if an order carries a `builderIdx` and builder codes are enabled protocol-wide.
 * @returns the unsigned `placeTriggerOrdersV1` `TransactionInstruction`.
 */
export async function buildPlaceTriggerOrdersInstruction(args: {
	program: VelocityProgram;
	orderParams: any[];
	state: PublicKey;
	user: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await args.program.instruction.placeTriggerOrdersV1(
		{ params: args.orderParams },
		{
			accounts: {
				state: args.state,
				user: args.user,
				authority: args.authority,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}

/**
 * allow-verbose: enumerates every taker-side semantic (postOnly gate, successCondition,
 * remainingAccounts ordering, clobAccounts shape) a caller must reproduce exactly.
 *
 * Builds a `placeAndTakePerpOrderV1` instruction: places an order and routes it as a
 * taker against the market's book, its quoters and the AMM.
 * `orderParams.postOnly` must be `PostOnlyParam.None` (`InvalidOrderPostOnly` otherwise).
 * A restable remainder rests on the book. Any portion left unfilled is auto-cancelled
 * when the order is immediate-or-cancel. A part that does not rest emits an
 * `OrderActionRecord` with `OrderAction.CANCEL`. A reduce-only order with less than one
 * step left to reduce counts as filled. A remainder that can never rest, such as an
 * `OrderType.ORACLE` one, fails the instruction with `InvalidOrder`.
 * @param args.orderParams - an `OrderParams` object; `baseAssetAmount` is BASE_PRECISION (1e9), `price`/`triggerPrice`/`oraclePriceOffset` are PRICE_PRECISION (1e6).
 * @param args.successCondition - a `PlaceAndTakeOrderSuccessCondition`. The instruction reverts unless the take fills at least partially or fully. `null` for no check.
 * @param args.authority - signer that must own or be a registered delegate of `user`.
 * @param args.remainingAccounts - writable perp market + oracle `AccountMeta[]` for `orderParams.marketIndex`, followed by maker/referrer `(User, UserStats)` pairs, followed by the taker's `RevenueShareEscrow` account if builder codes are enabled.
 * @param args.clobAccounts - the market's CLOB accounts, which are `quoterSlab`, a
 * writable `clobMarket`, and `clobProgram`. The order routes through them, and a
 * restable remainder rests on the book.
 * On a book with a speed bump the order carries no flow attestation, so it rests whole
 * and the cross cranks fill it. An IOC order or a `successCondition` is then refused
 * with `UnattestedSynchronousTake`.
 * @returns the unsigned `placeAndTakePerpOrderV1` `TransactionInstruction`.
 */
export async function buildPlaceAndTakePerpOrderInstruction(args: {
	program: VelocityProgram;
	orderParams: any;
	successCondition: PlaceAndTakeOrderSuccessCondition | null;
	state: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
	clobAccounts: ClobAccounts;
}): Promise<TransactionInstruction> {
	return await args.program.instruction.placeAndTakePerpOrderV1(
		{ params: args.orderParams, successCondition: args.successCondition },
		{
			accounts: {
				state: args.state,
				user: args.user,
				userStats: args.userStats,
				authority: args.authority,
				quoterSlab: args.clobAccounts.quoterSlab,
				clobMarket: args.clobAccounts.clobMarket,
				clobProgram: args.clobAccounts.clobProgram,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}

/**
 * allow-verbose: enumerates the maker-side invariants (order shape, post-only modes,
 * refusals, activationDelaySlots default/attestation rule) a caller must reproduce exactly.
 *
 * Builds a `placeAndMakePerpOrderV1` instruction. It posts a `Limit` maker order for
 * `user` that rests straight on the market's CLOB. It names no taker and matches
 * nothing on placement. The maker never occupies a `User.orders` slot.
 * The program refuses a non-`Limit` order with `InvalidOrderIOCPostOnly`, an IOC order
 * with `InvalidOrderIOC`, and a builder code with `InvalidOrder`. A `userOrderId` that a
 * live slot order holds fails with `UserOrderIdAlreadyInUse`.
 * A post-only order refuses to rest crossed. `TryPostOnly` skips the order without an
 * error when it would cross the vAMM or the book's best opposite order. `Slide` moves
 * it one tick behind the vAMM and then behind the book's best opposite order.
 * A reduce-only order rests at most the position it reduces. With no position to
 * reduce it fails with `InvalidOrderNotRiskReducing`.
 * An order the book or the margin gate refuses fails the instruction, for example with
 * `InvalidOrderMinOrderSize`, `InvalidOrderMaxTs`, `MaxNumberOfOrders` or
 * `InsufficientCollateral`. The instruction emits an `OrderActionRecord` with
 * `OrderAction.PLACE` and an `OrderRecord` for the resting order.
 * @param args.orderParams - an `OrderParams` object; `baseAssetAmount` is BASE_PRECISION (1e9), `price` is PRICE_PRECISION (1e6).
 * @param args.authority - signer that must own or be a registered delegate of `user` (the maker).
 * @param args.remainingAccounts - writable perp market + oracle `AccountMeta[]` for `orderParams.marketIndex`.
 * @param args.clobAccounts - the market's CLOB accounts. All of them are required.
 * `args.orderParams.activationDelaySlots` is the book speed bump the maker rests behind.
 * A `null` value takes the book's default. A value below the default is refused with
 * `UnattestedFastActivation`.
 * @returns the unsigned `placeAndMakePerpOrderV1` `TransactionInstruction`.
 */
export async function buildPlaceAndMakePerpOrderInstruction(args: {
	program: VelocityProgram;
	orderParams: any;
	state: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
	clobAccounts: ClobAccounts;
}): Promise<TransactionInstruction> {
	return await args.program.instruction.placeAndMakePerpOrderV1(
		{ params: args.orderParams },
		{
			accounts: {
				state: args.state,
				user: args.user,
				userStats: args.userStats,
				authority: args.authority,
				quoterSlab: args.clobAccounts.quoterSlab,
				clobMarket: args.clobAccounts.clobMarket,
				clobProgram: args.clobAccounts.clobProgram,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}

/**
 * allow-verbose: a public builder whose body delegates; the id rules decide whether it lands.
 *
 * Build a raw `cancelOrder` instruction. It cancels an order in a `User.orders` slot.
 * Pass `orderId: null` to cancel the user's most recently placed order, which the
 * program resolves through `get_last_order_id`. That is safe after a trigger placement
 * in the same transaction: `[placeTriggerOrders] → [cancelOrder(orderId: null)]`.
 * An id the user already minted that is not an open slot order fails with
 * `OrderDoesNotExist`. A resting CLOB order draws its id from the same counter, so its
 * id fails here too. Cancel it with `cancelOrderV1`.
 *
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.orderId - the order to cancel, or `null` per the above.
 * @param args.state - the global `State` PDA.
 * @param args.user - the `User` account whose order is cancelled.
 * @param args.authority - signer that must own or be a registered delegate of `user`.
 * @param args.remainingAccounts - oracle/market `AccountMeta[]` needed to re-derive the cancelled order's auction/oracle-offset price.
 * @returns the unsigned `cancelOrder` `TransactionInstruction`.
 */
export async function buildCancelOrderInstruction(args: {
	program: VelocityProgram;
	orderId: number | null;
	state: PublicKey;
	user: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await args.program.instruction.cancelOrder(args.orderId, {
		accounts: {
			state: args.state,
			user: args.user,
			authority: args.authority,
		},
		remainingAccounts: args.remainingAccounts,
	});
}

/**
 * Builds a `cancelOrderByUserId` instruction, cancelling the order matching the
 * caller-assigned `userOrderId` (the `OrderParams.userOrderId` tag set at placement)
 * rather than the program-assigned order ID.
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.userOrderId - the caller-assigned order tag (`u8`) supplied when the order was placed.
 * @param args.state - the global `State` PDA.
 * @param args.user - the `User` account whose order is cancelled.
 * @param args.authority - signer that must own or be a registered delegate of `user`.
 * @param args.oracle - accepted for forward-compatibility but not required by the current on-chain `cancel_order_by_user_id` accounts (only `state`/`user`/`authority` are used).
 * @param args.remainingAccounts - oracle/market `AccountMeta[]` needed to re-derive the cancelled order's auction/oracle-offset price.
 * @returns the unsigned `cancelOrderByUserId` `TransactionInstruction`.
 */
export async function buildCancelOrderByUserIdInstruction(args: {
	program: VelocityProgram;
	userOrderId: number;
	state: PublicKey;
	user: PublicKey;
	authority: PublicKey;
	oracle: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await (args.program.instruction as any).cancelOrderByUserId(
		args.userOrderId,
		{
			accounts: {
				state: args.state,
				user: args.user,
				authority: args.authority,
				oracle: args.oracle,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}

/**
 * Builds a `cancelOrdersByIds` instruction, cancelling multiple orders by their
 * program-assigned order IDs in one instruction (loops `cancel_order_by_order_id`
 * on-chain, one order at a time — an order ID that no longer exists is silently
 * skipped rather than failing the whole instruction).
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.orderIds - order IDs to cancel; passing `undefined` sends an empty list on-chain (cancels nothing).
 * @param args.state - the global `State` PDA.
 * @param args.user - the `User` account whose orders are cancelled.
 * @param args.authority - signer that must own or be a registered delegate of `user`.
 * @param args.remainingAccounts - oracle/market `AccountMeta[]` needed to re-derive the cancelled orders' auction/oracle-offset prices.
 * @returns the unsigned `cancelOrdersByIds` `TransactionInstruction`.
 */
export async function buildCancelOrdersByIdsInstruction(args: {
	program: VelocityProgram;
	orderIds: number[] | undefined;
	state: PublicKey;
	user: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await args.program.instruction.cancelOrdersByIds(args.orderIds, {
		accounts: {
			state: args.state,
			user: args.user,
			authority: args.authority,
		},
		remainingAccounts: args.remainingAccounts,
	});
}

/**
 * Builds a `modifyOrder` instruction, mutating an existing resting order in place (e.g.
 * new price/size/trigger) rather than cancel-and-replace.
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.orderId - the program-assigned order ID to modify (use `buildModifyOrderByUserIdInstruction` to target by the caller-assigned tag instead).
 * @param args.modifyParams - a `ModifyOrderParams` object; any `baseAssetAmount`/`price`/`triggerPrice` overrides use BASE_PRECISION (1e9) / PRICE_PRECISION (1e6) respectively.
 * @param args.state - the global `State` PDA.
 * @param args.user - the `User` account whose order is modified.
 * @param args.userStats - accepted for forward-compatibility but not required by the current on-chain `modify_order` accounts.
 * @param args.authority - signer that must own or be a registered delegate of `user`.
 * @param args.remainingAccounts - oracle/market `AccountMeta[]` for the order's market.
 * @returns the unsigned `modifyOrder` `TransactionInstruction`.
 */
export async function buildModifyOrderInstruction(args: {
	program: VelocityProgram;
	orderId: number;
	modifyParams: any;
	state: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await (args.program.instruction as any).modifyOrder(
		args.orderId,
		args.modifyParams,
		{
			accounts: {
				state: args.state,
				user: args.user,
				userStats: args.userStats,
				authority: args.authority,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}

/**
 * Builds a `modifyOrderByUserId` instruction, same as `buildModifyOrderInstruction` but
 * targeting the order by its caller-assigned `userOrderId` tag.
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.userOrderId - the caller-assigned order tag (`u8`) supplied when the order was placed.
 * @param args.modifyParams - a `ModifyOrderParams` object; any `baseAssetAmount`/`price`/`triggerPrice` overrides use BASE_PRECISION (1e9) / PRICE_PRECISION (1e6) respectively.
 * @param args.state - the global `State` PDA.
 * @param args.user - the `User` account whose order is modified.
 * @param args.userStats - accepted for forward-compatibility but not required by the current on-chain `modify_order_by_user_id` accounts.
 * @param args.authority - signer that must own or be a registered delegate of `user`.
 * @param args.remainingAccounts - oracle/market `AccountMeta[]` for the order's market.
 * @returns the unsigned `modifyOrderByUserId` `TransactionInstruction`.
 */
export async function buildModifyOrderByUserIdInstruction(args: {
	program: VelocityProgram;
	userOrderId: number;
	modifyParams: any;
	state: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await (args.program.instruction as any).modifyOrderByUserId(
		args.userOrderId,
		args.modifyParams,
		{
			accounts: {
				state: args.state,
				user: args.user,
				userStats: args.userStats,
				authority: args.authority,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}
