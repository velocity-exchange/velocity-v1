import {
	Connection,
	PublicKey,
	AddressLookupTableAccount,
	TransactionInstruction,
	TransactionMessage,
	VersionedTransaction,
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
	buildSwapQuote,
	expectProviderRoute,
} from '../swap/types';

export enum SwapMode {
	ExactIn = 'ExactIn',
	ExactOut = 'ExactOut',
}

/**
 * A u64 as msgpack decodes it under `useBigInt64`: `bigint` for a full 64-bit
 * int, `number` for the server's narrower small-value encodings. Never pass
 * one through `Number()` or arithmetic; a token amount above 2^53 silently
 * loses precision. `String()` and `.toString()` are exact for both halves.
 */
type U64 = bigint | number;

interface RoutePlanStep {
	ammKey: Uint8Array;
	label: string;
	inputMint: Uint8Array;
	outputMint: Uint8Array;
	inAmount: U64;
	outAmount: U64;
	allocPpb: U64;
	feeMint?: Uint8Array;
	feeAmount?: U64;
	contextSlot?: U64;
}

interface PlatformFee {
	amount: U64;
	fee_bps: U64;
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
	inAmount: U64;
	outAmount: U64;
	slippageBps: U64;
	platformFee?: PlatformFee;
	steps: RoutePlanStep[];
	instructions: Instruction[];
	addressLookupTables: Pubkey[];
	contextSlot?: U64;
	timeTaken?: U64;
	expiresAtMs?: U64;
	expiresAfterSlot?: U64;
	computeUnits?: U64;
	computeUnitsSafe?: U64;
	transaction?: Uint8Array;
	referenceId?: string;
}

interface SwapQuotes {
	id: string;
	inputMint?: Uint8Array;
	outputMint?: Uint8Array;
	swapMode: SwapMode;
	amount: U64;
	quotes: { [key: string]: SwapRoute };
}

const TITAN_API_URL = 'https://api.titan.exchange';

/**
 * Retries for a route's lookup tables. Every table must resolve for the
 * transaction to fit.
 */
const LOOKUP_TABLE_FETCH_RETRIES = 2;
const LOOKUP_TABLE_RETRY_BASE_DELAY_MS = 150;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Titan sends pubkeys as raw bytes. Absent fields decode to `undefined`. */
const decodePubkey = (bytes?: Uint8Array): string | undefined =>
	bytes ? new PublicKey(bytes).toString() : undefined;

/**
 * For the normalized quote's small metadata fields, which are typed `number`.
 * This is safe only because slots, durations and bps stay far below 2^53. Never
 * use it on a token amount.
 */
const toNumber = (value?: U64): number | undefined =>
	value === undefined ? undefined : Number(value);

export class TitanClient implements SwapProvider {
	public readonly providerName = 'titan';

