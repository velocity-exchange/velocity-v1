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
	 * Absent on the v2 API — `/swap/v2/build` omits per-hop fees.
	 * @type {string}
	 * @memberof SwapInfo
	 */
	feeAmount?: string;
	/**
	 * Absent on the v2 API — `/swap/v2/build` omits per-hop fees.
	 * @type {string}
	 * @memberof SwapInfo
	 */
	feeMint?: string;
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

/** A single instruction as both Jupiter APIs encode it on the wire. */
export interface JupiterApiInstruction {
	programId: string;
	accounts: Array<{ pubkey: string; isSigner: boolean; isWritable: boolean }>;
	/** base64 */
	data: string;
}

/**
 * The body `GET /swap/v2/build` returns: the quote and the instructions that
 * execute it, in one response.
 *
 * v2 replaces v1's `/quote` → `POST /swap` pair, so there is no separate quote
 * body and nothing to post back. The trade-off is that the route is built for
 * one `taker`, which is why {@link JupiterClient.getQuote} requires
 * `userPublicKey` under v2 and records it on the quote.
 */
export interface JupiterBuildResponse {
	inputMint: string;
	outputMint: string;
	inAmount: string;
	outAmount: string;
	otherAmountThreshold: string;
	swapMode: SwapMode;
	slippageBps: number;
	priceImpactPct: string;
	/** v2 reports each hop's share as both `percent` and `bps`. */
	routePlan: Array<RoutePlanStep & { bps?: number }>;
	computeBudgetInstructions: JupiterApiInstruction[];
	setupInstructions: JupiterApiInstruction[];
	swapInstruction: JupiterApiInstruction;
	cleanupInstruction: JupiterApiInstruction | null;
	otherInstructions: JupiterApiInstruction[];
	/** Non-null only when opted into Jupiter's own transaction landing. */
	tipInstruction: JupiterApiInstruction | null;
	addressesByLookupTableAddress: Record<string, string[]>;
	/** `blockhash` is a byte array, not base58. Unused — we fetch a fresh one. */
	blockhashWithMetadata?: { blockhash: number[]; lastValidBlockHeight: number };
	/** Object-valued on a validation failure — read it through {@link describeJupiterError}. */
	error?: string | Record<string, unknown>;
	errorCode?: string;
}

/** A Jupiter quote plus the payload {@link JupiterClient.getRouteInstructions} needs. */
export type JupiterSwapQuote = QuoteResponse & SwapQuote;

/** Which Jupiter Swap API a {@link JupiterClient} talks to. */
export type JupiterApiVersion = 'v1' | 'v2';

/** The route payload a Jupiter quote carries, per API version. */
type JupiterV2RoutePayload = {
	readonly apiVersion: 'v2';
	readonly build: JupiterBuildResponse;
};

const isV2Payload = (payload: unknown): payload is JupiterV2RoutePayload =>
	!!payload &&
	typeof payload === 'object' &&
	(payload as { apiVersion?: unknown }).apiVersion === 'v2';

/** Whether a quote's route payload is a v2 build rather than a v1 quote body. */
const carriesV2Build = (quote: unknown): boolean => {
	const route = (quote as SwapQuote | undefined)?.providerRoute;

	return !!route && 'quote' in route && isV2Payload(route.quote);
};

const toTransactionInstruction = (
	instruction: JupiterApiInstruction
): TransactionInstruction =>
	new TransactionInstruction({
		programId: new PublicKey(instruction.programId),
		keys: instruction.accounts.map((account) => ({
			pubkey: new PublicKey(account.pubkey),
			isSigner: account.isSigner,
			isWritable: account.isWritable,
		})),
		data: Buffer.from(instruction.data, 'base64'),
	});

/**
 * A v2 build's instructions in execution order: compute budget, setup, swap,
 * cleanup, other, tip.
 *
 * The tip and `otherInstructions` are included here because this is also what a
 * standalone transaction sends. {@link filterRouteInstructions} drops them again
 * for the velocity swap bracket — both are System-program transfers, which the
 * program rejects between `beginSwap` and `endSwap`.
 */
