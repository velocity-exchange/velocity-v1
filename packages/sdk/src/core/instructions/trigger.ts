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
}): Promise<TransactionInstruction> {
	return await (args.program.instruction as any).triggerOrder(args.orderId, {
		accounts: {
			state: args.state,
			filler: args.filler,
			user: args.user,
			userStats: args.userStats,
			authority: args.authority,
		},
		remainingAccounts: args.remainingAccounts,
	});
}
