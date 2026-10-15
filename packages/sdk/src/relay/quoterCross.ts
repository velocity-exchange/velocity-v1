/** A Custom quoter's relay cross-discovery conditions. Relay cranks no cross
 * against the entry until this block exists and a watch names it. */
import { BN } from '@coral-xyz/anchor';
import {
	PublicKey,
	SystemProgram,
	SYSVAR_RENT_PUBKEY,
	TransactionInstruction,
} from '@solana/web3.js';
import {
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getQuoterCrossConditionsPublicKey,
	getQuoterSlabPublicKey,
	getVelocityStateAccountPublicKey,
} from '../addresses/pda';

/** `QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS`. Only the maker's authority may
 * attach with another poll interval. */
export const QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS = 1_500;

/** Any anchor client of the velocity program, typed from the IDL or not. */
export type QuoterCrossProgram = {
	programId: PublicKey;
	instruction: Record<string, (...args: any[]) => TransactionInstruction>;
};

export type QuoterCrossAttach = {
	payer: PublicKey;
	quoter: PublicKey;
	marketIndex: number;
	expireFallbackSlots?: number;
};

/** Create or re-price `attach.quoter`'s cross conditions. The instruction is
 * permissionless, and the payer pays the rent. */
export async function getInitializeQuoterCrossConditionsIx(
	program: QuoterCrossProgram,
	attach: QuoterCrossAttach
): Promise<TransactionInstruction> {
	const programId = program.programId;
	return await program.instruction.initializeQuoterCrossConditions(
		{
			expireFallbackSlots: new BN(
				attach.expireFallbackSlots ?? QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS
			),
		},
		{
			accounts: {
				payer: attach.payer,
				state: await getVelocityStateAccountPublicKey(programId),
				quoter: attach.quoter,
				perpMarket: getPerpMarketPublicKeySync(programId, attach.marketIndex),
				quoterSlab: getQuoterSlabPublicKey(programId, attach.marketIndex),
				marketConditions: getClobCrankConditionsPublicKey(
					programId,
					attach.marketIndex
				),

				crossConditions: getQuoterCrossConditionsPublicKey(
					programId,
					attach.quoter
				),

				rent: SYSVAR_RENT_PUBKEY,
				systemProgram: SystemProgram.programId,
			},
		}
	);
}
