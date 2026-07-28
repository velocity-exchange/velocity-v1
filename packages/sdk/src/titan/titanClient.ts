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
 * A u64 as msgpack decodes it under `useBigInt64`: `bigint` when the server
 * encoded a full 64-bit int, `number` for the narrower encodings it uses for
 * small values.
 *
 * Never route one through `Number()` or arithmetic to produce an amount — u64
 * token amounts above 2^53 don't survive the conversion, and the loss is silent.
 * `String()`/`.toString()` is exact for both halves of the union.
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

/** Retries for a route's lookup tables, which must all resolve for the tx to fit. */
const LOOKUP_TABLE_FETCH_RETRIES = 2;
const LOOKUP_TABLE_RETRY_BASE_DELAY_MS = 150;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Titan sends pubkeys as raw bytes. Absent fields decode to `undefined`. */
const decodePubkey = (bytes?: Uint8Array): string | undefined =>
	bytes ? new PublicKey(bytes).toString() : undefined;

/**
 * For the normalized quote's small metadata fields, which are typed `number`.
 * Only safe because slots, durations and bps are far below 2^53 — never use it
 * on a token amount.
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
			// Only sent when explicitly true — Titan treats the field's presence,
			// not its value, as the toggle.
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
		// `useBigInt64` or every u64 above 2^53 is silently rounded on the way in,
		// including the `inAmount` callers size `beginSwap` off.
		const data = decode(buffer, { useBigInt64: true }) as SwapQuotes;

		// We are only querying for the best avaiable route so use that
		const route = data.quotes[Object.keys(data.quotes)[0]];

		if (!route) {
			throw new Error('No routes available');
		}

		if (!route.instructions?.length) {
			throw new Error('Titan route has no instructions');
		}

		// Titan echoes the pair it routed. Take the pair from the response rather
		// than assuming the request was honoured — a route for another pair pays
		// out into a token account `endSwap` isn't watching, and only fails
		// on-chain once the funds have already moved.
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
				// The route's own input, not the requested amount — under ExactOut the
				// request is the output, and callers size `beginSwap` off `inAmount`.
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
	 * returns instructions rather than a transaction, so unlike Jupiter there is
	 * nothing to strip — the route already includes its own setup and teardown.
	 * @throws If the quote came from a different provider or a different wallet,
	 * or if a lookup table the route depends on can't be loaded.
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
	 * Fetches a lookup table required by a route, retrying transient RPC
	 * failures (rate limiting in particular) before giving up. Checks the
	 * instance cache first and populates it on a fresh fetch.
	 * @throws If the table still can't be loaded, or doesn't exist on-chain.
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
				// Transient — rate limiting, connection reset. Worth another go.
				lastError = err;
				continue;
			}

			if (altAccount.value) {
				this.lookupTableCache.set(altPubkey.toString(), altAccount.value);
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

		// These all have to resolve. A table that fails to load isn't a slightly
		// worse route — every account it would have compressed to a 1-byte index
		// gets inlined as a 32-byte pubkey instead, which pushes the transaction
		// past the size limit and only surfaces later as an opaque
		// "encoding overruns Uint8Array". Failing here lets the caller re-quote.
		const lookupTables = await Promise.all(
			(route.addressLookupTables ?? []).map((altPubkey) =>
				this.fetchLookupTable(new PublicKey(altPubkey))
			)
		);

		return { instructions, lookupTables };
	}
}
