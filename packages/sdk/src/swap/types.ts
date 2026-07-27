import {
	AddressLookupTableAccount,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';
import { BN } from '../isomorphic/anchor';

export type SwapMode = 'ExactIn' | 'ExactOut';
export type SwapClientType = 'jupiter' | 'titan';

/** Account budget assumed when a caller doesn't specify one. */
export const DEFAULT_SWAP_MAX_ACCOUNTS = 50;

/**
 * Quote fields shared by every provider. Provider-specific extras are optional
 * and must never be required to build a swap — see {@link SwapQuote}.
 */
export interface UnifiedQuoteResponse {
	inputMint: string;
	inAmount: string;
	outputMint: string;
	outAmount: string;
	swapMode: SwapMode;
	/**
	 * The slippage the route was quoted at. Authoritative — this is what the
	 * swap executes with, since {@link SwapProvider.getRouteInstructions} takes
	 * no slippage override. To swap at different slippage, quote again.
	 */
	slippageBps: number;
	routePlan: Array<{ swapInfo: any; percent: number }>;

	/** Jupiter only. */
	otherAmountThreshold?: string;
	/** Jupiter provides this; Titan doesn't, so callers derive it. */
	priceImpactPct?: string;
	platformFee?: { amount?: string; feeBps?: number };
	contextSlot?: number;
	timeTaken?: number;
	error?: string;
	errorCode?: string;
}

/**
 * Everything a provider needs to turn its own quote back into instructions.
 *
 * Attached to the quote rather than held on the client so that building a swap
 * is a pure function of the quote you were given. A provider that kept this
 * internally would silently build against whatever route it happened to have
 * cached, which is invisible at the call site and wrong whenever more than one
 * quote is in flight.
 *
 * Opaque — read the normalized fields on {@link SwapQuote} instead.
 */
export type ProviderRoute =
	| {
			readonly provider: 'jupiter';
			readonly quote: unknown;
			readonly quotedFor?: string;
	  }
	| {
			readonly provider: 'titan';
			readonly route: unknown;
			readonly quotedFor?: string;
	  };

/**
 * A quote plus the provider payload needed to execute it. Always pass the quote
 * you intend to swap on; providers will not fall back to a previous one.
 */
export type SwapQuote = UnifiedQuoteResponse & {
	readonly providerRoute: ProviderRoute;
};

export interface SwapQuoteParams {
	inputMint: PublicKey;
	outputMint: PublicKey;
	amount: BN;
	/** Required by Titan, which bakes the user's token accounts into the route. */
	userPublicKey?: PublicKey;
	maxAccounts?: number;
	slippageBps?: number;
	swapMode?: SwapMode;
	onlyDirectRoutes?: boolean;
	excludeDexes?: string[];

	/** Titan only. */
	sizeConstraint?: number;
	/** Titan only. */
	accountsLimitWritable?: number;

	/** Jupiter only. */
	autoSlippage?: boolean;
	/** Jupiter only. */
	maxAutoSlippageBps?: number;
	/** Jupiter only. */
	usdEstimate?: number;
}

export interface SwapRouteInstructions {
	instructions: TransactionInstruction[];
	lookupTables: AddressLookupTableAccount[];
}

export interface GetRouteInstructionsParams {
	quote: SwapQuote;
	/**
	 * Wallet the swap executes as. Must be the wallet the quote was requested
	 * for when the provider binds routes to a wallet — see
	 * {@link expectProviderRoute}.
	 */
	userPublicKey: PublicKey;
}

/**
 * The contract every swap provider implements.
 *
 * Deliberately two methods: quote, then build. Both clients satisfying the same
 * interface is what stops one provider growing behaviour the other doesn't
 * have — callers get identical semantics regardless of which is configured.
 *
 * Provider-specific request fields live on {@link SwapQuoteParams} and are
 * mapped by the provider itself, so adding one never means editing the unified
 * client. Nothing that only one provider can honour belongs on
 * {@link GetRouteInstructionsParams} — a build-time parameter the other silently
 * ignores is indistinguishable from it being applied.
 */
export interface SwapProvider {
	readonly providerName: SwapClientType;

	getQuote(params: SwapQuoteParams): Promise<SwapQuote>;

	/**
	 * Builds the route instructions for a quote returned by this provider's
	 * `getQuote`, at the slippage that quote was priced at.
	 * @throws If the quote came from a different provider or a different wallet.
	 */
	getRouteInstructions(
		params: GetRouteInstructionsParams
	): Promise<SwapRouteInstructions>;
}

/**
 * Narrows a quote's payload to `provider` and checks it can be executed by
 * `userPublicKey`.
 *
 * A route is wallet-bound when the provider resolves the user's token accounts
 * at quote time (Titan does; Jupiter builds per-wallet at swap time). Those
 * providers record the wallet on `quotedFor`, and executing such a route as
 * anyone else moves funds through accounts the signer doesn't own.
 *
 * @throws If the quote was produced by a different provider, or for a wallet
 * other than `userPublicKey`.
 */
export function expectProviderRoute<T extends SwapClientType>(
	quote: SwapQuote,
	provider: T,
	userPublicKey?: PublicKey
): Extract<ProviderRoute, { provider: T }> {
	const route = quote?.providerRoute;

	if (!route) {
		throw new Error(
			`Quote is missing its provider route. It must come from ${provider}'s getQuote.`
		);
	}

	if (route.provider !== provider) {
		throw new Error(
			`Quote came from ${route.provider} but is being swapped on ${provider}.`
		);
	}

	if (
		route.quotedFor &&
		userPublicKey &&
		route.quotedFor !== userPublicKey.toString()
	) {
		throw new Error(
			`Quote was requested for ${
				route.quotedFor
			} but is being swapped by ${userPublicKey.toString()}.`
		);
	}

	return route as Extract<ProviderRoute, { provider: T }>;
}
