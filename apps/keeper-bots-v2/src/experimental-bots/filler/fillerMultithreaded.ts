/* eslint-disable @typescript-eslint/no-non-null-assertion */
import {
	AverageOverSlotsStrategy,
	BlockhashSubscriber,
	BN,
	DataAndSlot,
	decodeUser,
	DLOBNode,
	VelocityClient,
	FeeTier,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
	getUserWithoutOrderFilter,
	isFillableByVAMM,
	isOneOfVariant,
	isOrderExpired,
	isVariant,
	JupiterClient,
	MakerInfo,
	MarketType,
	NodeToFill,
	PerpMarkets,
	PriorityFeeSubscriber,
	QUOTE_PRECISION,
	ReferrerInfo,
	ReferrerMap,
	SignedMsgOrderParams,
	SlotSubscriber,
	TxSigAndSlot,
	UserAccount,
	UserMap,
} from '@velocity-exchange/sdk';
import { FillerMultiThreadedConfig, GlobalConfig } from '../../config';
import { JITO_METRIC_TYPES, BundleSender } from '../../bundleSender';
import {
	AddressLookupTableAccount,
	ComputeBudgetProgram,
	Connection,
	LAMPORTS_PER_SOL,
	PACKET_DATA_SIZE,
	PublicKey,
	SendTransactionError,
	TransactionInstruction,
	TransactionSignature,
	VersionedTransaction,
} from '@solana/web3.js';
import { logger } from '../../logger';
import { getErrorCode, getErrorCodeFromSimError } from '../../error';
import { selectMakers } from '../../makerSelection';
import {
	NodeToFillWithBuffer,
	SerializedNodeToFill,
} from '../filler-common/types';
import { assert } from 'console';
import {
	chunks,
	fillCorrelationSuffix,
	getAllPythOracleUpdateIxs,
	getFillSignatureFromUserAccountAndOrderId,
	getNodeToFillSignature,
	getSizeOfTransaction,
	// getStaleOracleMarketIndexes,
	handleSimResultError,
	logMessageForNodeToFill,
	logWideEvent,
	simulateAndGetTxWithCUs,
	SimulateAndGetTxWithCUsResponse,
	sleepMs,
	swapFillerHardEarnedUSDCForSOL,
	validMinimumGasAmount,
	validRebalanceSettledPnlThreshold,
} from '../../utils';
import {
	spawnChild,
	deserializeNodeToFill,
	getPriorityFeeInstruction,
	isTsRuntime,
} from '../filler-common/utils';
import {
	CounterValue,
	GaugeValue,
	HistogramValue,
	metricAttrFromUserAccount,
	Metrics,
	RuntimeSpec,
} from '../../metrics';
import {
	ExplicitBucketHistogramAggregation,
	InstrumentType,
	View,
} from '@opentelemetry/sdk-metrics-base';
import {
	CONFIRM_TX_RATE_LIMIT_BACKOFF_MS,
	TX_TIMEOUT_THRESHOLD_MS,
	TxType,
} from '../../bots/filler';
import { LRUCache } from 'lru-cache';
import {
	isEndIxLog,
	isErrFillingLog,
	isErrStaleOracle,
	isFillIxLog,
	isIxLog,
	isMakerBreachedMaintenanceMarginLog,
	isOrderDoesNotExistLog,
	isTakerBreachedMaintenanceMarginLog,
} from '../../bots/common/txLogParse';
import { bs58 } from '@project-serum/anchor/dist/cjs/utils/bytes';
import { ChildProcess } from 'child_process';
import { PythLazerSubscriber } from '../../pythLazerSubscriber';
import {
	RedisClient,
	RedisClientPrefix,
} from '@velocity-exchange/common/clients';
import path from 'path';

const logPrefix = '[Filler]';
export type MakerNodeMap = Map<string, DLOBNode[]>;

const FILL_ORDER_THROTTLE_BACKOFF = 1000; // the time to wait before trying to fill a throttled (error filling) node again
// Attempt a given order at most once every this many slots. The DLOB builder
// re-emits a still-fillable order every ~200ms; this paces re-attempts. Override
// via FillerMultiThreadedConfig.fillAttemptSlotInterval.
const DEFAULT_FILL_ATTEMPT_SLOT_INTERVAL_SLOTS = 5;

// Validate `fillAttemptSlotInterval` config: only a finite, non-negative integer
// is a meaningful slot count. Anything else (negative / fractional / NaN /
// Infinity) would silently break the pacing comparison in executeFillablePerpNodes
// (e.g. a negative or NaN interval disables pacing entirely), so fall back to the
// default and surface a warning. Omitted (undefined) is not an error — it takes
// the default. Exported for unit testing.
export function resolveFillAttemptSlotInterval(
	raw: number | undefined,
	defaultValue = DEFAULT_FILL_ATTEMPT_SLOT_INTERVAL_SLOTS
): { value: number; warning?: string } {
	if (raw === undefined) {
		return { value: defaultValue };
	}
	if (!Number.isInteger(raw) || raw < 0) {
		return {
			value: defaultValue,
			warning: `invalid fillAttemptSlotInterval ${raw}; expected a non-negative integer, falling back to ${defaultValue}`,
		};
	}
	return { value: raw };
}
// Backstop cap on attempts per order: ~30s market-order lifetime / ~2s (5-slot)
// attempt interval.
const MAX_FILL_ATTEMPTS_PER_ORDER = 15;
// Bound the attempt map so it can't grow for the process lifetime; an order
// lives at most one auction, so a short TTL reaps entries soon after.
const FILL_ATTEMPT_COUNTS_TTL_MS = 2 * 60 * 1000;
const FILL_ATTEMPT_COUNTS_MAX = 10_000;
// Drop-detection window for an in-flight signed-msg place+fill. A signed-msg
// node is guarded against re-attempt from the moment it is launched until its
// tx lands, its send fails, or this TTL elapses. It is deliberately longer than
// normal confirmation latency (a few seconds) so a slow-but-live tx is not
// duplicated, and shorter than a typical swift order's lifetime (~25-30s) so a
// silently-dropped place+fill is retried while the order is still valid and
// still emitted by the DLOB builder.
const SIGNED_MSG_FILL_IN_FLIGHT_TTL_MS = 15_000;
// Wide-event de-duplication. The DLOB builder re-emits a still-fillable order
// every ~200ms, so a `fill_decision` per evaluation would be ~5/s/order of pure
// noise. Each (order, skip reason) pair is therefore wide-logged only the FIRST
// time it occurs; `sent` is exempt (it is already capped by
// MAX_FILL_ATTEMPTS_PER_ORDER and each one has a distinct fill_id / tx event to
// correlate with). The TTL outlives an auction so a decision can't re-emit for
// an order that is still live.
const FILL_DECISION_DEDUPE_TTL_MS = 2 * 60 * 1000;
const FILL_DECISION_DEDUPE_MAX = 20_000;

const THROTTLED_NODE_SIZE_TO_PRUNE = 10; // Size of throttled nodes to get to before pruning the map
export const MAX_MAKERS_PER_FILL = 6; // max number of unique makers to include per fill
const MAX_ACCOUNTS_PER_TX = 64; // solana limit, track https://github.com/solana-labs/solana/issues/27241

const MAX_POSITIONS_PER_USER = 8;
export const SETTLE_POSITIVE_PNL_COOLDOWN_MS = 60_000;
export const CONFIRM_TX_INTERVAL_MS = 5_000;
const SIM_CU_ESTIMATE_MULTIPLIER = 3;
const SLOTS_UNTIL_JITO_LEADER_TO_SEND = 4;
export const TX_CONFIRMATION_BATCH_SIZE = 100;
export const CACHED_BLOCKHASH_OFFSET = 5;
const TX_COUNT_COOLDOWN_ON_BURST = 10; // send this many tx before resetting burst mode

const errorCodesToSuppress = [
	6004, // 0x1774 Error Number: 6004. Error Message: SufficientCollateral.
	6010, // 0x177a Error Number: 6010. Error Message: User Has No Position In Market.
	6081, // 0x17c1 Error Number: 6081. Error Message: MarketWrongMutability.
	// 6078, // 0x17BE Error Number: 6078. Error Message: PerpMarketNotFound
	// 6087, // 0x17c7 Error Number: 6087. Error Message: SpotMarketNotFound.
	6239, // 0x185F Error Number: 6239. Error Message: RevertFill.
	6003, // 0x1773 Error Number: 6003. Error Message: Insufficient collateral.
	6023, // 0x1787 Error Number: 6023. Error Message: PriceBandsBreached.
];

enum METRIC_TYPES {
	try_fill_duration_histogram = 'try_fill_duration_histogram',
	runtime_specs = 'runtime_specs',
	last_try_fill_time = 'last_try_fill_time',
	sent_transactions = 'sent_transactions',
	landed_transactions = 'landed_transactions',
	tx_sim_error_count = 'tx_sim_error_count',
	pending_tx_sigs_to_confirm = 'pending_tx_sigs_to_confirm',
	pending_tx_sigs_loop_rate_limited = 'pending_tx_sigs_loop_rate_limited',
	evicted_pending_tx_sigs_to_confirm = 'evicted_pending_tx_sigs_to_confirm',
	estimated_tx_cu_histogram = 'estimated_tx_cu_histogram',
	simulate_tx_duration_histogram = 'simulate_tx_duration_histogram',
	expired_nodes_set_size = 'expired_nodes_set_size',
}

type DLOBBuilderWithProcess = {
	process: ChildProcess;
	ready: boolean;
	marketIndexes: number[];
};

export class FillerMultithreaded {
	private name: string;
	private slotSubscriber: SlotSubscriber;
	private bundleSender?: BundleSender;
	private velocityClient: VelocityClient;
	private dryRun: boolean;
	private globalConfig: GlobalConfig;
	private config: FillerMultiThreadedConfig;
	private subaccount: number;

	private fillTxId: number = 0;
	private userMap: UserMap;
	private referrerMap: ReferrerMap;
	private throttledNodes = new Map<string, number>();
	private fillingNodes = new Map<string, number>();
	private revertOnFailure?: boolean;
	private lookupTableAccounts: AddressLookupTableAccount[];
	private lastSettlePnl = Date.now() - SETTLE_POSITIVE_PNL_COOLDOWN_MS;
	// Per-order fill-attempt state, keyed by getNodeToFillSignature. `count` feeds
	// the MAX_FILL_ATTEMPTS_PER_ORDER backstop; `lastAttemptSlot` feeds the pacing.
	private fillAttempts = new LRUCache<
		string,
		{ count: number; lastAttemptSlot: number }
	>({
		max: FILL_ATTEMPT_COUNTS_MAX,
		ttl: FILL_ATTEMPT_COUNTS_TTL_MS,
		ttlResolution: 1000,
	});
	// Signatures (getNodeToFillSignature) of signed-msg orders whose place+fill has
	// landed on-chain. Once placed, the order is filled through its on-chain order
	// node, so the signed-msg node must never be place+filled again. This is the
	// authoritative in-process guard; the DLOB-builder eviction (routed on the same
	// landing) additionally stops the builder from re-emitting the dead node.
	private placedSignedMsgOrders = new LRUCache<string, true>({
		max: FILL_ATTEMPT_COUNTS_MAX,
		ttl: FILL_ATTEMPT_COUNTS_TTL_MS,
		ttlResolution: 1000,
	});
	// Signatures of signed-msg orders with a place+fill currently in flight. Set
	// synchronously the instant a fill is launched — before the async
	// build/sim/send/registration chain runs — so the ~200ms DLOB re-emit cannot
	// launch a second place+fill for the same order before the first is tracked.
	// Cleared when the tx lands (confirmPendingTxSigs), when the send definitively
	// fails, or by the TTL (drop detection). See SIGNED_MSG_FILL_IN_FLIGHT_TTL_MS.
	private signedMsgFillsInFlight = new LRUCache<string, true>({
		max: FILL_ATTEMPT_COUNTS_MAX,
		ttl: SIGNED_MSG_FILL_IN_FLIGHT_TTL_MS,
		ttlResolution: 1000,
	});
	// Skip reasons already wide-logged for an order, keyed
	// `${getNodeToFillSignature(node)}:${action}`. See
	// FILL_DECISION_DEDUPE_TTL_MS.
	private emittedFillDecisions = new LRUCache<string, true>({
		max: FILL_DECISION_DEDUPE_MAX,
		ttl: FILL_DECISION_DEDUPE_TTL_MS,
		ttlResolution: 1000,
	});
	// Tx signatures that already reached a terminal `tx` wide event.
	private emittedTxEvents = new LRUCache<string, true>({
		max: FILL_DECISION_DEDUPE_MAX,
		ttl: FILL_DECISION_DEDUPE_TTL_MS,
		ttlResolution: 1000,
	});
	private fillAttemptSlotInterval: number;
	private blockhashSubscriber: BlockhashSubscriber;
	private priorityFeeSubscriber: PriorityFeeSubscriber;

	private dlobHealthy = true;
	private orderSubscriberHealthy = true;
	private swiftOrderSubscriberHealth = true;
	private simulateTxForCUEstimate?: boolean;

	// SignedMsg orders
	private signedMsgOrderMessages: Map<number, any> = new Map();

	private intervalIds: NodeJS.Timeout[] = [];

	protected txConfirmationConnection: Connection;
	protected pendingTxSigsToconfirm: LRUCache<
		string,
		{
			ts: number;
			nodeFilled: Array<NodeToFillWithBuffer>;
			fillTxId: number;
			txType: TxType;
			// Carried so the terminal `tx` wide event can report send->confirm
			// latency in slots and CU headroom the way keep-rs does.
			sentSlot?: number;
			cuLimit?: number;
			fillType?: string;
		}
	>;
	protected expiredNodesSet: LRUCache<string, boolean>;
	protected confirmLoopRunning = false;
	protected confirmLoopRateLimitTs =
		Date.now() - CONFIRM_TX_RATE_LIMIT_BACKOFF_MS;
	protected useBurstCULimit = false;
	protected fillTxSinceBurstCU = 0;

