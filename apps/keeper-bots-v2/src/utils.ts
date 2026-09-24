import { base64 } from '@project-serum/anchor/dist/cjs/utils/bytes';
import { logger } from './logger';
import {
	BN,
	VelocityClient,
	VelocityEnv,
	VelocityMarketInfo,
	MarketType,
	OraclePriceData,
	PERCENTAGE_PRECISION,
	PRICE_PRECISION,
	PerpMarketAccount,
	QUOTE_PRECISION,
	SpotMarketAccount,
	User,
	Wallet,
	convertToNumber,
	getVariant,
	isOneOfVariant,
	SpotMarketConfig,
	PerpMarketConfig,
	OracleInfo,
	PythLazerSubscriber,
	PriorityFeeSubscriberMap,
	decodeName,
	loadKeypair,
	getSizeOfTransaction as getTransactionByteSize,
} from '@velocity-exchange/sdk';
import {
	createAssociatedTokenAccountInstruction,
	getAssociatedTokenAddress,
} from '@solana/spl-token';
import { PythLazerSubscriber as PythLazerSubscriberDeprecated } from './pythLazerSubscriber';
import {
	AddressLookupTableAccount,
	ComputeBudgetProgram,
	Connection,
	Keypair,
	PublicKey,
	Transaction,
	TransactionError,
	TransactionInstruction,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import { webhookMessage } from './webhook';

export { decodeName, loadKeypair };

// devnet only
export const TOKEN_FAUCET_PROGRAM_ID = new PublicKey(
	'V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB'
);

export const PRIORITY_FEE_SERVER_RATE_LIMIT_PER_MIN = 300;

export async function getOrCreateAssociatedTokenAccount(
	connection: Connection,
	mint: PublicKey,
	wallet: Wallet
): Promise<PublicKey> {
	const associatedTokenAccount = await getAssociatedTokenAddress(
		mint,
		wallet.publicKey
	);

	const accountInfo = await connection.getAccountInfo(associatedTokenAccount);
	if (accountInfo == null) {
		const tx = new Transaction().add(
			createAssociatedTokenAccountInstruction(
				wallet.publicKey,
				associatedTokenAccount,
				wallet.publicKey,
				mint
			)
		);
		const txSig = await connection.sendTransaction(tx, [wallet.payer]);
		const latestBlock = await connection.getLatestBlockhash();
		await connection.confirmTransaction(
			{
				signature: txSig,
				blockhash: latestBlock.blockhash,
				lastValidBlockHeight: latestBlock.lastValidBlockHeight,
			},
			'confirmed'
		);
	}

	return associatedTokenAccount;
}

export function loadCommaDelimitToArray(str: string): number[] {
	try {
		return str
			.split(',')
			.filter((element) => {
				if (element.trim() === '') {
					return false;
				}

				return !isNaN(Number(element));
			})
			.map((element) => {
				return Number(element);
			});
	} catch (e) {
		return [];
	}
}

export function parsePositiveIntArray(
	intArray: string | undefined,
	separator = ','
): number[] | undefined {
	if (!intArray) {
		return undefined;
	}
	return intArray
		.split(separator)
		.map((s) => s.trim())
		.map((s) => parseInt(s))
		.filter((n) => !isNaN(n) && n >= 0);
}

export function loadCommaDelimitToStringArray(str: string): string[] {
	try {
		return str.split(',').filter((element) => {
			return element.trim() !== '';
		});
	} catch (e) {
		return [];
	}
}

export function convertToMarketType(input: string): MarketType {
	switch (input.toUpperCase()) {
		case 'PERP':
			return MarketType.PERP;
		case 'SPOT':
			return MarketType.SPOT;
		default:
			throw new Error(`Invalid market type: ${input}`);
	}
}

export function getWallet(privateKeyOrFilepath: string): [Keypair, Wallet] {
	const keypair = loadKeypair(privateKeyOrFilepath);
	return [keypair, new Wallet(keypair)];
}

export function sleepMs(ms: number) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

export function sleepS(s: number) {
	return sleepMs(s * 1000);
}

export async function waitForAllSubscribesToFinish(
	subscriptionPromises: Promise<boolean>[]
): Promise<boolean> {
	const results = await Promise.all(subscriptionPromises);
	const falsePromises = subscriptionPromises.filter(
		(_, index) => !results[index]
	);
	if (falsePromises.length > 0) {
		logger.info('waiting to subscribe to VelocityClient and User');
		await sleepMs(1000);
		return waitForAllSubscribesToFinish(falsePromises);
	} else {
		return true;
	}
}

export function calculateAccountValueUsd(user: User): number {
	const netSpotValue = convertToNumber(
		user.getNetSpotMarketValue(),
		QUOTE_PRECISION
	);
	const unrealizedPnl = convertToNumber(
		user.getUnrealizedPNL(true, undefined, undefined),
		QUOTE_PRECISION
	);
	return netSpotValue + unrealizedPnl;
}

export function calculateBaseAmountToMarketMakePerp(
	perpMarketAccount: PerpMarketAccount,
	user: User,
	targetLeverage = 1
) {
	const basePriceNormed = convertToNumber(
		perpMarketAccount.marketStats.historicalOracleData.lastOraclePriceTwap
	);

	const accountValueUsd = calculateAccountValueUsd(user);

	targetLeverage *= 0.95;

	const maxBase = (accountValueUsd / basePriceNormed) * targetLeverage;
	const marketSymbol = decodeName(perpMarketAccount.name);

	logger.info(
		`(mkt index: ${marketSymbol}) base to market make (targetLvg=${targetLeverage.toString()}): ${maxBase.toString()} = ${accountValueUsd.toString()} / ${basePriceNormed.toString()} * ${targetLeverage.toString()}`
	);

	return maxBase;
}

export function calculateBaseAmountToMarketMakeSpot(
	spotMarketAccount: SpotMarketAccount,
	user: User,
	targetLeverage = 1
) {
	const basePriceNormalized = convertToNumber(
		spotMarketAccount.historicalOracleData.lastOraclePriceTwap
	);

	const accountValueUsd = calculateAccountValueUsd(user);

	targetLeverage *= 0.95;

	const maxBase = (accountValueUsd / basePriceNormalized) * targetLeverage;
	const marketSymbol = decodeName(spotMarketAccount.name);

	logger.info(
		`(mkt index: ${marketSymbol}) base to market make (targetLvg=${targetLeverage.toString()}): ${maxBase.toString()} = ${accountValueUsd.toString()} / ${basePriceNormalized.toString()} * ${targetLeverage.toString()}`
	);

	return maxBase;
}

export function isMarketVolatile(
	perpMarketAccount: PerpMarketAccount,
	oraclePriceData: OraclePriceData,
	volatileThreshold = 0.005 // 50 bps
) {
	const twapPrice =
		perpMarketAccount.marketStats.historicalOracleData.lastOraclePriceTwap5Min;
	const lastPrice =
		perpMarketAccount.marketStats.historicalOracleData.lastOraclePrice;
	const currentPrice = oraclePriceData.price;
	const minDenom = BN.min(BN.min(currentPrice, lastPrice), twapPrice);
	const cVsL =
		Math.abs(
			currentPrice.sub(lastPrice).mul(PRICE_PRECISION).div(minDenom).toNumber()
		) / PERCENTAGE_PRECISION.toNumber();
	const cVsT =
		Math.abs(
			currentPrice.sub(twapPrice).mul(PRICE_PRECISION).div(minDenom).toNumber()
		) / PERCENTAGE_PRECISION.toNumber();

	const recentStd =
		perpMarketAccount.marketStats.oracleStd
			.mul(PRICE_PRECISION)
			.div(minDenom)
			.toNumber() / PERCENTAGE_PRECISION.toNumber();

	if (
		recentStd > volatileThreshold ||
		cVsT > volatileThreshold ||
		cVsL > volatileThreshold
	) {
		return true;
	}

	return false;
}

export function isSpotMarketVolatile(
	spotMarketAccount: SpotMarketAccount,
	oraclePriceData: OraclePriceData,
	volatileThreshold = 0.005
) {
	const twapPrice =
		spotMarketAccount.historicalOracleData.lastOraclePriceTwap5Min;
	const lastPrice = spotMarketAccount.historicalOracleData.lastOraclePrice;
	const currentPrice = oraclePriceData.price;
	const minDenom = BN.min(BN.min(currentPrice, lastPrice), twapPrice);
	const cVsL =
		Math.abs(
			currentPrice.sub(lastPrice).mul(PRICE_PRECISION).div(minDenom).toNumber()
		) / PERCENTAGE_PRECISION.toNumber();
	const cVsT =
		Math.abs(
			currentPrice.sub(twapPrice).mul(PRICE_PRECISION).div(minDenom).toNumber()
		) / PERCENTAGE_PRECISION.toNumber();

	if (cVsT > volatileThreshold || cVsL > volatileThreshold) {
		return true;
	}

	return false;
}
export function isSetComputeUnitsIx(ix: TransactionInstruction): boolean {
	// Compute budget program discriminator is first byte
	// 2: set compute unit limit
	// 3: set compute unit price
	if (ix.programId.equals(ComputeBudgetProgram.programId) && ix.data[0] === 2) {
		return true;
	}
	return false;
}

const PLACEHOLDER_BLOCKHASH = 'Fdum64WVeej6DeL85REV9NvfSxEJNPZ74DBk7A8kTrKP';
export function getVersionedTransaction(
	payerKey: PublicKey,
	ixs: Array<TransactionInstruction>,
	lookupTableAccounts: AddressLookupTableAccount[],
	recentBlockhash: string
): VersionedTransaction {
	const message = new TransactionMessage({
		payerKey,
		recentBlockhash,
		instructions: ixs,
	}).compileToV0Message(lookupTableAccounts);

	return new VersionedTransaction(message);
}

export type SimulateAndGetTxWithCUsParams = {
	connection: Connection;
	payerPublicKey: PublicKey;
	lookupTableAccounts: AddressLookupTableAccount[];
	/// instructions to simulate and create transaction from
	ixs: Array<TransactionInstruction>;
	/// multiplier to apply to the estimated CU usage, default: 1.0
	cuLimitMultiplier?: number;
	/// minimum CU limit to use, will not use a min CU if not set
	minCuLimit?: number;
	/// set false to only create a tx without simulating for CU estimate
	doSimulation?: boolean;
	/// recentBlockhash to use in the final tx. If undefined, PLACEHOLDER_BLOCKHASH
	/// will be used for simulation, the final tx will have an empty blockhash so
	/// attempts to sign it will throw.
	recentBlockhash?: string;
	/// set true to dump base64 transaction before and after simulating for CUs
	dumpTx?: boolean;
	removeLastIxPostSim?: boolean; // remove the last instruction post simulation (used for fillers)
};

export type SimulateAndGetTxWithCUsResponse = {
	cuEstimate: number;
	simTxLogs: Array<string> | null;
	simError: TransactionError | string | null;
	simSlot: number;
	simTxDuration: number;
	tx: VersionedTransaction;
};

/**
 * Simulates the instructions in order to determine how many CUs it needs,
 * applies `cuLimitMulitplier` to the estimate and inserts or modifies
 * the CU limit request ix.
 *
 * If `recentBlockhash` is provided, it is used as is to generate the final
 * tx. If it is undefined, uses `PLACEHOLDER_BLOCKHASH` which is a valid
 * blockhash to perform simulation and removes it from the final tx. Signing
 * a tx without a valid blockhash will throw.
 * @param params
 * @returns
 */
export async function simulateAndGetTxWithCUs(
	params: SimulateAndGetTxWithCUsParams
): Promise<SimulateAndGetTxWithCUsResponse> {
	if (params.ixs.length === 0) {
		throw new Error('cannot simulate empty tx');
	}

	let setCULimitIxIdx = -1;
	for (let idx = 0; idx < params.ixs.length; idx++) {
		if (isSetComputeUnitsIx(params.ixs[idx])) {
			setCULimitIxIdx = idx;
			break;
		}
	}

	// if we don't have a set CU limit ix, add one to the beginning
	// otherwise the default CU limit for sim is 400k, which may be too low
	if (setCULimitIxIdx === -1) {
		params.ixs.unshift(
			ComputeBudgetProgram.setComputeUnitLimit({
				units: 1_400_000,
			})
		);
		setCULimitIxIdx = 0;
	}
	let simTxDuration = 0;

	const tx = getVersionedTransaction(
		params.payerPublicKey,
		params.ixs,
		params.lookupTableAccounts,
		params.recentBlockhash ?? PLACEHOLDER_BLOCKHASH
	);

	if (!params.doSimulation) {
		return {
			cuEstimate: -1,
			simTxLogs: null,
			simError: null,
			simSlot: -1,
			simTxDuration,
			tx,
		};
	}
	if (params.dumpTx) {
		console.log(`===== Simulating The following transaction =====`);
		const serializedTx = base64.encode(Buffer.from(tx.serialize()));
		console.log(serializedTx);
		console.log(`================================================`);
	}

	let resp;
	try {
		const start = Date.now();
		resp = await params.connection.simulateTransaction(tx, {
			sigVerify: false,
			replaceRecentBlockhash: true,
			commitment: 'processed',
		});
		simTxDuration = Date.now() - start;
	} catch (e) {
		console.error(e);
		logger.error(`Error simulating transaction: ${JSON.stringify(e)}`);
	}
	if (!resp) {
		throw new Error('Failed to simulate transaction');
	}

	if (resp.value.unitsConsumed === undefined) {
		throw new Error(`Failed to get units consumed from simulateTransaction`);
	}

	const simTxLogs = resp.value.logs;
	const cuEstimate = resp.value.unitsConsumed!;
	const cusToUse = Math.max(
		cuEstimate * (params.cuLimitMultiplier ?? 1.0),
		params.minCuLimit ?? 0
	);
	params.ixs[setCULimitIxIdx] = ComputeBudgetProgram.setComputeUnitLimit({
		units: cusToUse,
	});

	const ixsToUse = params.removeLastIxPostSim
		? params.ixs.slice(0, -1)
		: params.ixs;
	const txWithCUs = getVersionedTransaction(
		params.payerPublicKey,
		ixsToUse,
		params.lookupTableAccounts,
		params.recentBlockhash ?? PLACEHOLDER_BLOCKHASH
	);

	if (params.dumpTx) {
		console.log(
			`== Simulation result, cuEstimate: ${cuEstimate}, using: ${cusToUse}, blockhash: ${params.recentBlockhash} ==`
		);
		const serializedTx = base64.encode(Buffer.from(txWithCUs.serialize()));
		console.log(serializedTx);
		console.log(`================================================`);
	}

	// strip out the placeholder blockhash so user doesn't try to send the tx.
	// sending a tx with placeholder blockhash will cause `blockhash not found error`
	// which is suppressed if flight checks are skipped.
	if (!params.recentBlockhash) {
		txWithCUs.message.recentBlockhash = '';
	}

	return {
		cuEstimate,
		simTxLogs,
		simTxDuration,
		simError: resp.value.err,
		simSlot: resp.context.slot,
		tx: txWithCUs,
	};
}

/**
 * Simulates `ixs` and returns a versioned transaction carrying the measured compute
 * unit limit. On a simulation failure it returns a `-1` estimate, which tells the
 * caller to fall back to the maximum limit.
 */
export async function buildVersionedTransactionWithSimulatedCus(
	velocityClient: VelocityClient,
	ixs: Array<TransactionInstruction>,
	luts: Array<AddressLookupTableAccount>,
	cuPriceMicroLamports?: number
): Promise<SimulateAndGetTxWithCUsResponse> {
	const fullIxs = [
		ComputeBudgetProgram.setComputeUnitLimit({
			units: 1_400_000, // the simulation overwrites this
		}),
	];

	if (cuPriceMicroLamports !== undefined) {
		fullIxs.push(
			ComputeBudgetProgram.setComputeUnitPrice({
				microLamports: cuPriceMicroLamports,
			})
		);
	}

	fullIxs.push(...ixs);

	try {
		const recentBlockhash = await velocityClient.connection.getLatestBlockhash(
			'confirmed'
		);

		return await simulateAndGetTxWithCUs({
			ixs: fullIxs,
			connection: velocityClient.connection,
			payerPublicKey: velocityClient.wallet.publicKey,
			lookupTableAccounts: luts,
			cuLimitMultiplier: 1.2,
			doSimulation: true,
			dumpTx: false,
			recentBlockhash: recentBlockhash.blockhash,
		});
	} catch (e) {
		const err = e as Error;
		logger.error(
			`error in buildVersionedTransactionWithSimulatedCus, using max CUs: ${err.message}\n${err.stack}`
		);

		return {
			cuEstimate: -1,
			simTxLogs: null,
			simError: err,
			simTxDuration: -1,
			// @ts-ignore
			tx: undefined,
		};
	}
}

export function handleSimResultError(
	simResult: SimulateAndGetTxWithCUsResponse,
	errorCodesToSuppress: number[],
	msgSuffix: string,
	suppressOutOfCUsMessage = true,
	suppressErrorString?: string
): undefined | number {
	if (
		(simResult.simError as ExtendedTransactionError).InstructionError ===
		undefined
	) {
		return;
	}
	const err = (simResult.simError as ExtendedTransactionError).InstructionError;
	if (!err) {
		return;
	}
	if (err.length < 2) {
		logger.error(
			`${msgSuffix} sim error has no error code. ${JSON.stringify(simResult)}`
		);
		return;
	}
	if (!err[1]) {
		return;
	}

	let errorCode: number | undefined;

	const shouldSuppressByString =
		suppressErrorString !== undefined &&
		Array.isArray(simResult.simTxLogs) &&
		simResult.simTxLogs.some((line) => line.includes(suppressErrorString));

	if (typeof err[1] === 'object' && 'Custom' in err[1]) {
		const customErrorCode = Number((err[1] as CustomError).Custom);
		errorCode = customErrorCode;
		if (errorCodesToSuppress.includes(customErrorCode)) {
			return errorCode;
		} else {
			const msg = `${msgSuffix} sim error with custom error code, simError: ${JSON.stringify(
				simResult.simError
			)}, cuEstimate: ${simResult.cuEstimate}, sim logs:\n${
				simResult.simTxLogs ? simResult.simTxLogs.join('\n') : 'none'
			}`;
			if (!shouldSuppressByString) {
				webhookMessage(msg, process.env.TX_LOG_WEBHOOK_URL);
			}
			logger.error(msg);
		}
	} else {
		const msg = `${msgSuffix} sim error with no error code, simError: ${JSON.stringify(
			simResult.simError
		)}, cuEstimate: ${simResult.cuEstimate}, sim logs:\n${
			simResult.simTxLogs ? simResult.simTxLogs.join('\n') : 'none'
		}`;
		logger.error(msg);

		// early return if out of CU error.
		if (
			suppressOutOfCUsMessage &&
			simResult.simTxLogs &&
			simResult.simTxLogs[simResult.simTxLogs.length - 1].includes(
				'exceeded CUs meter at BPF instruction'
			)
		) {
			return errorCode;
		}

		if (!shouldSuppressByString) {
			webhookMessage(msg, process.env.TX_LOG_WEBHOOK_URL);
		}
	}

	return errorCode;
}

export interface ExtendedTransactionError {
	InstructionError?: [number, string | object];
}

export interface CustomError {
	Custom?: number;
}

/**
 * Emits one wide event: the whole log message is one JSON object,
 * `{"event":"<name>", ...}`, snake_case keys serialized alphabetically to
 * match serde_json's BTreeMap ordering in keep-rs's `tx_event`
 * (`rust/keep-rs/src/filler.rs`). The Order Trace Grafana dashboard extracts
 * the JSON from the first `{` to the last `}` on the line, so the message
 * must be nothing but the JSON, and an `undefined` field is dropped rather
 * than serialized as `null`. Never throws: the confirmation loop calls this
 * inside a `try` that would abort the whole batch on an escaping exception.
 */
export function logWideEvent(
	event: string,
	fields: Record<string, unknown>
): void {
	try {
		const payload: Record<string, unknown> = {};
		const keys = Object.keys(fields).concat('event').sort();
		for (const key of keys) {
			const value = key === 'event' ? event : fields[key];
			if (value !== undefined) {
				payload[key] = value;
			}
		}
		logger.info(JSON.stringify(payload));
	} catch (e) {
		logger.error(
			`logWideEvent failed for event ${event}: ${
				e instanceof Error ? e.message : e
			}`
		);
	}
}

export function getVelocityPriorityFeeEndpoint(
	velocityEnv: VelocityEnv
): string {
	switch (velocityEnv) {
		case 'devnet':
			return 'https://dlob.master.velocity.exchange';
		case 'mainnet-beta':
			return 'https://dlob.velocity.exchange';
	}
}

/** The endpoint fields a bot reads to reach a priority fee source. */
export type PriorityFeeEndpointConfig = {
	priorityFeeEndpoint?: string;
	velocityEnv: VelocityEnv;
};

/**
 * Lists the markets a bot tracks priority fees for.
 * @param include which market types to list; both default to false.
 */
export function priorityFeeMarkets(
	velocityClient: VelocityClient,
	include: { perp?: boolean; spot?: boolean }
): VelocityMarketInfo[] {
	const markets: VelocityMarketInfo[] = [];

	if (include.perp) {
		for (const perpMarket of velocityClient.getPerpMarketAccounts()) {
			markets.push({
				marketType: 'perp',
				marketIndex: perpMarket.marketIndex,
			});
		}
	}

	if (include.spot) {
		for (const spotMarket of velocityClient.getSpotMarketAccounts()) {
			markets.push({
				marketType: 'spot',
				marketIndex: spotMarket.marketIndex,
			});
		}
	}

	return markets;
}

/**
 * Subscribes a priority fee map for `velocityMarkets`. The endpoint comes from the
 * configured value first, then from the default for the configured environment. A
 * hardcoded environment here would point every deployment at the production dlob.
 */
export async function subscribePriorityFeeMap(
	velocityMarkets: VelocityMarketInfo[],
	globalConfig: PriorityFeeEndpointConfig
): Promise<PriorityFeeSubscriberMap> {
	const priorityFeeSubscriberMap = new PriorityFeeSubscriberMap({
		velocityPriorityFeeEndpoint:
			globalConfig.priorityFeeEndpoint ??
			getVelocityPriorityFeeEndpoint(globalConfig.velocityEnv),
		velocityMarkets,
		frequencyMs: 10_000,
	});
	await priorityFeeSubscriberMap.subscribe();

	return priorityFeeSubscriberMap;
}

export const getAllPythOracleUpdateIxs = async (
	marketIndex: number,
	velocityClient: VelocityClient,
	pythLazerSubscriber?: PythLazerSubscriber | PythLazerSubscriberDeprecated,
	precedingIxs: TransactionInstruction[] = []
): Promise<TransactionInstruction[]> => {
	const updateMessage =
		await pythLazerSubscriber?.getLatestPriceMessageForMarketIndex(marketIndex);
	const feedIds =
		pythLazerSubscriber?.getPriceFeedIdsFromMarketIndex(marketIndex);
	if (!updateMessage || !feedIds) {
		logger.debug(
			'No update message or feed ids found for marketIndex',
			marketIndex
		);
		return [];
	}
	return await velocityClient.getPostPythLazerOracleUpdateIxs(
		feedIds,
		updateMessage,
		precedingIxs
	);
};

export function canFillSpotMarket(spotMarket: SpotMarketAccount): boolean {
	if (
		isOneOfVariant(spotMarket.status, ['initialized', 'fillPaused', 'delisted'])
	) {
		logger.info(
			`Skipping market ${decodeName(
				spotMarket.name
			)} because its SpotMarket.status is ${getVariant(spotMarket.status)}`
		);
		return false;
	}
	return true;
}

export const chunks = <T>(array: readonly T[], size: number): T[][] => {
	return new Array(Math.ceil(array.length / size))
		.fill(null)
		.map((_, index) => index * size)
		.map((begin) => array.slice(begin, begin + size));
};

export const shuffle = <T>(array: T[]): T[] => {
	let currentIndex = array.length,
		randomIndex;

	while (currentIndex !== 0) {
		randomIndex = Math.floor(Math.random() * currentIndex);
		currentIndex--;
		[array[currentIndex], array[randomIndex]] = [
			array[randomIndex],
			array[currentIndex],
		];
	}

	return array;
};

/**
 * Wraps the SDK's transaction sizer and also reports the account count the
 * instructions reference before any lookup table resolves them.
 */
export function getSizeOfTransaction(
	instructions: TransactionInstruction[],
	versionedTransaction = true,
	addressLookupTables: AddressLookupTableAccount[] = []
): { bytes: number; accounts: number } {
	const accounts = new Set<string>();
	for (const ix of instructions) {
		accounts.add(ix.programId.toBase58());
		for (const key of ix.keys) {
			accounts.add(key.pubkey.toBase58());
		}
	}

	return {
		bytes: getTransactionByteSize(
			instructions,
			versionedTransaction,
			addressLookupTables
		),
		accounts: accounts.size,
	};
}

export async function checkIfAccountExists(
	connection: Connection,
	account: PublicKey
): Promise<boolean> {
	try {
		const accountInfo = await connection.getAccountInfo(account);
		return accountInfo != null;
	} catch (e) {
		// Doesn't already exist
		return false;
	}
}

export function getMarketsAndOracleInfosToLoad(
	sdkConfig: any,
	perpMarketIndicies: number[] | undefined,
	spotMarketIndicies: number[] | undefined
): {
	oracleInfos: OracleInfo[] | undefined;
	perpMarketIndicies: number[] | undefined;
	spotMarketIndicies: number[] | undefined;
} {
	// When neither markets list is specified, leave everything undefined so
	// VelocityClient falls back to findAllMarketAndOracles and discovers every
	// market and oracle from on-chain state. Building the lists from the SDK's
	// static registry here would pin the bots to the registry compiled into the
	// installed SDK version, making them blind to markets listed after that
	// release until a new SDK + image ships.
	if (!perpMarketIndicies && !spotMarketIndicies) {
		logger.info(
			'No perp/spot markets specified; discovering all markets and oracles from on-chain state'
		);
		return {
			oracleInfos: undefined,
			perpMarketIndicies: undefined,
			spotMarketIndicies: undefined,
		};
	}

	const oracleInfos: OracleInfo[] = [];
	const oraclesTracked = new Set();

	const perpIndexes = perpMarketIndicies ?? [];
	const spotIndexes = spotMarketIndicies ?? [];

	if (perpIndexes && perpIndexes.length > 0) {
		for (const idx of perpIndexes) {
			const perpMarketConfig = sdkConfig.PERP_MARKETS[idx] as PerpMarketConfig;
			if (!perpMarketConfig) {
				throw new Error(`Perp market config for ${idx} not found`);
			}
			const oracleKey =
				perpMarketConfig.oracle.toBase58() +
				getVariant(perpMarketConfig.oracleSource);
			if (!oraclesTracked.has(oracleKey)) {
				logger.info(`Tracking oracle ${oracleKey} for perp market ${idx}`);
				oracleInfos.push({
					publicKey: perpMarketConfig.oracle,
					source: perpMarketConfig.oracleSource,
				});
				oraclesTracked.add(oracleKey);
			}
		}
		logger.info(`Bot tracking perp markets: ${JSON.stringify(perpIndexes)}`);
	}

	if (spotIndexes && spotIndexes.length > 0) {
		for (const idx of spotIndexes) {
			const spotMarketConfig = sdkConfig.SPOT_MARKETS[idx] as SpotMarketConfig;
			if (!spotMarketConfig) {
				throw new Error(`Spot market config for ${idx} not found`);
			}
			const oracleKey = spotMarketConfig.oracle.toBase58();
			if (!oraclesTracked.has(oracleKey)) {
				logger.info(`Tracking oracle ${oracleKey} for spot market ${idx}`);
				oracleInfos.push({
					publicKey: spotMarketConfig.oracle,
					source: spotMarketConfig.oracleSource,
				});
				oraclesTracked.add(oracleKey);
			}
		}
		logger.info(`Bot tracking spot markets: ${JSON.stringify(spotIndexes)}`);
	}

	return {
		oracleInfos,
		perpMarketIndicies:
			perpIndexes && perpIndexes.length > 0 ? perpIndexes : undefined,
		spotMarketIndicies:
			spotIndexes && spotIndexes.length > 0 ? spotIndexes : undefined,
	};
}

export function isSolLstToken(spotMarketIndex: number): boolean {
	return [
		2, // mSOL
		6, // jitoSOL
		8, // bSOL
		16, // INF
		17, // dSOL
		25, // BNSOL
	].includes(spotMarketIndex);
}

/** The compute-unit price instruction, in micro-lamports per compute unit. */
export const getPriorityFeeInstruction = (
	priorityFeeMicroLamports: number
): TransactionInstruction => {
	return ComputeBudgetProgram.setComputeUnitPrice({
		microLamports: priorityFeeMicroLamports,
	});
};
