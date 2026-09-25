import { AnchorProvider, BN, IdlAccounts, Program } from '@coral-xyz/anchor';
import { Connection, PublicKey, TransactionInstruction } from '@solana/web3.js';

import { ProtocolRevenueRouter } from './types/protocol_revenue_router';
import routerIDL from './idl/protocol_revenue_router.json';

export const IDL = routerIDL as ProtocolRevenueRouter;

export const PROTOCOL_REVENUE_ROUTER_PROGRAM_ID = new PublicKey(IDL.address);
export const DFX_REDEMPTION_PROGRAM_ID = new PublicKey(
	'rdemKHu2ueeKkhwmM2GfJFaqD3zsrj7s3oGMN3dQMJT'
);

export const MAINNET_USDT_MINT = new PublicKey(
	'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB'
);

const SYSTEM_PROGRAM_ID = new PublicKey('11111111111111111111111111111111');
const TOKEN_PROGRAM_ID = new PublicKey(
	'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
);
const ASSOCIATED_TOKEN_PROGRAM_ID = new PublicKey(
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
);

export type RouterConfigAccount =
	IdlAccounts<ProtocolRevenueRouter>['routerConfig'];

/** One rung of the marginal ladder: `threshold` is the day's cumulative gross
 *  (USDT base units) at which `poolBps` starts to apply. */
export type Tier = { threshold: BN; poolBps: number };

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
			systemProgram: SYSTEM_PROGRAM_ID,
		})
		.instruction();
}

/** One-time creation of the RouterConfig singleton. On mainnet `payer` must be
 *  the init authority baked into the program. */
export async function getInitializeIx(args: {
	connection: Connection;
	payer: PublicKey;
	admin: PublicKey;
	cranker: PublicKey;
	treasury: PublicKey;
	usdtMint: PublicKey;
	tiers: Tier[];
}): Promise<TransactionInstruction> {
	return await getRevenueRouterProgram({
		connection: args.connection,
	} as AnchorProvider)
		.methods.initialize(args.tiers)
		.accountsStrict({
			config: getRouterConfigPda(),
			usdtMint: args.usdtMint,
			redemptionConfig: getRedemptionConfigPda(),
			admin: args.admin,
			cranker: args.cranker,
			treasury: args.treasury,
			payer: args.payer,
			systemProgram: SYSTEM_PROGRAM_ID,
		})
		.instruction();
}

/** Admin-only. Every field left undefined keeps its stored value. */
export async function getUpdateConfigIx(args: {
	connection: Connection;
	admin: PublicKey;
	newAdmin?: PublicKey;
	newCranker?: PublicKey;
	newTreasury?: PublicKey;
	tiers?: Tier[];
}): Promise<TransactionInstruction> {
	return await getRevenueRouterProgram({
		connection: args.connection,
	} as AnchorProvider)
		.methods.updateConfig(args.tiers ?? null)
		.accountsStrict({
			config: getRouterConfigPda(),
			admin: args.admin,
			redemptionConfig: getRedemptionConfigPda(),
			newAdmin: args.newAdmin ?? null,
			newCranker: args.newCranker ?? null,
			newTreasury: args.newTreasury ?? null,
		})
		.instruction();
}

export type { ProtocolRevenueRouter };
