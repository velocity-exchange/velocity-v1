import { SYSVAR_INSTRUCTIONS_PUBKEY } from '@solana/web3.js';
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
 * @param args.remainingAccounts - writable perp market + oracle `AccountMeta[]` for the order's market, followed by any maker/referrer `(User, UserStats)` account pairs, followed by the taker's `RevenueShareEscrow` account if builder codes are enabled protocol-wide, and, when that taker is referred, the referrer's readonly `UserStats` after the escrow, followed by the quoter section (the market's `QuoterSlabV0` plus the consulted quoters' CPI accounts).
 * @param args.clobAccounts - pass the market's CLOB accounts to use `fillLegacyDlobOrder`, whose restable remainder of the filled order migrates to the book instead of resting in `User.orders`. `marketIndex` is checked against the order's own market on-chain.
 * @param args.signedRoute - must be empty. A DLOB order carries no route: only a signed message names one, and such an order routes at placement and rests any remainder on the market's CLOB, so what a route binds is the fill of that remainder rather than this call. A non-empty claim is rejected on-chain.
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
		quoterSlab: PublicKey;
		clobMarket: PublicKey;
		clobProgram: PublicKey;
		clobAuthority: PublicKey;
	};
}): Promise<TransactionInstruction> {
	if (args.clobAccounts) {
		return await (args.program.instruction as any).fillLegacyDlobOrder(
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
					quoterSlab: args.clobAccounts.quoterSlab,
					clobMarket: args.clobAccounts.clobMarket,
					clobProgram: args.clobAccounts.clobProgram,
					clobAuthority: args.clobAccounts.clobAuthority,
					// Always named. A fill that leaves a book short of an owner
					// is refused unless velocity can count the transaction's
					// accounts, and only this sysvar tells it. It costs one
					// lock; being refused costs the whole fill.
					instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
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
				// Always named, as on the v1 route: a fill that leaves a book
				// short of an owner is refused unless velocity can count the
				// transaction's accounts, and only this sysvar tells it.
				instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
			},
			remainingAccounts: args.remainingAccounts,
		}
	);
}
