import type {
	AccountMeta,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';

export async function buildPlacePerpOrderInstruction(args: {
	program: any;
	orderParams: any;
	state: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await (args.program.instruction as any).placePerpOrder(
		args.orderParams,
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

export async function buildPlaceAndTakePerpOrderInstruction(args: {
	program: any;
	orderParams: any;
	optionalParams: number | null;
	state: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await args.program.instruction.placeAndTakePerpOrder(
		args.orderParams,
		args.optionalParams,
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

export async function buildPlaceAndMakePerpOrderInstruction(args: {
	program: any;
	orderParams: any;
	takerOrderId: number;
	state: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	taker: PublicKey;
	takerStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await args.program.instruction.placeAndMakePerpOrder(
		args.orderParams,
		args.takerOrderId,
		{
			accounts: {
				state: args.state,
				user: args.user,
				userStats: args.userStats,
				taker: args.taker,
				takerStats: args.takerStats,
				authority: args.authority,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}

export async function buildCancelOrderInstruction(args: {
	program: any;
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

export async function buildCancelOrderByUserIdInstruction(args: {
	program: any;
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

export async function buildCancelOrdersByIdsInstruction(args: {
	program: any;
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

export async function buildModifyOrderInstruction(args: {
	program: any;
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

export async function buildModifyOrderByUserIdInstruction(args: {
	program: any;
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