	// metrics
	protected metricsInitialized = false;
	protected metricsPort?: number;
	protected metrics?: Metrics;
	protected bootTimeMs?: number;

	protected runtimeSpec: RuntimeSpec;
	protected runtimeSpecsGauge?: GaugeValue;
	protected estTxCuHistogram?: HistogramValue;
	protected simulateTxHistogram?: HistogramValue;
	protected lastTryFillTimeGauge?: GaugeValue;
	protected sentTxsCounter?: CounterValue;
	protected landedTxsCounter?: CounterValue;
	protected txSimErrorCounter?: CounterValue;
	protected pendingTxSigsToConfirmGauge?: GaugeValue;
	protected pendingTxSigsLoopRateLimitedCounter?: CounterValue;
	protected evictedPendingTxSigsToConfirmCounter?: CounterValue;
	protected expiredNodesSetSize?: GaugeValue;
	protected jitoConnectedGauge?: GaugeValue;
	protected jitoBundlesAcceptedGauge?: GaugeValue;
	protected jitoBundlesSimulationFailureGauge?: GaugeValue;
	protected jitoDroppedBundleGauge?: GaugeValue;
	protected jitoLandedTipsGauge?: GaugeValue;
	protected jitoBundleCount?: GaugeValue;

	protected rebalanceFiller?: boolean;
	protected hasEnoughSolToFill: boolean = true;
	protected minGasBalanceToFill: number;
	protected rebalanceSettledPnlThreshold: BN;

	protected jupiterClient?: JupiterClient;

	protected dlobBuilders: Map<number, DLOBBuilderWithProcess> = new Map();

	protected marketIndexes: Array<number[]>;
	protected marketIndexesFlattened: number[];

	protected pythLazerSubscriber?: PythLazerSubscriber;

	constructor(
		globalConfig: GlobalConfig,
		config: FillerMultiThreadedConfig,
		velocityClient: VelocityClient,
		slotSubscriber: SlotSubscriber,
		runtimeSpec: RuntimeSpec,
		bundleSender?: BundleSender,
		lookupTableAccounts: AddressLookupTableAccount[] = []
	) {
		this.globalConfig = globalConfig;

		this.name = config.botId;
		this.config = config;
		this.dryRun = config.dryRun;
		this.slotSubscriber = slotSubscriber;
		this.velocityClient = velocityClient;
		this.marketIndexes = config.marketIndexes;
		this.revertOnFailure = config.revertOnFailure ?? true;
		this.marketIndexesFlattened = config.marketIndexes.flat();
		this.bundleSender = bundleSender;
		this.simulateTxForCUEstimate = config.simulateTxForCUEstimate ?? true;
		const fillAttemptSlotInterval = resolveFillAttemptSlotInterval(
			config.fillAttemptSlotInterval
		);
		if (fillAttemptSlotInterval.warning) {
			logger.warn(`${logPrefix} ${fillAttemptSlotInterval.warning}`);
		}
		this.fillAttemptSlotInterval = fillAttemptSlotInterval.value;
		if (globalConfig.txConfirmationEndpoint) {
			this.txConfirmationConnection = new Connection(
				globalConfig.txConfirmationEndpoint
			);
		} else {
			this.txConfirmationConnection = this.velocityClient.connection;
		}
		this.lookupTableAccounts = lookupTableAccounts;

		this.userMap = new UserMap({
			velocityClient,
			fastDecode: true,
			includeIdle: false,
			subscriptionConfig: {
				type: 'websocket',
				resubTimeoutMs: 10_000,
				commitment: 'processed',
			},
			additionalFilters: [getUserWithoutOrderFilter()],
			skipInitialLoad: true,
		});
		this.referrerMap = new ReferrerMap(this.velocityClient, true);

		this.blockhashSubscriber = new BlockhashSubscriber({
			connection: velocityClient.connection,
		});

		const marketIndexesToUse = PerpMarkets[this.globalConfig.velocityEnv!].map(
			(m) => m.marketIndex
		);
		const perpMarketsToWatchForFees = marketIndexesToUse.map((m) => {
			return {
				marketType: 'perp',
				marketIndex: m,
			};
		});
		perpMarketsToWatchForFees.push({
			marketType: 'spot',
			marketIndex: 1,
		}); // For rebalancing
		this.priorityFeeSubscriber = new PriorityFeeSubscriber({
			connection: velocityClient.connection,
			frequencyMs: 5000,
			customStrategy: new AverageOverSlotsStrategy(),
			addresses: [],
			maxFeeMicroLamports: this.globalConfig.maxPriorityFeeMicroLamports,
			priorityFeeMultiplier: this.globalConfig.priorityFeeMultiplier ?? 1.0,
		});

		this.subaccount = config.subaccount ?? 0;
		// The on-chain user account for this.subaccount may not exist yet on a
		// fresh deployment; it is created (or added to client tracking) in init().

		this.runtimeSpec = runtimeSpec;
		this.initializeMetrics(config.metricsPort ?? this.globalConfig.metricsPort);

		this.rebalanceFiller = config.rebalanceFiller ?? true;
		if (
			this.rebalanceFiller &&
			this.runtimeSpec.velocityEnv === 'mainnet-beta'
		) {
			this.jupiterClient = new JupiterClient({
				connection: this.velocityClient.connection,
			});
		}
		logger.info(
			`${this.name}: rebalancing enabled: ${this.jupiterClient !== undefined}`
		);
		if (!validMinimumGasAmount(config.minGasBalanceToFill)) {
			this.minGasBalanceToFill = 0.2 * LAMPORTS_PER_SOL;
		} else {
			this.minGasBalanceToFill = config.minGasBalanceToFill! * LAMPORTS_PER_SOL;
		}

		if (
			!validRebalanceSettledPnlThreshold(config.rebalanceSettledPnlThreshold)
		) {
			this.rebalanceSettledPnlThreshold = new BN(20);
		} else {
			this.rebalanceSettledPnlThreshold = new BN(
				config.rebalanceSettledPnlThreshold!
			);
		}

		logger.info(
			`${this.name}: multiThreadedFillerConfig:\n${JSON.stringify(
				config,
				null,
				2
			)}`
		);

		this.pendingTxSigsToconfirm = new LRUCache<
			string,
			{
				ts: number;
				nodeFilled: Array<NodeToFillWithBuffer>;
				fillTxId: number;
				txType: TxType;
				sentSlot?: number;
				cuLimit?: number;
				fillType?: string;
			}
		>({
			max: 10_000,
			ttl: TX_TIMEOUT_THRESHOLD_MS,
			ttlResolution: 1000,
			disposeAfter: this.recordEvictedTxSig.bind(this),
		});

		this.expiredNodesSet = new LRUCache<string, boolean>({
			max: 10_000,
			ttl: TX_TIMEOUT_THRESHOLD_MS,
			ttlResolution: 1000,
		});

		// Pyth lazer: remember to remove devnet guard
		if (!this.globalConfig.lazerEndpoints || !this.globalConfig.lazerToken) {
			throw new Error('Missing lazerEndpoints or lazerToken in global config');
		}

		const markets = PerpMarkets[this.globalConfig.velocityEnv!]
			.filter((market) =>
				this.marketIndexesFlattened.includes(market.marketIndex)
			)
			.filter((market) => market.pythLazerId !== undefined);
		const pythLazerIds = markets.map((m) => m.pythLazerId!);
		if (pythLazerIds.length > 0) {
			const chunkSize = config.pythLazerChunkSize || 2;
			const pythLazerIdsChunks = chunks(pythLazerIds, chunkSize);
			// When pythLazerUseRelayRedis is set, read prices from the
			// pyth-lazer-relayer's Redis instead of opening our own Lazer WS
			// connections — eliminates this bot's contribution to the per-token
			// connection fan-out that rate-limits the shared Lazer token.
			const lazerRedisClient = this.globalConfig.pythLazerUseRelayRedis
				? new RedisClient({ prefix: RedisClientPrefix.DLOB })
				: undefined;
			this.pythLazerSubscriber = new PythLazerSubscriber(
				this.globalConfig.lazerEndpoints,
				this.globalConfig.lazerToken,
				pythLazerIdsChunks.map((ids) => {
					return {
						priceFeedIds: ids,
						channel: 'fixed_rate@200ms',
					};
				}),
				this.globalConfig.velocityEnv,
				lazerRedisClient
			);
		} else {
			logger.info(
				'No pyth lazer ids found, skipping initting PythLazerSubscriber'
			);
		}
	}

	async init() {
		await this.ensureUserAccount();

		await this.blockhashSubscriber.subscribe();
		await this.priorityFeeSubscriber.subscribe();
		await this.pythLazerSubscriber?.subscribe();

		const fillerSolBalance = await this.velocityClient.connection.getBalance(
			this.velocityClient.authority
		);
		this.hasEnoughSolToFill = fillerSolBalance >= this.minGasBalanceToFill;
		logger.info(
			`${this.name}: hasEnoughSolToFill: ${this.hasEnoughSolToFill}, balance: ${fillerSolBalance}`
		);

		await this.userMap.subscribe();
		await this.referrerMap.subscribe();

		this.lookupTableAccounts.push(
			...(await this.velocityClient.fetchAllLookupTableAccounts())
		);
		assert(this.lookupTableAccounts, 'Lookup table account not found');
		this.startProcesses();
	}

	/**
	 * Ensure the on-chain user account for the configured subaccount exists and
	 * is tracked by the client. On a fresh deployment the account won't exist
	 * yet, so create it (and bootstrap sub-0 + UserStats first if needed, since
	 * initializing any subaccount requires UserStats to exist). If it already
	 * exists on chain but isn't tracked, just add it to the client.
	 */
	private async ensureUserAccount(): Promise<void> {
		if (this.velocityClient.hasUser(this.subaccount)) {
			return;
		}

		const userAccountPublicKey = getUserAccountPublicKeySync(
			this.velocityClient.program.programId,
			this.velocityClient.wallet.publicKey,
			this.subaccount
		);
		const accountInfo = await this.velocityClient.connection.getAccountInfo(
			userAccountPublicKey
		);

		if (!accountInfo) {
			// InitializeUser for any subaccount requires UserStats to exist, but
			// only initializeUserAccount(0) creates UserStats. A fresh wallet has
			// neither, so bootstrap sub-0 + UserStats first, then the configured
			// subaccount. (initializeUserAccount also adds the user to the client.)
			const userStatsPublicKey = getUserStatsAccountPublicKey(
				this.velocityClient.program.programId,
				this.velocityClient.wallet.publicKey
			);
			const userStatsInfo = await this.velocityClient.connection.getAccountInfo(
				userStatsPublicKey
			);
			if (!userStatsInfo) {
				logger.info(
					`${this.name}: UserStats does not exist; initializing sub-0 + UserStats`
				);
				await this.velocityClient.initializeUserAccount(0);
			}
			if (this.subaccount !== 0) {
				logger.info(
					`${this.name}: Subaccount ${
						this.subaccount
					} user account ${userAccountPublicKey.toBase58()} does not exist; initializing`
				);
				const [txSig] = await this.velocityClient.initializeUserAccount(
					this.subaccount,
					`filler-${this.subaccount}`
				);
				logger.info(
					`${this.name}: Initialized subaccount ${this.subaccount} user account in tx: ${txSig}`
				);
			}
		} else if (!this.velocityClient.hasUser(this.subaccount)) {
			logger.info(
				`${this.name}: Adding subaccount ${this.subaccount} to velocityClient`
			);
			await this.velocityClient.addUser(this.subaccount);
		}
	}

