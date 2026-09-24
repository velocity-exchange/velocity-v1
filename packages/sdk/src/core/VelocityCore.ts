import type {
	Connection,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';

import * as pdas from '../addresses/pda';
import * as constants from '../constants';
import { decodeUser } from '../decode/user';
import { CustomBorshCoder } from '../decode/customCoder';
import velocityIDL from '../idl/velocity.json';
import type { Velocity } from '../idl/velocity';
import type { UserAccount } from '../types';
import { fetchAccount } from '../accounts/fetch';
import { buildDepositInstruction } from './instructions/deposit';
import { buildWithdrawInstruction } from './instructions/withdraw';
import { buildCancelOrdersInstruction } from './instructions/orders';
import { buildTriggerMarketOrderV1Instruction } from './instructions/trigger';
import { buildSettlePnlInstruction } from './instructions/settlement';
import { buildLiquidatePerpInstruction } from './instructions/liquidation';
import { buildUpdateFundingRateInstruction } from './instructions/funding';
import {
	buildCancelOrderByUserIdInstruction,
	buildCancelOrderInstruction,
	buildCancelOrdersByIdsInstruction,
	buildModifyOrderByUserIdInstruction,
	buildModifyOrderInstruction,
	buildPlaceAndMakePerpOrderInstruction,
	buildPlaceAndTakePerpOrderInstruction,
	buildPlaceTriggerOrdersInstruction,
} from './instructions/perpOrders';
import * as remainingAccounts from './remainingAccounts';
import * as signedMsg from './signedMsg';

/**
 * Configuration passed to subscription-free `VelocityCore` helpers that need to know
 * which deployment (program id / IDL) they are building instructions against.
 */
export type VelocityCoreContext = {
	/** Velocity program id. */
	programId: PublicKey;

	/** Anchor IDL json for Velocity (defaults to bundled `idl/velocity.json`). */
	idl?: Velocity;
};

/**
 * `VelocityCore` is the subscription-free escape hatch of the SDK: a static-method
 * surface for deriving PDAs, decoding raw account buffers, building `remainingAccounts`,
 * and constructing `TransactionInstruction`s directly against a `Program<Velocity>` —
 * with no `VelocityClient` subscription, polling, or websocket state required. Every
 * method here is pure (or a single RPC call) and safe to use from a stateless backend,
 * a keeper bot, or any context where the full subscribed client is overkill.
 *
 * Transaction/instruction builders will be progressively moved here from `VelocityClient`.
 */
export class VelocityCore {
	/** Re-export of `../addresses/pda` — pure PDA derivation helpers (state, user, spot/perp market, vaults, etc). */
	static readonly pdas = pdas;

	/** Re-export of `../constants` — SDK-wide numeric precisions, market configs, and program constants. */
	static readonly constants = constants;

	/** Re-export of `./remainingAccounts` — pure builder for the oracle/market `AccountMeta[]` every trading/keeper instruction needs in `remainingAccounts`. */
	static readonly remainingAccounts = remainingAccounts;

	/** Re-export of `./signedMsg` — Borsh encode/decode helpers for Swift (signed order) message envelopes. */
	static readonly signedMsg = signedMsg;

	/** Returns the SDK's bundled Velocity Anchor IDL (`idl/velocity.json`), typed as `Velocity`. */
	static defaultIdl(): Velocity {
		return velocityIDL as unknown as Velocity;
	}

	/**
	 * Builds a `CustomBorshCoder` for the given IDL, usable to encode/decode Anchor
	 * account and type layouts without constructing a full `Program`.
	 * @param idl - IDL to build the coder from. Defaults to `VelocityCore.defaultIdl()`.
	 * @returns a coder that can (de)serialize any account/type defined in `idl`.
	 */
	static coder(idl: Velocity = VelocityCore.defaultIdl()): CustomBorshCoder {
		return new CustomBorshCoder(idl as any);
	}

	/**
	 * Decodes a raw Velocity `User` account buffer (including its 8-byte Anchor
	 * discriminator) into a `UserAccount`, without needing a `Program` or RPC call.
	 * Does not verify the discriminator — passing a buffer for a different account
	 * type will produce garbage or throw from an out-of-bounds read.
	 * @param buffer - raw on-chain account data for a `User` PDA.
	 * @returns the decoded `UserAccount`.
	 */
	static decodeUserAccount(buffer: Buffer): UserAccount {
		return decodeUser(buffer);
	}

	/**
	 * Fetches a `User` account over RPC and decodes it, with no subscription required.
	 * @param connection - RPC connection to fetch the account from.
	 * @param userAccountPublicKey - the `User` PDA's address.
	 * @returns the decoded `UserAccount`, or `null` if the account does not exist.
	 */
	static async fetchUserAccount(
		connection: Connection,
		userAccountPublicKey: PublicKey
	): Promise<UserAccount | null> {
		const data = await fetchAccount(connection, userAccountPublicKey);
		return data ? VelocityCore.decodeUserAccount(data) : null;
	}

	/** Re-export of {@link buildDepositInstruction}, which documents the arguments. */
	static readonly buildDepositInstruction = buildDepositInstruction;

	/** Re-export of {@link buildWithdrawInstruction}, which documents the arguments. */
	static readonly buildWithdrawInstruction = buildWithdrawInstruction;

	/** Re-export of {@link buildCancelOrdersInstruction}, which documents the arguments. */
	static readonly buildCancelOrdersInstruction = buildCancelOrdersInstruction;

	/** Re-export of {@link buildTriggerMarketOrderV1Instruction}, which documents the arguments. */
	static readonly buildTriggerMarketOrderV1Instruction =
		buildTriggerMarketOrderV1Instruction;

	/** Re-export of {@link buildSettlePnlInstruction}, which documents the arguments. */
	static readonly buildSettlePnlInstruction = buildSettlePnlInstruction;

	/** Re-export of {@link buildLiquidatePerpInstruction}, which documents the arguments. */
	static readonly buildLiquidatePerpInstruction = buildLiquidatePerpInstruction;

	/** Re-export of {@link buildPlaceTriggerOrdersInstruction}, which documents the arguments. */
	static readonly buildPlaceTriggerOrdersInstruction =
		buildPlaceTriggerOrdersInstruction;

	/** Re-export of {@link buildPlaceAndTakePerpOrderInstruction}, which documents the arguments. */
	static readonly buildPlaceAndTakePerpOrderInstruction =
		buildPlaceAndTakePerpOrderInstruction;

	/** Re-export of {@link buildPlaceAndMakePerpOrderInstruction}, which documents the arguments. */
	static readonly buildPlaceAndMakePerpOrderInstruction =
		buildPlaceAndMakePerpOrderInstruction;

	/** Re-export of {@link buildCancelOrderInstruction}, which documents the arguments. */
	static readonly buildCancelOrderInstruction = buildCancelOrderInstruction;

	/** Re-export of {@link buildCancelOrderByUserIdInstruction}, which documents the arguments. */
	static readonly buildCancelOrderByUserIdInstruction =
		buildCancelOrderByUserIdInstruction;

	/** Re-export of {@link buildCancelOrdersByIdsInstruction}, which documents the arguments. */
	static readonly buildCancelOrdersByIdsInstruction =
		buildCancelOrdersByIdsInstruction;

	/** Re-export of {@link buildModifyOrderInstruction}, which documents the arguments. */
	static readonly buildModifyOrderInstruction = buildModifyOrderInstruction;

	/** Re-export of {@link buildModifyOrderByUserIdInstruction}, which documents the arguments. */
	static readonly buildModifyOrderByUserIdInstruction =
		buildModifyOrderByUserIdInstruction;

	/** Re-export of {@link buildUpdateFundingRateInstruction}, which documents the arguments. */
	static readonly buildUpdateFundingRateInstruction =
		buildUpdateFundingRateInstruction;

	/**
	 * Placeholder for instruction builders.
	 *
	 * In follow-up refactors, VelocityClient methods like `getDepositInstruction`,
	 * `getPlaceOrdersIx`, etc. will be moved here as pure builders.
	 */
	static buildInstructions(
		_ctx: VelocityCoreContext
	): TransactionInstruction[] {
		return [];
	}
}
