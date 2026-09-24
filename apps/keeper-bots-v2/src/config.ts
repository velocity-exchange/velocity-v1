import * as fs from 'fs';
import YAML from 'yaml';
import {
	loadCommaDelimitToArray,
	loadCommaDelimitToStringArray,
	parsePositiveIntArray,
} from './utils';
import {
	BN,
	ConfirmationStrategy,
	VelocityEnv,
	MarketType,
	PerpMarkets,
} from '@velocity-exchange/sdk';
import { EquityFloorGuardConfig } from './bots/equityFloorGuard';
import { PriceFeedProperty } from '@pythnetwork/pyth-lazer-sdk';

export type BaseBotConfig = {
	botId: string;
	dryRun: boolean;
	/// will override {@link GlobalConfig.metricsPort}
	metricsPort?: number;
	runOnce?: boolean;
};

export type UserPnlSettlerConfig = BaseBotConfig & {
	/// perp market indexes to filter for settling pnl
	perpMarketIndicies?: Array<number>;
	/// min abs. USDC threshold before settling pnl
	/// in USDC human terms (100 for 100 USDC)
	settlePnlThresholdUsdc?: number;
	/// max number of users to consider for settling pnl on each iteration
	maxUsersToConsider?: number;
};

export type MakerBidAskTwapCrankConfig = BaseBotConfig & {
	crankIntervalToMarketIndicies?: { [key: number]: number[] };
	/**
	 * When true, on init the bot ensures the keeper has enough insurance-fund
	 * stake in the quote market to run `update_perp_bid_ask_twap`. The program
	 * gates that ix on `if_staked_quote_asset_amount >= 1000` whole quote tokens
	 * and otherwise throws `CantUpdatePerpBidAskTwap` ("Keeper doesnt have min if
	 * stake"). If the keeper's stake is below that floor, the bot tops it up to
	 * `ifStakeTargetQuote` whole quote tokens from the keeper's quote token
	 * account, creating the IF-stake account on first run. Disabled by default.
	 */
	autoStakeIfBelowMin?: boolean;
	/**
	 * Whole quote tokens to top the IF stake up to when auto-staking.
	 * Default 1500.
	 */
	ifStakeTargetQuote?: number;
};

export type SubaccountConfig = {
	[key: number]: Array<number>;
};

export type LiquidatorConfig = BaseBotConfig & {
	/// dlob-server base URL. The liquidator reads a liquidatee's resting CLOB
	/// orders from it and force-cancels them before a perp liquidation, because
	/// a perp liquidation reverts while the account holds CLOB orders. When this
	/// is unset, the liquidator skips the force-cancel and relies on that revert.
	dlobServerHttpUrl?: string;
	disableAutoDerisking: boolean;
	/// Skip the startup sweep that deposits idle wallet token balances into
	/// liquidation subaccounts that have no free collateral.
	disableAutoDeposit?: boolean;
	/// @deprecated, use {@link perpSubAccountConfig} to restrict markets
	perpMarketIndicies?: Array<number>;
	/// @deprecated, use {@link spotSubAccountConfig} to restrict markets
	spotMarketIndicies?: Array<number>;
	perpSubAccountConfig?: SubaccountConfig;
	spotSubAccountConfig?: SubaccountConfig;

	// deprecated: use {@link LiquidatorConfig.maxSlippageBps} (misnamed)
	maxSlippagePct?: number;
	maxSlippageBps?: number;

	/// Wall-clock ms for a derisk order auction. The bot converts it to slots at
	/// the current slot duration.
	deriskAuctionDurationMs?: number;
	twapDurationSec?: number;
	minDepositToLiq?: Map<number, number>;
	excludedAccounts?: Set<string>;
	maxPositionTakeoverPctOfCollateral?: number;
	notifyOnLiquidation?: boolean;

	/// The threshold at which to consider spot asset "dust". Dust will be periodically withdrawn to
	/// authority wallet to free up spot position slots.
	/// In human precision: 100.0 for 100.0 USD worth of spot assets
	spotDustValueThreshold?: number;
	/// Placeholder, liquidator will set this to the raw BN of {@link LiquidatorConfig.spotDustValueThreshold}
	spotDustValueThresholdBN?: BN;
};

