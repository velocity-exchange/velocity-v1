import type {
	AccountMeta,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';
import type { VelocityProgram } from '../../config';

/**
 * Builds a `fillPerpOrder` instruction, matching a resting perp order against the AMM
 * and/or the supplied maker accounts. Permissionless: `user` (the order owner) does not
 * need to sign — only `authority` (owner/delegate of `filler`) does. Hardcodes the
 * on-chain `makerOrderId` arg to `null` (no specific maker order is targeted; the
 * program matches from whichever maker/referrer accounts are supplied).
 * @param args.program - Anchor `Program<Velocity>` used to build the instruction.
 * @param args.orderId - the order to fill, or `null` to fill `user`'s most recently placed order (resolved on-chain via `get_last_order_id`).
 * @param args.state - the global `State` PDA.
 * @param args.filler - the keeper's `User` account that earns the filler reward.
 * @param args.fillerStats - the filler's `UserStats` PDA.
 * @param args.user - the order owner's `User` account (the taker being filled).
 * @param args.userStats - the taker's `UserStats` PDA.
 * @param args.authority - signer that must own or be a registered delegate of `filler`.
 * @param args.remainingAccounts - writable perp market + oracle `AccountMeta[]` for the order's market, followed by any maker/referrer `(User, UserStats)` account pairs, followed by the taker's `RevenueShareEscrow` account if builder codes are enabled protocol-wide, followed by the quoter section (each `QuoterV0` entry plus the accounts its CPI resolves against).
 * @param args.clobAccounts - pass the market's CLOB accounts to use `fillPerpOrderV1`, whose restable remainder of the filled order migrates to the book instead of resting in `User.orders`. `crankConditions` is optional (it only maintains the crank wake hint); `marketIndex` is required by the conditions PDA seed and is checked against the order's own market on-chain.
 * @param args.signedRoute - the `QuoterV0` entries the order's signer chose, as read off their signed message. Checked on-chain against the digest the order carries, and every entry must appear in `remainingAccounts` — a filler cannot drop a quoter the taker picked. Omit (or pass `[]`) for an order placed without a signed route.
 * @returns the unsigned `fillPerpOrder` `TransactionInstruction`.
 */
export async function buildFillPerpOrderInstruction(args: {
	program: VelocityProgram;
	orderId: number | null;
	state: PublicKey;
	filler: PublicKey;
	fillerStats: PublicKey;
	user: PublicKey;
	userStats: PublicKey;
	authority: PublicKey;
	remainingAccounts: AccountMeta[];
	signedRoute?: PublicKey[];
	clobAccounts?: {
		marketIndex: number;
		quoter: PublicKey;
		clobMarket: PublicKey;
		clobProgram: PublicKey;
		clobAuthority: PublicKey;
		crankConditions?: PublicKey;
	};
}): Promise<TransactionInstruction> {
	if (args.clobAccounts) {
		// An omitted `Option` account is encoded as the program id, which the
		// program decodes as `None`.
		const omitted = args.program.programId;
		return await (args.program.instruction as any).fillPerpOrderV1(
			args.orderId,
			null,
			args.signedRoute ?? [],
			args.clobAccounts.marketIndex,
			{
				accounts: {
					state: args.state,
					authority: args.authority,
					filler: args.filler,
					fillerStats: args.fillerStats,
					user: args.user,
					userStats: args.userStats,
					quoter: args.clobAccounts.quoter,
					clobMarket: args.clobAccounts.clobMarket,
					clobProgram: args.clobAccounts.clobProgram,
					clobAuthority: args.clobAccounts.clobAuthority,
					crankConditions: args.clobAccounts.crankConditions ?? omitted,
				},
				remainingAccounts: args.remainingAccounts,
			}
		);
	}
	return await (args.program.instruction as any).fillPerpOrder(
		args.orderId,
		null,
		args.signedRoute ?? [],
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