const flattenBuildInstructions = (
	build: JupiterBuildResponse
): TransactionInstruction[] =>
	[
		...build.computeBudgetInstructions,
		...build.setupInstructions,
		build.swapInstruction,
		...(build.cleanupInstruction ? [build.cleanupInstruction] : []),
		...build.otherInstructions,
		...(build.tipInstruction ? [build.tipInstruction] : []),
	].map(toTransactionInstruction);

/** A ZodError issue as the v2 API reports it. */
type ZodIssue = { path?: unknown[]; message?: string };

/**
 * Renders whatever a Jupiter response says went wrong as a readable string.
 *
 * v2 answers with three unrelated shapes — a Zod validation object
 * (`{ error: { issues, name: 'ZodError' } }`), a rate-limit body
 * (`{ code, message }`), and v1's `{ error, errorCode }`. Interpolating `error`
 * directly renders the first as `[object Object]`, which is how a missing
 * required param used to reach the caller.
 */
const describeJupiterError = (
	body: Record<string, any> | undefined,
	response: { status: number; statusText?: string }
): string => {
	const error = body?.error;

	if (error && typeof error === 'object') {
		const issues = (error.issues as ZodIssue[] | undefined) ?? [];
		const described = issues
			.map((issue) =>
				[(issue.path ?? []).join('.'), issue.message]
					.filter((part) => part !== '' && part !== undefined)
					.join(': ')
			)
			.filter((issue) => issue !== '');

		if (described.length > 0) {
			return `${error.name ?? 'error'}: ${described.join('; ')}`;
		}

		return JSON.stringify(error);
	}

	return (
		error ??
		body?.errorCode ??
		body?.message ??
		response.statusText ??
		`HTTP ${response.status}`
	);
};

export const RECOMMENDED_JUPITER_API_VERSION = '/v1';
export const JUPITER_API_V2_VERSION = '/v2';
/** @deprecated Use RECOMMENDED_JUPITER_API instead. lite-api.jup.ag requires migration to api.jup.ag with API key. */
export const LEGACY_JUPITER_API = 'https://lite-api.jup.ag/swap';
export const RECOMMENDED_JUPITER_API = 'https://api.jup.ag/swap';

/**
 * Jupiter swap client, over either Swap API version.
 *
 * `apiVersion: 'v1'` (the default) quotes with `GET /swap/v1/quote` and builds
 * with `POST /swap/v1/swap`, then decompiles the returned transaction.
 *
 * `apiVersion: 'v2'` uses `GET /swap/v2/build`, which answers the quote and its
 * raw instructions in a single round trip — no `/swap` post, no transaction to
 * deserialize. Two consequences for callers:
 * - `getQuote` requires `userPublicKey`: v2 builds for a specific `taker`, so
 *   the quote is wallet-bound and rejected if swapped by anyone else.
 * - `autoSlippage` is unsupported. v2 has no equivalent and silently ignores the
 *   params, returning zero slippage tolerance, so `getQuote` throws rather than
 *   pass them through. Auto-slippage needs `apiVersion: 'v1'`.
 */
export class JupiterClient implements SwapProvider {
	public readonly providerName = 'jupiter' as const;

	url: string;
	connection: Connection;
	lookupTableCache = new Map<string, AddressLookupTableAccount>();
	private apiKey?: string;
	private apiVersion: JupiterApiVersion;

	/**
	 * Create a Jupiter client
	 * @param connection - Solana connection
	 * @param url - Optional custom API URL. Defaults to https://api.jup.ag/swap
	 * @param apiKey - API key for Jupiter API. Required for api.jup.ag (free tier available at https://portal.jup.ag)
	 * @param apiVersion - Which Swap API to use. Defaults to 'v1'.
	 */
	constructor({
		connection,
		url,
		apiKey,
		apiVersion,
	}: {
		connection: Connection;
		url?: string;
		apiKey?: string;
		apiVersion?: JupiterApiVersion;
	}) {
		this.connection = connection;
		this.url = url ?? RECOMMENDED_JUPITER_API;
		this.apiKey = apiKey;
		this.apiVersion = apiVersion ?? 'v1';
	}