export type PythLazerCrankerBotConfig = BaseBotConfig & {
	skipSimulation?: boolean;
	pythLazerChannel?: string;
	ignorePythLazerIds?: number[];
	pythLazerIds?: number[];
	pythLazerIdsByChannel?: {
		real_time: number[];
		'fixed_rate@50ms': number[];
		'fixed_rate@200ms': number[];
	};
	slotStalenessThresholdRestart: number;
	txSuccessRateThreshold: number;
	intervalMs: number;
	onlyCrankUsedOracles?: boolean;
	feedProperties?: PriceFeedProperty[];
	/// Adaptive cranking: when set, each intervalMs tick only posts a chunk if
	/// maxCrankIntervalMs has elapsed since its last post OR any feed in the
	/// chunk moved >= this many bps from its last posted price. intervalMs then
	/// acts as the condition poll rate rather than the post rate. Unset
	/// preserves the legacy post-every-tick behavior.
	crankDivergenceBps?: number;
	/// Max time between posts for one chunk in adaptive mode. It defaults to four
	/// times the live slot duration.
	maxCrankIntervalMs?: number;
};

export type LpPoolTargetBaseCrankerConfig = BaseBotConfig & {
	intervalMs: number;
	lpPoolId: number;
};

export type BotConfigMap = {
	liquidator?: LiquidatorConfig;
	ifRevenueSettler?: BaseBotConfig;
	protocolFeeCollector?: BaseBotConfig;
	fundingRateUpdater?: BaseBotConfig;
	userPnlSettler?: UserPnlSettlerConfig;
	userIdleFlipper?: BaseBotConfig;
	equityFloorGuard?: EquityFloorGuardConfig;
	markTwapCrank?: MakerBidAskTwapCrankConfig;
	pythLazerCranker?: PythLazerCrankerBotConfig;
	swiftTaker?: BaseBotConfig;
	swiftMaker?: BaseBotConfig;
	swiftPlacer?: BaseBotConfig;
	lpTargetBaseCranker?: LpPoolTargetBaseCrankerConfig;
};

export interface GlobalConfig {
	velocityEnv: VelocityEnv;
	/// rpc endpoint to use
	endpoint: string;
	/// ws endpoint to use (inferred from endpoint using web3.js rules, only provide if you want to use a different one)
	wsEndpoint?: string;
	lazerHttpEndpoints?: string[];
	lazerEndpoints?: string[];
	lazerToken?: string;
	/// Read Lazer price messages from the pyth-lazer-relayer's Redis
	/// (`pythLazerData:<feedId>`) instead of each bot opening its own Lazer WS
	/// connections. Requires ELASTICACHE_HOST/PORT and a running relayer.
	pythLazerUseRelayRedis?: boolean;

	// Optional to specify markets loaded by velocity client
	perpMarketsToLoad?: Array<number>;
	spotMarketsToLoad?: Array<number>;

	/// helius endpoint to use helius priority fee strategy
	heliusEndpoint?: string;
	/// additional rpc endpoints to send transactions to
	additionalSendTxEndpoints?: string[];
	/// endpoint to confirm txs on
	txConfirmationEndpoint?: string;
	/// default metrics port to use, will be overridden by {@link BaseBotConfig.metricsPort} if provided
	metricsPort?: number;
	/// disable all metrics
	disableMetrics?: boolean;

	priorityFeeMethod?: string;
	/// HTTP base URL of the cached priority-fee service (velocity dlob-server's
	/// /batchPriorityFees). Set via PRIORITY_FEE_ENDPOINT; defaults per-env.
	/// Point at the in-cluster dlob-server to avoid the public dlob endpoint.
	priorityFeeEndpoint?: string;
	maxPriorityFeeMicroLamports?: number;
	resubTimeoutMs?: number;
	priorityFeeMultiplier?: number;
	keeperPrivateKey?: string;
	initUser?: boolean;
	testLiveness?: boolean;
	cancelOpenOrders?: boolean;
	closeOpenPositions?: boolean;
	forceDeposit?: number | null;
	websocket?: boolean;
	eventSubscriber?: false;
	runOnce?: boolean;
	debug?: boolean;
	subaccounts?: Array<number>;