	private startProcesses() {
		logger.info(`${this.name}: Starting processes`);
		const orderSubscriberArgs = [
			`--velocity-env=${this.runtimeSpec.velocityEnv}`,
			`--market-type=${this.config.marketType}`,
			`--market-indexes=${this.config.marketIndexes.map(String)}`,
		];
		const user = this.velocityClient.getUser(this.subaccount);

		for (const marketIndexes of this.marketIndexes) {
			logger.info(
				`${this.name}: Spawning dlobBuilder for marketIndexes: ${marketIndexes}`
			);
			const dlobBuilderArgs = [
				`--velocity-env=${this.runtimeSpec.velocityEnv}`,
				`--market-type=${this.config.marketType}`,
				`--market-indexes=${marketIndexes.map(String)}`,
			];
			const dlobBuilderFileName =
				'dlobBuilder' + (isTsRuntime() ? '.ts' : '.js');
			const dlobBuilderProcess = spawnChild(
				path.join(
					__dirname,
					isTsRuntime() ? '..' : '.',
					'filler-common',
					dlobBuilderFileName
				),
				dlobBuilderArgs,
				'dlobBuilder',
				(msg: any) => {
					switch (msg.type) {
						case 'initialized':
							{
								const dlobBuilder = this.dlobBuilders.get(msg.data[0]);
								if (dlobBuilder) {
									dlobBuilder.ready = true;
									for (const marketIndex of msg.data) {
										this.dlobBuilders.set(Number(marketIndex), dlobBuilder);
									}
									logger.info(
										`${logPrefix} dlobBuilderProcess initialized and acknowledged`
									);
								}
							}
							break;
						case 'fillableNodes':
							if (this.dryRun) {
								logger.info(`Fillable node received`);
							} else {
								this.fillNodes(msg.data);
							}
							this.lastTryFillTimeGauge?.setLatestValue(
								Date.now(),
								metricAttrFromUserAccount(
									user.getUserAccountPublicKey(),
									user.getUserAccountOrThrow()
								)
							);
							break;
						case 'health':
							this.dlobHealthy = msg.data.healthy;
							break;
					}
				},
				'[FillerMultithreaded]'
			);

			dlobBuilderProcess.on('exit', (code) => {
				logger.error(`dlobBuilder exited with code ${code}`);
				process.exit(code || 1);
			});

			for (const marketIndex of marketIndexes) {
				this.dlobBuilders.set(Number(marketIndex), {
					process: dlobBuilderProcess,
					ready: false,
					marketIndexes: marketIndexes.map(Number),
				});
			}

			logger.info(
				`dlobBuilder spawned with pid: ${dlobBuilderProcess.pid} marketIndexes: ${dlobBuilderArgs}`
			);
		}

		const orderSubscriberFileName =
			'orderSubscriberFiltered' + (isTsRuntime() ? '.ts' : '.js');
		const orderSubscriberProcess = spawnChild(
			path.join(
				__dirname,
				isTsRuntime() ? '..' : '.',
				'filler-common',
				orderSubscriberFileName
			),
			orderSubscriberArgs,
			'orderSubscriber',
			(msg: any) => {
				switch (msg.type) {
					case 'userAccountUpdate':
						this.routeMessageToDlobBuilder(msg);
						break;
					case 'health':
						this.orderSubscriberHealthy = msg.data.healthy;
						break;
				}
			},
			'[FillerMultithreaded]'
		);

		orderSubscriberProcess.on('exit', (code) => {
			logger.error(`dlobBuilder exited with code ${code}`);
			process.exit(code || 1);
		});

		logger.info(
			`orderSubscriber spawned with pid: ${orderSubscriberProcess.pid}`
		);

		// SignedMsg Subscriber process
		const swiftOrderSubscriberFileName =
			'swiftOrderSubscriber' + (isTsRuntime() ? '.ts' : '.js');
		const swiftOrderSubscriberProcess = spawnChild(
			path.join(
				__dirname,
				isTsRuntime() ? '..' : '.',
				'filler-common',
				swiftOrderSubscriberFileName
			),
			orderSubscriberArgs,
			'swiftOrderSubscriber',
			(msg: any) => {
				switch (msg.type) {
					case 'signedMsgOrderParamsMessage':
						if (msg.data.type === 'signedMsgOrderParamsMessage') {
							this.signedMsgOrderMessages.set(
								msg.data.uuid,
								msg.data.signedMsgOrder
							);
							this.routeMessageToDlobBuilder(msg);
						} else if (msg.data.type === 'delete') {
							this.signedMsgOrderMessages.delete(msg.data.uuid);
						}
						break;
					case 'health':
						this.swiftOrderSubscriberHealth = msg.data.healthy;
						break;
				}
			}
		);

		swiftOrderSubscriberProcess.on('exit', (code) => {
			logger.error(`swiftOrderSubscriber exited with code ${code}`);
			process.exit(code || 1);
		});

		process.on('SIGINT', () => {
			logger.info(`${logPrefix} Received SIGINT, killing children`);
			this.dlobBuilders.forEach((value: DLOBBuilderWithProcess, _: number) => {
				value.process.kill();
			});
			orderSubscriberProcess.kill();
			swiftOrderSubscriberProcess.kill();
			process.exit(0);
		});

		logger.info(
			`swiftOrderSubscriber process spawned with pid: ${swiftOrderSubscriberProcess.pid}`
		);

		this.intervalIds.push(
			setInterval(
				this.settlePnls.bind(this),
				SETTLE_POSITIVE_PNL_COOLDOWN_MS / 2
			)
		);
		this.intervalIds.push(
			setInterval(this.confirmPendingTxSigs.bind(this), CONFIRM_TX_INTERVAL_MS)
		);
		if (this.bundleSender) {
			this.intervalIds.push(
				setInterval(this.recordJitoBundleStats.bind(this), 10_000)
			);
		}
	}

	routeMessageToDlobBuilder = (msg: any) => {
		const dlobBuilder = this.dlobBuilders.get(Number(msg.data.marketIndex));
		if (dlobBuilder === undefined) {
			logger.error(
				`Received message for unknown marketIndex: ${msg.data.marketIndex}`
			);
			return;
		}
		if (dlobBuilder.marketIndexes.includes(Number(msg.data.marketIndex))) {
			if (typeof dlobBuilder.process.send == 'function') {
				if (dlobBuilder.ready) {
					dlobBuilder.process.send(msg);
					return;
				}
			}
		}
	};

	protected recordEvictedTxSig(
		_tsTxSigAdded: { ts: number; nodeFilled: Array<NodeToFillWithBuffer> },
		txSig: string,
		reason: 'evict' | 'set' | 'delete'
	) {
		if (reason === 'evict') {
			logger.info(
				`${this.name}: Evicted tx sig ${txSig} from this.txSigsToConfirm`
			);
			const user = this.velocityClient.getUser(this.subaccount);
			this.evictedPendingTxSigsToConfirmCounter?.add(1, {
				...metricAttrFromUserAccount(
					user.userAccountPublicKey,
					user.getUserAccountOrThrow()
				),
			});
		}
	}

	protected initializeMetrics(metricsPort?: number) {
		if (this.globalConfig.disableMetrics) {
			logger.info(
				`${this.name}: globalConfig.disableMetrics is true, not initializing metrics`
			);
			return;
		}

		if (!metricsPort) {
			logger.info(
				`${this.name}: bot.metricsPort and global.metricsPort not set, not initializing metrics`
			);
			return;
		}

		if (this.metricsInitialized) {
			logger.error('Tried to initilaize metrics multiple times');
			return;
		}

		this.metrics = new Metrics(
			this.name,
			[
				new View({
					instrumentName: METRIC_TYPES.try_fill_duration_histogram,
					instrumentType: InstrumentType.HISTOGRAM,
					meterName: this.name,
					aggregation: new ExplicitBucketHistogramAggregation(
						Array.from(new Array(20), (_, i) => 0 + i * 5),
						true
					),
				}),
				new View({
					instrumentName: METRIC_TYPES.estimated_tx_cu_histogram,
					instrumentType: InstrumentType.HISTOGRAM,
					meterName: this.name,
					aggregation: new ExplicitBucketHistogramAggregation(
						Array.from(new Array(15), (_, i) => 0 + i * 100_000),
						true
					),
				}),
				new View({
					instrumentName: METRIC_TYPES.simulate_tx_duration_histogram,
					instrumentType: InstrumentType.HISTOGRAM,
					meterName: this.name,
					aggregation: new ExplicitBucketHistogramAggregation(
						Array.from(new Array(20), (_, i) => 50 + i * 50),
						true
					),
				}),
			],
			metricsPort!
		);
		this.bootTimeMs = Date.now();
		this.runtimeSpecsGauge = this.metrics.addGauge(
			METRIC_TYPES.runtime_specs,
			'Runtime sepcification of this program'
		);
		this.estTxCuHistogram = this.metrics.addHistogram(
			METRIC_TYPES.estimated_tx_cu_histogram,
			'Histogram of the estimated fill cu used'
		);
		this.simulateTxHistogram = this.metrics.addHistogram(
			METRIC_TYPES.simulate_tx_duration_histogram,
			'Histogram of the duration of simulateTransaction RPC calls'
		);
		this.lastTryFillTimeGauge = this.metrics.addGauge(
			METRIC_TYPES.last_try_fill_time,
			'Last time that fill was attempted'
		);
		this.landedTxsCounter = this.metrics.addCounter(
			METRIC_TYPES.landed_transactions,
			'Count of fills that we successfully landed'
		);
		this.sentTxsCounter = this.metrics.addCounter(
			METRIC_TYPES.sent_transactions,
			'Count of transactions we sent out'
		);
		this.txSimErrorCounter = this.metrics.addCounter(
			METRIC_TYPES.tx_sim_error_count,
			'Count of errors from simulating transactions'
		);
		this.pendingTxSigsToConfirmGauge = this.metrics.addGauge(
			METRIC_TYPES.pending_tx_sigs_to_confirm,
			'Count of tx sigs that are pending confirmation'
		);
		this.pendingTxSigsLoopRateLimitedCounter = this.metrics.addCounter(
			METRIC_TYPES.pending_tx_sigs_loop_rate_limited,
			'Count of times the pending tx sigs loop was rate limited'
		);
		this.evictedPendingTxSigsToConfirmCounter = this.metrics.addCounter(
			METRIC_TYPES.evicted_pending_tx_sigs_to_confirm,
			'Count of tx sigs that were evicted from the pending tx sigs to confirm cache'
		);
		this.expiredNodesSetSize = this.metrics.addGauge(
			METRIC_TYPES.expired_nodes_set_size,
			'Count of nodes that are expired'
		);
		this.jitoConnectedGauge = this.metrics.addGauge(
			JITO_METRIC_TYPES.jito_connected,
			'Whether the jito bundle sender is connected'
		);
		this.jitoBundlesAcceptedGauge = this.metrics.addGauge(
			JITO_METRIC_TYPES.jito_bundles_accepted,
			'Count of jito bundles that were accepted'
		);
		this.jitoBundlesSimulationFailureGauge = this.metrics.addGauge(
			JITO_METRIC_TYPES.jito_bundles_simulation_failure,
			'Count of jito bundles that failed simulation'
		);
		this.jitoDroppedBundleGauge = this.metrics.addGauge(
			JITO_METRIC_TYPES.jito_dropped_bundle,
			'Count of jito bundles that were dropped'
		);
		this.jitoLandedTipsGauge = this.metrics.addGauge(
			JITO_METRIC_TYPES.jito_landed_tips,
			'Gauge of historic bundle tips that landed'
		);
		this.jitoBundleCount = this.metrics.addGauge(
			JITO_METRIC_TYPES.jito_bundle_count,
			'Count of jito bundles that were sent, and their status'
		);

		this.metrics?.finalizeObservables();

		this.runtimeSpecsGauge.setLatestValue(this.bootTimeMs, this.runtimeSpec);
		this.metricsInitialized = true;
	}

	public healthCheck(): boolean {
		if (!this.dlobHealthy) {
			logger.error(`${logPrefix} DLOB not healthy`);
		}
		if (!this.orderSubscriberHealthy) {
			logger.error(`${logPrefix} Order subscriber not healthy`);
		}
		if (!this.swiftOrderSubscriberHealth) {
			logger.error(`${logPrefix} SignedMsg order subscriber not healthy`);
		}
		return (
			this.dlobHealthy &&
			this.orderSubscriberHealthy &&
			this.swiftOrderSubscriberHealth
		);
	}

