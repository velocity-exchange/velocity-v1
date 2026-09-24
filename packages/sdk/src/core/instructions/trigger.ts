import { SYSVAR_INSTRUCTIONS_PUBKEY } from '@solana/web3.js';
import type {
	AccountMeta,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';
import type { VelocityProgram } from '../../config';
import type { ClobAccounts } from '../../types';

/**
 * Builds a `triggerMarketOrderV1` instruction, which fires an armed stop-market straight
 * to the book. It fills the order in the same instruction and rests any restable
 * remainder as a taker-origin order on the CLOB, so nothing stays live in `User.orders`;
 * `triggerOrder` does neither. The instruction is permissionless: `user`, the order
 * owner, does not sign, only `authority`, the owner or delegate of `filler`. Handles
 * trigger-market orders only; a trigger-limit uses `triggerLimitOrderV1`.
 * @param args.userStats writable; checked for the authority-wide equity breaker, as in
 *   `triggerOrder`.
 * @param args.authority signer that must own or be a registered delegate of `filler`.
 * @param args.remainingAccounts the writable perp market and oracle `AccountMeta[]` for
 *   the order's market, then the maker and referrer `(User, UserStats)` pairs, then the
 *   taker's `RevenueShareEscrow` if builder codes are enabled, then the referrer's
 *   read-only `UserStats` when the taker is referred, then the market's `QuoterSlabV0`
 *   plus the consulted quoters' CPI accounts.
 * @param args.signedRoute must be empty. A trigger carries no signed route; the keeper
 *   answers for its account list through the filler obligation.
 * @param args.crankConditions the market's crank-conditions PDA, holding the wake hint.
 *   Omit to pass the program id as a placeholder.
 * @param args.triggerConditions the user's relay trigger conditions PDA. Omit to pass the
 *   program id as a placeholder.
 */
export async function buildTriggerMarketOrderV1Instruction(
	args: {
		program: VelocityProgram;
		marketIndex: number;
		orderId: number;
		state: PublicKey;
		filler: PublicKey;
		fillerStats: PublicKey;
		user: PublicKey;
		userStats: PublicKey;
		authority: PublicKey;
		remainingAccounts: AccountMeta[];
		signedRoute?: PublicKey[];
		crankConditions?: PublicKey;
		triggerConditions?: PublicKey;
	} & ClobAccounts
): Promise<TransactionInstruction> {
	// An omitted optional account is encoded as the program id, which is anchor's
	// `None`.
	const omitted = args.program.programId;
	return await (args.program.instruction as any).triggerMarketOrderV1(
		{
			marketIndex: args.marketIndex,
			orderId: args.orderId,
			signedRoute: args.signedRoute ?? [],
		},

		{
			accounts: {
				state: args.state,
				authority: args.authority,
				filler: args.filler,
				fillerStats: args.fillerStats,
				user: args.user,
				userStats: args.userStats,
				quoterSlab: args.quoterSlab,
				clobMarket: args.clobMarket,
				clobProgram: args.clobProgram,
				crankConditions: args.crankConditions ?? omitted,
				triggerConditions: args.triggerConditions ?? omitted,
				// This account is always named. A fill that leaves a book short of an
				// owner is refused unless velocity can count the transaction's accounts,
				// and a trigger crank's owner never signs, so this sysvar is the only
				// source. Costs one lock; being refused costs the whole fill.
				ixSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
			},

			remainingAccounts: args.remainingAccounts,
		}
	);
}
