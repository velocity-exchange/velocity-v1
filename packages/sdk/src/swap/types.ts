import {
	AddressLookupTableAccount,
	PublicKey,
	TransactionInstruction,
	VersionedTransaction,
} from '@solana/web3.js';
import { BN } from '../isomorphic/anchor';

export type SwapMode = 'ExactIn' | 'ExactOut';
export type SwapClientType = 'jupiter' | 'titan';

/** Account budget used when a caller does not specify one. */
export const DEFAULT_SWAP_MAX_ACCOUNTS = 50;

/**
 * Quote fields shared by every provider. A provider-specific extra is optional.
 * Building a swap must never require one. See {@link SwapQuote}.
 */
export interface UnifiedQuoteResponse {
	inputMint: string;
	inAmount: string;
	outputMint: string;
	outAmount: string;
	swapMode: SwapMode;
	/**
	 * Slippage the route was quoted at. {@link SwapProvider.getRouteInstructions}
	 * takes no override, so a different slippage needs a new quote.
	 */
	slippageBps: number;
	routePlan: Array<{ swapInfo: any; percent: number }>;

	/** Jupiter only. */
	otherAmountThreshold?: string;
	/** Jupiter provides this. Titan does not, so a caller derives it. */
	priceImpactPct?: string;
	platformFee?: { amount?: string; feeBps?: number };
	contextSlot?: number;
	timeTaken?: number;
	error?: string;
	errorCode?: string;
}

/**
 * What the provider says its route does, recorded at quote time by
 * {@link buildSwapQuote}. Used by the tamper check in {@link expectProviderRoute}.
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
 * Rides on the quote, not the client, so a provider never replays a stale
 * route. See {@link SwapQuote} for the normalized fields.
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
 * A {@link SwapProviderRoute} without the `routed` fields. {@link buildSwapQuote}
 * records those itself.
 */
export type SwapProviderRoutePayload = OmitRouted<SwapProviderRoute>;

/** Distributes over the union, so each member keeps its own payload field. */
type OmitRouted<R> = R extends unknown ? Omit<R, 'routed'> : never;

/**
 * A quote and the provider payload that executes it, treated as immutable.
 * An edited copy fails the payload check and is rejected rather than run.
 */
export type SwapQuote = UnifiedQuoteResponse & {
	readonly providerRoute: SwapProviderRoute;
};

export interface SwapQuoteParams {
	inputMint: PublicKey;
	outputMint: PublicKey;
	amount: BN;
	/**
	 * Required by Titan, which bakes the user's token accounts into the route,
	 * and by Jupiter's v2 API, which builds the route for one `taker`.
	 */
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
	 * Wallet the swap executes as, not the Velocity user account public key.
	 * Usually the Velocity authority. See {@link expectProviderRoute}.
	 */
	userPublicKey: PublicKey;
}

/**
 * A caller quotes, then builds either velocity's swap bracket or a standalone
 * transaction through one shared interface. Provider-specific fields live on
 * {@link SwapQuoteParams}, never on {@link GetRouteInstructionsParams}.
 */
export interface SwapProvider {
	readonly providerName: SwapClientType;

	getQuote(params: SwapQuoteParams): Promise<SwapQuote>;

	/**
	 * Builds route instructions for a quote from `getQuote`, at its priced slippage.
	 * @throws If the quote came from a different provider or wallet.
	 */
	getRouteInstructions(
		params: GetRouteInstructionsParams
	): Promise<SwapRouteInstructions>;

	/**
	 * Keeps the setup and teardown {@link getRouteInstructions} strips.
	 * Do not call it inside velocity's bracket. It double-wraps SOL that nothing unwraps.
	 */
	getSwapTransaction(
		params: GetRouteInstructionsParams
	): Promise<VersionedTransaction>;
}

/**
 * Assemble the {@link SwapQuote} a provider's `getQuote` returns, from the
 * normalized quote and the route payload that executes it.
 *
 * A provider calls this rather than building the object itself. It records what
 * the route was quoted with next to the payload. That record lets
 * {@link expectProviderRoute} reject a quote edited after it was returned, so
 * the check needs nothing else from the provider.
 */
export function buildSwapQuote<Q extends UnifiedQuoteResponse>(
	quote: Q,
	providerRoute: SwapProviderRoutePayload
): Q & SwapQuote {
	const { inputMint, outputMint, inAmount, outAmount, swapMode, slippageBps } =
		quote;

	const routed: SwapRouteFields = {
		inputMint,
		outputMint,
		inAmount,
		outAmount,
		swapMode,
		slippageBps,
	};

	return {
		...quote,
		providerRoute: { ...providerRoute, routed },
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
 * Throw unless the quote's normalized fields still describe the route it
 * carries.
 *
 * A caller and velocity's swap guards read the normalized fields. The provider
 * executes the opaque payload. This check is the only thing that ties the two
 * together. Without it, a copied and edited quote such as
 * `{ ...quote, inAmount }` would pass every pair, size and slippage check and
 * then execute the route it was originally quoted for.
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
		// Compare as strings, because a provider may normalize a numeric field it
		// decoded.
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
 * Narrow a quote's payload to `provider`, check that `userPublicKey` can execute
 * it, and check that it still matches the quote it travels on.
 *
 * A route is wallet-bound when the provider builds it for one wallet at quote
 * time. Titan does, and so does Jupiter's v2 API. Those providers record the
 * wallet on `quotedFor`. Executing such a route as anyone else moves funds
 * through accounts the signer does not own.
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