	eventSubscriberPollingInterval: number;
	bulkAccountLoaderPollingInterval: number;

	useJito?: boolean;
	jitoStrategy?: 'jito-only' | 'non-jito-only' | 'hybrid';
	jitoBlockEngineUrl?: string;
	jitoAuthPrivateKey?: string;
	jitoMinBundleTip?: number;
	jitoMaxBundleTip?: number;
	jitoMaxBundleFailCount?: number;
	jitoTipMultiplier?: number;
	onlySendDuringJitoLeader?: boolean;

	txRetryTimeoutMs?: number;
	txSenderType?: 'fast' | 'retry' | 'while-valid' | 'jet';
	txSenderConfirmationStrategy: ConfirmationStrategy;
	txSkipPreflight?: boolean;
	txMaxRetries?: number;
	trackTxLandRate?: boolean;
	jetTxEndpoints?: string[];

	lutPubkey?: string;
}

export interface Config {
	global: GlobalConfig;
	enabledBots: Array<keyof BotConfigMap>;
	botConfigs?: BotConfigMap;
}

const defaultConfig: Partial<Config> = {
	global: {
		velocityEnv: (process.env.ENV ?? 'devnet') as VelocityEnv,
		initUser: false,
		testLiveness: false,
		cancelOpenOrders: false,
		closeOpenPositions: false,
		forceDeposit: null,
		websocket: false,
		eventSubscriber: false,
		runOnce: false,
		debug: false,
		subaccounts: [0],

		perpMarketsToLoad: parsePositiveIntArray(process.env.PERP_MARKETS_TO_LOAD),
		spotMarketsToLoad: parsePositiveIntArray(process.env.SPOT_MARKETS_TO_LOAD),

		eventSubscriberPollingInterval: 5000,
		bulkAccountLoaderPollingInterval: 5000,

		endpoint: process.env.ENDPOINT!,
		wsEndpoint: process.env.WS_ENDPOINT,
		heliusEndpoint: process.env.HELIUS_ENDPOINT,
		additionalSendTxEndpoints: [],
		txConfirmationEndpoint: process.env.TX_CONFIRMATION_ENDPOINT,
		priorityFeeMethod: process.env.PRIORITY_FEE_METHOD ?? 'solana',
		priorityFeeEndpoint: process.env.PRIORITY_FEE_ENDPOINT,
		maxPriorityFeeMicroLamports: parseInt(
			process.env.MAX_PRIORITY_FEE_MICRO_LAMPORTS ?? '1000000'
		),
		priorityFeeMultiplier: 1.0,
		keeperPrivateKey: process.env.KEEPER_PRIVATE_KEY,

		// Pyth Lazer token is a secret, injected via the PYTH_LAZER_TOKEN env
		// (loadConfigFromFile does no env interpolation, so the YAML config can't
		// carry it without committing the secret). Same pattern as keeperPrivateKey.
		lazerToken: process.env.PYTH_LAZER_TOKEN,

		useJito: false,
		jitoStrategy: 'jito-only',
		jitoMinBundleTip: 10_000,
		jitoMaxBundleTip: 100_000,
		jitoMaxBundleFailCount: 200,
		jitoTipMultiplier: 3,
		jitoBlockEngineUrl: process.env.JITO_BLOCK_ENGINE_URL,
		jitoAuthPrivateKey: process.env.JITO_AUTH_PRIVATE_KEY,
		txRetryTimeoutMs: parseInt(process.env.TX_RETRY_TIMEOUT_MS ?? '30000'),
		onlySendDuringJitoLeader: false,
		txSkipPreflight: false,
		txMaxRetries: 0,
		txSenderConfirmationStrategy: ConfirmationStrategy.Combo,

		metricsPort: 9464,
		disableMetrics: false,
	},
	enabledBots: [],
	botConfigs: {},
};

