import { Connection, VersionedTransaction } from '@solana/web3.js';
import { JupiterApiVersion, JupiterClient } from '../jupiter/jupiterClient';
import { TitanClient } from '../titan/titanClient';
import { MAX_TX_BYTE_SIZE } from '../tx/utils';
import {
	GetRouteInstructionsParams,
	SwapClientType,
	SwapProvider,
	SwapQuote,
	SwapQuoteParams,
	SwapRouteInstructions,
} from './types';

// These re-exports keep a deep import of this module resolving. `./types` is
// the definition site and the module the package index exports.
export type {
	GetRouteInstructionsParams,
	SwapProviderRoute,
	SwapClientType,
	SwapMode,
	SwapProvider,
	SwapQuote,
	SwapQuoteParams,
	SwapRouteInstructions,
	UnifiedQuoteResponse,
} from './types';

/**
 * Bytes reserved for the velocity `beginSwap` and `endSwap` instructions that
 * wrap the route. The provider gets only the budget left for the route.
 */
const VELOCITY_SWAP_IX_SIZE_BUFFER = 375;

/** Byte budget handed to a swap provider for the route portion of the tx. */
export const DEFAULT_ROUTE_SIZE_CONSTRAINT =
	MAX_TX_BYTE_SIZE - VELOCITY_SWAP_IX_SIZE_BUFFER;

/**
 * Routes swap calls to the configured provider.
 *
 * The class stays thin. It picks a provider and forwards. Behaviour that varies
 * between Jupiter and Titan belongs in the provider, behind
 * {@link SwapProvider}. A branch on the provider here lets the two paths drift,
 * because nothing then holds them to the same semantics.
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
	 * @param jupiterApiVersion - For Jupiter: which Swap API to use. Ignored for Titan.
	 */
	constructor({
		clientType,
		connection,
		authToken,
		url,
		proxyUrl,
		jupiterApiVersion,
	}: {
		clientType: SwapClientType;
		connection: Connection;
		authToken?: string;
		url?: string;
		proxyUrl?: string;
		jupiterApiVersion?: JupiterApiVersion;
	}) {
		this.clientType = clientType;

		if (clientType === 'jupiter') {
			this.client = new JupiterClient({
				connection,
				url,
				apiKey: authToken,
				apiVersion: jupiterApiVersion,
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
	 * The configured client, seen only as {@link SwapProvider}.
	 *
	 * Forwarding through the interface rather than the concrete union makes the
	 * contract enforceable. A provider added to the union that implements only
	 * part of the contract fails to compile here. Through the union it would
	 * instead resolve against whichever call signatures the members share.
	 */
	private get provider(): SwapProvider {
		return this.client;
	}

	/**
	 * Get a swap quote from the configured provider.
	 *
	 * The provider maps the provider-specific fields on
	 * {@link SwapQuoteParams} and ignores the ones it does not recognise.
	 */
	public async getQuote(params: SwapQuoteParams): Promise<SwapQuote> {
		return this.provider.getQuote({
			...params,
			sizeConstraint: params.sizeConstraint ?? DEFAULT_ROUTE_SIZE_CONSTRAINT,
		});
	}

	/**
	 * Builds the route instructions for a quote from {@link getQuote}, at the
	 * slippage that quote was priced at.
	 * @throws If the quote came from a different provider or a different wallet.
	 */
	public async getRouteInstructions(
		params: GetRouteInstructionsParams
	): Promise<SwapRouteInstructions> {
		return this.provider.getRouteInstructions(params);
	}

	/**
	 * Builds a standalone swap transaction for a quote from {@link getQuote}. It
	 * keeps the provider's own compute budget, token account creation and SOL
	 * wrapping. Use {@link getRouteInstructions} for a swap that runs inside
	 * velocity's `beginSwap` and `endSwap` bracket.
	 * @throws If the quote came from a different provider or a different wallet.
	 */
	public async getSwapTransaction(
		params: GetRouteInstructionsParams
	): Promise<VersionedTransaction> {
		return this.provider.getSwapTransaction(params);
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