	/**
	 * The version path segment for the configured API.
	 *
	 * Empty for a custom `url`, which is assumed to already carry one.
	 */
	private versionSegment(): string {
		if (
			this.url !== RECOMMENDED_JUPITER_API &&
			this.url !== LEGACY_JUPITER_API
		) {
			return '';
		}

		return this.apiVersion === 'v2'
			? JUPITER_API_V2_VERSION
			: RECOMMENDED_JUPITER_API_VERSION;
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
	 * @param userPublicKey the taker's wallet. Required under `apiVersion: 'v2'`,
	 * which builds the route for one wallet at quote time.
	 * @param slippageBps the slippage tolerance in basis points
	 * @param swapMode the swap mode (ExactIn or ExactOut)
	 * @param onlyDirectRoutes whether to only return direct routes
	 */
	public async getQuote(params: SwapQuoteParams): Promise<JupiterSwapQuote> {
		return this.apiVersion === 'v2'
			? this.getV2Quote(params)
			: this.getV1Quote(params);
	}

	/**
	 * Quotes through `GET /swap/v2/build`, which returns the route's instructions
	 * along with the quote.
	 *
	 * The build is carried on the quote as an opaque payload, so
	 * {@link getRouteInstructions} and {@link getSwapTransaction} issue no further
	 * HTTP request.
	 */
	private async getV2Quote({
		inputMint,
		outputMint,
		amount,
		userPublicKey,
		maxAccounts = DEFAULT_SWAP_MAX_ACCOUNTS,
		slippageBps = 50,
		swapMode = 'ExactIn',
		onlyDirectRoutes = false,
		excludeDexes,
		autoSlippage = false,
	}: SwapQuoteParams): Promise<JupiterSwapQuote> {
		if (autoSlippage) {
			throw new Error(
				'JupiterClient.getQuote: autoSlippage is not supported by the Jupiter v2 API (v2 silently ignores it and returns zero slippage tolerance); construct the client with apiVersion: "v1"'
			);
		}
		if (!userPublicKey) {
			throw new Error(
				'JupiterClient.getQuote: userPublicKey is required for the Jupiter v2 API (the /swap/v2/build endpoint builds instructions for a specific taker)'
			);
		}

		const params = new URLSearchParams({
			inputMint: inputMint.toString(),
			outputMint: outputMint.toString(),
			amount: amount.toString(),
			slippageBps: slippageBps.toString(),
			swapMode,
			taker: userPublicKey.toString(),
			onlyDirectRoutes: onlyDirectRoutes.toString(),
			maxAccounts: maxAccounts.toString(),
			...(excludeDexes && { excludeDexes: excludeDexes.join(',') }),
		});
		// An ExactOut route has no account budget to spend — same as v1.
		if (swapMode === 'ExactOut') {
			params.delete('maxAccounts');
		}

		const headers = this.getHeaders();
		const fetchOptions: RequestInit =
			Object.keys(headers).length > 0 ? { headers } : {};
		const response = await fetch(
			`${this.url}${this.versionSegment()}/build?${params.toString()}`,
			fetchOptions
		);

		const build = (await response.json().catch(() => undefined)) as
			| JupiterBuildResponse
			| undefined;

		if (!response.ok || !build) {
			throw new Error(
				`Jupiter quote failed: ${response.status} ${describeJupiterError(
					build,
					response
				)}`
			);
		}

		if (build.error || build.errorCode) {
			throw new Error(
				`Jupiter quote failed: ${describeJupiterError(build, response)}`
			);
		}

		if (!build.inputMint || !build.outputMint || !build.outAmount) {
			throw new Error('Jupiter quote failed: response is missing route fields');
		}

		// Dropped now they are known absent, so the quote carries the same fields a
		// v1 one does rather than v2's wider `error` type.
		const { error: _error, errorCode: _errorCode, ...quote } = build;

		return buildSwapQuote(quote, {
			provider: 'jupiter',
			quote: { apiVersion: 'v2', build },
			// v2 builds for one taker, so the route is only executable by them.
			quotedFor: userPublicKey.toString(),
		});
	}

	/** Quotes through `GET /swap/v1/quote`; the quote is posted back to `/swap`. */
	private async getV1Quote({
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
		const headers = this.getHeaders();
		const fetchOptions: RequestInit =
			Object.keys(headers).length > 0 ? { headers } : {};
		const response = await fetch(
			`${this.url}${this.versionSegment()}/quote?${params.toString()}`,
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
	 * Get a swap transaction for quote, by posting it back to `POST /swap`.
	 * @param quote quote to perform swap, from {@link getQuote}
	 * @param userPublicKey the signer's wallet public key
	 *
	 * Always builds at the quote's own slippage — the price the caller was
	 * shown. Re-quote to change it rather than overriding it here.
	 *
	 * v1 only. A v2 quote already carries its instructions and has no `/swap`
	 * endpoint to post to — use {@link getSwapTransaction} or
	 * {@link getRouteInstructions}.
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

		if (carriesV2Build(quote)) {
			throw new Error(
				'JupiterClient.getSwap posts to the v1 /swap endpoint; a v2 quote carries its instructions already — use getSwapTransaction or getRouteInstructions'
			);
		}

		// `providerRoute` is our own wrapper, not part of Jupiter's quote body.
		const { providerRoute: _providerRoute, ...quoteResponse } =
			quote as JupiterSwapQuote;

		const resp = await (
			await fetch(`${this.url}${this.versionSegment()}/swap`, {
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
	 * The route as a standalone transaction, setup and teardown included.
	 *
	 * Under v1 this is the transaction `/swap` returns. Under v2 the build's
	 * instructions are compiled locally, against a **freshly fetched** blockhash
	 * rather than the build's own `blockhashWithMetadata` — a build response may
	 * be minutes old by the time it is signed, and this matches what the Titan
	 * client does.
	 * @throws If the quote came from a different provider or a different wallet.
	 */
	public async getSwapTransaction({
		quote,
		userPublicKey,
	}: GetRouteInstructionsParams): Promise<VersionedTransaction> {
		const route = expectProviderRoute(quote, 'jupiter', userPublicKey);

		if (isV2Payload(route.quote)) {
			const { build } = route.quote;

			const [lookupTables, { blockhash }] = await Promise.all([
				this.getBuildLookupTables(build),
				this.connection.getLatestBlockhash(),
			]);

			return new VersionedTransaction(
				new TransactionMessage({
					payerKey: userPublicKey,
					recentBlockhash: blockhash,
					instructions: flattenBuildInstructions(build),
				}).compileToV0Message(lookupTables)
			);
		}

		return this.getSwap({
			quote: route.quote as QuoteResponse,
			userPublicKey,
		});
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
		const route = expectProviderRoute(quote, 'jupiter', userPublicKey);

		if (isV2Payload(route.quote)) {
			const { build } = route.quote;

			return {
				instructions: filterRouteInstructions({
					instructions: flattenBuildInstructions(build),
					inputMint: new PublicKey(quote.inputMint),
					outputMint: new PublicKey(quote.outputMint),
				}),
				lookupTables: await this.getBuildLookupTables(build),
			};
		}

		const transaction = await this.getSwap({
			quote: route.quote as QuoteResponse,
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

	/**
	 * The lookup tables a v2 build depends on, read from chain.
	 *
	 * The build already lists each table's addresses inline
	 * (`addressesByLookupTableAddress`), which would let this skip the RPC
	 * entirely — but a synthesized `AddressLookupTableAccount` has to invent the
	 * `state` metadata (authority, `deactivationSlot`) that compiling a
	 * transaction reads, and would not notice a table deactivated since the
	 * build. Fetching keeps that semantics identical to the v1 path; dropping it
	 * is a separate change that needs its own verification.
	 */
	private async getBuildLookupTables(
		build: JupiterBuildResponse
	): Promise<AddressLookupTableAccount[]> {
		return (
			await Promise.all(
				Object.keys(build.addressesByLookupTableAddress).map((address) =>
					this.getLookupTable(new PublicKey(address))
				)
			)
		).filter(
			(lookupTable): lookupTable is AddressLookupTableAccount =>
				lookupTable !== undefined
		);
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
