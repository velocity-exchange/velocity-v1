import {
	Connection,
	PublicKey,
	TransactionMessage,
	AddressLookupTableAccount,
	TransactionInstruction,
} from '@solana/web3.js';
import { BN } from '../isomorphic/anchor';
import { decode } from '@msgpack/msgpack';
import { filterRouteInstructions } from '../swap/routeInstructions';
import {
	DEFAULT_SWAP_MAX_ACCOUNTS,
	GetRouteInstructionsParams,
	SwapProvider,
	SwapQuote,
	SwapQuoteParams,
	SwapRouteInstructions,
	expectProviderRoute,
} from '../swap/types';

export enum SwapMode {
	ExactIn = 'ExactIn',
	ExactOut = 'ExactOut',
}

interface RoutePlanStep {
	ammKey: Uint8Array;
	label: string;
	inputMint: Uint8Array;
	outputMint: Uint8Array;
	inAmount: number;
	outAmount: number;
	allocPpb: number;
	feeMint?: Uint8Array;
	feeAmount?: number;
	contextSlot?: number;
}

interface PlatformFee {
	amount: number;
	fee_bps: number;
}

type Pubkey = Uint8Array;

interface AccountMeta {
	p: Pubkey;
	s: boolean;
	w: boolean;
}

interface Instruction {
	p: Pubkey;
	a: AccountMeta[];
	d: Uint8Array;
}

interface SwapRoute {
	inAmount: number;
	outAmount: number;
	slippageBps: number;
	platformFee?: PlatformFee;
	steps: RoutePlanStep[];
	instructions: Instruction[];
	addressLookupTables: Pubkey[];
	contextSlot?: number;
	timeTaken?: number;
	expiresAtMs?: number;
	expiresAfterSlot?: number;
	computeUnits?: number;
	computeUnitsSafe?: number;
	transaction?: Uint8Array;
	referenceId?: string;
}

interface SwapQuotes {
	id: string;
	inputMint: Uint8Array;
	outputMint: Uint8Array;
	swapMode: SwapMode;
	amount: number;
	quotes: { [key: string]: SwapRoute };
}

const TITAN_API_URL = 'https://api.titan.exchange';