	protected recordJitoBundleStats() {
		const user = this.velocityClient.getUser(this.subaccount);
		const bundleStats = this.bundleSender?.getBundleStats();
		if (bundleStats) {
			this.jitoConnectedGauge?.setLatestValue(
				this.bundleSender?.connected() ? 1 : 0,
				{
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoBundlesAcceptedGauge?.setLatestValue(bundleStats.accepted, {
				...metricAttrFromUserAccount(
					user.userAccountPublicKey,
					user.getUserAccountOrThrow()
				),
			});
			this.jitoBundlesSimulationFailureGauge?.setLatestValue(
				bundleStats.simulationFailure,
				{
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoDroppedBundleGauge?.setLatestValue(bundleStats.droppedPruned, {
				type: 'pruned',
				...metricAttrFromUserAccount(
					user.userAccountPublicKey,
					user.getUserAccountOrThrow()
				),
			});
			this.jitoDroppedBundleGauge?.setLatestValue(
				bundleStats.droppedBlockhashExpired,
				{
					type: 'blockhash_expired',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoDroppedBundleGauge?.setLatestValue(
				bundleStats.droppedBlockhashNotFound,
				{
					type: 'blockhash_not_found',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
		}

		const tipStream = this.bundleSender?.getTipStream();
		if (tipStream) {
			this.jitoLandedTipsGauge?.setLatestValue(
				tipStream.landed_tips_25th_percentile,
				{
					percentile: 'p25',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoLandedTipsGauge?.setLatestValue(
				tipStream.landed_tips_50th_percentile,
				{
					percentile: 'p50',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoLandedTipsGauge?.setLatestValue(
				tipStream.landed_tips_75th_percentile,
				{
					percentile: 'p75',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoLandedTipsGauge?.setLatestValue(
				tipStream.landed_tips_95th_percentile,
				{
					percentile: 'p95',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoLandedTipsGauge?.setLatestValue(
				tipStream.landed_tips_99th_percentile,
				{
					percentile: 'p99',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);
			this.jitoLandedTipsGauge?.setLatestValue(
				tipStream.ema_landed_tips_50th_percentile,
				{
					percentile: 'ema_p50',
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				}
			);

			const bundleFailCount = this.bundleSender?.getBundleFailCount();
			const bundleLandedCount = this.bundleSender?.getLandedCount();
			const bundleDroppedCount = this.bundleSender?.getDroppedCount();
			this.jitoBundleCount?.setLatestValue(bundleFailCount ?? 0, {
				type: 'fail_count',
			});
			this.jitoBundleCount?.setLatestValue(bundleLandedCount ?? 0, {
				type: 'landed',
			});
			this.jitoBundleCount?.setLatestValue(bundleDroppedCount ?? 0, {
				type: 'dropped',
			});
		}
	}

	protected async confirmPendingTxSigs() {
		const user = this.velocityClient.getUser(this.subaccount);
		this.pendingTxSigsToConfirmGauge?.setLatestValue(
			this.pendingTxSigsToconfirm.size,
			{
				...metricAttrFromUserAccount(
					user.userAccountPublicKey,
					user.getUserAccountOrThrow()
				),
			}
		);
		this.expiredNodesSetSize?.setLatestValue(this.expiredNodesSet.size, {
			...metricAttrFromUserAccount(
				user.userAccountPublicKey,
				user.getUserAccountOrThrow()
			),
		});
		const nextTimeCanRun =
			this.confirmLoopRateLimitTs + CONFIRM_TX_RATE_LIMIT_BACKOFF_MS;
		if (Date.now() < nextTimeCanRun) {
			logger.warn(
				`Skipping confirm loop due to rate limit, next run in ${
					nextTimeCanRun - Date.now()
				} ms`
			);
			return;
		}
		if (this.confirmLoopRunning) {
			return;
		}
		this.confirmLoopRunning = true;
		try {
			logger.debug(`Confirming tx sigs: ${this.pendingTxSigsToconfirm.size}`);
			const start = Date.now();
			const txEntries = Array.from(this.pendingTxSigsToconfirm.entries());
			for (let i = 0; i < txEntries.length; i += TX_CONFIRMATION_BATCH_SIZE) {
				const txSigsBatch = txEntries.slice(i, i + TX_CONFIRMATION_BATCH_SIZE);
				const txs = await this.txConfirmationConnection?.getTransactions(
					txSigsBatch.map((tx) => tx[0]),
					{
						commitment: 'confirmed',
						maxSupportedTransactionVersion: 0,
					}
				);
				for (let j = 0; j < txs.length; j++) {
					const txResp = txs[j];
					const txConfirmationInfo = txSigsBatch[j];
					const txSig = txConfirmationInfo[0];
					const txAge = txConfirmationInfo[1].ts - Date.now();
					const nodeFilled = txConfirmationInfo[1].nodeFilled;
					const txType = txConfirmationInfo[1].txType;
					const fillTxId = txConfirmationInfo[1].fillTxId;
					const sentSlot = txConfirmationInfo[1].sentSlot;
					const cuLimit = txConfirmationInfo[1].cuLimit;
					const fillType = txConfirmationInfo[1].fillType;
					if (txResp === null) {
						logger.info(
							`Tx not found, (fillTxId: ${fillTxId}) (txType: ${txType}): ${txSig}, tx age: ${
								txAge / 1000
							} s${fillCorrelationSuffix(nodeFilled)}`
						);
						if (Math.abs(txAge) > TX_TIMEOUT_THRESHOLD_MS) {
							this.pendingTxSigsToconfirm.delete(txSig);
							// Only the give-up poll is terminal — the earlier "not found"
							// polls are the tx still being in flight, and wide-logging them
							// would emit one event per 5s confirm tick per tx.
							if (txType === 'fill') {
								this.emitTxEvent({
									nodes: nodeFilled,
									fillTxId,
									status: 'expired',
									fillType,
									sig: txSig,
									sentSlot,
									cuLimit,
									actualFills: 0,
									error: `not confirmed within ${TX_TIMEOUT_THRESHOLD_MS} ms`,
								});
							}
						}
					} else {
						logger.info(
							`Tx landed (fillTxId: ${fillTxId}) (txType: ${txType}): ${txSig}, tx age: ${
								txAge / 1000
							} s${fillCorrelationSuffix(nodeFilled)}`
						);

						// The place+fill attempt has resolved: release the in-flight
						// reservation for every signed-msg node in this tx. If it landed
						// Ok the order is placed on-chain (a single-maker signed-msg
						// place+fill carries no RevertFill ix, so even a 0-base no-op
						// lands Ok), so additionally retire the node: mark it placed so
						// we never re-run the place+fill, and evict it from the DLOB
						// builder so it stops being emitted — any remaining base fills
						// through the order's on-chain node instead. A landed-but-errored
						// place+fill (e.g. a multi-maker RevertFill, or expiry) is not
						// marked placed, so it stays retriable while still valid.
						const landedOk = txResp.meta?.err === null;
						for (const node of nodeFilled) {
							if (!node.node.isSignedMsg) {
								continue;
							}
							const sig = getNodeToFillSignature(node);
							this.signedMsgFillsInFlight.delete(sig);
							const orderId = node.node.order?.orderId;
							if (landedOk && orderId !== undefined) {
								this.placedSignedMsgOrders.set(sig, true);
								this.routeMessageToDlobBuilder({
									data: {
										marketIndex: node.node.order?.marketIndex,
										type: 'confirmed',
										uuid: orderId,
									},
								});
							}
						}
						this.pendingTxSigsToconfirm.delete(txSig);
						if (txType === 'fill') {
							const result = await this.handleTransactionLogs(
								nodeFilled,
								txResp.meta?.logMessages
							);
							if (result) {
								this.landedTxsCounter?.add(result.filledNodes, {
									type: txType,
									...metricAttrFromUserAccount(
										user.userAccountPublicKey,
										user.getUserAccountOrThrow()
									),
								});
							}
							// Terminal outcome for a landed fill tx. `no_fills` is the
							// interesting one: the tx landed Ok but the fill ix produced
							// no base — the same distinction keep-rs draws.
							const actualFills = result?.filledNodes ?? 0;
							let status: string;
							if (!landedOk) {
								status = 'failed';
							} else if (actualFills === 0) {
								status = 'no_fills';
							} else if (actualFills < nodeFilled.length) {
								status = 'partial';
							} else {
								status = 'ok';
							}
							this.emitTxEvent({
								nodes: nodeFilled,
								fillTxId,
								status,
								fillType,
								sig: txSig,
								sentSlot,
								confirmedSlot: txResp.slot,
								actualFills,
								cuLimit,
								cuConsumed: txResp.meta?.computeUnitsConsumed,
								feeLamports: txResp.meta?.fee,
								error: landedOk ? undefined : JSON.stringify(txResp.meta?.err),
								errorCode:
									getErrorCodeFromSimError(txResp.meta?.err ?? null) ??
									undefined,
							});
						} else {
							this.landedTxsCounter?.add(1, {
								type: txType,
								...metricAttrFromUserAccount(
									user.userAccountPublicKey,
									user.getUserAccountOrThrow()
								),
							});
						}
					}
					await sleepMs(500);
				}
			}
			logger.debug(`Confirming tx sigs took: ${Date.now() - start} ms`);
		} catch (e) {
			const err = e as Error;
			if (err.message.includes('429')) {
				logger.info(`Confirming tx loop rate limited: ${err.message}`);
				this.confirmLoopRateLimitTs = Date.now();
				this.pendingTxSigsLoopRateLimitedCounter?.add(1, {
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				});
			} else {
				logger.error(`Other error confirming tx sigs: ${err.message}`);
			}
		} finally {
			this.confirmLoopRunning = false;
		}
	}

	private async getPythIxsFromNode(
		node: NodeToFillWithBuffer,
		precedingIxs: TransactionInstruction[] = [],
		isSignedMsg = false
	): Promise<TransactionInstruction[]> {
		const marketIndex = node.node.order?.marketIndex;
		if (marketIndex === undefined) {
			throw new Error('Market index not found on node');
		}

		if (
			isVariant(
				this.velocityClient.getPerpMarketAccount(marketIndex)?.oracleSource,
				'prelaunch'
			)
		) {
			return [];
		}

		let pythIxs: TransactionInstruction[] = [];
		if (
			isVariant(
				this.velocityClient.getPerpMarketAccount(marketIndex)?.oracleSource,
				'pythLazer'
			)
		) {
			const pythLazerIds =
				this.pythLazerSubscriber?.getPriceFeedIdsFromMarketIndex(marketIndex);
			if (!pythLazerIds) {
				logger.error(
					`Pyth lazer ids not found for marketIndex: ${marketIndex}`
				);
				return pythIxs;
			}

			const latestLazerUpdate =
				await this.pythLazerSubscriber?.getLatestPriceMessageForMarketIndex(
					marketIndex
				);
			if (!latestLazerUpdate) {
				logger.error(
					`Latest lazer update not found for marketIndex: ${marketIndex}, pythLazerIds: ${pythLazerIds}`
				);
				return pythIxs;
			}

			pythIxs = await this.velocityClient.getPostPythLazerOracleUpdateIxs(
				pythLazerIds,
				latestLazerUpdate,
				precedingIxs
			);
		} else if (!isSignedMsg) {
			pythIxs = await getAllPythOracleUpdateIxs(
				marketIndex,
				this.velocityClient,
				this.pythLazerSubscriber,
				precedingIxs
			);
		}

		return pythIxs;
	}

	private async getBlockhashForTx(): Promise<string> {
		const cachedBlockhash = this.blockhashSubscriber.getLatestBlockhash(10);
		if (cachedBlockhash) {
			return cachedBlockhash.blockhash as string;
		}

		const recentBlockhash =
			await this.velocityClient.connection.getLatestBlockhash({
				commitment: 'confirmed',
			});

		return recentBlockhash.blockhash;
	}

	protected removeFillingNodes(nodes: Array<NodeToFillWithBuffer>) {
		for (const node of nodes) {
			this.fillingNodes.delete(getNodeToFillSignature(node));
		}
	}

	protected isThrottledNodeStillThrottled(throttleKey: string): boolean {
		const lastFillAttempt = this.throttledNodes.get(throttleKey) || 0;
		if (lastFillAttempt + FILL_ORDER_THROTTLE_BACKOFF > Date.now()) {
			return true;
		} else {
			this.clearThrottledNode(throttleKey);
			return false;
		}
	}

	protected isDLOBNodeThrottled(dlobNode: DLOBNode): boolean {
		if (!dlobNode.userAccount || !dlobNode.order) {
			return false;
		}

		// first check if the userAccount itself is throttled
		const userAccountPubkey = dlobNode.userAccount;
		if (this.throttledNodes.has(userAccountPubkey)) {
			if (this.isThrottledNodeStillThrottled(userAccountPubkey)) {
				return true;
			} else {
				return false;
			}
		}

		// then check if the specific order is throttled
		const orderSignature = getFillSignatureFromUserAccountAndOrderId(
			dlobNode.userAccount,
			dlobNode.order.orderId.toString()
		);
		if (this.throttledNodes.has(orderSignature)) {
			if (this.isThrottledNodeStillThrottled(orderSignature)) {
				return true;
			} else {
				return false;
			}
		}

		return false;
	}

	protected clearThrottledNode(signature: string) {
		this.throttledNodes.delete(signature);
	}

	protected setThrottledNode(signature: string) {
		this.throttledNodes.set(signature, Date.now());
	}

	protected pruneThrottledNode() {
		if (this.throttledNodes.size > THROTTLED_NODE_SIZE_TO_PRUNE) {
			for (const [key, value] of this.throttledNodes.entries()) {
				if (value + 2 * FILL_ORDER_THROTTLE_BACKOFF > Date.now()) {
					this.throttledNodes.delete(key);
				}
			}
		}
	}

	protected usingJito(): boolean {
		return !!this.globalConfig.useJito;
	}

	protected canSendOutsideJito(): boolean {
		return (
			!this.usingJito() ||
			this.bundleSender?.strategy === 'non-jito-only' ||
			this.bundleSender?.strategy === 'hybrid'
		);
	}

	protected async sendTxThroughJito(
		tx: VersionedTransaction,
		metadata: number | string,
		nodesSent?: Array<NodeToFill>
	) {
		const blockhash = await this.getBlockhashForTx();
		tx.message.recentBlockhash = blockhash;

		tx.sign([
			// @ts-ignore;
			this.velocityClient.wallet.payer,
		]);

		if (this.bundleSender === undefined) {
			logger.error(
				`${logPrefix} Called sendTxThroughJito without jito properly enabled`
			);
			return;
		}
		const slotsUntilNextLeader = this.bundleSender?.slotsUntilNextLeader();
		if (slotsUntilNextLeader !== undefined) {
			this.bundleSender.sendTransactions(
				[tx],
				`(fillTxId: ${metadata})${fillCorrelationSuffix(nodesSent ?? [])}`,
				undefined,
				false
			);
		}
	}

	protected slotsUntilJitoLeader(): number | undefined {
		return this.bundleSender?.slotsUntilNextLeader();
	}

	protected shouldBuildForBundle(): boolean {
		if (!this.globalConfig.useJito) {
			return false;
		}
		if (this.globalConfig.onlySendDuringJitoLeader === true) {
			const slotsUntilJito = this.slotsUntilJitoLeader();
			if (slotsUntilJito === undefined) {
				return false;
			}
			return slotsUntilJito < SLOTS_UNTIL_JITO_LEADER_TO_SEND;
		}
		if (!this.bundleSender?.connected()) {
			return false;
		}
		return true;
	}

	protected async getUserAccountAndSlotFromMap(
		key: string
	): Promise<DataAndSlot<UserAccount>> {
		const user = await this.userMap!.mustGetWithSlot(
			key,
			this.velocityClient.userAccountSubscriptionConfig
		);
		return {
			data: user.data.getUserAccountOrThrow(),
			slot: user.slot,
		};
	}

	public async fillNodes(serializedNodesToFill: SerializedNodeToFill[]) {
		if (!this.hasEnoughSolToFill) {
			logger.info(`Not enough SOL to fill, skipping fillNodes`);
			return;
		}

		logger.debug(
			`${logPrefix} Filling ${serializedNodesToFill.length} nodes...`
		);
		const deserializedNodesToFill = serializedNodesToFill.map(
			deserializeNodeToFill
		);

		const seenFillableNodes = new Set<string>();
		const filteredFillableNodes = deserializedNodesToFill.filter((node) => {
			const sig = getNodeToFillSignature(node);
			if (seenFillableNodes.has(sig)) {
				return false;
			}
			seenFillableNodes.add(sig);
			return this.filterFillableNodes(node);
		});
		logger.debug(
			`${logPrefix} Filtered down to ${filteredFillableNodes.length} fillable nodes...`
		);

		try {
			await this.executeFillablePerpNodes(filteredFillableNodes);
		} catch (e) {
			if (e instanceof Error) {
				logger.error(
					`${logPrefix} Error filling nodes: ${e.stack ? e.stack : e.message}`
				);
			}
		}
	}

	/**
	 * The order-identity fields every wide event this bot emits carries.
	 *
	 * `order_id` vs `synthetic_order_id`: a signed-msg (swift) order has no
	 * on-chain order id until its place+fill lands, so the DLOB builder gives the
	 * synthetic node `orderId = convertUuidToNumber(uuid)` (swiftOrderSubscriber)
	 * — a real id and a synthetic id are therefore never both known for the same
	 * node. Emitting them under different keys keeps a synthetic id from being
	 * read as an on-chain one; once the order is placed it fills through its
	 * on-chain node, which reports `order_id` normally. `uuid` is recovered from
	 * the cached swift payload so the two id spaces can be joined.
	 */
	private wideEventOrderRef(nodeToFill: NodeToFillWithBuffer): {
		market?: number;
		taker?: string;
		order_id?: number;
		synthetic_order_id?: number;
		uuid?: string;
		intent: string;
	} {
		const order = nodeToFill.node.order;
		const isSignedMsg = nodeToFill.node.isSignedMsg === true;
		const orderId = order?.orderId;
		return {
			market: order?.marketIndex,
			taker: nodeToFill.node.userAccount?.toString(),
			order_id: isSignedMsg ? undefined : orderId,
			synthetic_order_id: isSignedMsg ? orderId : undefined,
			uuid:
				isSignedMsg && orderId !== undefined
					? this.signedMsgOrderMessages.get(orderId)?.['uuid']
					: undefined,
			intent: isSignedMsg ? 'signed_msg_fill' : 'fill',
		};
	}

	/**
	 * Wide event for the attempt-or-skip verdict on a fillable node — this bot's
	 * analogue of keep-rs's `cross_decision`. Named `fill_decision` rather than
	 * `cross_decision` because the TS filler does not do the vAMM-vs-makers
	 * routing that event describes: the crossing math happens upstream in the
	 * DLOB builder's `findNodesToFill`, and what is decided here is whether an
	 * already-crossing node is worth a tx (throttles, cooldowns, attempt budget,
	 * the signed-msg once-only guard). Field names match keep-rs wherever the
	 * concept is the same.
	 *
	 * Skips are emitted once per (order, reason); `sent` is emitted per attempt.
	 */
	private emitFillDecision(
		nodeToFill: NodeToFillWithBuffer,
		action: string,
		gates: {
			has_vamm_cross?: boolean;
			oracle_delay?: number;
			attempt?: number;
			post_only?: boolean;
		} = {}
	) {
		if (action !== 'sent') {
			const dedupeKey = `${getNodeToFillSignature(nodeToFill)}:${action}`;
			if (this.emittedFillDecisions.has(dedupeKey)) {
				return;
			}
			this.emittedFillDecisions.set(dedupeKey, true);
		}
		logWideEvent('fill_decision', {
			...this.wideEventOrderRef(nodeToFill),
			slot: this.slotSubscriber.getSlot(),
			action,
			n_makers: nodeToFill.makerNodes.length,
			...gates,
		});
	}

	/**
	 * Wide event for a terminal transaction outcome — the TS counterpart of
	 * keep-rs's `emit_tx_event`. One event per taker node in the tx, so a bundled
	 * tx still yields a row per order.
	 *
	 * `error_code` is the raw Anchor `Custom` number, never a decoded name: the
	 * Order Trace dashboard owns that mapping.
	 *
	 * A signature reaches a terminal outcome exactly once: a tx whose send failed
	 * would otherwise also be reported `expired` by the confirm loop, which never
	 * sees it land. The first status for a signature wins.
	 */
	private emitTxEvent(params: {
		nodes: Array<NodeToFillWithBuffer>;
		fillTxId: number;
		status: string;
		fillType?: string;
		sig?: string;
		sentSlot?: number;
		confirmedSlot?: number;
		actualFills?: number;
		cuLimit?: number;
		cuConsumed?: number;
		feeLamports?: number;
		error?: string;
		errorCode?: number;
	}) {
		if (params.sig !== undefined) {
			if (this.emittedTxEvents.has(params.sig)) {
				return;
			}
			this.emittedTxEvents.set(params.sig, true);
		}
		for (const node of params.nodes) {
			logWideEvent('tx', {
				...this.wideEventOrderRef(node),
				fill_id: params.fillTxId,
				fill_type: params.fillType,
				sig: params.sig,
				status: params.status,
				sent_slot: params.sentSlot,
				confirmed_slot: params.confirmedSlot,
				latency_slots:
					params.sentSlot !== undefined && params.confirmedSlot !== undefined
						? Math.max(params.confirmedSlot - params.sentSlot, 0)
						: undefined,
				expected_fills: params.nodes.length,
				actual_fills: params.actualFills,
				cu_limit: params.cuLimit,
				cu_consumed: params.cuConsumed,
				fee_lamports: params.feeLamports,
				error: params.error,
				error_code: params.errorCode,
			});
		}
	}

	protected filterFillableNodes(nodeToFill: NodeToFillWithBuffer): boolean {
		if (!nodeToFill.node.order) {
			return false;
		}

		if (nodeToFill.node.isVammNode()) {
			logger.warn(
				`filtered out a vAMM node on market ${nodeToFill.node.order.marketIndex} for user ${nodeToFill.node.userAccount}-${nodeToFill.node.order.orderId}`
			);
			this.emitFillDecision(nodeToFill, 'skip_vamm_node');
			return false;
		}

		if (nodeToFill.node.haveFilled) {
			logger.warn(
				`filtered out filled node on market ${nodeToFill.node.order.marketIndex} for user ${nodeToFill.node.userAccount}-${nodeToFill.node.order.orderId}`
			);
			this.emitFillDecision(nodeToFill, 'skip_have_filled');
			return false;
		}

		const now = Date.now();
		const nodeToFillSignature = getNodeToFillSignature(nodeToFill);
		if (this.fillingNodes.has(nodeToFillSignature)) {
			const timeStartedToFillNode =
				this.fillingNodes.get(nodeToFillSignature) || 0;
			if (timeStartedToFillNode + FILL_ORDER_THROTTLE_BACKOFF > now) {
				// still cooling down on this node, filter it out
				this.emitFillDecision(nodeToFill, 'skip_filling');
				return false;
			}
		}

		// check if taker node is throttled
		if (this.isDLOBNodeThrottled(nodeToFill.node)) {
			this.emitFillDecision(nodeToFill, 'skip_throttled');
			return false;
		}

		const marketIndex = nodeToFill.node.order.marketIndex;
		const mmOraclePriceData =
			this.velocityClient.getMMOracleDataForPerpMarket(marketIndex);
		const currentSlot = this.slotSubscriber.getSlot();
		// keep-rs reports the oracle's own `delay`; the TS filler's equivalent is
		// how far the mm-oracle price it is about to gate on lags the current slot.
		const oracleDelay = currentSlot - mmOraclePriceData.slot.toNumber();

		if (isOrderExpired(nodeToFill.node.order, Date.now() / 1000, true)) {
			if (isOneOfVariant(nodeToFill.node.order.orderType, ['limit'])) {
				// do not try to fill (expire) limit orders b/c they will auto expire when filled against
				// or the user places a new order
				this.emitFillDecision(nodeToFill, 'skip_expired_limit', {
					oracle_delay: oracleDelay,
				});
				return false;
			}
			return true;
		}

		if (
			nodeToFill.makerNodes.length === 0 &&
			isVariant(nodeToFill.node.order.marketType, 'perp')
		) {
			const hasVammCross = isFillableByVAMM(
				nodeToFill.node.order,
				this.velocityClient.getPerpMarketAccount(
					nodeToFill.node.order.marketIndex
				)!,
				mmOraclePriceData,
				currentSlot,
				Date.now() / 1000,
				this.velocityClient.getStateAccount()
			);
			if (!hasVammCross) {
				this.emitFillDecision(nodeToFill, 'skip_no_cross', {
					has_vamm_cross: false,
					oracle_delay: oracleDelay,
					post_only: nodeToFill.node.order.postOnly,
				});
				return false;
			}
		}

		return true;
	}

	// Retry policy differs by node origin:
	//
	// - Signed-msg (swift) nodes are submitted as an atomic place+fill. We attempt
	//   that place+fill only ONCE per order: the node is reserved in
	//   `signedMsgFillsInFlight` synchronously the moment the fill is launched (so
	//   the ~200ms DLOB re-emit can't fire a second place+fill before the first is
	//   even built/sent), and once it lands Ok the order is placed on-chain and the
	//   node is retired (`placedSignedMsgOrders` + eviction in
	//   confirmPendingTxSigs). A single-maker signed-msg place+fill carries no
	//   RevertFill ix (see tryFillPerpNode), so even a 0-base no-op lands Ok and
	//   places the order; thereafter it fills through its on-chain order node via
	//   the non-signed path below. Re-attempting the signed node would only re-run
	//   the heavier place+fill (redundant place ix + ed25519 + oracle updates) and
	//   race its own on-chain node — which is what produced two concurrent fill txs
	//   for the same order. A place+fill that fails to send or is dropped releases
	//   the reservation (send-error path, or SIGNED_MSG_FILL_IN_FLIGHT_TTL_MS) so
	//   it can be retried while the order is still valid.
	// - Non-signed nodes (including a signed order's on-chain node once placed)
	//   keep retrying through the auction, paced to once per fillAttemptSlotInterval
	//   slots so the fill lands as the Dutch auction ramps into a cross.
	//
	// Re-attempts are further bounded by the crossability and expiry filters in
	// filterFillableNodes, the DLOB builder's per-order TTL, and
	// MAX_FILL_ATTEMPTS_PER_ORDER.
	async executeFillablePerpNodes(nodesToFill: NodeToFillWithBuffer[]) {
		const currentSlot = this.slotSubscriber.getSlot();
		for (const node of nodesToFill) {
			const sig = getNodeToFillSignature(node);
			const prior = this.fillAttempts.get(sig);
			const attempts = prior?.count ?? 0;

			if (attempts >= MAX_FILL_ATTEMPTS_PER_ORDER) {
				logger.debug(
					// @ts-ignore
					`${logPrefix} hit max fill attempts (${MAX_FILL_ATTEMPTS_PER_ORDER}) for order (account: ${
						node.node.userAccount
					}, order ${node.node.order?.orderId.toString()}), skipping`
				);
				this.emitFillDecision(node, 'skip_max_attempts', {
					attempt: attempts,
				});
				continue;
			}

			if (node.node.isSignedMsg) {
				// Place+fill a signed-msg order at most once: skip while a prior
				// place+fill is in flight, and skip forever once it has landed
				// (the order is placed and its on-chain node carries any remaining
				// base through the auction).
				if (this.placedSignedMsgOrders.has(sig)) {
					this.emitFillDecision(node, 'skip_signed_msg_placed');
					continue;
				}
				if (this.signedMsgFillsInFlight.has(sig)) {
					this.emitFillDecision(node, 'skip_signed_msg_in_flight');
					continue;
				}
			} else if (
				prior !== undefined &&
				currentSlot - prior.lastAttemptSlot < this.fillAttemptSlotInterval
			) {
				// Pace non-signed re-attempts to at most once per
				// fillAttemptSlotInterval slots.
				this.emitFillDecision(node, 'skip_attempt_interval', {
					attempt: attempts,
				});
				continue;
			}

			// Record before attempting so a failed/no-op attempt still counts.
			this.fillAttempts.set(sig, {
				count: attempts + 1,
				lastAttemptSlot: currentSlot,
			});
			// Reserve the signed-msg order synchronously, before the async fill
			// launches, so a subsequent tick can't race a second place+fill in the
			// window before the tx is registered for confirmation.
			if (node.node.isSignedMsg) {
				this.signedMsgFillsInFlight.set(sig, true);
			}
			this.emitFillDecision(node, 'sent', { attempt: attempts + 1 });
			if (node.makerNodes.length > 1) {
				this.tryFillMultiMakerPerpNodes(node);
			} else {
				this.tryFillPerpNode(node);
			}
		}
	}

	protected async tryFillMultiMakerPerpNodes(nodeToFill: NodeToFillWithBuffer) {
		const fillTxId = this.fillTxId++;

		let nodeWithMakerSet = nodeToFill;
		while (!(await this.fillMultiMakerPerpNodes(fillTxId, nodeWithMakerSet))) {
			const newMakerSet = nodeWithMakerSet.makerNodes
				.sort(() => 0.5 - Math.random())
				.slice(0, Math.ceil(nodeWithMakerSet.makerNodes.length / 2));
			nodeWithMakerSet = {
				userAccountData: nodeWithMakerSet.userAccountData,
				makerAccountData: nodeWithMakerSet.makerAccountData,
				node: nodeWithMakerSet.node,
				makerNodes: newMakerSet,
			};
			if (newMakerSet.length === 0) {
				logger.error(
					`No makers left to use for multi maker perp node (fillTxId: ${fillTxId})`
				);
				return;
			}
		}
	}

	private async fillMultiMakerPerpNodes(
		fillTxId: number,
		nodeToFill: NodeToFillWithBuffer
	): Promise<boolean> {
		try {
			const buildForBundle = this.shouldBuildForBundle();

			const {
				makerInfos,
				takerUser,
				takerUserPubKey,
				takerUserSlot,
				referrerInfo,
				takerIsReferred,
				marketType,
				takerStatsPubKey,
				isSignedMsg,
				authority,
			} = await this.getNodeFillInfo(nodeToFill);

			const getSignedMsgIxsFromNodeToFillInfo = async (
				signedMsgOrderMessages: Map<number, any>,
				velocityClient: VelocityClient,
				precedingIxs: TransactionInstruction[]
			): Promise<TransactionInstruction[]> => {
				const signedMsgOrderMessageParams = signedMsgOrderMessages.get(
					nodeToFill.node.order!.orderId
				);
				const signedSignedMsgOrderMessageParams: SignedMsgOrderParams = {
					orderParams: Buffer.from(
						signedMsgOrderMessageParams['order_message']
					),
					signature: Buffer.from(
						signedMsgOrderMessageParams['order_signature'],
						'base64'
					),
				};
				const ixs = await velocityClient.getPlaceSignedMsgTakerPerpOrderIxs(
					signedSignedMsgOrderMessageParams,
					nodeToFill.node.order!.marketIndex,
					{
						taker: new PublicKey(takerUserPubKey),
						takerStats: takerStatsPubKey,
						takerUserAccount: takerUser,
						signingAuthority: authority!,
					},
					precedingIxs
				);
				return ixs;
			};

			if (!isVariant(marketType, 'perp')) {
				throw new Error('expected perp market type');
			}

			let makerInfosToUse = makerInfos;

			const buildTxWithMakerInfos = async (
				makers: DataAndSlot<MakerInfo>[]
			): Promise<SimulateAndGetTxWithCUsResponse | undefined> => {
				if (makers.length === 0) {
					return undefined;
				}

				const computeBudgetIxs: Array<TransactionInstruction> = [
					ComputeBudgetProgram.setComputeUnitLimit({
						units: 1_400_000,
					}),
				];

				const priorityFeePrice = Math.floor(
					this.priorityFeeSubscriber.getAvgStrategyResult() *
						this.velocityClient.txSender.getSuggestedPriorityFeeMultiplier()
				);

				if (buildForBundle) {
					computeBudgetIxs.push(this.bundleSender!.getTipIx());
				} else {
					computeBudgetIxs.push(getPriorityFeeInstruction(priorityFeePrice));
				}

				let removeLastIxPostSim = this.revertOnFailure;
				const pythIxs: TransactionInstruction[] = [];
				if (
					this.pythLazerSubscriber &&
					((makerInfos.length === 2 && !referrerInfo) || makerInfos.length < 2)
				) {
					const ixs = await this.getPythIxsFromNode(nodeToFill);
					pythIxs.push(...ixs);
					removeLastIxPostSim = false;
				}

				logMessageForNodeToFill(
					nodeToFill,
					takerUserPubKey,
					takerUserSlot,
					makerInfos,
					this.slotSubscriber.getSlot(),
					fillTxId,
					'multiMakerFill',
					this.revertOnFailure ?? false,
					removeLastIxPostSim ?? false
				);

				if (!isVariant(marketType, 'perp')) {
					throw new Error('expected perp market type');
				}

				let signedMsgIxs: TransactionInstruction[] = [];
				if (isSignedMsg) {
					signedMsgIxs = await getSignedMsgIxsFromNodeToFillInfo(
						this.signedMsgOrderMessages,
						this.velocityClient,
						[...computeBudgetIxs, ...pythIxs]
					);
				}
				const fillIxs: TransactionInstruction[] = [];
				const fillIx = await this.velocityClient.getFillPerpOrderIx(
					new PublicKey(nodeToFill.node.userAccount!),
					takerUser!,
					nodeToFill.node.order!,
					makers.map((m) => m.data),
					// referrer param removed from velocity SDK; 5th arg is now
					// fillerSubAccountId.
					this.subaccount,
					isSignedMsg,
					undefined, // fillerAuthority
					undefined, // hasBuilderFee (derived from order bitflags)
					undefined, // takerEscrow (referred case signalled below)
					takerIsReferred
				);
				fillIxs.push(fillIx);

				this.fillingNodes.set(getNodeToFillSignature(nodeToFill), Date.now());
				const user = this.velocityClient.getUser(this.subaccount);

				if (this.revertOnFailure) {
					fillIxs.push(
						await this.velocityClient.getRevertFillIx(user.userAccountPublicKey)
					);
				}

				let ixsToUse = [
					...computeBudgetIxs,
					...pythIxs,
					...signedMsgIxs,
					...fillIxs,
				];
				const txSize = getSizeOfTransaction(
					ixsToUse,
					true,
					this.lookupTableAccounts
				).bytes;
				if (txSize > PACKET_DATA_SIZE && this.pythLazerSubscriber) {
					logger.info(`tx too large, removing pyth ixs.
							keys: ${ixsToUse.map((ix) => ix.keys.map((key) => key.pubkey.toString()))}
							total number of maker positions: ${makerInfos.reduce(
								(acc, maker) =>
									acc +
									(maker.data.makerUserAccount.perpPositions.length +
										maker.data.makerUserAccount.spotPositions.length),
								0
							)}`);
					if (isSignedMsg) {
						signedMsgIxs = await getSignedMsgIxsFromNodeToFillInfo(
							this.signedMsgOrderMessages,
							this.velocityClient,
							[...computeBudgetIxs]
						);
					}
					ixsToUse = [...computeBudgetIxs, ...signedMsgIxs, ...fillIxs];
				}

				let simResult;
				try {
					simResult = await simulateAndGetTxWithCUs({
						ixs: ixsToUse,
						connection: this.velocityClient.connection,
						payerPublicKey: this.velocityClient.wallet.publicKey,
						lookupTableAccounts: this.lookupTableAccounts!,
						cuLimitMultiplier: SIM_CU_ESTIMATE_MULTIPLIER,
						doSimulation: this.simulateTxForCUEstimate,
						recentBlockhash: await this.getBlockhashForTx(),
						removeLastIxPostSim,
					});
				} catch (error) {
					logger.error(
						`${logPrefix} Error simulating tx (fillTxId: ${fillTxId})${fillCorrelationSuffix(
							[nodeToFill]
						)}: ${error}`
					);
					this.emitTxEvent({
						nodes: [nodeToFill],
						fillTxId,
						status: 'sim_rpc_error',
						fillType: 'multiMakerFill',
						error: `${error}`,
					});
					return;
				}
				if (simResult.simError) {
					logger.error(
						`Error simulating tx result: ${simResult.simError.toString()}`
					);
				}

				this.simulateTxHistogram?.record(simResult.simTxDuration, {
					type: 'multiMakerFill',
					simError: simResult.simError !== null,
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				});
				this.estTxCuHistogram?.record(simResult.cuEstimate, {
					type: 'multiMakerFill',
					simError: simResult.simError !== null,
					...metricAttrFromUserAccount(
						user.userAccountPublicKey,
						user.getUserAccountOrThrow()
					),
				});
				return simResult;
			};

			let simResult = await buildTxWithMakerInfos(makerInfosToUse);
			if (simResult === undefined) {
				return true;
			}
			let txAccounts = simResult.tx.message.getAccountKeys({
				addressLookupTableAccounts: this.lookupTableAccounts,
			}).length;
			let attempt = 0;
			while (txAccounts > MAX_ACCOUNTS_PER_TX && makerInfosToUse.length > 0) {
				logger.info(
					`${logPrefix} (fillTxId: ${fillTxId} attempt ${attempt++})${fillCorrelationSuffix(
						[nodeToFill]
					)} Too many accounts, remove 1 and try again (had ${
						makerInfosToUse.length
					} maker and ${txAccounts} accounts)`
				);
				makerInfosToUse = makerInfosToUse.slice(0, makerInfosToUse.length - 1);
				simResult = await buildTxWithMakerInfos(makerInfosToUse);
			}

			if (makerInfosToUse.length === 0) {
				logger.error(
					`${logPrefix} No makerInfos left to use for multi maker perp node (fillTxId: ${fillTxId})${fillCorrelationSuffix(
						[nodeToFill]
					)}`
				);
				return true;
			}

			if (simResult === undefined) {
				logger.error(
					`${logPrefix} No simResult after ${attempt} attempts (fillTxId: ${fillTxId})${fillCorrelationSuffix(
						[nodeToFill]
					)}`
				);
				return true;
			}

			txAccounts = simResult.tx.message.getAccountKeys({
				addressLookupTableAccounts: this.lookupTableAccounts!,
			}).length;

			logger.info(
				`${logPrefix} tryFillMultiMakerPerpNodes estimated CUs: ${
					simResult!.cuEstimate
				} (fillTxId: ${fillTxId})${fillCorrelationSuffix([nodeToFill])}`
			);

			if (simResult!.simError) {
				logger.error(
					`${logPrefix} Error simulating multi maker perp node (fillTxId: ${fillTxId})${fillCorrelationSuffix(
						[nodeToFill]
					)}: ${JSON.stringify(
						simResult!.simError
					)}\nTaker slot: ${takerUserSlot}\nMaker slots: ${makerInfosToUse
						.map((m) => `  ${m.data.maker.toBase58()}: ${m.slot}`)
						.join('\n')}`
				);
				this.emitTxEvent({
					nodes: [nodeToFill],
					fillTxId,
					status: 'sim_failed',
					fillType: 'multiMakerFill',
					cuLimit: simResult!.cuEstimate,
					error: JSON.stringify(simResult!.simError),
					errorCode: getErrorCodeFromSimError(simResult!.simError) ?? undefined,
				});
				try {
					if (
						(simResult.simError as any)['InstructionError'] &&
						(simResult.simError as any)['InstructionError'][1]['Custom'] < 6000
					) {
						logger.info(
							`${logPrefix} (fillTxId: ${fillTxId})${fillCorrelationSuffix([
								nodeToFill,
							])} sim logs: ${simResult.simTxLogs?.join('\n')}`
						);
					}
				} catch (e) {
					logger.error(
						`${logPrefix} Error parsing sim logs (fillTxId: ${fillTxId})${fillCorrelationSuffix(
							[nodeToFill]
						)}: ${e}`
					);
				}
			} else {
				if (this.hasEnoughSolToFill) {
					this.sendFillTxAndParseLogs(
						fillTxId,
						[nodeToFill],
						simResult!.tx,
						buildForBundle,
						simResult!.cuEstimate,
						'multiMakerFill'
					);
				} else {
					logger.info(
						`Not enough SOL to fill, skipping executeFillablePerpNodesForMarket`
					);
					this.emitTxEvent({
						nodes: [nodeToFill],
						fillTxId,
						status: 'skip_no_sol',
						fillType: 'multiMakerFill',
						cuLimit: simResult!.cuEstimate,
					});
				}
			}
		} catch (e) {
			if (e instanceof Error) {
				logger.error(
					`${logPrefix} Error filling multi maker perp node (fillTxId: ${fillTxId})${fillCorrelationSuffix(
						[nodeToFill]
					)}: ${e.stack ? e.stack : e.message}`
				);
			}
		}
		return true;
	}

	protected async tryFillPerpNode(nodeToFill: NodeToFillWithBuffer) {
		const priorityFeePrice = Math.floor(
			this.priorityFeeSubscriber.getAvgStrategyResult() *
				this.velocityClient.txSender.getSuggestedPriorityFeeMultiplier()
		);
		const buildForBundle = this.shouldBuildForBundle();

		const computeBudgetIxs: TransactionInstruction[] = [
			ComputeBudgetProgram.setComputeUnitLimit({
				units: 1_400_000,
			}),
		];

		if (buildForBundle) {
			computeBudgetIxs.push(this.bundleSender!.getTipIx());
		} else {
			computeBudgetIxs.push(getPriorityFeeInstruction(priorityFeePrice));
		}

		const fillTxId = this.fillTxId++;

		const {
			makerInfos,
			takerUser,
			takerUserPubKey,
			takerUserSlot,
			marketType,
			takerStatsPubKey,
			isSignedMsg,
			authority,
			takerIsReferred,
		} = await this.getNodeFillInfo(nodeToFill);

		let removeLastIxPostSim = this.revertOnFailure && !isSignedMsg;
		const pythIxs: TransactionInstruction[] = [];
		if (this.pythLazerSubscriber && makerInfos.length <= 2) {
			pythIxs.push(
				...(await this.getPythIxsFromNode(
					nodeToFill,
					computeBudgetIxs,
					isSignedMsg
				))
			);
			removeLastIxPostSim = false;
		}

		logMessageForNodeToFill(
			nodeToFill,
			takerUserPubKey,
			takerUserSlot,
			makerInfos,
			this.slotSubscriber.getSlot(),
			fillTxId,
			'single',
			this.revertOnFailure ?? false,
			removeLastIxPostSim ?? false
		);

		if (!isVariant(marketType, 'perp')) {
			throw new Error('expected perp market type');
		}

		async function getSignedMsgIxsFromNodeToFillInfo(
			signedMsgOrderMessages: Map<number, any>,
			velocityClient: VelocityClient,
			precedingIxs: TransactionInstruction[]
		): Promise<TransactionInstruction[]> {
			const signedMsgOrderMessageParams = signedMsgOrderMessages.get(
				nodeToFill.node.order!.orderId
			);
			const signedSignedMsgOrderMessageParams: SignedMsgOrderParams = {
				orderParams: Buffer.from(signedMsgOrderMessageParams['order_message']),
				signature: Buffer.from(
					signedMsgOrderMessageParams['order_signature'],
					'base64'
				),
			};
			const ixs = await velocityClient.getPlaceSignedMsgTakerPerpOrderIxs(
				signedSignedMsgOrderMessageParams,
				nodeToFill.node.order!.marketIndex,
				{
					taker: new PublicKey(takerUserPubKey),
					takerStats: takerStatsPubKey,
					takerUserAccount: takerUser,
					signingAuthority: authority!,
				},
				precedingIxs
			);
			return ixs;
		}

		let signedMsgIxs: TransactionInstruction[] = [];
		if (isSignedMsg) {
			signedMsgIxs = await getSignedMsgIxsFromNodeToFillInfo(
				this.signedMsgOrderMessages,
				this.velocityClient,
				[...computeBudgetIxs, ...pythIxs]
			);
		}

		const fillIxs: TransactionInstruction[] = [];
		const fillIx = await this.velocityClient.getFillPerpOrderIx(
			new PublicKey(nodeToFill.node.userAccount!),
			takerUser!,
			nodeToFill.node.order!,
			makerInfos.map((m) => m.data),
			// referrer param removed from velocity SDK; 5th arg is now
			// fillerSubAccountId.
			this.subaccount,
			isSignedMsg,
			undefined, // fillerAuthority
			undefined, // hasBuilderFee (derived from order bitflags)
			undefined, // takerEscrow (referred case signalled below)
			takerIsReferred
		);
		fillIxs.push(fillIx);

		const user = this.velocityClient.getUser(this.subaccount);
		if (this.revertOnFailure && !isSignedMsg) {
			fillIxs.push(
				await this.velocityClient.getRevertFillIx(user.userAccountPublicKey)
			);
		}

		let ixsToUse = [
			...computeBudgetIxs,
			...pythIxs,
			...signedMsgIxs,
			...fillIxs,
		];
		const txSize = getSizeOfTransaction(
			ixsToUse,
			true,
			this.lookupTableAccounts
		).bytes;
		if (txSize > PACKET_DATA_SIZE) {
			const lutAccounts = this.lookupTableAccounts
				.map((lut) => lut.state.addresses.map((a) => a.toBase58()))
				.flat();
			logger.info(`tx too large: ${txSize} bytes, removing pyth ixs.
				keys not in LUT: ${ixsToUse
					.map((ix) => ix.keys.map((key) => key.pubkey.toString()))
					.flat()
					.filter((key) => !lutAccounts.includes(key))}
				`);

			if (isSignedMsg) {
				signedMsgIxs = await getSignedMsgIxsFromNodeToFillInfo(
					this.signedMsgOrderMessages,
					this.velocityClient,
					[...computeBudgetIxs]
				);
			}
			ixsToUse = [...computeBudgetIxs, ...signedMsgIxs, ...fillIxs];
		}

		let simResult;
		try {
			simResult = await simulateAndGetTxWithCUs({
				ixs: ixsToUse,
				connection: this.velocityClient.connection,
				payerPublicKey: this.velocityClient.wallet.publicKey,
				lookupTableAccounts: this.lookupTableAccounts!,
				cuLimitMultiplier: SIM_CU_ESTIMATE_MULTIPLIER,
				doSimulation: this.simulateTxForCUEstimate,
				recentBlockhash: await this.getBlockhashForTx(),
				removeLastIxPostSim,
			});
		} catch (error) {
			logger.error(
				`${logPrefix} Error simulating tx (fillTxId: ${fillTxId})${fillCorrelationSuffix(
					[nodeToFill]
				)}: ${error}`
			);
			this.emitTxEvent({
				nodes: [nodeToFill],
				fillTxId,
				status: 'sim_rpc_error',
				fillType: 'single',
				error: `${error}`,
			});
			return;
		}

		logger.info(
			`tryFillPerpNode estimated CUs: ${
				simResult.cuEstimate
			} (fillTxId: ${fillTxId})${fillCorrelationSuffix([nodeToFill])}`
		);

		if (simResult.simError) {
			for (const ix of pythIxs) {
				console.log('pyth ix');
				console.log(`pyth ixs: ${JSON.stringify(ix)}`);
			}
			logger.error(
				`simError: ${JSON.stringify(
					simResult.simError
				)} (fillTxId: ${fillTxId})${fillCorrelationSuffix([
					nodeToFill,
				])}, sim logs:\n${
					simResult.simTxLogs ? simResult.simTxLogs.join('\n') : 'none'
				}`
			);
			this.emitTxEvent({
				nodes: [nodeToFill],
				fillTxId,
				status: 'sim_failed',
				fillType: 'single',
				cuLimit: simResult.cuEstimate,
				error: JSON.stringify(simResult.simError),
				errorCode: getErrorCodeFromSimError(simResult.simError) ?? undefined,
			});
		} else {
			if (this.hasEnoughSolToFill) {
				this.sendFillTxAndParseLogs(
					fillTxId,
					[nodeToFill],
					simResult.tx,
					buildForBundle,
					simResult.cuEstimate,
					'single'
				);
			} else {
				logger.info(
					`Not enough SOL to fill, skipping executeFillablePerpNodesForMarket`
				);
				this.emitTxEvent({
					nodes: [nodeToFill],
					fillTxId,
					status: 'skip_no_sol',
					fillType: 'single',
					cuLimit: simResult.cuEstimate,
				});
			}
		}
	}

	protected async sendFillTxAndParseLogs(
		fillTxId: number,
		nodesSent: Array<NodeToFillWithBuffer>,
		tx: VersionedTransaction,
		buildForBundle: boolean,
		cuLimit?: number,
		fillType?: string
	) {
		let txResp: Promise<TxSigAndSlot> | undefined = undefined;
		let estTxSize: number | undefined = undefined;
		let txAccounts = 0;
		let writeAccs = 0;
		const accountMetas: any[] = [];
		const txStart = Date.now();
		const sentSlot = this.slotSubscriber.getSlot();
		// @ts-ignore;
		tx.sign([this.velocityClient.wallet.payer]);
		const txSig = bs58.encode(tx.signatures[0]);

		if (buildForBundle) {
			await this.sendTxThroughJito(tx, fillTxId, nodesSent);
			this.removeFillingNodes(nodesSent);
		} else {
			estTxSize = tx.message.serialize().length;
			const acc = tx.message.getAccountKeys({
				addressLookupTableAccounts: this.lookupTableAccounts!,
			});
			txAccounts = acc.length;
			for (let i = 0; i < txAccounts; i++) {
				const meta: any = {};
				if (tx.message.isAccountWritable(i)) {
					writeAccs++;
					meta['writeable'] = true;
				}
				if (tx.message.isAccountSigner(i)) {
					meta['signer'] = true;
				}
				meta['address'] = acc.get(i)!.toBase58();
				accountMetas.push(meta);
			}

			txResp = this.velocityClient.txSender.sendVersionedTransaction(
				tx,
				[],
				this.velocityClient.opts,
				true
			);
		}

		this.registerTxSigToConfirm(
			txSig,
			Date.now(),
			nodesSent,
			fillTxId,
			'fill',
			sentSlot,
			cuLimit,
			fillType
		);

		if (txResp) {
			txResp
				.then((resp: TxSigAndSlot) => {
					const duration = Date.now() - txStart;
					logger.info(
						`${logPrefix} sent tx: ${
							resp.txSig
						}, took: ${duration}ms (fillTxId: ${fillTxId})${fillCorrelationSuffix(
							nodesSent
						)}`
					);
				})
				.catch(async (e) => {
					const simError = e as SendTransactionError;
					logger.error(
						`${logPrefix} Failed to send packed tx txAccountKeys: ${txAccounts} (${writeAccs} writeable) (fillTxId: ${fillTxId})${fillCorrelationSuffix(
							nodesSent
						)}, error: ${simError.message}`
					);

					// The send definitively failed, so no place+fill is in flight for
					// these signed-msg orders: release the in-flight guard now (rather
					// than waiting for the drop-detection TTL) so the order can be
					// retried while it is still valid.
					for (const node of nodesSent) {
						if (node.node.isSignedMsg) {
							this.signedMsgFillsInFlight.delete(getNodeToFillSignature(node));
						}
					}

					this.emitTxEvent({
						nodes: nodesSent,
						fillTxId,
						status: 'send_error',
						fillType,
						sig: txSig,
						sentSlot,
						cuLimit,
						error: simError.message,
						errorCode: getErrorCode(e),
					});

					if (e.message.includes('too large:')) {
						logger.error(
							`${logPrefix}: :boxing_glove: Tx too large, estimated to be ${estTxSize} (fillId: ${fillTxId}). ${
								e.message
							}\n${JSON.stringify(accountMetas)}`
						);
						return;
					}

					if (simError.logs && simError.logs.length > 0) {
						const errorCode = getErrorCode(e);
						logger.error(
							`${logPrefix} Failed to send tx, sim error (fillTxId: ${fillTxId}) error code: ${errorCode}`
						);
					}
				})
				.finally(() => {
					this.removeFillingNodes(nodesSent);
				});
		}
	}

	protected async settlePnls() {
		// Check if we have enough SOL to fill
		const fillerSolBalance = await this.velocityClient.connection.getBalance(
			this.velocityClient.authority
		);
		this.hasEnoughSolToFill = fillerSolBalance >= this.minGasBalanceToFill;

		const user = this.velocityClient.getUser(this.subaccount);
		const activePerpPositions = user.getActivePerpPositions().sort((a, b) => {
			return b.quoteAssetAmount.sub(a.quoteAssetAmount).toNumber();
		});
		const marketIds = activePerpPositions.map((pos) => pos.marketIndex);
		const totalUnsettledPnl = activePerpPositions.reduce(
			(totalUnsettledPnl, position) => {
				return totalUnsettledPnl.add(position.quoteAssetAmount);
			},
			new BN(0)
		);

		const now = Date.now();
		// Settle pnl if:
		// - we are rebalancing and have enough unsettled pnl to rebalance preemptively
		// - we are rebalancing and don't have enough SOL to fill
		// - we have hit max positions to free up slots
		if (
			(this.rebalanceFiller &&
				(totalUnsettledPnl.gte(
					this.rebalanceSettledPnlThreshold.mul(QUOTE_PRECISION)
				) ||
					!this.hasEnoughSolToFill)) ||
			marketIds.length >= MAX_POSITIONS_PER_USER
		) {
			logger.info(
				`Settling positive PNLs for markets: ${JSON.stringify(marketIds)}`
			);
			if (now < this.lastSettlePnl + SETTLE_POSITIVE_PNL_COOLDOWN_MS) {
				logger.info(`Want to settle positive pnl, but in cooldown...`);
			} else {
				let chunk_size;
				if (marketIds.length < 5) {
					chunk_size = marketIds.length;
				} else {
					chunk_size = marketIds.length / 2;
				}
				const settlePnlPromises: Array<Promise<TxSigAndSlot>> = [];
				for (let i = 0; i < marketIds.length; i += chunk_size) {
					const marketIdChunks = marketIds.slice(i, i + chunk_size);
					try {
						const priorityFeePrice =
							Math.floor(this.priorityFeeSubscriber.getAvgStrategyResult()) *
							this.velocityClient.txSender.getSuggestedPriorityFeeMultiplier();
						const buildForBundle = this.shouldBuildForBundle();

						const ixs = [
							ComputeBudgetProgram.setComputeUnitLimit({
								units: 1_400_000, // will be overridden by simulateTx
							}),
						];

						if (buildForBundle) {
							ixs.push(this.bundleSender!.getTipIx());
						} else {
							ixs.push(
								ComputeBudgetProgram.setComputeUnitPrice({
									microLamports: priorityFeePrice,
								})
							);
						}

						ixs.push(
							...(await this.velocityClient.getSettlePNLsIxs(
								[
									{
										settleeUserAccountPublicKey: user.getUserAccountPublicKey(),
										settleeUserAccount: this.velocityClient.getUserAccount(
											this.subaccount
										)!,
									},
								],
								marketIdChunks
							))
						);

						const simResult = await simulateAndGetTxWithCUs({
							ixs,
							connection: this.velocityClient.connection,
							payerPublicKey: this.velocityClient.wallet.publicKey,
							lookupTableAccounts: this.lookupTableAccounts!,
							cuLimitMultiplier: SIM_CU_ESTIMATE_MULTIPLIER,
							doSimulation: this.simulateTxForCUEstimate,
							recentBlockhash: await this.getBlockhashForTx(),
							removeLastIxPostSim: this.revertOnFailure,
						});
						this.simulateTxHistogram?.record(simResult.simTxDuration, {
							type: 'settlePnl',
							simError: simResult.simError !== null,
							...metricAttrFromUserAccount(
								user.userAccountPublicKey,
								user.getUserAccountOrThrow()
							),
						});
						this.estTxCuHistogram?.record(simResult.cuEstimate, {
							type: 'settlePnl',
							simError: simResult.simError !== null,
							...metricAttrFromUserAccount(
								user.userAccountPublicKey,
								user.getUserAccountOrThrow()
							),
						});

						if (this.simulateTxForCUEstimate && simResult.simError) {
							logger.info(
								`settlePnls simError: ${JSON.stringify(
									simResult.simError
								)}, sim logs:\n${
									simResult.simTxLogs ? simResult.simTxLogs.join('\n') : 'none'
								}`
							);
							handleSimResultError(
								simResult,
								errorCodesToSuppress,
								`${this.name}: (settlePnls)`
							);
						} else {
							if (!this.dryRun) {
								// @ts-ignore;
								simResult.tx.sign([this.velocityClient.wallet.payer]);

								if (buildForBundle) {
									this.sendTxThroughJito(simResult.tx, 'settlePnl');
								} else if (this.canSendOutsideJito()) {
									settlePnlPromises.push(
										this.velocityClient.txSender.sendVersionedTransaction(
											simResult.tx,
											[],
											this.velocityClient.opts,
											true
										)
									);
								}

								const txSig = bs58.encode(simResult.tx.signatures[0]);
								this.registerTxSigToConfirm(
									txSig,
									Date.now(),
									[],
									-2,
									'settlePnl'
								);
							} else {
								logger.info(`dry run, skipping settlePnls)`);
							}
						}
					} catch (err) {
						if (!(err instanceof Error)) {
							return;
						}
						const errorCode = getErrorCode(err) ?? 0;
						logger.error(
							`Error code: ${errorCode} while settling pnls for markets ${JSON.stringify(
								marketIds
							)}: ${err.message}`
						);
						console.error(err);
					}
				}
				try {
					const txs = await Promise.all(settlePnlPromises);
					for (const tx of txs) {
						logger.info(
							`Settle positive PNLs tx: https://solscan/io/tx/${tx.txSig}`
						);
					}
				} catch (e) {
					logger.error(`Error settling positive pnls: ${e}`);
				}
				this.lastSettlePnl = now;
			}
		}

		// If we are rebalancing, check if we have enough settled pnl in usdc account to rebalance,
		// or if we have to go below threshold since we don't have enough sol
		if (this.rebalanceFiller) {
			const fillerVelocityAccountUsdcBalance =
				this.velocityClient.getTokenAmount(0);
			const usdcSpotMarket = this.velocityClient.getSpotMarketAccount(0);
			const normalizedFillerVelocityAccountUsdcBalance =
				fillerVelocityAccountUsdcBalance.divn(10 ** usdcSpotMarket!.decimals);

			if (
				normalizedFillerVelocityAccountUsdcBalance.gte(
					this.rebalanceSettledPnlThreshold
				) ||
				!this.hasEnoughSolToFill
			) {
				logger.info(
					`Filler has ${normalizedFillerVelocityAccountUsdcBalance.toNumber()} usdc to rebalance`
				);
				await this.rebalance();
			}
		}
	}

	protected async rebalance() {
		logger.info(`Rebalancing filler`);
		if (this.jupiterClient !== undefined) {
			logger.info(`Swapping USDC for SOL to rebalance filler`);
			swapFillerHardEarnedUSDCForSOL(
				this.priorityFeeSubscriber,
				this.velocityClient,
				this.jupiterClient,
				await this.getBlockhashForTx(),
				this.subaccount
			).then(async () => {
				const fillerSolBalanceAfterSwap =
					await this.velocityClient.connection.getBalance(
						this.velocityClient.authority,
						'processed'
					);
				this.hasEnoughSolToFill =
					fillerSolBalanceAfterSwap >= this.minGasBalanceToFill;
			});
		} else {
			throw new Error('Jupiter client not initialized but trying to rebalance');
		}
	}

	/**
	 * Gives filler reward estimate
	 *
	 * @param taker
	 * @param quoteAssetAmount
	 */
	protected calculateFillerRewardEstimate(
		feeTier: FeeTier,
		quoteAssetAmount: BN
	) {
		const takerFee = quoteAssetAmount
			.muln(feeTier.feeNumerator)
			.divn(feeTier.feeDenominator);
		const fillerReward = BN.min(new BN(10_000), takerFee.divn(10));
		return fillerReward;
	}

	protected async getNodeFillInfo(nodeToFill: NodeToFillWithBuffer): Promise<{
		makerInfos: Array<DataAndSlot<MakerInfo>>;
		takerUserPubKey: string;
		takerUser: UserAccount;
		takerStatsPubKey: PublicKey;
		takerUserSlot: number;
		referrerInfo: ReferrerInfo | undefined;
		takerIsReferred: boolean;
		marketType: MarketType;
		isSignedMsg: boolean | undefined;
		authority: PublicKey;
	}> {
		const makerInfos: Array<DataAndSlot<MakerInfo>> = [];

		if (nodeToFill.makerNodes.length > 0) {
			let makerNodesMap: MakerNodeMap = new Map<string, DLOBNode[]>();
			for (const makerNode of nodeToFill.makerNodes) {
				if (this.isDLOBNodeThrottled(makerNode)) {
					continue;
				}

				if (!makerNode.userAccount) {
					continue;
				}

				if (makerNodesMap.has(makerNode.userAccount!)) {
					makerNodesMap.get(makerNode.userAccount!)!.push(makerNode);
				} else {
					makerNodesMap.set(makerNode.userAccount!, [makerNode]);
				}
			}

			if (makerNodesMap.size > MAX_MAKERS_PER_FILL) {
				logger.info(`selecting from ${makerNodesMap.size} makers`);
				makerNodesMap = selectMakers(makerNodesMap);
				logger.info(`selected: ${Array.from(makerNodesMap.keys()).join(',')}`);
			}

			const makerInfoMap = new Map(JSON.parse(nodeToFill.makerAccountData));
			for (const [makerAccount, makerNodes] of makerNodesMap) {
				const makerNode = makerNodes[0];
				const makerUserAccount = decodeUser(
					// @ts-ignore
					Buffer.from(makerInfoMap.get(makerAccount)!.data)
				);
				const makerAuthority = makerUserAccount.authority;
				const makerUserStats = getUserStatsAccountPublicKey(
					this.velocityClient.program.programId,
					new PublicKey(makerAuthority)
				);
				makerInfos.push({
					slot: this.slotSubscriber.getSlot(),
					data: {
						maker: new PublicKey(makerAccount),
						makerUserAccount: makerUserAccount,
						order: makerNode.order,
						makerStats: makerUserStats,
					},
				});
			}
		}

		const takerUserPubKey = nodeToFill.node.userAccount!.toString();

		// @ts-ignore
		const takerUserAccount = nodeToFill.userAccountData?.data
			? decodeUser(
					// @ts-ignore
					Buffer.from(nodeToFill.userAccountData.data)
			  )
			: (await this.userMap.mustGet(takerUserPubKey)).getUserAccountOrThrow();

		// `nodeToFill.authority` is the swift message's signing authority, which
		// may be a delegate wallet. UserStats/referrer PDAs are derived from the
		// user account's own authority; the signing authority is only used to
		// verify the signed-msg signature.
		const signingAuthority = nodeToFill.authority
			? nodeToFill.authority
			: takerUserAccount.authority.toString();
		const takerAuthority = takerUserAccount.authority.toString();

		let referrerInfo: ReferrerInfo | undefined;
		try {
			referrerInfo = await this.referrerMap?.mustGet(takerAuthority);
		} catch (e) {
			logger.warn(`getNodeFillInfo: Failed to get referrer info: ${e}`);
			referrerInfo = undefined;
		}

		// The program's fill gate requires the taker's RevenueShareEscrow when the
		// taker is referred (their escrow was initialized with a referrer) — the
		// UserStats.referrerStatus BuilderReferral bit. ReferrerMap reads it from
		// the same UserStats fetch it already does for referrerInfo.
		let takerIsReferred = false;
		try {
			takerIsReferred = await this.referrerMap.mustGetIsBuilderReferral(
				takerAuthority
			);
		} catch (e) {
			logger.warn(
				`getNodeFillInfo: Failed to get builder-referral status: ${e}`
			);
		}

		return Promise.resolve({
			makerInfos,
			takerUserPubKey,
			takerUser: takerUserAccount,
			takerStatsPubKey: getUserStatsAccountPublicKey(
				this.velocityClient.program.programId,
				new PublicKey(takerAuthority)
			),
			takerUserSlot: this.slotSubscriber.getSlot(),
			referrerInfo,
			takerIsReferred,
			marketType: nodeToFill.node.order!.marketType,
			isSignedMsg: nodeToFill.node.isSignedMsg,
			authority: new PublicKey(signingAuthority),
		});
	}

	/**
	 * Queues up the txSig to be confirmed in a slower loop, and have tx logs handled
	 * @param txSig
	 */
	protected async registerTxSigToConfirm(
		txSig: TransactionSignature,
		now: number,
		nodeFilled: Array<NodeToFillWithBuffer>,
		fillTxId: number,
		txType: TxType,
		sentSlot?: number,
		cuLimit?: number,
		fillType?: string
	) {
		this.pendingTxSigsToconfirm.set(txSig, {
			ts: now,
			nodeFilled,
			fillTxId,
			txType,
			sentSlot,
			cuLimit,
			fillType,
		});
		const user = this.velocityClient.getUser(this.subaccount);
		this.sentTxsCounter?.add(1, {
			txType,
			...metricAttrFromUserAccount(
				user.userAccountPublicKey,
				user.getUserAccountOrThrow()
			),
		});
	}

	/**
	 * Iterates through a tx's logs and handles it appropriately (e.g. throttling users, updating metrics, etc.)
	 *
	 * @param nodesFilled nodes that we sent a transaction to fill
	 * @param logs logs from tx.meta.logMessages or this.clearingHouse.program._events._eventParser.parseLogs
	 * @returns number of nodes successfully filled, and whether the tx exceeded CUs
	 */
	protected async handleTransactionLogs(
		nodesFilled: Array<NodeToFill>,
		logs: string[] | null | undefined
	): Promise<{ filledNodes: number; exceededCUs: boolean }> {
		if (!logs) {
			return {
				filledNodes: 0,
				exceededCUs: false,
			};
		}

		let inFillIx = false;
		let errorThisFillIx = false;
		let ixIdx = -1; // skip ComputeBudgetProgram
		let successCount = 0;
		let burstedCU = false;
		for (const log of logs) {
			if (log === null) {
				logger.error(`log is null`);
				continue;
			}

			if (log.includes('exceeded maximum number of instructions allowed')) {
				// temporary burst CU limit
				logger.warn(`Using bursted CU limit`);
				this.useBurstCULimit = true;
				this.fillTxSinceBurstCU = 0;
				burstedCU = true;
				continue;
			}

			if (isEndIxLog(this.velocityClient.program.programId.toBase58(), log)) {
				if (!errorThisFillIx) {
					successCount++;
				}

				inFillIx = false;
				errorThisFillIx = false;
				continue;
			}

			if (isIxLog(log)) {
				if (isFillIxLog(log)) {
					inFillIx = true;
					errorThisFillIx = false;
					ixIdx++;
				} else {
					inFillIx = false;
				}
				continue;
			}

			if (!inFillIx) {
				// this is not a log for a fill instruction
				continue;
			}

			// try to handle the log line
			const orderIdDoesNotExist = isOrderDoesNotExistLog(log);
			if (orderIdDoesNotExist) {
				const filledNode = nodesFilled[ixIdx];
				if (filledNode) {
					const isExpired = isOrderExpired(
						filledNode.node.order!,
						Date.now() / 1000,
						true
					);
					logger.error(
						`assoc node (ixIdx: ${ixIdx}): ${filledNode.node.userAccount!.toString()}, ${
							filledNode.node.order!.orderId
						}; does not exist (filled by someone else); ${log}, expired: ${isExpired}, orderTs: ${
							filledNode.node.order!.maxTs
						}, now: ${Date.now() / 1000}`
					);
					if (isExpired) {
						const sig = getNodeToFillSignature(filledNode);
						this.expiredNodesSet.set(sig, true);
					}
				}
				errorThisFillIx = true;
				continue;
			}

			const makerBreachedMaintenanceMargin =
				isMakerBreachedMaintenanceMarginLog(log);
			if (makerBreachedMaintenanceMargin !== null) {
				logger.error(
					`Throttling maker breached maintenance margin: ${makerBreachedMaintenanceMargin}`
				);
				this.setThrottledNode(makerBreachedMaintenanceMargin);
				errorThisFillIx = true;
				break;
			}

			const takerBreachedMaintenanceMargin =
				isTakerBreachedMaintenanceMarginLog(log);
			if (takerBreachedMaintenanceMargin && nodesFilled[ixIdx]) {
				const filledNode = nodesFilled[ixIdx];
				const takerNodeSignature = filledNode.node.userAccount!;
				logger.error(
					`taker breach maint. margin, assoc node (ixIdx: ${ixIdx}): ${filledNode.node.userAccount!.toString()}, ${
						filledNode.node.order!.orderId
					}; (throttling ${takerNodeSignature} and force cancelling orders); ${log}`
				);
				this.setThrottledNode(takerNodeSignature);
				errorThisFillIx = true;
				continue;
			}

			const errFillingLog = isErrFillingLog(log);
			if (errFillingLog) {
				const orderId = errFillingLog[0];
				const userAcc = errFillingLog[1];
				const extractedSig = getFillSignatureFromUserAccountAndOrderId(
					userAcc,
					orderId
				);
				this.setThrottledNode(extractedSig);

				const filledNode = nodesFilled[ixIdx];
				const assocNodeSig = getNodeToFillSignature(filledNode);
				logger.warn(
					`Throttling node due to fill error. extractedSig: ${extractedSig}, assocNodeSig: ${assocNodeSig}, assocNodeIdx: ${ixIdx}`
				);
				errorThisFillIx = true;
				continue;
			}

			if (isErrStaleOracle(log)) {
				logger.error(`Stale oracle error: ${log}`);
				errorThisFillIx = true;
				continue;
			}
		}

		if (!burstedCU) {
			if (this.fillTxSinceBurstCU > TX_COUNT_COOLDOWN_ON_BURST) {
				this.useBurstCULimit = false;
			}
			this.fillTxSinceBurstCU += 1;
		}

		if (logs.length > 0) {
			if (
				logs[logs.length - 1].includes('exceeded CUs meter at BPF instruction')
			) {
				return {
					filledNodes: successCount,
					exceededCUs: true,
				};
			}
		}

		return {
			filledNodes: successCount,
			exceededCUs: false,
		};
	}
}
