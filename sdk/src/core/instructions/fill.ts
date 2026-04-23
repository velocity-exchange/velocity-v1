import type {
	AccountMeta,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';

export async function buildFillPerpOrderInstruction(args: {
	program: any;
	orderId: number | null;
	state: PublicKey;
	filler: PublicKey;
	fillerStats: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
}): Promise<TransactionInstruction> {
	return await (args.program.instruction as any).fillPerpOrder(
		args.orderId,
		null,
		{
			accounts: {
				state: args.state,
				filler: args.filler,
				fillerStats: args.fillerStats,
				user: args.user,
				userStats: args.userStats,
				authority: args.authority,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}
