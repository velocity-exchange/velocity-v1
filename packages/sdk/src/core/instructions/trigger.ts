import { SYSVAR_INSTRUCTIONS_PUBKEY } from '@solana/web3.js';
import type {
	AccountMeta,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';
import type { VelocityProgram } from '../../config';

/**
 * Builds a `triggerMarketOrderV1` instruction, which fires an armed
 * stop-market straight to the book. It fills the fired order in the same
 * instruction and rests any restable remainder as a taker-origin order on the
 * market's CLOB, so nothing stays live in `User.orders`. `triggerOrder` does
 * neither. The instruction is permissionless. `user`, the order owner, does not
 * sign, and only `authority`, the owner or delegate of `filler`, signs. It
 * handles trigger-market orders only. A trigger-limit uses
 * `triggerLimitOrderV1`.
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.marketIndex - the order's perp market. It is checked on chain and is
 *   a seed of the crank-conditions PDA.
 * @param args.orderId - the trigger order's on-chain order ID.
 * @param args.state - the global `State` PDA.
 * @param args.filler - the keeper's `User` account that earns the filler reward.
 * @param args.fillerStats - the filler's `UserStats` PDA.
 * @param args.user - the order owner's `User` account.
 * @param args.userStats - the order owner's `UserStats` account, writable. It is
 *   checked for the authority-wide equity breaker, as in `triggerOrder`.
 * @param args.authority - signer that must own or be a registered delegate of `filler`.
 * @param args.quoterSlab - the market's quoter slab (`QuoterSlabV0`).
 * @param args.clobMarket - the CLOB market account.
 * @param args.clobProgram - the CLOB program id.
 * @param args.remainingAccounts - the writable perp market and oracle
 *   `AccountMeta[]` for the order's market, then the maker and referrer
 *   `(User, UserStats)` pairs, then the taker's `RevenueShareEscrow` if builder codes
 *   are enabled, then the referrer's read-only `UserStats` when the taker is referred,
 *   then the quoter section. The quoter section is the market's `QuoterSlabV0` plus the
 *   consulted quoters' CPI accounts.
 * @param args.signedRoute - must be empty. A trigger carries no signed route.
 *   The keeper answers for its account list through the filler obligation.
 * @param args.crankConditions - the market's crank-conditions PDA, which holds the
 *   wake hint. Omit it to pass the program id as a placeholder.
 * @param args.triggerConditions - the user's relay trigger conditions PDA. Omit it to
 *   pass the program id as a placeholder.
 * @returns the unsigned `triggerMarketOrderV1` `TransactionInstruction`.
 */
export async function buildTriggerMarketOrderV1Instruction(args: {
	program: VelocityProgram;
	marketIndex: number;
	orderId: number;
	state: PublicKey;
	filler: PublicKey;
	fillerStats: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	quoterSlab: PublicKey;
	clobMarket: PublicKey;
	clobProgram: PublicKey;
	remainingAccounts: AccountMeta[];
	signedRoute?: PublicKey[];
	crankConditions?: PublicKey;
	triggerConditions?: PublicKey;
}): Promise<TransactionInstruction> {
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
				// This account is always named. A fill that leaves a book short
				// of an owner is refused unless velocity can count the
				// transaction's accounts, and a trigger crank's owner never
				// signs, so this sysvar is the only source. It costs one lock,
				// and being refused costs the whole fill.
				ixSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}
