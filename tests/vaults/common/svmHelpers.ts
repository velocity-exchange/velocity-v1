import { BN } from '@coral-xyz/anchor';
import { LAMPORTS_PER_SOL, PublicKey } from '@solana/web3.js';
import {
	getBalance,
	PerpMarketAccount,
	SpotBalanceType,
	SpotMarketAccount,
	UserAccount,
	VelocityClient,
} from '@velocity-exchange/sdk';
import { LiteSVMContextWrapper } from './litesvmConnection';

// Little-endian u64 reader used by the shares-accounting assertions. Ported
// verbatim from velocity-vaults' tests/common/svmHelpers.ts.
export function readUnsignedBigInt64LE(buffer: Buffer, offset: number): BN {
	return new BN(buffer.subarray(offset, offset + 8), 10, 'le');
}

/**
 * Encodes an account with its own borsh layout into a buffer sized for it,
 * bypassing `coder.accounts.encode`'s fixed 1000-byte scratch buffer, which
 * overruns for a busy `PerpMarket` or a `User` (around 4.5KB decoded).
 */
function encodeOversizedAccount(
	velocityClient: VelocityClient,
	accountName: string,
	account: unknown
): Buffer {
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	const accountsCoder = velocityClient.program.coder.accounts as any;
	const { discriminator, layout } =
		accountsCoder.accountLayouts.get(accountName);
	const buffer = Buffer.alloc(8192);
	const len = layout.encode(account, buffer);
	return Buffer.concat([Buffer.from(discriminator), buffer.subarray(0, len)]);
}

/**
 * Overwrites a velocity user account on LiteSVM with the given decoded data.
 * Seeds a position directly where LiteSVM cannot execute the order-fill path
 * that would otherwise create it (see the `Long SOL-PERP` skip reason).
 */
export async function overWriteUser(
	velocityClient: VelocityClient,
	svmContextWrapper: LiteSVMContextWrapper,
	userKey: PublicKey,
	user: UserAccount
): Promise<void> {
	svmContextWrapper.context.setAccount(userKey, {
		executable: false,
		owner: velocityClient.program.programId,
		lamports: LAMPORTS_PER_SOL,
		data: encodeOversizedAccount(velocityClient, 'user', user),
	});
}

/**
 * Increases a perp market's pnl pool so `settle_pnl` can pay out a fixture
 * position's profit; nothing traded to put a loser's fee in there instead.
 */
export async function fundPerpMarketPnlPool(params: {
	velocityClient: VelocityClient;
	svmContextWrapper: LiteSVMContextWrapper;
	perpMarketKey: PublicKey;
	perpMarket: PerpMarketAccount;
	spotMarket: SpotMarketAccount;
	tokenAmount: BN;
}): Promise<void> {
	const {
		velocityClient,
		svmContextWrapper,
		perpMarketKey,
		perpMarket,
		spotMarket,
		tokenAmount,
	} = params;
	perpMarket.pnlPool.scaledBalance = perpMarket.pnlPool.scaledBalance.add(
		getBalance(tokenAmount, spotMarket, SpotBalanceType.DEPOSIT)
	);

	svmContextWrapper.context.setAccount(perpMarketKey, {
		executable: false,
		owner: velocityClient.program.programId,
		lamports: LAMPORTS_PER_SOL,
		data: encodeOversizedAccount(velocityClient, 'perpMarket', perpMarket),
	});
}
