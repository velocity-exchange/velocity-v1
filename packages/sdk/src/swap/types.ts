import {
	AddressLookupTableAccount,
	PublicKey,
	TransactionInstruction,
	VersionedTransaction,
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
 * What the provider says its route does, recorded when the quote was produced.
 *
 * An implementation detail of the tamper check described on
 * {@link expectProviderRoute} — you never write one of these. {@link
 * buildSwapQuote} records it from the quote a provider is already returning.
 */
export interface SwapRouteFields {
	readonly inputMint: string;
	readonly outputMint: string;
	readonly inAmount: string;
	readonly outAmount: string;
	readonly swapMode: SwapMode;
	readonly slippageBps: number;
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
export type SwapProviderRoute =
	| {
			readonly provider: 'jupiter';
			readonly quote: unknown;
			readonly routed: SwapRouteFields;
			readonly quotedFor?: string;
	  }
	| {
			readonly provider: 'titan';
			readonly route: unknown;
			readonly routed: SwapRouteFields;
			readonly quotedFor?: string;
	  };

/**
 * A quote plus the provider payload needed to execute it. Always pass the quote
 * you intend to swap on; providers will not fall back to a previous one.
 *
 * Treat it as immutable. The normalized fields are checked against the payload
 * before a swap is built, so an edited copy is rejected rather than silently
 * executed as the swap it was originally quoted for — re-quote instead.
 */
export type SwapQuote = UnifiedQuoteResponse & {
	readonly providerRoute: SwapProviderRoute;
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
	 * 
	 * Don't be confused by the Velocity user account public key, which is different.
	 * This is usually the Velocity authority.
	 */
	userPublicKey: PublicKey;
}

/**
 * The contract every swap provider implements.
 *
 * Quote, then build — either into velocity's swap bracket, or into a standalone
 * transaction. Both clients satisfying the same interface is what stops one
 * provider growing behaviour the other doesn't have; callers get identical
 * semantics regardless of which is configured.
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

	/**
	 * Builds a complete, self-contained swap transaction — the setup and
	 * teardown `getRouteInstructions` strips are still attached: compute budget,
	 * token account creation, and SOL wrapping.
	 *
	 * For swaps the caller signs and sends on its own. A swap running inside
	 * velocity's `beginSwap`/`endSwap` bracket wants `getRouteInstructions`
	 * instead — velocity supplies all three itself, and a second copy wraps SOL
	 * that nothing then unwraps.
	 *
	 * @throws If the quote came from a different provider or a different wallet.
	 */
	getSwapTransaction(
		params: GetRouteInstructionsParams
	): Promise<VersionedTransaction>;
}

/**
 * Assembles the {@link SwapQuote} a provider's `getQuote` returns, from the
 * normalized quote and the route payload that will execute it.
 *
 * Providers call this rather than building the object themselves. It records
 * what the route was quoted with next to the payload, which is what lets
 * {@link expectProviderRoute} reject a quote edited after it was returned — so
 * that check needs no cooperation from the provider beyond calling this.
 */
export function buildSwapQuote<
	Q extends UnifiedQuoteResponse,
	P extends { readonly provider: SwapClientType; readonly quotedFor?: string },
>(quote: Q, providerRoute: P): Q & SwapQuote {
	const { inputMint, outputMint, inAmount, outAmount, swapMode, slippageBps } =
		quote;

	return {
		...quote,
		// Cast: `P` is only constrained to the fields both members of
		// `SwapProviderRoute` share, so the spread can't be proven to reconstitute
		// one — the payload field it carries is the caller's to get right.
		providerRoute: {
			...providerRoute,
			routed: {
				inputMint,
				outputMint,
				inAmount,
				outAmount,
				swapMode,
				slippageBps,
			},
		} as unknown as SwapProviderRoute,
	};
}

/** Fields that must describe the same swap on the quote and on its route. */
const ROUTED_FIELDS = [
	'inputMint',
	'outputMint',
	'inAmount',
	'outAmount',
	'swapMode',
	'slippageBps',
] as const satisfies ReadonlyArray<keyof SwapRouteFields>;

/**
 * Throws unless the quote's normalized fields still describe the route it
 * carries.
 *
 * Callers and velocity's swap guards read the normalized fields; the provider
 * executes the opaque payload. Nothing but this ties the two together, so a
 * quote that was copied and edited — `{ ...quote, inAmount }`, a mint rewritten
 * in place — would pass every pair, size and slippage check and then execute the
 * route it was originally quoted for.
 */
function assertQuoteMatchesRoute(
	quote: SwapQuote,
	provider: SwapClientType,
	routed?: SwapRouteFields
): void {
	if (!routed) {
		throw new Error(
			`Quote is missing the route fields recorded at quote time. It must come from ${provider}'s getQuote, which builds it with buildSwapQuote.`
		);
	}

	for (const field of ROUTED_FIELDS) {
		// Stringified: a provider may normalize a numeric field it decoded.
		if (String(quote[field]) !== String(routed[field])) {
			throw new Error(
				`Quote reports ${field} ${String(
					quote[field]
				)} but the route it carries was quoted with ${String(
					routed[field]
				)}. The quote was modified after it was returned; re-quote instead.`
			);
		}
	}
}

/**
 * Narrows a quote's payload to `provider`, checks it can be executed by
 * `userPublicKey`, and checks it still matches the quote it travels on.
 *
 * A route is wallet-bound when the provider resolves the user's token accounts
 * at quote time (Titan does; Jupiter builds per-wallet at swap time). Those
 * providers record the wallet on `quotedFor`, and executing such a route as
 * anyone else moves funds through accounts the signer doesn't own.
 *
 * @throws If the quote was produced by a different provider, for a wallet other
 * than `userPublicKey`, or for a different pair, size, mode or slippage than the
 * quote now claims.
 */
export function expectProviderRoute<T extends SwapClientType>(
	quote: SwapQuote,
	provider: T,
	userPublicKey: PublicKey
): Extract<SwapProviderRoute, { provider: T }> {
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

	if (route.quotedFor && route.quotedFor !== userPublicKey.toString()) {
		throw new Error(
			`Quote was requested for ${
				route.quotedFor
			} but is being swapped by ${userPublicKey.toString()}.`
		);
	}

	assertQuoteMatchesRoute(quote, provider, route.routed);

	return route as Extract<SwapProviderRoute, { provider: T }>;
}