function mergeDefaults<T>(defaults: T, data: Partial<T>): T {
	const result: T = { ...defaults } as T;

	for (const key in data) {
		const value = data[key];

		if (value === undefined || value === null) {
			continue;
		}
		if (typeof value === 'object' && !Array.isArray(value)) {
			result[key] = mergeDefaults(
				result[key],
				value as Partial<T[Extract<keyof T, string>]>
			);
		} else if (Array.isArray(value)) {
			if (!Array.isArray(result[key])) {
				result[key] = [] as any;
			}

			for (let i = 0; i < value.length; i++) {
				if (typeof value[i] === 'object' && !Array.isArray(value[i])) {
					const existingObj = (result[key] as unknown as any[])[i] || {};
					(result[key] as unknown as any[])[i] = mergeDefaults(
						existingObj,
						value[i]
					);
				} else {
					(result[key] as unknown as any[])[i] = value[i];
				}
			}
		} else {
			result[key] = value as T[Extract<keyof T, string>];
		}
	}

	return result;
}

/**
 * Accepts the old `*Slots` config keys after the rename to `*Ms`. Slot time is
 * no longer a fixed 400ms, so these intervals now carry wall-clock ms. When a
 * deprecated slot-denominated key is present, this function warns. When the new
 * ms key is also unset, it converts the old value at the 400ms baseline, which
 * is the wall-clock time that value originally meant, so behavior stays the
 * same.
 */
function migrateDeprecatedSlotConfigs(config: Partial<Config>): void {
	const BASELINE_MS = 400;
	const renames = [
		{
			bot: 'liquidator',
			oldKey: 'deriskAuctionDurationSlots',
			newKey: 'deriskAuctionDurationMs',
		},
	];

	const botConfigs = config.botConfigs as
		| Record<string, Record<string, unknown>>
		| undefined;
	if (!botConfigs) {
		return;
	}

	for (const { bot, oldKey, newKey } of renames) {
		const botConfig = botConfigs[bot];
		if (!botConfig || botConfig[oldKey] === undefined) {
			continue;
		}
		const oldSlots = botConfig[oldKey];
		if (botConfig[newKey] === undefined && typeof oldSlots === 'number') {
			botConfig[newKey] = oldSlots * BASELINE_MS;
			console.warn(
				`config "${bot}.${oldKey}" is deprecated (renamed to "${newKey}", now in ms): ` +
					`interpreting ${oldSlots} slots as ${
						oldSlots * BASELINE_MS
					}ms at the 400ms baseline. ` +
					`Set "${newKey}" directly to silence this.`
			);
		} else {
			console.warn(
				`config "${bot}.${oldKey}" is deprecated and ignored; use "${newKey}" (ms).`
			);
		}
		delete botConfig[oldKey];
	}
}

export function loadConfigFromFile(path: string): Config {
	if (!path.endsWith('.yaml') && !path.endsWith('.yml')) {
		throw new Error('Config file must be a yaml file');
	}

	const configFile = fs.readFileSync(path, 'utf8');
	const config = YAML.parse(configFile) as Partial<Config>;
	migrateDeprecatedSlotConfigs(config);

	return mergeDefaults(defaultConfig, config) as Config;
}

/**
 * For backwards compatibility, we allow the user to specify the config via command line arguments.
 * @param opts from program.opts()
 * @returns
 */
