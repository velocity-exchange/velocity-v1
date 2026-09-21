import { AnchorProvider, IdlAccounts, Program } from '@coral-xyz/anchor';
import { Connection, PublicKey, TransactionInstruction } from '@solana/web3.js';

import { ProtocolRevenueRouter } from './types/protocol_revenue_router';
import routerIDL from './idl/protocol_revenue_router.json';

export const IDL = routerIDL as ProtocolRevenueRouter;

export const PROTOCOL_REVENUE_ROUTER_PROGRAM_ID = new PublicKey(IDL.address);
export const DFX_REDEMPTION_PROGRAM_ID = new PublicKey(
	'rdemKHu2ueeKkhwmM2GfJFaqD3zsrj7s3oGMN3dQMJT'
);

const TOKEN_PROGRAM_ID = new PublicKey(
	'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
);
const ASSOCIATED_TOKEN_PROGRAM_ID = new PublicKey(
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
);

export type RouterConfigAccount =
	IdlAccounts<ProtocolRevenueRouter>['routerConfig'];

export function getRouterConfigPda(): PublicKey {
	return PublicKey.findProgramAddressSync(
		[Buffer.from('router_config')],
		PROTOCOL_REVENUE_ROUTER_PROGRAM_ID
	)[0];
}

export function getRedemptionConfigPda(): PublicKey {
	return PublicKey.findProgramAddressSync(
		[Buffer.from('config')],
		DFX_REDEMPTION_PROGRAM_ID
	)[0];
}

export function getRedemptionLedgerPda(): PublicKey {
	return PublicKey.findProgramAddressSync(
		[Buffer.from('contribution_ledger')],
		DFX_REDEMPTION_PROGRAM_ID
	)[0];
}

export function getAssociatedTokenAddress(
	mint: PublicKey,
	owner: PublicKey
): PublicKey {
	return PublicKey.findProgramAddressSync(
		[owner.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), mint.toBuffer()],
		ASSOCIATED_TOKEN_PROGRAM_ID
	)[0];
}

export function getRevenueRouterProgram(
	provider: AnchorProvider
): Program<ProtocolRevenueRouter> {
	return new Program<ProtocolRevenueRouter>(IDL, provider);
}

export async function fetchRouterConfig(
	connection: Connection
): Promise<RouterConfigAccount | null> {
	// Reads only, so a connection-only provider is enough; no wallet needed.
	const program = getRevenueRouterProgram({ connection } as AnchorProvider);
	return await program.account.routerConfig.fetchNullable(getRouterConfigPda());
}

export async function getDistributeIx(args: {
	connection: Connection;
	cranker: PublicKey;
	payer?: PublicKey;
}): Promise<TransactionInstruction> {
	const { connection, cranker } = args;
	const payer = args.payer ?? cranker;

	const config = await fetchRouterConfig(connection);
	if (config === null) {
		throw new Error('RouterConfig is not initialized');
	}

	const routerConfig = getRouterConfigPda();
	const redemptionConfig = getRedemptionConfigPda();

	return await getRevenueRouterProgram({ connection } as AnchorProvider)
		.methods.distribute()
		.accountsStrict({
			config: routerConfig,
			cranker,
			usdtMint: config.usdtMint,
			routerAta: getAssociatedTokenAddress(config.usdtMint, routerConfig),
			treasury: config.treasury,
			treasuryAta: getAssociatedTokenAddress(config.usdtMint, config.treasury),
			payer,
			redemptionConfig,
			redemptionLedger: getRedemptionLedgerPda(),
			// The redemption program creates its vault as the config PDA's ATA and
			// has no setter for it, so it is derivable rather than fetched.
			redemptionVault: getAssociatedTokenAddress(
				config.usdtMint,
				redemptionConfig
			),
			dfxRedemptionProgram: DFX_REDEMPTION_PROGRAM_ID,
			tokenProgram: TOKEN_PROGRAM_ID,
			associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
			systemProgram: new PublicKey('11111111111111111111111111111111'),
		})
		.instruction();
}

export type { ProtocolRevenueRouter };
