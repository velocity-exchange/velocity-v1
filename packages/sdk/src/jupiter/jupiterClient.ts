import {
	AddressLookupTableAccount,
	Connection,
	PublicKey,
	TransactionInstruction,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import fetch, { RequestInit } from 'node-fetch';
import { filterRouteInstructions } from '../swap/routeInstructions';
import {
	DEFAULT_SWAP_MAX_ACCOUNTS,
	GetRouteInstructionsParams,
	SwapMode,
	SwapProvider,
	SwapQuote,
	SwapQuoteParams,
	SwapRouteInstructions,
	buildSwapQuote,
	expectProviderRoute,
} from '../swap/types';

export interface MarketInfo {
	id: string;
	inAmount: number;
	inputMint: string;
	label: string;
	lpFee: Fee;
	notEnoughLiquidity: boolean;
	outAmount: number;
	outputMint: string;
	platformFee: Fee;
	priceImpactPct: number;
}

export interface Fee {
	amount: number;
	mint: string;
	pct: number;
}

export interface Route {
	amount: number;
	inAmount: number;
	marketInfos: MarketInfo[];
	otherAmountThreshold: number;
	outAmount: number;
	priceImpactPct: number;
	slippageBps: number;
	swapMode: SwapMode;
}

/**
 *
 * @export
 * @interface RoutePlanStep
 */
export interface RoutePlanStep {
	/**
	 *
	 * @type {SwapInfo}
	 * @memberof RoutePlanStep
	 */
	swapInfo: SwapInfo;
	/**
	 *
	 * @type {number}
	 * @memberof RoutePlanStep
	 */
	percent: number;
}

export interface SwapInfo {
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	ammKey: string;
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	label?: string;
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	inputMint: string;
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	outputMint: string;
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	inAmount: string;
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	outAmount: string;
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	feeAmount: string;
	/**
	 *
	 * @type {string}
	 * @memberof SwapInfo
	 */
	feeMint: string;
}

/**
 *
 * @export
 * @interface PlatformFee
 */
export interface PlatformFee {
	/**
	 *
	 * @type {string}
	 * @memberof PlatformFee
	 */
	amount?: string;
	/**
	 *
	 * @type {number}
	 * @memberof PlatformFee
	 */
	feeBps?: number;
}

/**
 *
 * @export
 * @interface QuoteResponse
 */
export interface QuoteResponse {
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	inputMint: string;
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	inAmount: string;
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	outputMint: string;
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	outAmount: string;
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	otherAmountThreshold: string;
	/**
	 *
	 * @type {SwapMode}
	 * @memberof QuoteResponse
	 */
	swapMode: SwapMode;
	/**
	 *
	 * @type {number}
	 * @memberof QuoteResponse
	 */
	slippageBps: number;
	/**
	 *
	 * @type {PlatformFee}
	 * @memberof QuoteResponse
	 */
	platformFee?: PlatformFee;
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	priceImpactPct: string;
	/**
	 *
	 * @type {Array<RoutePlanStep>}
	 * @memberof QuoteResponse
	 */
	routePlan: Array<RoutePlanStep>;
	/**
	 *
	 * @type {number}
	 * @memberof QuoteResponse
	 */
	contextSlot?: number;
	/**
	 *
	 * @type {number}
	 * @memberof QuoteResponse
	 */
	timeTaken?: number;
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	error?: string;
	/**
	 *
	 * @type {string}
	 * @memberof QuoteResponse
	 */
	errorCode?: string;
}

/** A Jupiter quote plus the payload {@link JupiterClient.getRouteInstructions} needs. */
export type JupiterSwapQuote = QuoteResponse & SwapQuote;

export const RECOMMENDED_JUPITER_API_VERSION = '/v1';
/** @deprecated Use RECOMMENDED_JUPITER_API instead. lite-api.jup.ag requires migration to api.jup.ag with API key. */
export const LEGACY_JUPITER_API = 'https://lite-api.jup.ag/swap';
export const RECOMMENDED_JUPITER_API = 'https://api.jup.ag/swap';

export class JupiterClient implements SwapProvider {
	public readonly providerName = 'jupiter' as const;

	url: string;
	connection: Connection;
	lookupTableCache = new Map<string, AddressLookupTableAccount>();
	private apiKey?: string;

	/**
	 * Create a Jupiter client
	 * @param connection - Solana connection
	 * @param url - Optional custom API URL. Defaults to https://api.jup.ag/swap
	 * @param apiKey - API key for Jupiter API. Required for api.jup.ag (free tier available at https://portal.jup.ag)
	 */
	constructor({
		connection,
		url,
		apiKey,
	}: {
		connection: Connection;
		url?: string;
		apiKey?: string;
	}) {
		this.connection = connection;
		this.url = url ?? RECOMMENDED_JUPITER_API;
		this.apiKey = apiKey;
	}

	/**
	 * Get the headers for API requests, including API key if configured
	 */
	private getHeaders(contentType?: string): Record<string, string> {
		const headers: Record<string, string> = {};
		if (contentType) {
			headers['Content-Type'] = contentType;
		}
		if (this.apiKey) {
			headers['x-api-key'] = this.apiKey;
		}
		return headers;
	}

	/**
	 * Get routes for a swap
	 * @param inputMint the mint of the input token
	 * @param outputMint the mint of the output token
	 * @param amount the amount of the input token
	 * @param slippageBps the slippage tolerance in basis points
	 * @param swapMode the swap mode (ExactIn or ExactOut)
	 * @param onlyDirectRoutes whether to only return direct routes
	 */
	public async getQuote({
		inputMint,
		outputMint,
		amount,
		maxAccounts = DEFAULT_SWAP_MAX_ACCOUNTS,
		slippageBps = 50,
		swapMode = 'ExactIn',
		onlyDirectRoutes = false,
		excludeDexes,
		autoSlippage = false,
		maxAutoSlippageBps,
		usdEstimate,
	}: SwapQuoteParams): Promise<JupiterSwapQuote> {
		if (autoSlippage && maxAutoSlippageBps === undefined) {
			throw new Error(
				'JupiterClient.getQuote: maxAutoSlippageBps is required when autoSlippage is enabled'
			);
		}
		if (autoSlippage && usdEstimate === undefined) {
			throw new Error(
				'JupiterClient.getQuote: usdEstimate is required when autoSlippage is enabled'
			);
		}
		const maxAutoSlippageBpsParam =
			autoSlippage && maxAutoSlippageBps !== undefined
				? maxAutoSlippageBps.toString()
				: '0';
		const autoSlippageCollisionUsdValueParam =
			autoSlippage && usdEstimate !== undefined ? usdEstimate.toString() : '0';
		const params = new URLSearchParams({
			inputMint: inputMint.toString(),
			outputMint: outputMint.toString(),
			amount: amount.toString(),
			slippageBps: autoSlippage ? '0' : slippageBps.toString(),
			swapMode,
			onlyDirectRoutes: onlyDirectRoutes.toString(),
			maxAccounts: maxAccounts.toString(),
			autoSlippage: autoSlippage.toString(),
			maxAutoSlippageBps: maxAutoSlippageBpsParam,
			autoSlippageCollisionUsdValue: autoSlippageCollisionUsdValueParam,
			...(excludeDexes && { excludeDexes: excludeDexes.join(',') }),
		});
		if (swapMode === 'ExactOut') {
			params.delete('maxAccounts');
		}
		const apiVersionParam =
			this.url === RECOMMENDED_JUPITER_API || this.url === LEGACY_JUPITER_API
				? RECOMMENDED_JUPITER_API_VERSION
				: '';
		const headers = this.getHeaders();
		const fetchOptions: RequestInit =
			Object.keys(headers).length > 0 ? { headers } : {};
		const response = await fetch(
			`${this.url}${apiVersionParam}/quote?${params.toString()}`,
			fetchOptions
		);

		const quote = (await response.json().catch(() => undefined)) as
			| QuoteResponse
			| undefined;

		// A failed quote still returns parseable JSON — an `{ error, errorCode }`
		// body with no mints or amounts. Returning it unchecked pushes the failure
		// downstream to /swap, which rejects it with an opaque deserialization
		// error ("missing field `inputMint`") that hides the real cause.
		if (!response.ok || !quote) {
			throw new Error(
				`Jupiter quote failed: ${response.status} ${
					quote?.error || quote?.errorCode || response.statusText
				}`
			);
		}

		if (quote.error || quote.errorCode) {
			throw new Error(
				`Jupiter quote failed: ${quote.error ?? quote.errorCode}`
			);
		}

		if (!quote.inputMint || !quote.outputMint || !quote.outAmount) {
			throw new Error('Jupiter quote failed: response is missing route fields');
		}

		// Jupiter's /swap endpoint takes the quote body back verbatim, so the
		// quote is its own route payload.
		return buildSwapQuote(quote, { provider: 'jupiter', quote });
	}

	/**
	 * Get a swap transaction for quote
	 * @param quote quote to perform swap, from {@link getQuote}
	 * @param userPublicKey the signer's wallet public key
	 *
	 * Always builds at the quote's own slippage — the price the caller was
	 * shown. Re-quote to change it rather than overriding it here.
	 */
	public async getSwap({
		quote,
		userPublicKey,
	}: {
		quote: QuoteResponse | JupiterSwapQuote;
		userPublicKey: PublicKey;
	}): Promise<VersionedTransaction> {
		if (!quote) {
			throw new Error('Jupiter swap quote not provided. Please try again.');
		}

		// `providerRoute` is our own wrapper, not part of Jupiter's quote body.
		const { providerRoute: _providerRoute, ...quoteResponse } =
			quote as JupiterSwapQuote;

		const apiVersionParam =
			this.url === RECOMMENDED_JUPITER_API || this.url === LEGACY_JUPITER_API
				? RECOMMENDED_JUPITER_API_VERSION
				: '';
		const resp = await (
			await fetch(`${this.url}${apiVersionParam}/swap`, {
				method: 'POST',
				headers: this.getHeaders('application/json'),
				body: JSON.stringify({
					quoteResponse,
					userPublicKey,
					slippageBps: quoteResponse.slippageBps,
				}),
			})
		).json();
		if (!('swapTransaction' in resp)) {
			throw new Error(
				`swapTransaction not found, error from Jupiter: ${resp.error} ${
					', ' + (resp.message ?? '')
				}`
			);
		}
		const { swapTransaction } = resp;

		try {
			const swapTransactionBuf = Buffer.from(swapTransaction, 'base64');
			return VersionedTransaction.deserialize(swapTransactionBuf);
		} catch (err) {
			throw new Error(
				'Something went wrong with creating the Jupiter swap transaction. Please try again.'
			);
		}
	}

	/**
	 * The standalone transaction Jupiter's `/swap` endpoint returns, setup and
	 * teardown included.
	 * @throws If the quote came from a different provider or a different wallet.
	 */
	public async getSwapTransaction({
		quote,
		userPublicKey,
	}: GetRouteInstructionsParams): Promise<VersionedTransaction> {
		const jupiterQuote = expectProviderRoute(quote, 'jupiter', userPublicKey)
			.quote as QuoteResponse;

		return this.getSwap({ quote: jupiterQuote, userPublicKey });
	}

	/**
	 * Builds the route instructions for a quote returned by {@link getQuote}.
	 *
	 * The quote carries its own route payload, so this reads no client state
	 * and two quotes in flight can never be confused for one another. The swap
	 * is built at the slippage the quote was priced at — re-quote to change it.
	 * @throws If the quote came from a different provider or a different wallet.
	 */
	public async getRouteInstructions({
		quote,
		userPublicKey,
	}: GetRouteInstructionsParams): Promise<SwapRouteInstructions> {
		const jupiterQuote = expectProviderRoute(quote, 'jupiter', userPublicKey)
			.quote as QuoteResponse;

		const transaction = await this.getSwap({
			quote: jupiterQuote,
			userPublicKey,
		});

		const { transactionMessage, lookupTables } =
			await this.getTransactionMessageAndLookupTables({ transaction });

		return {
			instructions: filterRouteInstructions({
				instructions: transactionMessage.instructions,
				inputMint: new PublicKey(quote.inputMint),
				outputMint: new PublicKey(quote.outputMint),
			}),
			lookupTables,
		};
	}

	/**
	 * Get the transaction message and lookup tables for a transaction
	 * @param transaction
	 */
	public async getTransactionMessageAndLookupTables({
		transaction,
	}: {
		transaction: VersionedTransaction;
	}): Promise<{
		transactionMessage: TransactionMessage;
		lookupTables: AddressLookupTableAccount[];
	}> {
		const message = transaction.message;

		const lookupTables = (
			await Promise.all(
				message.addressTableLookups.map(async (lookup) => {
					return await this.getLookupTable(lookup.accountKey);
				})
			)
		).filter(
			(lookup): lookup is AddressLookupTableAccount => lookup !== undefined
		);

		const transactionMessage = TransactionMessage.decompile(message, {
			addressLookupTableAccounts: lookupTables,
		});
		return {
			transactionMessage,
			lookupTables,
		};
	}

	async getLookupTable(
		accountKey: PublicKey
	): Promise<AddressLookupTableAccount | undefined> {
		const cached = this.lookupTableCache.get(accountKey.toString());
		if (cached !== undefined) {
			return cached;
		}

		const lookupTable = (
			await this.connection.getAddressLookupTable(accountKey)
		).value;

		if (!lookupTable) {
			return undefined;
		}

		// Populate the cache — without this every route re-fetches the same tables,
		// which is a large share of the RPC calls a swap makes.
		this.lookupTableCache.set(accountKey.toString(), lookupTable);

		return lookupTable;
	}

	/**
	 * Strips the setup/teardown Jupiter wraps around its route.
	 * @deprecated Use {@link getRouteInstructions}, which quotes and filters in
	 * one step. Kept for callers holding a decompiled message of their own.
	 */
	public getJupiterInstructions({
		transactionMessage,
		inputMint,
		outputMint,
	}: {
		transactionMessage: TransactionMessage;
		inputMint: PublicKey;
		outputMint: PublicKey;
	}): TransactionInstruction[] {
		return filterRouteInstructions({
			instructions: transactionMessage.instructions,
			inputMint,
			outputMint,
		});
	}
}