export function loadConfigFromOpts(opts: any): Config {
	const config: Config = {
		global: {
			velocityEnv: (process.env.ENV ?? 'devnet') as VelocityEnv,
			endpoint: opts.endpoint ?? process.env.ENDPOINT,
			wsEndpoint: opts.wsEndpoint ?? process.env.WS_ENDPOINT,
			heliusEndpoint: opts.heliusEndpoint ?? process.env.HELIUS_ENDPOINT,
			additionalSendTxEndpoints: loadCommaDelimitToStringArray(
				opts.additionalSendTxEndpoints
			),
			txConfirmationEndpoint:
				opts.txConfirmationEndpoint ?? process.env.TX_CONFIRMATION_ENDPOINT,
			priorityFeeMethod:
				opts.priorityFeeMethod ?? process.env.PRIORITY_FEE_METHOD,
			priorityFeeEndpoint:
				opts.priorityFeeEndpoint ?? process.env.PRIORITY_FEE_ENDPOINT,
			maxPriorityFeeMicroLamports: parseInt(
				opts.maxPriorityFeeMicroLamports ??
					process.env.MAX_PRIORITY_FEE_MICRO_LAMPORTS ??
					'1000000'
			),
			priorityFeeMultiplier: parseFloat(opts.priorityFeeMultiplier ?? '1.0'),
			keeperPrivateKey: opts.privateKey ?? process.env.KEEPER_PRIVATE_KEY,
			eventSubscriberPollingInterval: parseInt(
				process.env.BULK_ACCOUNT_LOADER_POLLING_INTERVAL ?? '5000'
			),
			bulkAccountLoaderPollingInterval: parseInt(
				process.env.EVENT_SUBSCRIBER_POLLING_INTERVAL ?? '5000'
			),
			initUser: opts.initUser ?? false,
			testLiveness: opts.testLiveness ?? false,
			cancelOpenOrders: opts.cancelOpenOrders ?? false,
			closeOpenPositions: opts.closeOpenPositions ?? false,
			forceDeposit: opts.forceDeposit ?? null,
			websocket: opts.websocket ?? false,
			eventSubscriber: opts.eventSubscriber ?? false,
			runOnce: opts.runOnce ?? false,
			debug: opts.debug ?? false,
			subaccounts: loadCommaDelimitToArray(opts.subaccount),
			useJito: opts.useJito ?? false,
			jitoStrategy: opts.jitoStrategy ?? 'exclusive',
			jitoMinBundleTip: opts.jitoMinBundleTip ?? 10_000,
			jitoMaxBundleTip: opts.jitoMaxBundleTip ?? 100_000,
			jitoMaxBundleFailCount: opts.jitoMaxBundleFailCount ?? 200,
			jitoTipMultiplier: opts.jitoTipMultiplier ?? 3,
			txRetryTimeoutMs: parseInt(opts.txRetryTimeoutMs ?? '30000'),
			txSenderType: opts.txSenderType ?? 'fast',
			txSkipPreflight: opts.txSkipPreflight
				? opts.txSkipPreflight.toLowerCase() === 'true'
				: false,
			txMaxRetries: parseInt(opts.txMaxRetries ?? '0'),
			trackTxLandRate: opts.trackTxLandRate ?? false,
			txSenderConfirmationStrategy:
				opts.txSenderConfirmationStrategy ?? ConfirmationStrategy.Combo,

			metricsPort: opts.metricsPort ?? 9464,
			disableMetrics: opts.disableMetrics ?? false,
		},
		enabledBots: [],
		botConfigs: {},
	};

	if (opts.liquidator) {
		config.enabledBots.push('liquidator');
		config.botConfigs!.liquidator = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'liquidator',
			metricsPort: 9464,

			disableAutoDerisking: opts.disableAutoDerisking ?? false,
			perpMarketIndicies: loadCommaDelimitToArray(opts.perpMarketIndicies),
			spotMarketIndicies: loadCommaDelimitToArray(opts.spotMarketIndicies),
			runOnce: opts.runOnce ?? false,
			// deprecated: use {@link LiquidatorConfig.maxSlippageBps}
			maxSlippagePct: opts.maxSlippagePct ?? 50,
			maxSlippageBps: opts.maxSlippageBps ?? 50,
			deriskAuctionDurationMs: ((): number => {
				if (opts.deriskAuctionDurationMs !== undefined) {
					return opts.deriskAuctionDurationMs;
				}
				if (opts.deriskAuctionDurationSlots !== undefined) {
					console.warn(
						`opt "deriskAuctionDurationSlots" is deprecated (renamed to "deriskAuctionDurationMs", now in ms): ` +
							`interpreting ${opts.deriskAuctionDurationSlots} slots as ${
								opts.deriskAuctionDurationSlots * 400
							}ms at the 400ms baseline.`
					);
					return opts.deriskAuctionDurationSlots * 400;
				}
				return 40_000;
			})(),
			twapDurationSec: parseInt(opts.twapDurationSec ?? '300'),
			notifyOnLiquidation: opts.notifyOnLiquidation ?? false,
		};
	}
	if (opts.ifRevenueSettler) {
		config.enabledBots.push('ifRevenueSettler');
		config.botConfigs!.ifRevenueSettler = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'ifRevenueSettler',
			metricsPort: 9464,
			runOnce: opts.runOnce ?? false,
		};
	}
	if (opts.protocolFeeCollector) {
		config.enabledBots.push('protocolFeeCollector');
		config.botConfigs!.protocolFeeCollector = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'protocolFeeCollector',
			metricsPort: 9464,
			runOnce: opts.runOnce ?? false,
		};
	}
	if (opts.userPnlSettler) {
		config.enabledBots.push('userPnlSettler');
		config.botConfigs!.userPnlSettler = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'userPnlSettler',
			metricsPort: 9464,
			runOnce: opts.runOnce ?? false,
			perpMarketIndicies: loadCommaDelimitToArray(opts.perpMarketIndicies),
			settlePnlThresholdUsdc: Number(opts.settlePnlThresholdUsdc) ?? 10,
			maxUsersToConsider: Number(opts.maxUsersToConsider) ?? 50,
		};
	}
	if (opts.userIdleFlipper) {
		config.enabledBots.push('userIdleFlipper');
		config.botConfigs!.userIdleFlipper = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'userIdleFlipper',
			metricsPort: 9464,
			runOnce: opts.runOnce ?? false,
		};
	}
	if (opts.equityFloorGuard) {
		config.enabledBots.push('equityFloorGuard');
		config.botConfigs!.equityFloorGuard = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'equityFloorGuard',
			metricsPort: 9464,
			runOnce: opts.runOnce ?? false,
		};
	}
	if (opts.fundingRateUpdater) {
		config.enabledBots.push('fundingRateUpdater');
		config.botConfigs!.fundingRateUpdater = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'fundingRateUpdater',
			metricsPort: 9464,
			runOnce: opts.runOnce ?? false,
		};
	}

	if (opts.markTwapCrank) {
		config.enabledBots.push('markTwapCrank');
		config.botConfigs!.markTwapCrank = {
			dryRun: opts.dryRun ?? false,
			botId: process.env.BOT_ID ?? 'crank',
			metricsPort: 9464,
			runOnce: opts.runOnce ?? false,
			autoStakeIfBelowMin: process.env.AUTO_STAKE_IF_BELOW_MIN === 'true',
			ifStakeTargetQuote: process.env.IF_STAKE_TARGET_QUOTE
				? parseInt(process.env.IF_STAKE_TARGET_QUOTE)
				: undefined,
		};
	}
	return mergeDefaults(defaultConfig, config) as Config;
}

export function configHasBot(
	config: Config,
	botName: keyof BotConfigMap
): boolean {
	const botEnabled = config.enabledBots.includes(botName) ?? false;
	const botConfigExists = config.botConfigs![botName] !== undefined;
	if (botEnabled && !botConfigExists) {
		throw new Error(
			`Bot ${botName} is enabled but no config was found for it.`
		);
	}
	return botEnabled && botConfigExists;
}

export const PULL_ORACLE_WHITELIST: {
	marketType: MarketType;
	marketIndex: number;
}[] = [
	{
		marketType: MarketType.PERP,
		marketIndex: 17,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 3,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 26,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 25,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 32,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 13,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 11,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 28,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 35,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 8,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 33,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 14,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 6,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 5,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 27,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 29,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 21,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 22,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 16,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 20,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 34,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 15,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 15,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 7,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 10,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 18,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 9,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 19,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 24,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 30,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 12,
	},
	{
		marketType: MarketType.PERP,
		marketIndex: 4,
	},
];

export const DEVNET_PULL_ORACLE_WHITELIST: {
	marketType: MarketType;
	marketIndex: number;
}[] = PerpMarkets['devnet'].map((mkt) => {
	return { marketType: MarketType.PERP, marketIndex: mkt.marketIndex };
});
