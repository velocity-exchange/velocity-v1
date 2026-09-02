import { SYSVAR_INSTRUCTIONS_PUBKEY } from '@solana/web3.js';
import type {
	AccountMeta,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';
import type { VelocityProgram } from '../../config';

/**
 * Builds a `triggerOrder` instruction, flipping a resting trigger order (stop-loss /
 * take-profit) into a fillable order once its trigger condition is met against the
 * oracle price. Permissionless: `user` (the order owner) does not need to sign — only
 * `authority` (owner/delegate of `filler`) does. Does not fill the order itself; a
 * subsequent `fillPerpOrder` (or a `placeAndTake`) is needed to execute it.
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.orderId - the trigger order's on-chain order ID.
 * @param args.state - the global `State` PDA.
 * @param args.filler - the keeper's `User` account submitting the trigger.
 * @param args.user - the order owner's `User` account.
 * @param args.userStats - the order owner's `UserStats` account, writable (checked for
 *   the authority-wide equity breaker; a risk-increasing trigger order is cancelled
 *   rather than activated while the breaker is tripped, and a cancel that observes the
 *   owner below its raw equity floor arms the breaker inline).
 * @param args.authority - signer that must own or be a registered delegate of `filler`.
 * @param args.remainingAccounts - oracle/market `AccountMeta[]` for the order's market.
 * @returns the unsigned `triggerOrder` `TransactionInstruction`.
 */
export async function buildTriggerOrderInstruction(args: {
	program: VelocityProgram;
	orderId: number;
	state: PublicKey;
	filler: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
	/** the user's relay trigger conditions PDA; omitted = program-id placeholder */
	triggerConditions?: PublicKey;
	/** the market's crank-conditions PDA (reservoir); omitted = placeholder */
	crankConditions?: PublicKey;
}): Promise<TransactionInstruction> {
	// Omitted optional relay accounts (the user's trigger conditions and the
	// market's crank conditions) encode as the program id — anchor's `None`.
	const omitted = args.program.programId;
	return await (args.program.instruction as any).triggerOrder(args.orderId, {
		accounts: {
			state: args.state,
			filler: args.filler,
			user: args.user,
			userStats: args.userStats,
			authority: args.authority,
			triggerConditions: args.triggerConditions ?? omitted,
			crankConditions: args.crankConditions ?? omitted,
		},
		remainingAccounts: args.remainingAccounts,
	});
}

/**
 * Builds a `triggerMarketOrderV1` instruction, firing a resting DLOB stop-market
 * straight to the book. Unlike `triggerOrder`, it fills the fired order in the
 * same instruction and rests any restable remainder as a taker-origin order on
 * the market's CLOB, so nothing lingers live in `User.orders`. Permissionless:
 * `user` (the order owner) does not sign — only `authority` (owner/delegate of
 * `filler`) does. For DLOB trigger-market orders only; a trigger-limit uses
 * `triggerLimitOrderV1`.
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.marketIndex - the order's perp market; checked on-chain and used by the crank-conditions PDA seed.
 * @param args.orderId - the trigger order's on-chain order ID.
 * @param args.state - the global `State` PDA.
 * @param args.filler - the keeper's `User` account that earns the filler reward.
 * @param args.fillerStats - the filler's `UserStats` PDA.
 * @param args.user - the order owner's `User` account.
 * @param args.userStats - the order owner's `UserStats` account, writable (checked for the authority-wide equity breaker, as in `triggerOrder`).
 * @param args.authority - signer that must own or be a registered delegate of `filler`.
 * @param args.quoter - the market's CLOB registry entry (`QuoterV0`).
 * @param args.clobMarket - the CLOB market account.
 * @param args.clobProgram - the CLOB program id.
 * @param args.clobAuthority - the CLOB place-authority PDA.
 * @param args.remainingAccounts - writable perp market + oracle `AccountMeta[]` for the order's market, followed by maker/referrer `(User, UserStats)` pairs, the taker's `RevenueShareEscrow` if builder codes are enabled, the referrer's readonly `UserStats` when referred, then the quoter section (each `QuoterV0` entry plus its CPI accounts).
 * @param args.signedRoute - must be empty. A DLOB trigger carries no signed route; the keeper answers for its account list through the filler obligation.
 * @param args.crankConditions - the market's crank-conditions PDA (wake hint); omitted = program-id placeholder.
 * @param args.triggerConditions - the user's relay trigger conditions PDA; omitted = placeholder.
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
	quoter: PublicKey;
	clobMarket: PublicKey;
	clobProgram: PublicKey;
	clobAuthority: PublicKey;
	remainingAccounts: AccountMeta[];
	signedRoute?: PublicKey[];
	crankConditions?: PublicKey;
	triggerConditions?: PublicKey;
}): Promise<TransactionInstruction> {
	// An omitted optional account is encoded as the program id — anchor's `None`.
	const omitted = args.program.programId;
	return await (args.program.instruction as any).triggerMarketOrderV1(
		args.marketIndex,
		args.orderId,
		args.signedRoute ?? [],
		{
			accounts: {
				state: args.state,
				authority: args.authority,
				filler: args.filler,
				fillerStats: args.fillerStats,
				user: args.user,
				userStats: args.userStats,
				quoter: args.quoter,
				clobMarket: args.clobMarket,
				clobProgram: args.clobProgram,
				clobAuthority: args.clobAuthority,
				crankConditions: args.crankConditions ?? omitted,
				triggerConditions: args.triggerConditions ?? omitted,
				// Always named. A fill that leaves a book short of an owner is
				// refused unless velocity can count the transaction's accounts,
				// and a trigger crank's owner never signs, so this sysvar is the
				// only source. It costs one lock; being refused costs the fill.
				ixSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}
