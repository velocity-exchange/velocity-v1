import type { PublicKey, TransactionInstruction } from '@solana/web3.js';

export async function buildUpdateFundingRateInstruction(args: {
	program: any;
	perpMarketIndex: number;
	state: PublicKey;
	perpMarket: PublicKey;
	oracle: PublicKey;
}): Promise<TransactionInstruction> {
	return await (args.program.instruction as any).updateFundingRate(
		args.perpMarketIndex,
		{
			accounts: {
				state: args.state,
				perpMarket: args.perpMarket,
				oracle: args.oracle,
			},
		}
	);
}