/** Retries for a route's lookup tables, which must all resolve for the tx to fit. */
const LOOKUP_TABLE_FETCH_RETRIES = 2;
const LOOKUP_TABLE_RETRY_BASE_DELAY_MS = 150;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export class TitanClient implements SwapProvider {
	public readonly providerName = 'titan' as const;

	authToken: string;
	url: string;
	connection: Connection;
	proxyUrl?: string;

	constructor({
		connection,
		authToken,
		url,
		proxyUrl,
	}: {
		connection: Connection;
		authToken: string;
		url?: string;
		proxyUrl?: string;
	}) {
		this.connection = connection;
		this.authToken = authToken;
		this.url = url ?? TITAN_API_URL;
		this.proxyUrl = proxyUrl;
	}

	private buildParams({
		inputMint,
		outputMint,
		amount,
		userPublicKey,
		maxAccounts,
		slippageBps,
		swapMode,
		onlyDirectRoutes,
		excludeDexes,
		sizeConstraint,
		accountsLimitWritable,
	}: {
		inputMint: PublicKey;
		outputMint: PublicKey;
		amount: BN;
		userPublicKey: PublicKey;
		maxAccounts?: number;
		slippageBps?: number;
		swapMode?: string | SwapMode;
		onlyDirectRoutes?: boolean;
		excludeDexes?: string[];
		sizeConstraint?: number;
		accountsLimitWritable?: number;
	}): URLSearchParams {
		// Normalize swapMode to enum value
		const normalizedSwapMode =
			swapMode === 'ExactOut' || swapMode === SwapMode.ExactOut
				? SwapMode.ExactOut
				: SwapMode.ExactIn;

		return new URLSearchParams({
			inputMint: inputMint.toString(),
			outputMint: outputMint.toString(),
			amount: amount.toString(),
			userPublicKey: userPublicKey.toString(),
			...(slippageBps && { slippageBps: slippageBps.toString() }),
			...(swapMode && { swapMode: normalizedSwapMode.toString() }),
			...(maxAccounts && { accountsLimitTotal: maxAccounts.toString() }),
			...(excludeDexes && { excludeDexes: excludeDexes.join(',') }),
			...(onlyDirectRoutes && {
				onlyDirectRoutes: onlyDirectRoutes.toString(),
			}),
			...(sizeConstraint && { sizeConstraint: sizeConstraint.toString() }),
			...(accountsLimitWritable && {
				accountsLimitWritable: accountsLimitWritable.toString(),
			}),
		});
	}

	/**
	 * Get the best available route for a swap.
	 *
	 * The route is returned on the quote's `providerRoute`, so
	 * {@link getRouteInstructions} builds exactly what was quoted here, at the
	 * slippage quoted here.
	 * @throws If `userPublicKey` is missing — Titan bakes the user's token
	 * accounts into the route, so a route quoted for one wallet cannot be
	 * executed by another. The wallet is recorded on the route and enforced
	 * when the route is built.
	 */
	public async getQuote({
		inputMint,
		outputMint,
		amount,
		userPublicKey,
		maxAccounts = DEFAULT_SWAP_MAX_ACCOUNTS,
		slippageBps,
		swapMode,
		onlyDirectRoutes,
		excludeDexes,
		sizeConstraint,
		accountsLimitWritable,
	}: SwapQuoteParams): Promise<SwapQuote> {
		if (!userPublicKey) {
			throw new Error('Titan quotes require a userPublicKey.');
		}

		const params = this.buildParams({
			inputMint,
			outputMint,
			amount,
			userPublicKey,
			maxAccounts,
			slippageBps,
			swapMode,
			onlyDirectRoutes,
			excludeDexes,
			sizeConstraint,
			accountsLimitWritable,
		});

		let response: Response;

		if (this.proxyUrl) {
			// Use proxy route - send parameters in request body
			response = await fetch(this.proxyUrl, {
				method: 'POST',
				headers: {
					'Content-Type': 'application/json',
				},
				body: JSON.stringify(Object.fromEntries(params.entries())),
			});
		} else {
			// Direct request to Titan API
			response = await fetch(
				`${this.url}/api/v1/quote/swap?${params.toString()}`,
				{
					headers: {
						Accept: 'application/vnd.msgpack',
						'Accept-Encoding': 'gzip, deflate, br',
						Authorization: `Bearer ${this.authToken}`,
					},
				}
			);
		}

		if (!response.ok) {
			throw new Error(
				`Titan API error: ${response.status} ${response.statusText}`
			);
		}

		const buffer = await response.arrayBuffer();
		const data = decode(buffer) as SwapQuotes;

		// We are only querying for the best avaiable route so use that
		const route = data.quotes[Object.keys(data.quotes)[0]];

		if (!route) {
			throw new Error('No routes available');
		}

		if (!route.instructions?.length) {
			throw new Error('Titan route has no instructions');
		}

		return {
			providerRoute: {
				provider: 'titan',
				route,
				quotedFor: userPublicKey.toString(),
			},
			inputMint: inputMint.toString(),
			// The route's own input, not the requested amount — under ExactOut the
			// request is the output, and callers size `beginSwap` off `inAmount`.
			inAmount: (route.inAmount ?? amount).toString(),
			outputMint: outputMint.toString(),
			outAmount: route.outAmount.toString(),
			swapMode: data.swapMode,
			slippageBps: route.slippageBps,
			platformFee: route.platformFee
				? {
						amount: route.platformFee.amount.toString(),
						feeBps: route.platformFee.fee_bps,
				  }
				: undefined,
			routePlan:
				route.steps?.map((step: any) => ({
					swapInfo: {
						ammKey: new PublicKey(step.ammKey).toString(),
						label: step.label,
						inputMint: new PublicKey(step.inputMint).toString(),
						outputMint: new PublicKey(step.outputMint).toString(),
						inAmount: step.inAmount.toString(),
						outAmount: step.outAmount.toString(),
						feeAmount: step.feeAmount?.toString() || '0',
						feeMint: step.feeMint ? new PublicKey(step.feeMint).toString() : '',
					},
					percent: 100,
				})) || [],
			contextSlot: route.contextSlot,
			timeTaken: route.timeTaken,
		};
	}

	/**
	 * Builds the route instructions for a quote returned by {@link getQuote}.
	 *
	 * The route travels on the quote, so this reads no client state and two
	 * quotes in flight can never be confused for one another. Slippage was
	 * fixed when Titan built the route, so there is nothing to apply here.
	 * @throws If the quote came from a different provider or a different wallet,
	 * or if a lookup table the route depends on can't be loaded.
	 */
	public async getRouteInstructions({
		quote,
		userPublicKey,
	}: GetRouteInstructionsParams): Promise<SwapRouteInstructions> {
		const route = expectProviderRoute(quote, 'titan', userPublicKey)
			.route as SwapRoute;

		if (!route.instructions?.length) {
			throw new Error('No instructions provided in the route');
		}

		// Errors propagate as-is. Replacing them with generic copy here loses
		// the reason the swap can't be built — an unresolvable lookup table,
		// say — which the caller needs to decide whether re-quoting will help.
		const { transactionMessage, lookupTables } =
			await this.getTransactionMessageAndLookupTables(route, userPublicKey);

		return {
			instructions: filterRouteInstructions({
				transactionMessage,
				inputMint: new PublicKey(quote.inputMint),
				outputMint: new PublicKey(quote.outputMint),
			}),
			lookupTables,
		};
	}

	/**
	 * Fetches a lookup table required by a route, retrying transient RPC
	 * failures (rate limiting in particular) before giving up.
	 * @throws If the table still can't be loaded, or doesn't exist on-chain.
	 */
	private async fetchLookupTable(
		altPubkey: PublicKey
	): Promise<AddressLookupTableAccount> {
		let lastError: unknown;

		for (let attempt = 0; attempt <= LOOKUP_TABLE_FETCH_RETRIES; attempt++) {
			if (attempt > 0) {
				await sleep(LOOKUP_TABLE_RETRY_BASE_DELAY_MS * 2 ** (attempt - 1));
			}

			let altAccount: Awaited<ReturnType<Connection['getAddressLookupTable']>>;

			try {
				altAccount = await this.connection.getAddressLookupTable(altPubkey);
			} catch (err) {
				// Transient — rate limiting, connection reset. Worth another go.
				lastError = err;
				continue;
			}

			if (altAccount.value) {
				return altAccount.value;
			}

			// A successful response with no value means the route references a
			// table that isn't on-chain. Retrying won't conjure it up.
			throw new Error(
				`Address lookup table ${altPubkey.toString()} does not exist`
			);
		}

		throw new Error(
			`Failed to fetch address lookup table ${altPubkey.toString()}: ${
				lastError instanceof Error ? lastError.message : String(lastError)
			}`
		);
	}

	private async getTransactionMessageAndLookupTables(
		route: SwapRoute,
		userPublicKey: PublicKey
	): Promise<{
		transactionMessage: TransactionMessage;
		lookupTables: AddressLookupTableAccount[];
	}> {
		const solanaInstructions: TransactionInstruction[] = route.instructions.map(
			(instruction) => ({
				programId: new PublicKey(instruction.p),
				keys: instruction.a.map((meta) => ({
					pubkey: new PublicKey(meta.p),
					isSigner: meta.s,
					isWritable: meta.w,
				})),
				data: Buffer.from(instruction.d),
			})
		);

		// Get recent blockhash
		const { blockhash } = await this.connection.getLatestBlockhash();

		// Build address lookup tables if provided.
		//
		// These all have to resolve. A table that fails to load isn't a slightly
		// worse route — every account it would have compressed to a 1-byte index
		// gets inlined as a 32-byte pubkey instead, which pushes the transaction
		// past the size limit and only surfaces later as an opaque
		// "encoding overruns Uint8Array". Failing here lets the caller re-quote.
		const addressLookupTables: AddressLookupTableAccount[] = [];
		if (route.addressLookupTables && route.addressLookupTables.length > 0) {
			for (const altPubkey of route.addressLookupTables) {
				addressLookupTables.push(
					await this.fetchLookupTable(new PublicKey(altPubkey))
				);
			}
		}

		const transactionMessage = new TransactionMessage({
			payerKey: userPublicKey,
			recentBlockhash: blockhash,
			instructions: solanaInstructions,
		});

		return { transactionMessage, lookupTables: addressLookupTables };
	}
}