	authToken: string;
	url: string;
	connection: Connection;
	proxyUrl?: string;
	lookupTableCache = new Map<string, AddressLookupTableAccount>();

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
			...(slippageBps != null && { slippageBps: slippageBps.toString() }),
			...(swapMode != null && { swapMode: normalizedSwapMode.toString() }),
			...(maxAccounts != null && {
				accountsLimitTotal: maxAccounts.toString(),
			}),
			...(excludeDexes != null && { excludeDexes: excludeDexes.join(',') }),
			// Sent only when the caller passes true. Titan reads the field's
			// presence as the toggle and ignores its value.
			...(onlyDirectRoutes === true && {
				onlyDirectRoutes: onlyDirectRoutes.toString(),
			}),
			...(sizeConstraint != null && {
				sizeConstraint: sizeConstraint.toString(),
			}),
			...(accountsLimitWritable != null && {
				accountsLimitWritable: accountsLimitWritable.toString(),
			}),
		});
	}

	/**
	 * Get the best available route for a swap.
	 *
	 * The quote carries the route on its `providerRoute`, so
	 * {@link getRouteInstructions} builds exactly what this call quoted, at the
	 * slippage this call quoted.
	 * @throws If `userPublicKey` is missing. Titan writes the user's token
	 * accounts into the route, so one wallet cannot execute a route quoted for
	 * another. The route records the wallet, and the build step enforces it.
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
		// Decode with `useBigInt64`. Without it every u64 above 2^53 rounds on the
		// way in, including the `inAmount` that callers size `beginSwap` from.
		const data = decode(buffer, { useBigInt64: true }) as SwapQuotes;

		// The request asks for the best available route only, so take the first quote.
		const route = data.quotes[Object.keys(data.quotes)[0]];

		if (!route) {
			throw new Error('No routes available');
		}

		if (!route.instructions?.length) {
			throw new Error('Titan route has no instructions');
		}

		// Titan echoes the pair it routed. Read the pair from the response rather
		// than assume the request was honoured. A route for another pair pays out
		// into a token account that `endSwap` does not watch. It then fails on
		// chain only after the funds have already moved.
		const routedInputMint =
			decodePubkey(data.inputMint) ?? inputMint.toString();
		const routedOutputMint =
			decodePubkey(data.outputMint) ?? outputMint.toString();

		if (
			routedInputMint !== inputMint.toString() ||
			routedOutputMint !== outputMint.toString()
		) {
			throw new Error(
				`Titan quoted ${routedInputMint} -> ${routedOutputMint} but the swap asked for ${inputMint.toString()} -> ${outputMint.toString()}.`
			);
		}

		return buildSwapQuote(
			{
				inputMint: routedInputMint,
				outputMint: routedOutputMint,
				// The route's own input, and not the requested amount. Under ExactOut
				// the request is the output, and callers size `beginSwap` from
				// `inAmount`.
				inAmount: (route.inAmount ?? amount).toString(),
				outAmount: route.outAmount.toString(),
				swapMode: data.swapMode,
				slippageBps: Number(route.slippageBps),
				platformFee: route.platformFee
					? {
							amount: route.platformFee.amount.toString(),
							feeBps: Number(route.platformFee.fee_bps),
					  }
					: undefined,
				routePlan:
					route.steps?.map((step) => ({
						swapInfo: {
							ammKey: new PublicKey(step.ammKey).toString(),
							label: step.label,
							inputMint: new PublicKey(step.inputMint).toString(),
							outputMint: new PublicKey(step.outputMint).toString(),
							inAmount: step.inAmount.toString(),
							outAmount: step.outAmount.toString(),
							feeAmount: step.feeAmount?.toString() || '0',
							feeMint: step.feeMint
								? new PublicKey(step.feeMint).toString()
								: '',
						},
						percent: 100,
					})) || [],
				contextSlot: toNumber(route.contextSlot),
				timeTaken: toNumber(route.timeTaken),
			},
			{ provider: 'titan', route, quotedFor: userPublicKey.toString() }
		);
	}

	/**
	 * The route as Titan built it, compiled into a signable transaction. Titan
	 * returns instructions rather than a transaction, so this method strips
	 * nothing. The route already includes its own setup and teardown.
	 * @throws If the quote came from a different provider or a different wallet,
	 * or if a lookup table the route depends on fails to load.
	 */
	public async getSwapTransaction({
		quote,
		userPublicKey,
	}: GetRouteInstructionsParams): Promise<VersionedTransaction> {
		const route = expectProviderRoute(quote, 'titan', userPublicKey)
			.route as SwapRoute;

		if (!route.instructions?.length) {
			throw new Error('No instructions provided in the route');
		}

		const [{ instructions, lookupTables }, { blockhash }] = await Promise.all([
			this.getInstructionsAndLookupTables(route),
			this.connection.getLatestBlockhash(),
		]);

		return new VersionedTransaction(
			new TransactionMessage({
				payerKey: userPublicKey,
				recentBlockhash: blockhash,
				instructions,
			}).compileToV0Message(lookupTables)
		);
	}

	/**
	 * Builds the route instructions for a quote returned by {@link getQuote}.
	 *
	 * The quote carries the route, so this method reads no client state and cannot
	 * confuse two quotes that are in flight together. Titan fixed the slippage
	 * when it built the route, so this method applies none.
	 * @throws If the quote came from a different provider or a different wallet,
	 * or if a lookup table the route depends on fails to load.
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

		// Errors propagate unchanged. A generic message here would drop the reason
		// the swap cannot be built, such as a lookup table that does not resolve.
		// The caller needs that reason to decide whether a new quote helps.
		const { instructions, lookupTables } =
			await this.getInstructionsAndLookupTables(route);

		return {
			instructions: filterRouteInstructions({
				instructions,
				inputMint: new PublicKey(quote.inputMint),
				outputMint: new PublicKey(quote.outputMint),
			}),
			lookupTables,
		};
	}

	/**
	 * Fetches a lookup table that a route requires. It retries a transient RPC
	 * failure, such as rate limiting, before it fails. It reads the instance cache
	 * first and fills the cache on a fresh fetch.
	 * @throws If the table still fails to load, or does not exist on chain.
	 */
	private async fetchLookupTable(
		altPubkey: PublicKey
	): Promise<AddressLookupTableAccount> {
		const cached = this.lookupTableCache.get(altPubkey.toString());
		if (cached !== undefined) {
			return cached;
		}

		let lastError: unknown;

		for (let attempt = 0; attempt <= LOOKUP_TABLE_FETCH_RETRIES; attempt++) {
			if (attempt > 0) {
				await sleep(LOOKUP_TABLE_RETRY_BASE_DELAY_MS * 2 ** (attempt - 1));
			}

			let altAccount: Awaited<ReturnType<Connection['getAddressLookupTable']>>;

			try {
				altAccount = await this.connection.getAddressLookupTable(altPubkey);
			} catch (err) {
				// A transient failure, such as rate limiting or a connection reset.
				lastError = err;
				continue;
			}

			if (altAccount.value) {
				this.lookupTableCache.set(altPubkey.toString(), altAccount.value);
				return altAccount.value;
			}

			// A successful response with no value means the route names a table that
			// is not on chain. A retry does not create it.
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

	private async getInstructionsAndLookupTables(route: SwapRoute): Promise<{
		instructions: TransactionInstruction[];
		lookupTables: AddressLookupTableAccount[];
	}> {
		const instructions: TransactionInstruction[] = route.instructions.map(
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

		// Every table must resolve. When one fails to load, each account it would
		// have compressed to a 1-byte index goes inline as a 32-byte pubkey. That
		// pushes the transaction past the size limit, and the failure then appears
		// as "encoding overruns Uint8Array". Failing here lets the caller re-quote.
		const lookupTables = await Promise.all(
			(route.addressLookupTables ?? []).map((altPubkey) =>
				this.fetchLookupTable(new PublicKey(altPubkey))
			)
		);

		return { instructions, lookupTables };
	}
}
