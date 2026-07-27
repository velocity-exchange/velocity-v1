import { Connection, PublicKey } from '@solana/web3.js';
import { BN } from '../isomorphic/anchor';
import { JupiterClient } from '../jupiter/jupiterClient';
import { TitanClient } from '../titan/titanClient';
import { MAX_TX_BYTE_SIZE } from '../tx/utils';
import {
	SwapClientType,
	SwapMode,
	SwapProvider,
	SwapQuote,
	SwapQuoteParams,
	SwapRouteInstructions,
} from './types';

export type {
	GetRouteInstructionsParams,
	ProviderRoute,
	SwapClientType,
	SwapMode,
	SwapProvider,
	SwapQuote,
	SwapQuoteParams,
	SwapRouteInstructions,
	UnifiedQuoteResponse,
} from './types';

/**
 * Bytes reserved for the velocity begin/end swap instructions that wrap the
 * route, so the provider only gets the budget actually left for the route.
 */
const VELOCITY_SWAP_IX_SIZE_BUFFER = 375;

/** Byte budget handed to a swap provider for the route portion of the tx. */
const DEFAULT_ROUTE_SIZE_CONSTRAINT =
	MAX_TX_BYTE_SIZE - VELOCITY_SWAP_IX_SIZE_BUFFER;

/**
 * Routes swap calls to the configured provider.
 *
 * Intentionally thin: it picks a provider and forwards. Anything that varies
 * between Jupiter and Titan belongs in the provider, behind
 * {@link SwapProvider} — branching on the provider here is how the two paths
 * drifted apart previously, since nothing forced them to keep the same
 * semantics.
 */
export class UnifiedSwapClient implements SwapProvider {
	private client: JupiterClient | TitanClient;
	private clientType: SwapClientType;

	/**
	 * @param clientType - 'jupiter' or 'titan'
	 * @param connection - Solana connection
	 * @param authToken - For Titan: auth token (required when not using proxy). For Jupiter: API key (required for api.jup.ag, get free key at https://portal.jup.ag)
	 * @param url - Optional custom URL
	 * @param proxyUrl - Optional proxy URL for Titan
	 */
	constructor({
		clientType,
		connection,
		authToken,
		url,
		proxyUrl,
	}: {
		clientType: SwapClientType;
		connection: Connection;
		authToken?: string;
		url?: string;
		proxyUrl?: string;
	}) {
		this.clientType = clientType;

		if (clientType === 'jupiter') {
			this.client = new JupiterClient({
				connection,
				url,
				apiKey: authToken,
			});
		} else if (clientType === 'titan') {
			this.client = new TitanClient({
				connection,
				authToken: authToken || '', // Not needed when using proxy
				url,
				proxyUrl,
			});
		} else {
			throw new Error(`Unsupported client type: ${clientType}`);
		}
	}

	public get providerName(): SwapClientType {
		return this.clientType;
	}

	/**
	 * Get a swap quote from the configured provider.
	 *
	 * Provider-specific fields on {@link SwapQuoteParams} are mapped by the
	 * provider, so the ones it doesn't recognise are simply ignored.
	 */
	public async getQuote(params: SwapQuoteParams): Promise<SwapQuote> {
		return this.client.getQuote({
			...params,
			sizeConstraint: params.sizeConstraint ?? DEFAULT_ROUTE_SIZE_CONSTRAINT,
		});
	}

	/**
	 * Builds the route instructions for a quote from {@link getQuote}.
	 * @throws If the quote came from a different provider.
	 */
	public async getRouteInstructions(params: {
		quote: SwapQuote;
		userPublicKey: PublicKey;
		slippageBps?: number;
	}): Promise<SwapRouteInstructions> {
		return this.client.getRouteInstructions(params);
	}

	/**
	 * Quote (if needed) and build in one step.
	 *
	 * Prefer passing a `quote` you already showed the user — re-quoting here
	 * builds a route they never saw. Identical for both providers: the quote
	 * carries its own route, so neither can fall back to a stale one.
	 */
	public async getSwapInstructions({
		inputMint,
		outputMint,
		amount,
		userPublicKey,
		slippageBps,
		swapMode = 'ExactIn',
		onlyDirectRoutes = false,
		maxAccounts,
		quote,
		sizeConstraint,
	}: {
		inputMint: PublicKey;
		outputMint: PublicKey;
		amount: BN;
		userPublicKey: PublicKey;
		slippageBps?: number;
		swapMode?: SwapMode;
		onlyDirectRoutes?: boolean;
		maxAccounts?: number;
		quote?: SwapQuote;
		sizeConstraint?: number;
	}): Promise<SwapRouteInstructions> {
		const quoteToUse =
			quote ??
			(await this.getQuote({
				inputMint,
				outputMint,
				amount,
				userPublicKey,
				slippageBps,
				swapMode,
				onlyDirectRoutes,
				maxAccounts,
				sizeConstraint,
			}));

		return this.getRouteInstructions({
			quote: quoteToUse,
			userPublicKey,
			slippageBps,
		});
	}

	/**
	 * Get the underlying client instance
	 */
	public getClient(): JupiterClient | TitanClient {
		return this.client;
	}

	/**
	 * Get the client type
	 */
	public getClientType(): SwapClientType {
		return this.clientType;
	}

	/**
	 * Check if this is a Jupiter client
	 */
	public isJupiter(): boolean {
		return this.clientType === 'jupiter';
	}

	/**
	 * Check if this is a Titan client
	 */
	public isTitan(): boolean {
		return this.clientType === 'titan';
	}
}
