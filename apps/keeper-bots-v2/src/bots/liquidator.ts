/* eslint-disable @typescript-eslint/no-non-null-assertion */
import {
	BN,
	convertToNumber,
	VelocityClient,
	User,
	isVariant,
	BASE_PRECISION,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	PerpPosition,
	UserMap,
	ZERO,
	getTokenAmount,
	SpotPosition,
	PerpMarketAccount,
	SpotMarketAccount,
	QUOTE_SPOT_MARKET_INDEX,
	TEN_THOUSAND,
	isUserBankrupt,
	hasIsolatedMarginBankrupt,
	TEN,
	PriorityFeeSubscriber,
	getTokenValue,
	calculateClaimablePnl,
	isOperationPaused,
	PerpOperation,
	standardizeBaseAssetAmount,
} from '@velocity-exchange/sdk';

import { PrometheusExporter } from '@opentelemetry/exporter-prometheus';
import {
	ExplicitBucketHistogramAggregation,
	InstrumentType,
	MeterProvider,
	View,
} from '@opentelemetry/sdk-metrics-base';
import {
	Meter,
	ObservableGauge,
	BatchObservableResult,
	Histogram,
} from '@opentelemetry/api-metrics';

import { logger } from '../logger';
import { Bot } from '../types';
import { RuntimeSpec, metricAttrFromUserAccount } from '../metrics';
import { webhookMessage } from '../webhook';
import { LiquidatorConfig } from '../config';
import {
	getPerpMarketTierNumber,
	perpTierIsAsSafeAs,
} from '@velocity-exchange/sdk';
import {
	PublicKey,
	AddressLookupTableAccount,
	TransactionInstruction,
} from '@solana/web3.js';
import { getAssociatedTokenAddressSync } from '@solana/spl-token';
import {
	buildVersionedTransactionWithSimulatedCus as buildSimulatedCuTx,
	calculateAccountValueUsd,
	handleSimResultError,
	SimulateAndGetTxWithCUsResponse,
} from '../utils';
import { LiquidatorDerisk } from './liquidatorDerisk';

const errorCodesToSuppress = [
	6004, // Error Number: 6004. Error Message: Sufficient collateral.
	6010, // Error Number: 6010. Error Message: User Has No Position In Market.
	6114, // Error Number: 6114. Error Message: Position Doesnt Have Open Position Or Orders.
	6122, // Error Number: 6122. Error Message: Invalid Base Asset Amount For Liquidate Perp.
];

const LIQUIDATE_THROTTLE_BACKOFF = 5000; // the time to wait before trying to liquidate a throttled user again

// Markets whose positions are closed out of band. Never send a liquidation
// against one of these. The list can go once every market settles normally.
const PERP_MARKETS_NEVER_LIQUIDATED: number[] = [];

/** A perp position, reduced to what liquidation-exit market selection depends on. */
export type LiquidationExitPosition = {
	marketIndex: number;
	hasBase: boolean;
	hasOpenOrder: boolean;
	hasPnl: boolean;
	isolated: boolean;
	/** The position's own `BeingLiquidated`/`Bankruptcy` flag (isolated only). */
	flagged: boolean;
};

export type LiquidationExitCandidate = {
	/**
	 * `crank` clears a stuck flag via zero-base liquidate_perp; `base` is
	 * step-sized; `pnl` is liquidate_perp_pnl_for_deposit or settle_pnl.
	 */
	kind: 'crank' | 'base' | 'pnl';
	marketIndex: number;
};

/**
 * Orders the calls that could clear a stuck `beingLiquidated` flag, best first.
 *
 * The scope matters because the program picks its liquidation mode from the
 * position in the target market. An isolated position's own flag is cleared only
 * by targeting that market. The account-level flag is cleared only by targeting
 * a cross position, or a market holding no position at all. Within a scope the
 * order is cheapest first. Cancelling orders can free enough margin on its own,
 * a sized liquidation is the fallback, and a pnl-only slot needs a different
 * instruction.
 */
export function selectLiquidationExitCandidates(
	positions: LiquidationExitPosition[],
	isCrossFlagged: boolean,
	actionableMarkets: number[]
): LiquidationExitCandidate[] {
	const candidates: LiquidationExitCandidate[] = [];

	const rank = (
		position: LiquidationExitPosition
	): LiquidationExitCandidate['kind'] | undefined => {
		if (position.hasOpenOrder && !position.hasBase) {
			return 'crank';
		}
		if (position.hasBase) {
			return 'base';
		}
		return position.hasPnl ? 'pnl' : undefined;
	};
	const order: LiquidationExitCandidate['kind'][] = ['crank', 'base', 'pnl'];
	const push = (from: LiquidationExitPosition[]) => {
		for (const kind of order) {
			for (const position of from) {
				if (
					actionableMarkets.includes(position.marketIndex) &&
					rank(position) === kind
				) {
					candidates.push({ kind, marketIndex: position.marketIndex });
				}
			}
		}
	};

	push(positions.filter((p) => p.isolated && p.flagged));

	if (isCrossFlagged) {
		push(positions.filter((p) => !p.isolated));

		// Last resort for the account-level flag. A market the user holds nothing
		// in takes cross mode on chain, so the early exit still fires. This is the
		// only call that clears an account whose positions are all gone, which is
		// the common shape once the last position is settled away.
		const emptyMarket = actionableMarkets.find(
			(marketIndex) => !positions.some((p) => p.marketIndex === marketIndex)
		);
		if (emptyMarket !== undefined) {
			candidates.push({ kind: 'crank', marketIndex: emptyMarket });
		}
	}

	return candidates;
}

function calculateSpotTokenAmountToLiquidate(
	velocityClient: VelocityClient,
	liquidatorUser: User,
	liquidateePosition: SpotPosition,
	maxPositionTakeoverPctOfCollateralNum: BN,
	maxPositionTakeoverPctOfCollateralDenom: BN
): BN {
	const spotMarket = velocityClient.getSpotMarketAccount(
		liquidateePosition.marketIndex
	);
	if (!spotMarket) {
		logger.error(`No spot market found for ${liquidateePosition.marketIndex}`);
		return ZERO;
	}

	const tokenPrecision = new BN(10 ** spotMarket.decimals);

	const oraclePrice = velocityClient.getOracleDataForSpotMarket(
		liquidateePosition.marketIndex
	).price;
	const collateralToSpend = liquidatorUser
		.getFreeCollateral()
		.mul(PRICE_PRECISION)
		.mul(maxPositionTakeoverPctOfCollateralNum)
		.mul(tokenPrecision);
	const maxSpendTokenAmountToLiquidate = collateralToSpend.div(
		oraclePrice
			.mul(QUOTE_PRECISION)
			.mul(maxPositionTakeoverPctOfCollateralDenom)
	);

	const liquidateeTokenAmount = getTokenAmount(
		liquidateePosition.scaledBalance,
		spotMarket,
		liquidateePosition.balanceType
	);

	if (maxSpendTokenAmountToLiquidate.gt(liquidateeTokenAmount)) {
		return liquidateeTokenAmount;
	} else {
		return maxSpendTokenAmountToLiquidate;
	}
}

enum METRIC_TYPES {
	total_leverage = 'total_leverage',
	total_collateral = 'total_collateral',
	free_collateral = 'free_collateral',
	perp_posiiton_value = 'perp_position_value',
	perp_posiiton_base = 'perp_position_base',
	perp_posiiton_quote = 'perp_position_quote',
	initial_margin_requirement = 'initial_margin_requirement',
	maintenance_margin_requirement = 'maintenance_margin_requirement',
	initial_margin = 'initial_margin',
	maintenance_margin = 'maintenance_margin',
	unrealized_pnl = 'unrealized_pnl',
	unrealized_funding_pnl = 'unrealized_funding_pnl',
	sdk_call_duration_histogram = 'sdk_call_duration_histogram',
	runtime_specs = 'runtime_specs',
	user_map_user_account_keys = 'user_map_user_account_keys',
}

enum BOT_STATE {
	IDLE = 'IDLE',
	LIQUIDATING = 'LIQUIDATING',
	DERISKING = 'DERISKING',
}

/**
 * LiquidatorBot implements a simple liquidation bot for the Velocity V1 Protocol. Liquidations work by taking over
 * a portion of the endangered account's position, so collateral is required in order to run this bot. The bot
 * will spend at most maxPositionTakeoverPctOfCollateral of its free collateral on any endangered account.
 *
 */
export class LiquidatorBot implements Bot {
	public readonly name: string;
	public readonly dryRun: boolean;
	public readonly defaultIntervalMs: number = 5000;

	private metricsInitialized = false;
	private metricsPort?: number;
	private meter?: Meter;
	private exporter?: PrometheusExporter;
	private throttledUsers = new Map<string, number>();
	private disableAutoDerisking: boolean;
	private liquidatorConfig: LiquidatorConfig;

	// metrics
	private runtimeSpecsGauge?: ObservableGauge;
	private totalLeverage?: ObservableGauge;
	private totalCollateral?: ObservableGauge;
	private freeCollateral?: ObservableGauge;
	private perpPositionValue?: ObservableGauge;
	private perpPositionBase?: ObservableGauge;
	private perpPositionQuote?: ObservableGauge;
	private initialMarginRequirement?: ObservableGauge;
	private maintenanceMarginRequirement?: ObservableGauge;
	private unrealizedPnL?: ObservableGauge;
	private unrealizedFundingPnL?: ObservableGauge;
	private sdkCallDurationHistogram?: Histogram;
	private userMapUserAccountKeysGauge?: ObservableGauge;

	private velocityClient: VelocityClient;
	/// Reads a liquidatee's resting CLOB orders off the dlob-server user-orders
	/// feed, so they can be force-cancelled before a perp liquidation. Undefined
	/// when no dlob-server URL is configured.
	private serumLookupTableAddress?: PublicKey;
	private velocityLookupTables?: AddressLookupTableAccount[];
	private velocitySpotLookupTables?: AddressLookupTableAccount;

	private perpMarketIndicies: number[] = [];
	private spotMarketIndicies: number[] = [];
	private defaultSubaccountId: number;
	private allSubaccounts: Set<number>;
	private perpMarketToSubAccount: Map<number, number>;
	private spotMarketToSubAccount: Map<number, number>;
	private intervalIds: Array<NodeJS.Timer> = [];
	private userMap: UserMap;
	private runtimeSpecs: RuntimeSpec;
	private excludedAccounts: Set<string>;

	private priorityFeeSubscriber: PriorityFeeSubscriber;

	private botState: BOT_STATE = BOT_STATE.IDLE;
	private deriskHelper?: LiquidatorDerisk;

	private tickInProgress = false;

	/**
	 * Max percentage of collateral to spend on liquidating a single position.
	 */
	private maxPositionTakeoverPctOfCollateralNum: BN;
	private maxPositionTakeoverPctOfCollateralDenom = new BN(100);

	private watchdogTimerLastPatTime = Date.now();

	constructor(
		velocityClient: VelocityClient,
		userMap: UserMap,
		runtimeSpec: RuntimeSpec,
		config: LiquidatorConfig,
		defaultSubaccountId: number,
		priorityFeeSubscriber: PriorityFeeSubscriber,
		SERUM_LOOKUP_TABLE?: PublicKey
	) {
		this.liquidatorConfig = config;
		if (this.liquidatorConfig.maxSlippageBps === undefined) {
			this.liquidatorConfig.maxSlippageBps =
				this.liquidatorConfig.maxSlippagePct!;
		}
		if (this.liquidatorConfig.deriskAuctionDurationMs === undefined) {
			this.liquidatorConfig.deriskAuctionDurationMs = 40_000;
		}

		if (this.liquidatorConfig.spotDustValueThreshold !== undefined) {
			this.liquidatorConfig.spotDustValueThresholdBN = new BN(
				Math.floor(
					this.liquidatorConfig.spotDustValueThreshold *
						QUOTE_PRECISION.toNumber()
				)
			);
			logger.info(
				`Liquidator config.spotDustValueThresholdBN: ${this.liquidatorConfig.spotDustValueThresholdBN.toString()}`
			);
		}

		this.name = config.botId;
		this.dryRun = config.dryRun;
		this.velocityClient = velocityClient;

		this.runtimeSpecs = runtimeSpec;
		this.userMap = userMap;

		this.metricsPort = config.metricsPort;
		if (this.metricsPort) {
			this.initializeMetrics();
		}

		if (!config.disableAutoDerisking) {
			this.disableAutoDerisking = false;
		} else {
			this.disableAutoDerisking = config.disableAutoDerisking;
		}
		logger.info(
			`${this.name} disableAutoDerisking: ${this.disableAutoDerisking}, notifyLiquidations: ${this.liquidatorConfig.notifyOnLiquidation}`
		);

		this.defaultSubaccountId = defaultSubaccountId;
		this.allSubaccounts = new Set<number>();
		this.allSubaccounts.add(defaultSubaccountId);
		this.perpMarketToSubAccount = new Map<number, number>();
		this.spotMarketToSubAccount = new Map<number, number>();

		if (
			config.perpSubAccountConfig &&
			Object.keys(config.perpSubAccountConfig).length > 0
		) {
			logger.info('Loading perp markets to watch from perpSubAccountConfig');
			for (const subAccount of Object.keys(config.perpSubAccountConfig)) {
				const marketsForAccount =
					config.perpSubAccountConfig[parseInt(subAccount)] || [];
				for (const market of marketsForAccount) {
					this.perpMarketToSubAccount.set(market, parseInt(subAccount));
					this.allSubaccounts.add(parseInt(subAccount));
				}
			}
			this.perpMarketIndicies = Object.values(
				config.perpSubAccountConfig
			).flat();
		} else {
			// this will be done in `init` since velocityClient needs to be subcribed to first
			logger.info(
				`No perpSubAccountConfig provided, will watch all perp markets on subaccount ${this.defaultSubaccountId}`
			);
		}

		if (
			config.spotSubAccountConfig &&
			Object.keys(config.spotSubAccountConfig).length > 0
		) {
			logger.info('Loading spot markets to watch from spotSubAccountConfig');
			for (const subAccount of Object.keys(config.spotSubAccountConfig)) {
				const marketsForAccount =
					config.spotSubAccountConfig[parseInt(subAccount)] || [];
				for (const market of marketsForAccount) {
					this.spotMarketToSubAccount.set(market, parseInt(subAccount));
					this.allSubaccounts.add(parseInt(subAccount));
				}
			}
			this.spotMarketIndicies = Object.values(
				config.spotSubAccountConfig
			).flat();
		} else {
			// this will be done in `init` since velocityClient needs to be subcribed to first
			logger.info(
				`No spotSubAccountConfig provided, will watch all spot markets on subaccount ${this.defaultSubaccountId}`
			);
		}

		// Load a list of accounts that we will *not* bother trying to liquidate
		// For whatever reason, this value is being parsed as an array even though
		// it is declared as a set, so if we merely assign it, then we'd end up
		// with a compile error later saying `has is not a function`.
		this.excludedAccounts = new Set<string>(config.excludedAccounts);
		logger.info(`Liquidator disregarding accounts: ${config.excludedAccounts}`);

		// ensure velocityClient has all subaccounts tracked and subscribed
		for (const subaccount of this.allSubaccounts) {
			if (!this.velocityClient.hasUser(subaccount)) {
				this.velocityClient.addUser(subaccount).then((subscribed) => {
					logger.info(
						`Added subaccount ${subaccount} to velocityClient since it was missing (subscribed: ${subscribed}))`
					);
				});
			}
		}

		this.priorityFeeSubscriber = priorityFeeSubscriber;
		this.priorityFeeSubscriber.updateAddresses([
			new PublicKey('6gMq3mRCKf8aP3ttTyYhuijVZ2LGi14oDsBbkgubfLB3'), // USDC SPOT
		]);

		// initialize derisk helper after lookup tables, and optional jupiter are ready
		this.deriskHelper = new LiquidatorDerisk({
			velocityClient: this.velocityClient,
			userMap: this.userMap,
			config: this.liquidatorConfig,
			name: this.name,
			priorityFeeSubscriber: this.priorityFeeSubscriber,
			velocityLookupTables: this.velocityLookupTables,
			velocitySpotLookupTables: this.velocitySpotLookupTables,
		});

		if (!config.maxPositionTakeoverPctOfCollateral) {
			// spend 50% of collateral by default
			this.maxPositionTakeoverPctOfCollateralNum = new BN(50);
		} else {
			if (config.maxPositionTakeoverPctOfCollateral > 1.0) {
				logger.warn(
					`liquidator.config.maxPositionTakeoverPctOfCollateral is > 1.00: ${config.maxPositionTakeoverPctOfCollateral}`
				);
			}
			this.maxPositionTakeoverPctOfCollateralNum = new BN(
				config.maxPositionTakeoverPctOfCollateral * 100.0
			);
		}
		this.serumLookupTableAddress = SERUM_LOOKUP_TABLE;
	}

	private getSubAccountIdToLiquidatePerp(
		marketIndex: number
	): number | undefined {
		if (!this.perpMarketIndicies.includes(marketIndex)) {
			return undefined;
		}
		const subAccountId = this.perpMarketToSubAccount.get(marketIndex);
		if (subAccountId === undefined) {
			logger.info(
				`no configured subaccount to liquidate perp market ${marketIndex}`
			);
			return undefined;
		}

		if (!this.hasCollateralToLiquidate(subAccountId)) {
			logger.info(
				`subaccount ${subAccountId} has insufficient collateral to liquidate perp market ${marketIndex}`
			);
			return undefined;
		}
		return subAccountId;
	}

	private getSubAccountIdToLiquidateSpot(
		marketIndex: number
	): number | undefined {
		if (!this.spotMarketIndicies.includes(marketIndex)) {
			return undefined;
		}
		const subAccountId = this.spotMarketToSubAccount.get(marketIndex);
		if (subAccountId === undefined) {
			logger.info(
				`no configured subaccount to liquidate spot market ${marketIndex}`
			);
			return undefined;
		}
		if (!this.hasCollateralToLiquidate(subAccountId)) {
			logger.info(
				`subaccount ${subAccountId} has insufficient collateral to liquidate spot market ${marketIndex}`
			);
			return undefined;
		}
		return subAccountId;
	}

	private getLiquidatorUserForSpotMarket(
		marketIndex: number
	): User | undefined {
		const subAccountId = this.spotMarketToSubAccount.get(marketIndex);
		if (subAccountId === undefined) {
			return undefined;
		}
		return this.velocityClient.getUser(subAccountId);
	}

	private getLiquidatorUserForPerpMarket(
		marketIndex: number
	): User | undefined {
		const subAccountId = this.perpMarketToSubAccount.get(marketIndex);
		if (subAccountId === undefined) {
			return undefined;
		}
		return this.velocityClient.getUser(subAccountId);
	}

	private async buildVersionedTransactionWithSimulatedCus(
		ixs: Array<TransactionInstruction>,
		luts: Array<AddressLookupTableAccount>,
		cuPriceMicroLamports?: number
	): Promise<SimulateAndGetTxWithCUsResponse> {
		return await buildSimulatedCuTx(
			this.velocityClient,
			ixs,
			luts,
			cuPriceMicroLamports
		);
	}

	public async init() {
		logger.info(`${this.name} initing`);

		this.velocityLookupTables =
			await this.velocityClient.fetchAllLookupTableAccounts();

		let serumLut: AddressLookupTableAccount | null = null;
		if (this.serumLookupTableAddress !== undefined) {
			serumLut = (
				await this.velocityClient.connection.getAddressLookupTable(
					this.serumLookupTableAddress
				)
			).value;
		}
		if (this.runtimeSpecs.velocityEnv === 'mainnet-beta' && serumLut === null) {
			throw new Error(
				`Failed to load LUT for velocity spot accounts at ${this.serumLookupTableAddress?.toBase58()}, jupiter swaps will fail`
			);
		} else {
			this.velocitySpotLookupTables = serumLut!;
		}

		// If no perp subaccount config was provided, map all perp markets to the default subaccount
		if (this.perpMarketIndicies.length === 0) {
			this.perpMarketIndicies = this.velocityClient
				.getPerpMarketAccounts()
				.map((m) => {
					return m.marketIndex;
				});

			for (const market of this.perpMarketIndicies) {
				this.perpMarketToSubAccount.set(market, this.defaultSubaccountId);
			}
		}

		// If no spot subaccount config was provided, map all spot markets to the default subaccount
		if (this.spotMarketIndicies.length === 0) {
			this.spotMarketIndicies = this.velocityClient
				.getSpotMarketAccounts()
				.map((m) => {
					return m.marketIndex;
				});

			for (const market of this.spotMarketIndicies) {
				this.spotMarketToSubAccount.set(market, this.defaultSubaccountId);
			}
		}
		logger.info(
			`SubAccountID -> perpMarketIndex:\n${JSON.stringify(
				Object.fromEntries(this.perpMarketToSubAccount),
				null,
				2
			)}`
		);
		logger.info(
			`SubAccountID -> spotMarketIndex:\n${JSON.stringify(
				Object.fromEntries(this.spotMarketToSubAccount),
				null,
				2
			)}`
		);

		// Update derisk helper with loaded lookup tables
		this.deriskHelper?.setLookupTables(
			this.velocityLookupTables,
			this.velocitySpotLookupTables
		);

		if (!this.liquidatorConfig.disableAutoDeposit) {
			await this.autoDepositIdleWalletFunds();
		}

		await webhookMessage(`[${this.name}]: started`);
	}

	/**
	 * Deposits idle wallet token balances into liquidation subaccounts that
	 * cannot size a liquidation. A subaccount needs enough free collateral to
	 * buy one step of the perp markets it liquidates. Below that amount it
	 * sizes every liquidation under the step size, and the bot skips it. A
	 * small balance is as unusable as no balance, because the subaccount never
	 * fills a liquidation and never grows the balance. For each such
	 * subaccount, sweep the wallet's full ATA balance of every spot market
	 * mapped to it. Disable with `disableAutoDeposit`.
	 */
	private async autoDepositIdleWalletFunds(): Promise<void> {
		for (const subAccountId of this.allSubaccounts) {
			const freeCollateral = this.velocityClient
				.getUser(subAccountId)
				.getFreeCollateral('Initial');
			if (
				freeCollateral.gt(this.minFreeCollateralForSubAccount(subAccountId))
			) {
				continue;
			}

			for (const [
				marketIndex,
				mappedSubAccount,
			] of this.spotMarketToSubAccount.entries()) {
				if (mappedSubAccount !== subAccountId) {
					continue;
				}
				const spotMarket =
					this.velocityClient.getSpotMarketAccount(marketIndex);
				if (!spotMarket) {
					continue;
				}
				const ata = getAssociatedTokenAddressSync(
					spotMarket.mint,
					this.velocityClient.wallet.publicKey,
					true,
					this.velocityClient.getTokenProgramForSpotMarket(spotMarket)
				);
				let walletBalance = ZERO;
				try {
					const balance =
						await this.velocityClient.connection.getTokenAccountBalance(ata);
					walletBalance = new BN(balance.value.amount);
				} catch (e) {
					// no ATA for this mint
					continue;
				}
				if (walletBalance.lte(ZERO)) {
					continue;
				}

				if (this.dryRun) {
					logger.info(
						`[${
							this.name
						}]: dry run - would auto-deposit ${walletBalance.toString()} into spot market ${marketIndex} for subaccount ${subAccountId}`
					);
					continue;
				}

				try {
					const txSig = await this.velocityClient.deposit(
						walletBalance,
						marketIndex,
						ata,
						subAccountId
					);
					const msg = `[${
						this.name
					}]: subaccount ${subAccountId} cannot size a liquidation, auto-deposited wallet balance ${walletBalance.toString()} into spot market ${marketIndex}. tx: ${txSig}`;
					logger.info(msg);
					webhookMessage(msg);
				} catch (e) {
					const msg = `[${
						this.name
					}]: :x: failed to auto-deposit ${walletBalance.toString()} into spot market ${marketIndex} for subaccount ${subAccountId}: ${
						e instanceof Error ? e.message : e
					}`;
					logger.error(msg);
					webhookMessage(msg);
				}
			}
		}
	}

	public async reset() {
		for (const intervalId of this.intervalIds) {
			clearInterval(intervalId as NodeJS.Timeout);
		}
		this.intervalIds = [];

		await this.userMap!.unsubscribe();
	}

	public async startIntervalLoop(intervalMs?: number): Promise<void> {
		const effectiveIntervalMs = intervalMs ?? this.defaultIntervalMs;
		await this.runMainTick();
		const intervalId = setInterval(
			this.runMainTick.bind(this),
			effectiveIntervalMs
		);
		this.intervalIds.push(intervalId);

		logger.info(`${this.name} Bot started!`);

		for (const subAccount of this.allSubaccounts) {
			const freeCollateral = this.velocityClient
				.getUser(subAccount)
				.getFreeCollateral();
			const accountValue = calculateAccountValueUsd(
				this.velocityClient.getUser(subAccount)
			);
			logger.info(
				`[${
					this.name
				}] Subaccount: ${subAccount}:  free collateral: $${convertToNumber(
					freeCollateral,
					QUOTE_PRECISION
				)}, accountValueUsd: ${accountValue} spending at most ${
					(this.maxPositionTakeoverPctOfCollateralNum.toNumber() /
						this.maxPositionTakeoverPctOfCollateralDenom.toNumber()) *
					100.0
				}% per liquidation`
			);
		}
	}

	private async runMainTick(): Promise<void> {
		if (this.tickInProgress) {
			logger.info(`${this.name} tick in progress, skipping.`);
			return;
		}
		this.tickInProgress = true;
		try {
			const didDerisk = await this.derisk();
			if (!didDerisk) {
				await this.tryLiquidateStart();
			} else {
				logger.info(
					`${this.name} skipped liquidation after derisking changes.`
				);
			}
		} finally {
			// Pat watchdog at end of every tick, regardless of outcome
			this.watchdogTimerLastPatTime = Date.now();
			this.tickInProgress = false;
		}
	}

	public async healthCheck(): Promise<boolean> {
		// check if we've ran the main loop recently
		const timeSinceLastPat = Date.now() - this.watchdogTimerLastPatTime;
		const healthy = timeSinceLastPat < 3 * this.defaultIntervalMs;

		if (!healthy) {
			logger.error(
				`Bot ${this.name} is unhealthy, last pat time: ${timeSinceLastPat}ms ago`
			);
		}

		return healthy;
	}

	/**
	 * attempts to close out any open positions on this account. It starts by cancelling any open orders
	 */
	private async derisk(): Promise<boolean> {
		if (this.disableAutoDerisking) {
			return false;
		}
		if (this.botState !== BOT_STATE.IDLE) {
			logger.info(`${this.name} derisk skipped, state=${this.botState}`);
			return false;
		}
		this.setState(BOT_STATE.DERISKING);
		try {
			const didWork = await this.deriskHelper!.deriskAllSubaccounts(
				Array.from(this.allSubaccounts)
			);
			return didWork;
		} catch (e) {
			const err = e as Error;
			logger.error(
				`Error in derisk: ${err.message}\n${err.stack ? err.stack : ''}`
			);
			return false;
		} finally {
			this.setState(BOT_STATE.IDLE);
		}
	}

	/**
	 * Sizes the perp liquidation for one endangered position.
	 *
	 * The program rounds the amount down to the market step size. It then
	 * refuses a zero amount with InvalidBaseAssetAmountForLiquidatePerp. This
	 * function standardizes the size for the same step. A size below one step
	 * becomes zero, and the caller skips a transaction that must fail.
	 */
	private calculateBaseAmountToLiquidate(liquidateePosition: PerpPosition): BN {
		const liquidatorUser = this.getLiquidatorUserForPerpMarket(
			liquidateePosition.marketIndex
		);
		if (!liquidatorUser) {
			return ZERO;
		}

		const perpMarket = this.velocityClient.getPerpMarketAccount(
			liquidateePosition.marketIndex
		);
		if (!perpMarket) {
			return ZERO;
		}

		const oraclePrice = this.velocityClient.getOracleDataForPerpMarket(
			liquidateePosition.marketIndex
		).price;
		const collateralToSpend = liquidatorUser
			.getFreeCollateral()
			.mul(PRICE_PRECISION)
			.mul(this.maxPositionTakeoverPctOfCollateralNum)
			.mul(BASE_PRECISION);
		const baseAssetAmountToLiquidate = collateralToSpend.div(
			oraclePrice
				.mul(QUOTE_PRECISION)
				.mul(this.maxPositionTakeoverPctOfCollateralDenom)
		);

		const liquidateeBaseAssetAmount = liquidateePosition.baseAssetAmount.abs();
		return standardizeBaseAssetAmount(
			BN.min(baseAssetAmountToLiquidate, liquidateeBaseAssetAmount),
			perpMarket.orderStepSize
		);
	}

	/**
	 * The free collateral that a subaccount needs to size one liquidation. The
	 * value is the step notional of the largest perp market that it
	 * liquidates. The bot spends at most maxPositionTakeoverPctOfCollateral of
	 * free collateral per liquidation. The notional is divided by that
	 * fraction.
	 */
	private minFreeCollateralForSubAccount(subAccountId: number): BN {
		if (this.maxPositionTakeoverPctOfCollateralNum.lte(ZERO)) {
			return ZERO;
		}

		let minFreeCollateral = ZERO;
		for (const [
			marketIndex,
			mappedSubAccount,
		] of this.perpMarketToSubAccount.entries()) {
			if (mappedSubAccount !== subAccountId) {
				continue;
			}
			const perpMarket = this.velocityClient.getPerpMarketAccount(marketIndex);
			if (!perpMarket) {
				continue;
			}
			const oraclePrice =
				this.velocityClient.getOracleDataForPerpMarket(marketIndex).price;
			const stepNotional = perpMarket.orderStepSize
				.mul(oraclePrice)
				.mul(this.maxPositionTakeoverPctOfCollateralDenom)
				.div(BASE_PRECISION)
				.div(this.maxPositionTakeoverPctOfCollateralNum);
			minFreeCollateral = BN.max(minFreeCollateral, stepNotional);
		}
		return minFreeCollateral;
	}

	/**
	 * Find the user perp position with the largest loss, resolve bankruptcy on this market.
	 *
	 * @param chUserToCheck
	 * @returns
	 */
	private findPerpBankruptingMarkets(chUserToCheck: User): Array<number> {
		const bankruptMarketIndices: Array<number> = [];

		for (const market of this.velocityClient.getPerpMarketAccounts()) {
			const position = chUserToCheck.getPerpPosition(market.marketIndex);
			if (!position || position.quoteAssetAmount.gte(ZERO)) {
				// invalid position to liquidate
				continue;
			}
			bankruptMarketIndices.push(position.marketIndex);
		}

		return bankruptMarketIndices;
	}

	/**
	 * Find the user spot position with the largest loss, resolve bankruptcy on this market.
	 *
	 * @param chUserToCheck
	 * @returns
	 */
	private findSpotBankruptingMarkets(chUserToCheck: User): Array<number> {
		const bankruptMarketIndices: Array<number> = [];

		for (const market of this.velocityClient.getSpotMarketAccounts()) {
			const position = chUserToCheck.getSpotPosition(market.marketIndex);
			if (!position) {
				continue;
			}
			if (!isVariant(position.balanceType, 'borrow')) {
				// not possible to resolve non-borrow markets
				continue;
			}
			if (position.scaledBalance.lte(ZERO)) {
				// invalid borrow
				continue;
			}
			bankruptMarketIndices.push(position.marketIndex);
		}

		return bankruptMarketIndices;
	}

	private async tryResolveBankruptUser(user: User) {
		const userAcc = user.getUserAccountOrThrow();
		const userKey = user.getUserAccountPublicKey();

		// find out whether the user is perp-bankrupt or spot-bankrupt
		const bankruptPerpMarkets = this.findPerpBankruptingMarkets(user);
		const bankruptSpotMarkets = this.findSpotBankruptingMarkets(user);

		logger.info(
			`User ${userKey.toBase58()} is bankrupt in perpMarkets: ${bankruptPerpMarkets} and spotMarkets: ${bankruptSpotMarkets} `
		);

		// resolve bankrupt markets
		for (const perpIdx of bankruptPerpMarkets) {
			logger.info(
				`Resolving perp market for userAcc: ${userKey.toBase58()}, marketIndex: ${perpIdx} `
			);
			if (this.liquidatorConfig.notifyOnLiquidation) {
				webhookMessage(
					`[${
						this.name
					}]: Resolving perp market for userAcc: ${userKey.toBase58()}, marketIndex: ${perpIdx} `
				);
			}
			const ix = await this.velocityClient.getResolvePerpBankruptcyIx(
				userKey,
				userAcc,
				perpIdx
			);
			const simResult = await this.buildVersionedTransactionWithSimulatedCus(
				[ix],
				this.velocityLookupTables!,
				Math.floor(this.priorityFeeSubscriber.getCustomStrategyResult())
			);
			if (simResult.simError !== null) {
				logger.error(
					`Error in resolveBankruptcy for userAccount ${userKey.toBase58()} on market ${perpIdx}, simError: ${JSON.stringify(
						simResult.simError
					)}`
				);

				const errorCode = handleSimResultError(
					simResult,
					errorCodesToSuppress,
					`${this.name}: resolveBankruptcy`
				);
				if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
					const msg = `[${
						this.name
					}]: :x: error in resolveBankruptcy for userAccount ${userKey.toBase58()}: \n${
						simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''
					}`;
					logger.error(msg);
					webhookMessage(msg);
				}
			} else {
				const resp =
					await this.velocityClient.txSender.sendVersionedTransaction(
						simResult.tx,
						undefined,
						this.velocityClient.opts
					);
				logger.info(
					`Sent resolveBankruptcy tx for ${userKey.toBase58()} in perp market ${perpIdx} tx: ${
						resp.txSig
					} `
				);
			}
		}

		for (const spotIdx of bankruptSpotMarkets) {
			logger.info(
				`Resolving spot market for userAcc: ${userKey.toBase58()}, marketIndex: ${spotIdx} `
			);
			if (this.liquidatorConfig.notifyOnLiquidation) {
				webhookMessage(
					`[${
						this.name
					}]: Resolving spot market for userAcc: ${userKey.toBase58()}, marketIndex: ${spotIdx} `
				);
			}

			const ix = await this.velocityClient.getResolveSpotBankruptcyIx(
				userKey,
				userAcc,
				spotIdx
			);
			const simResult = await this.buildVersionedTransactionWithSimulatedCus(
				[ix],
				this.velocityLookupTables!,
				Math.floor(this.priorityFeeSubscriber.getCustomStrategyResult())
			);
			if (simResult.simError !== null) {
				logger.error(
					`Error in resolveBankruptcy for userAccount ${userKey.toBase58()} in spot market ${spotIdx}, simError: ${JSON.stringify(
						simResult.simError
					)}`
				);

				const errorCode = handleSimResultError(
					simResult,
					errorCodesToSuppress,
					`${this.name}: resolveBankruptcy`
				);
				if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
					const msg = `[${
						this.name
					}]: :x: error in resolveBankruptcy for userAccount ${userKey.toBase58()}: \n${
						simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''
					}`;
					logger.error(msg);
					webhookMessage(msg);
				}
			} else {
				const resp =
					await this.velocityClient.txSender.sendVersionedTransaction(
						simResult.tx,
						undefined,
						this.velocityClient.opts
					);
				logger.info(
					`Sent resolveBankruptcy tx for ${userKey.toBase58()} in spot market ${spotIdx} tx: ${
						resp.txSig
					} `
				);
			}
		}
	}

	private hasCollateralToLiquidate(subAccountId: number): boolean {
		const currUser = this.velocityClient.getUser(subAccountId);
		const freeCollateral = currUser.getFreeCollateral('Initial');
		const subAccountValue = new BN(
			calculateAccountValueUsd(currUser) * QUOTE_PRECISION.toNumber()
		);
		const minFreeCollateral = subAccountValue
			.mul(this.maxPositionTakeoverPctOfCollateralNum)
			.div(this.maxPositionTakeoverPctOfCollateralDenom);
		return freeCollateral.gte(minFreeCollateral);
	}

	private findBestSpotPosition(
		liquidateeUser: User,
		spotPositions: SpotPosition[],
		isBorrow: boolean,
		positionTakerOverPctNumerator: BN,
		positionTakerOverPctDenominator: BN
	): {
		bestIndex: number;
		bestAmount: BN;
		indexWithMaxAssets: number;
		indexWithOpenOrders: number;
	} {
		let bestIndex = -1;
		let bestAmount = ZERO;
		let currentAstWeight = 0;
		let currentLibWeight = Number.MAX_VALUE;
		let indexWithMaxAssets = -1;
		let maxAssets = new BN(-1);
		let indexWithOpenOrders = -1;

		for (const position of spotPositions) {
			const market = this.velocityClient.getSpotMarketAccount(
				position.marketIndex
			);
			if (!market) {
				throw new Error('No spot market found, velocity client misconfigured');
				continue;
			}

			if (position.scaledBalance.eq(ZERO)) {
				continue;
			}

			if (position.openOrders > 0) {
				indexWithOpenOrders = position.marketIndex;
			}

			const totalAssetValue = liquidateeUser.getSpotMarketAssetValue(
				position.marketIndex,
				'Maintenance',
				true,
				undefined
			);
			if (totalAssetValue.abs().gt(maxAssets)) {
				maxAssets = totalAssetValue.abs();
				indexWithMaxAssets = position.marketIndex;
			}

			if (
				(isBorrow && isVariant(position.balanceType, 'deposit')) ||
				(!isBorrow && isVariant(position.balanceType, 'borrow'))
			) {
				continue;
			}

			const spotMarket = this.velocityClient.getSpotMarketAccount(
				position.marketIndex
			);
			if (!spotMarket) {
				logger.error(`No spot market found for ${position.marketIndex}`);
				continue;
			}

			const liquidatorUser = this.getLiquidatorUserForSpotMarket(
				position.marketIndex
			);
			if (!liquidatorUser) {
				continue;
			}

			const tokenAmount = calculateSpotTokenAmountToLiquidate(
				this.velocityClient,
				liquidatorUser,
				position,
				positionTakerOverPctNumerator,
				positionTakerOverPctDenominator
			);

			if (isBorrow) {
				if (spotMarket.maintenanceLiabilityWeight < currentLibWeight) {
					bestAmount = tokenAmount;
					bestIndex = position.marketIndex;
					currentAstWeight = spotMarket.maintenanceAssetWeight;
					currentLibWeight = spotMarket.maintenanceLiabilityWeight;
				}
			} else {
				if (spotMarket.maintenanceAssetWeight > currentAstWeight) {
					bestAmount = tokenAmount;
					bestIndex = position.marketIndex;
					currentAstWeight = spotMarket.maintenanceAssetWeight;
					currentLibWeight = spotMarket.maintenanceLiabilityWeight;
				}
			}
		}

		return {
			bestIndex,
			bestAmount,
			indexWithMaxAssets: indexWithMaxAssets,
			indexWithOpenOrders,
		};
	}

	private async liqSpot(
		user: User,
		depositMarketIndexToLiq: number,
		borrowMarketIndexToLiq: number,
		subAccountToLiqSpot: number,
		amountToLiqBN: BN
	): Promise<boolean> {
		let sentTx = false;
		const ix = await this.velocityClient.getLiquidateSpotIx(
			user.userAccountPublicKey,
			user.getUserAccountOrThrow(),
			depositMarketIndexToLiq,
			borrowMarketIndexToLiq,
			amountToLiqBN,
			undefined,
			subAccountToLiqSpot
		);
		const simResult = await this.buildVersionedTransactionWithSimulatedCus(
			[ix],
			this.velocityLookupTables!,
			Math.floor(this.priorityFeeSubscriber.getCustomStrategyResult())
		);
		if (simResult.simError !== null) {
			logger.error(
				`Error in liquidateSpot for userAccount ${user.userAccountPublicKey.toBase58()} in spot market ${borrowMarketIndexToLiq}, simError: ${JSON.stringify(
					simResult.simError
				)}`
			);

			const errorCode = handleSimResultError(
				simResult,
				errorCodesToSuppress,
				`${this.name}: liquidateSpot`
			);
			if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
				const msg = `[${
					this.name
				}]: :x: error in liquidateSpot for userAccount ${user.userAccountPublicKey.toBase58()}: \n${
					simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''
				}`;
				logger.error(msg);
				webhookMessage(msg);
			}
		} else {
			const resp = await this.velocityClient.txSender.sendVersionedTransaction(
				simResult.tx,
				undefined,
				this.velocityClient.opts
			);
			sentTx = true;
			logger.info(
				`Sent liquidateSpot tx for ${user.userAccountPublicKey.toBase58()} in spot market ${borrowMarketIndexToLiq} tx: ${
					resp.txSig
				} `
			);

			if (this.liquidatorConfig.notifyOnLiquidation) {
				webhookMessage(
					`[${
						this.name
					}]: liquidateBorrow for userAccount ${user.userAccountPublicKey.toBase58()} in spot market ${borrowMarketIndexToLiq} tx: ${
						resp.txSig
					} `
				);
			}
		}

		return sentTx;
	}

	private async liqBorrow(
		depositMarketIndexToLiq: number,
		borrowMarketIndexToLiq: number,
		borrowAmountToLiq: BN,
		user: User
	): Promise<boolean> {
		const borrowMarket = this.velocityClient.getSpotMarketAccount(
			borrowMarketIndexToLiq
		)!;
		const spotPrecision = TEN.pow(new BN(borrowMarket.decimals));
		logger.info(
			`liqBorrow: ${user.userAccountPublicKey.toBase58()} amountToLiq: ${convertToNumber(
				borrowAmountToLiq,
				spotPrecision
			)} on market ${borrowMarketIndexToLiq}`
		);

		const subAccountToLiqSpot = this.getSubAccountIdToLiquidateSpot(
			borrowMarketIndexToLiq
		);
		if (subAccountToLiqSpot === undefined) {
			return false;
		}

		const currUser = this.velocityClient.getUser(subAccountToLiqSpot);
		const oracle = this.velocityClient.getOracleDataForSpotMarket(
			borrowMarketIndexToLiq
		);
		const borrowValue = getTokenValue(
			borrowAmountToLiq,
			borrowMarket.decimals,
			oracle
		);
		const subAccountValue = calculateAccountValueUsd(currUser);
		const freeCollateral = currUser.getFreeCollateral('Initial');
		const collateralAvailable = freeCollateral
			.mul(this.maxPositionTakeoverPctOfCollateralNum)
			.div(this.maxPositionTakeoverPctOfCollateralDenom);

		let amountToLiqBN = borrowAmountToLiq.muln(2); // double to make sure it clears out extra interest
		if (borrowValue.gt(collateralAvailable)) {
			amountToLiqBN = spotPrecision.mul(collateralAvailable).div(oracle.price);
		}
		logger.info(
			`Subaccount ${subAccountToLiqSpot}: BorrowValue: ${borrowValue}, accountValue: ${subAccountValue}, collateralAvailable: ${convertToNumber(
				collateralAvailable,
				QUOTE_PRECISION
			)}, amountToLiq: ${convertToNumber(amountToLiqBN, spotPrecision)}`
		);

		return await this.liqSpot(
			user,
			depositMarketIndexToLiq,
			borrowMarketIndexToLiq,
			subAccountToLiqSpot,
			amountToLiqBN
		);
	}

	private async liqPerpPnl(
		user: User,
		perpMarketAccount: PerpMarketAccount,
		usdcAccount: SpotMarketAccount,
		liquidateePosition: PerpPosition,
		depositMarketIndextoLiq: number,
		depositAmountToLiq: BN,
		borrowMarketIndextoLiq: number,
		borrowAmountToLiq: BN
	): Promise<boolean> {
		logger.info(
			`liqPerpPnl: ${user.userAccountPublicKey.toBase58()} depositAmountToLiq: ${depositAmountToLiq.toString()} (depMktIndex: ${depositMarketIndextoLiq}). borrowAmountToLiq: ${borrowAmountToLiq.toString()} (brwMktIndex: ${borrowMarketIndextoLiq}). liqeePositionQuoteAmount: ${liquidateePosition.quoteAssetAmount.toNumber()} (perpMktIdx: ${
				perpMarketAccount.marketIndex
			})`
		);
		let sentTx = false;

		if (liquidateePosition.quoteAssetAmount.gt(ZERO)) {
			const claimablePnl = calculateClaimablePnl(
				perpMarketAccount,
				usdcAccount,
				liquidateePosition,
				this.velocityClient.getOracleDataForPerpMarket(
					liquidateePosition.marketIndex
				)
			);

			if (claimablePnl.gt(ZERO) && borrowMarketIndextoLiq === -1) {
				const ix = await this.velocityClient.getSettlePNLsIxs(
					[
						{
							settleeUserAccountPublicKey: user.userAccountPublicKey,
							settleeUserAccount: user.getUserAccountOrThrow(),
						},
					],
					[liquidateePosition.marketIndex]
				);
				const simResult = await this.buildVersionedTransactionWithSimulatedCus(
					ix,
					this.velocityLookupTables!,
					Math.floor(this.priorityFeeSubscriber.getCustomStrategyResult())
				);
				if (simResult.simError !== null) {
					logger.error(
						`Error in settlePnl for userAccount ${user.userAccountPublicKey.toBase58()} in perp market ${
							liquidateePosition.marketIndex
						}, simError: ${JSON.stringify(simResult.simError)}`
					);

					const errorCode = handleSimResultError(
						simResult,
						errorCodesToSuppress,
						`${this.name}: settlePnl`
					);
					if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
						const msg = `[${
							this.name
						}]: :x: error in settlePnl for userAccount ${user.userAccountPublicKey.toBase58()}: \n${
							simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''
						}`;
						logger.error(msg);
						webhookMessage(msg);
					}
				} else {
					const resp =
						await this.velocityClient.txSender.sendVersionedTransaction(
							simResult.tx,
							undefined,
							this.velocityClient.opts
						);
					sentTx = true;
					logger.info(
						`Sent settlePnl tx for ${user.userAccountPublicKey.toBase58()} in perp market ${
							liquidateePosition.marketIndex
						} tx: ${resp.txSig} `
					);
				}

				return sentTx;
			}

			let frac = new BN(100000000);
			if (claimablePnl.gt(ZERO)) {
				frac = BN.max(
					liquidateePosition.quoteAssetAmount.div(claimablePnl),
					new BN(1)
				);
			}

			if (frac.lt(new BN(100000000))) {
				const subAccountToLiqBorrow = this.getSubAccountIdToLiquidateSpot(
					borrowMarketIndextoLiq
				);
				if (subAccountToLiqBorrow === undefined) {
					return sentTx;
				}

				const ix = await this.velocityClient.getLiquidateBorrowForPerpPnlIx(
					user.userAccountPublicKey,
					user.getUserAccountOrThrow(),
					liquidateePosition.marketIndex,
					borrowMarketIndextoLiq,
					borrowAmountToLiq.div(frac),
					undefined,
					subAccountToLiqBorrow
				);
				const simResult = await this.buildVersionedTransactionWithSimulatedCus(
					[ix],
					this.velocityLookupTables!,
					Math.floor(this.priorityFeeSubscriber.getCustomStrategyResult())
				);
				if (simResult.simError !== null) {
					logger.error(
						`Error in liquidateBorrowForPerpPnl for userAccount ${user.userAccountPublicKey.toBase58()} in spot market ${borrowMarketIndextoLiq}, simError: ${JSON.stringify(
							simResult.simError
						)}`
					);

					const errorCode = handleSimResultError(
						simResult,
						errorCodesToSuppress,
						`${this.name}: liquidateBorrowForPerpPnl`
					);
					if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
						const msg = `[${
							this.name
						}]: :x: error in liquidateBorrowForPerpPnl for userAccount ${user.userAccountPublicKey.toBase58()}: \n${
							simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''
						}`;
						logger.error(msg);
						webhookMessage(msg);
					}
				} else {
					const resp =
						await this.velocityClient.txSender.sendVersionedTransaction(
							simResult.tx,
							undefined,
							this.velocityClient.opts
						);
					sentTx = true;
					logger.info(
						`Sent liquidateBorrowForPerpPnl tx for ${user.userAccountPublicKey.toBase58()} in spot market ${borrowMarketIndextoLiq} tx: ${
							resp.txSig
						} `
					);

					if (this.liquidatorConfig.notifyOnLiquidation) {
						webhookMessage(
							`[${
								this.name
							}]: liquidateBorrowForPerpPnl for userAccount ${user.userAccountPublicKey.toBase58()} in spot market ${borrowMarketIndextoLiq} tx: ${
								resp.txSig
							} `
						);
					}
				}
			} else {
				logger.info(
					`claimablePnl = ${claimablePnl.toString()} << liquidateePosition.quoteAssetAmount=${liquidateePosition.quoteAssetAmount.toString()} `
				);
				logger.info(`skipping liquidateBorrowForPerpPnl`);
			}
		} else {
			const start = Date.now();

			const { perpTier: safestPerpTier, spotTier: safestSpotTier } =
				user.getSafestTiers();
			const perpTier = getPerpMarketTierNumber(perpMarketAccount);
			if (!perpTierIsAsSafeAs(perpTier, safestPerpTier, safestSpotTier)) {
				return sentTx;
			}

			if (!this.spotMarketIndicies.includes(depositMarketIndextoLiq)) {
				logger.info(
					`skipping liquidatePerpPnlForDeposit of ${user.userAccountPublicKey.toBase58()} on spot market ${depositMarketIndextoLiq} because it is not in spotMarketIndices`
				);
				return sentTx;
			}

			const subAccountToTakeOverPerpPnl = this.getSubAccountIdToLiquidatePerp(
				liquidateePosition.marketIndex
			);
			if (subAccountToTakeOverPerpPnl === undefined) {
				return sentTx;
			}

			const pnlToLiq = this.velocityClient
				.getUser(subAccountToTakeOverPerpPnl)
				.getFreeCollateral('Initial');
			try {
				const ix = await this.velocityClient.getLiquidatePerpPnlForDepositIx(
					user.userAccountPublicKey,
					user.getUserAccountOrThrow(),
					liquidateePosition.marketIndex,
					depositMarketIndextoLiq,
					pnlToLiq,
					undefined,
					subAccountToTakeOverPerpPnl
				);
				const simResult = await this.buildVersionedTransactionWithSimulatedCus(
					[ix],
					this.velocityLookupTables!,
					Math.floor(this.priorityFeeSubscriber.getCustomStrategyResult())
				);

				if (simResult.simError !== null) {
					logger.error(
						`Error in liquidatePerpPnlForDeposit for userAccount ${user.userAccountPublicKey.toBase58()} on market ${
							liquidateePosition.marketIndex
						}: simError: ${JSON.stringify(simResult.simError)}`
					);
					const errorCode = handleSimResultError(
						simResult,
						errorCodesToSuppress,
						`${this.name}: liquidatePerpPnlForDeposit`
					);
					if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
						const msg = `[${
							this.name
						}]: :x: error in liquidatePerpPnlForDeposit for userAccount ${user.userAccountPublicKey.toBase58()} on market ${
							liquidateePosition.marketIndex
						}: \n${simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''}`;
						logger.error(msg);
						webhookMessage(msg);
					} else {
						this.throttledUsers.set(
							user.userAccountPublicKey.toBase58(),
							Date.now()
						);
					}
				} else {
					const resp =
						await this.velocityClient.txSender.sendVersionedTransaction(
							simResult.tx,
							undefined,
							this.velocityClient.opts
						);
					sentTx = true;
					logger.info(
						`Sent liquidatePerpPnlForDeposit tx for ${user.userAccountPublicKey.toBase58()} on market ${
							liquidateePosition.marketIndex
						} tx: ${resp.txSig} `
					);
					if (this.liquidatorConfig.notifyOnLiquidation) {
						webhookMessage(
							`[${
								this.name
							}]: liquidatePerpPnlForDeposit for ${user.userAccountPublicKey.toBase58()} on market ${
								liquidateePosition.marketIndex
							} tx: ${resp.txSig} `
						);
					}
				}
			} catch (err) {
				logger.error(
					`Error in liquidatePerpPnlForDeposit for userAccount ${user.userAccountPublicKey.toBase58()} on market ${
						liquidateePosition.marketIndex
					}`
				);
				console.error(err);
			} finally {
				this.recordHistogram(start, 'liquidatePerpPnlForDeposit');
			}
		}

		return sentTx;
	}

	private isThrottled(userKey: string, auth: string): boolean {
		const lastAttempt = this.throttledUsers.get(userKey);
		if (!lastAttempt) {
			return false;
		}
		const now = Date.now();
		if (lastAttempt + LIQUIDATE_THROTTLE_BACKOFF > now) {
			logger.warn(
				`skipping user(throttled, retry in ${
					lastAttempt + LIQUIDATE_THROTTLE_BACKOFF - now
				}ms) ${auth}: ${userKey} `
			);
			return true;
		}
		this.throttledUsers.delete(userKey);
		return false;
	}

	/**
	 * Clears the `beingLiquidated` flag on a user who is no longer liquidatable.
	 *
	 * `liquidate_perp` with a zero base amount is a crank rather than a real
	 * liquidation. When the account already clears the maintenance-plus-buffer
	 * band, the program exits liquidation before it looks at the position. That
	 * early exit is why a market the user holds nothing in still works, and it is
	 * the only thing that clears an account whose positions are all gone.
	 *
	 * Past the early exit the market matters.
	 *
	 * - The program derives its liquidation mode from the position in the target
	 *   market. An isolated position's own flag is cleared only by targeting that
	 *   market. The account-level flag is cleared only by targeting a cross
	 *   position, or a market with no position, which falls back to cross mode.
	 * - The program then requires a base position or an open order, and otherwise
	 *   returns `PositionDoesntHaveOpenPositionOrOrders`. It rejects a zero base
	 *   amount once it reaches the transfer with a base position still open, and
	 *   returns `InvalidBaseAssetAmountForLiquidatePerp`.
	 * - A slot holding only unsettled pnl satisfies neither requirement. It has
	 *   to go through `liquidate_perp_pnl_for_deposit` or `settle_pnl` instead.
	 */
	private async clearBeingLiquidatedStatus(user: User): Promise<{
		liquidatePerp: number;
		liquidatePerpPnlForDeposit: number;
	}> {
		const sent = { liquidatePerp: 0, liquidatePerpPnlForDeposit: 0 };
		const userKey = user.userAccountPublicKey.toBase58();

		if (this.dryRun) {
			logger.warn(
				`[${this.name}]: --dry run flag enabled - not clearing beingLiquidated status for ${userKey}`
			);
			return sent;
		}

		const positions = user.getActivePerpPositions();
		const candidates = selectLiquidationExitCandidates(
			positions.map((position) => ({
				marketIndex: position.marketIndex,
				hasBase: !position.baseAssetAmount.isZero(),
				// This mirrors the program's has_open_order(), which also counts
				// the bid and ask exposure an order leaves behind.
				hasOpenOrder:
					position.openOrders > 0 ||
					!position.openBids.isZero() ||
					!position.openAsks.isZero(),
				hasPnl: !position.quoteAssetAmount.isZero(),
				isolated: user.isPerpPositionIsolated(position),
				flagged: user.isIsolatedPositionBeingLiquidated(position.marketIndex),
			})),
			user.isCrossMarginBeingLiquidated(),
			this.perpMarketIndicies.filter((marketIndex) =>
				this.canLiquidatePerpMarket(marketIndex)
			)
		);

		if (candidates.length === 0) {
			logger.warn(
				`[${this.name}]: no actionable perp market to clear beingLiquidated status for ${userKey}`
			);
			return sent;
		}

		// Walk the candidates rather than take the first one. The first candidate
		// may sit in a market with no configured subaccount, or be too small to
		// size, and stopping there would stall on the same slot every tick.
		for (const candidate of candidates) {
			const issued = await this.issueLiquidationExitCandidate(
				user,
				positions,
				candidate
			);
			if (issued === 'liquidatePerp') {
				sent.liquidatePerp++;
				return sent;
			}
			if (issued === 'liquidatePerpPnlForDeposit') {
				sent.liquidatePerpPnlForDeposit++;
				return sent;
			}
		}

		logger.warn(
			`[${
				this.name
			}]: could not clear beingLiquidated status for ${userKey}, tried ${candidates
				.map((c) => `${c.kind}:${c.marketIndex}`)
				.join(', ')}`
		);
		return sent;
	}

	/** A perp market this bot may send a liquidation against right now. */
	private canLiquidatePerpMarket(marketIndex: number): boolean {
		if (PERP_MARKETS_NEVER_LIQUIDATED.includes(marketIndex)) {
			return false;
		}
		const perpMarket = this.velocityClient.getPerpMarketAccount(marketIndex);
		if (!perpMarket) {
			return false;
		}
		// TODO: use enum on new sdk release
		return !isOperationPaused(perpMarket.pausedOperations, 32 as PerpOperation);
	}

	/**
	 * Sends the instruction a candidate calls for. Returns which counter to bump,
	 * or undefined when nothing could be sent so the caller tries the next one.
	 */
	private async issueLiquidationExitCandidate(
		user: User,
		positions: PerpPosition[],
		candidate: LiquidationExitCandidate
	): Promise<'liquidatePerp' | 'liquidatePerpPnlForDeposit' | undefined> {
		const userKey = user.userAccountPublicKey.toBase58();

		if (candidate.kind === 'crank') {
			// A zero base amount transfers no liability, so this does not go
			// through getSubAccountIdToLiquidatePerp. Gating the crank on a
			// subaccount that holds collateral would block the one call that
			// clears a recovered account for free.
			return (await this.liqPerp(
				user,
				candidate.marketIndex,
				this.defaultSubaccountId,
				ZERO
			))
				? 'liquidatePerp'
				: undefined;
		}

		const position = positions.find(
			(p) => p.marketIndex === candidate.marketIndex
		);
		if (!position) {
			return undefined;
		}

		if (candidate.kind === 'base') {
			const subAccountToLiqPerp = this.getSubAccountIdToLiquidatePerp(
				candidate.marketIndex
			);
			if (subAccountToLiqPerp === undefined) {
				return undefined;
			}
			const baseAmountToLiquidate =
				this.calculateBaseAmountToLiquidate(position);
			if (baseAmountToLiquidate.lte(ZERO)) {
				logger.warn(
					`[${this.name}]: cannot size a liquidation of stuck user ${userKey} on perp market ${candidate.marketIndex} above the step size`
				);
				return undefined;
			}
			return (await this.liqPerp(
				user,
				candidate.marketIndex,
				subAccountToLiqPerp,
				baseAmountToLiquidate
			))
				? 'liquidatePerp'
				: undefined;
		}

		const perpMarket = this.velocityClient.getPerpMarketAccount(
			candidate.marketIndex
		);
		const usdcMarket = this.velocityClient.getSpotMarketAccount(
			QUOTE_SPOT_MARKET_INDEX
		);
		if (!perpMarket || !usdcMarket) {
			logger.error(
				`[${this.name}]: perp market ${candidate.marketIndex} or quote spot market not loaded, cannot settle pnl for stuck user ${userKey}`
			);
			return undefined;
		}

		const spotPositions = user.getUserAccountOrThrow().spotPositions;
		const {
			bestIndex: depositMarketIndextoLiq,
			bestAmount: depositAmountToLiq,
		} = this.findBestSpotPosition(
			user,
			spotPositions,
			false,
			this.maxPositionTakeoverPctOfCollateralNum,
			this.maxPositionTakeoverPctOfCollateralDenom
		);
		const { bestIndex: borrowMarketIndextoLiq, bestAmount: borrowAmountToLiq } =
			this.findBestSpotPosition(
				user,
				spotPositions,
				true,
				this.maxPositionTakeoverPctOfCollateralNum,
				this.maxPositionTakeoverPctOfCollateralDenom
			);

		// liquidate_perp_pnl_for_deposit needs a deposit to seize. Without one the
		// call has nothing to take, and liqPerpPnl logs a spot market of -1.
		if (position.quoteAssetAmount.lt(ZERO) && depositMarketIndextoLiq === -1) {
			return undefined;
		}

		return (await this.liqPerpPnl(
			user,
			perpMarket,
			usdcMarket,
			position,
			depositMarketIndextoLiq,
			depositAmountToLiq,
			borrowMarketIndextoLiq,
			borrowAmountToLiq
		))
			? 'liquidatePerpPnlForDeposit'
			: undefined;
	}

	private async liqPerp(
		user: User,
		perpMarketIndex: number,
		subAccountToLiqPerp: number,
		baseAmountToLiquidate: BN
	): Promise<boolean> {
		if (PERP_MARKETS_NEVER_LIQUIDATED.includes(perpMarketIndex)) {
			logger.warn(
				`[${this.name}]: perp market ${perpMarketIndex} is never liquidated, skipping`
			);
			return false;
		}

		let txSent = false;
		const ix = await this.velocityClient.getLiquidatePerpIx(
			user.userAccountPublicKey,
			user.getUserAccountOrThrow(),
			perpMarketIndex,
			baseAmountToLiquidate,
			undefined,
			subAccountToLiqPerp
		);

		// The liquidation cancels the liquidatee's CLOB orders itself. The SDK
		// builder carries the books of the markets that hold them.
		const simResult = await this.buildVersionedTransactionWithSimulatedCus(
			[ix],
			this.velocityLookupTables!,
			Math.floor(this.priorityFeeSubscriber.getCustomStrategyResult())
		);
		if (simResult.simError !== null) {
			logger.error(
				`Error in liquidatePerp for userAccount ${user.userAccountPublicKey.toBase58()} on market ${perpMarketIndex}, simError: ${JSON.stringify(
					simResult.simError
				)}`
			);
			const errorCode = handleSimResultError(
				simResult,
				errorCodesToSuppress,
				`${this.name}: liquidatePerp`
			);
			if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
				const msg = `[${
					this.name
				}]: :x: error in liquidatePerp for userAccount ${user.userAccountPublicKey.toBase58()} on market ${perpMarketIndex}: \n${
					simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''
				}`;
				logger.error(msg);
				webhookMessage(msg);
			} else {
				this.throttledUsers.set(
					user.userAccountPublicKey.toBase58(),
					Date.now()
				);
			}
		} else {
			const resp = await this.velocityClient.txSender.sendVersionedTransaction(
				simResult.tx,
				undefined,
				this.velocityClient.opts
			);
			txSent = true;
			logger.info(
				`Sent liquidatePerp tx for ${user.userAccountPublicKey.toBase58()} on market ${perpMarketIndex} tx: ${
					resp.txSig
				} `
			);
			if (this.liquidatorConfig.notifyOnLiquidation) {
				webhookMessage(
					`[${
						this.name
					}]: liquidatePerp for ${user.userAccountPublicKey.toBase58()} on market ${perpMarketIndex} tx: ${
						resp.txSig
					} `
				);
			}
		}
		return txSent;
	}

	private findSortedUsers(): {
		usersCanBeLiquidated: Array<{
			user: User;
			userKey: string;
			marginRequirement: BN;
			canBeLiquidated: boolean;
			bankrupt: boolean;
		}>;
		checkedUsers: number;
		liquidatableUsers: number;
	} {
		const usersCanBeLiquidated = new Array<{
			user: User;
			userKey: string;
			marginRequirement: BN;
			canBeLiquidated: boolean;
			bankrupt: boolean;
		}>();

		let checkedUsers = 0;
		let liquidatableUsers = 0;
		for (const user of this.userMap!.values()) {
			checkedUsers++;
			const { canBeLiquidated, marginRequirement } = user.canBeLiquidated();
			// `canBeLiquidated` is the cross-margin flag and `isBeingLiquidated()` only reads
			// the on-chain BeingLiquidated/Bankruptcy status flags. An isolated position that is
			// economically bankrupt but not yet flagged on-chain (with a healthy cross account)
			// would otherwise be filtered out here and never reach the `hasIsolatedMarginBankrupt`
			// resolve gate in tryLiquidate. Include it in candidate selection so it does.
			const isolatedBankrupt = hasIsolatedMarginBankrupt(user);
			if (canBeLiquidated || user.isBeingLiquidated() || isolatedBankrupt) {
				liquidatableUsers++;
				const userKey = user.userAccountPublicKey.toBase58();
				if (this.excludedAccounts.has(userKey)) {
					// Debug log precisely because the intent is to avoid noise
					logger.debug(
						`Skipping liquidation for ${userKey} due to configuration`
					);
				} else {
					usersCanBeLiquidated.push({
						user,
						userKey,
						marginRequirement,
						canBeLiquidated,
						bankrupt:
							isUserBankrupt(user) || user.isBankrupt() || isolatedBankrupt,
					});
				}
			}
		}

		// bankrupt users first (an isolated-only bankruptcy can have a cross-margin
		// requirement of zero and would otherwise sort last), then by cross-margin
		// requirement, largest to smallest
		usersCanBeLiquidated.sort((a, b) => {
			if (a.bankrupt !== b.bankrupt) {
				return a.bankrupt ? -1 : 1;
			}
			return b.marginRequirement.gt(a.marginRequirement) ? 1 : -1;
		});

		return {
			usersCanBeLiquidated,
			checkedUsers,
			liquidatableUsers,
		};
	}

	private async tryLiquidate(): Promise<{
		checkedUsers: number;
		liquidatableUsers: number;
		ran: boolean;
		tickSummary: {
			skipped: {
				untrackedPerpMarket: number;
				untrackedSpotMarket: number;
				throttledUser: number;
			};
			txsSent: {
				liquidatePerpPnlForDeposit: number;
				liquidatePerp: number;
				liquidateSpot: number;
			};
		};
	}> {
		const { usersCanBeLiquidated, checkedUsers, liquidatableUsers } =
			this.findSortedUsers();

		let untrackedPerpMarket = 0;
		let untrackedSpotMarket = 0;
		let throttledUser = 0;
		let liquidatePerpPnlForDepositSent = 0;
		let liquidatePerpSent = 0;
		let liquidateSpotSent = 0;

		for (const {
			user,
			userKey,
			marginRequirement: _marginRequirement,
			canBeLiquidated,
			bankrupt,
		} of usersCanBeLiquidated) {
			const userAcc = user.getUserAccountOrThrow();
			const auth = userAcc.authority.toBase58();

			if (bankrupt) {
				await this.tryResolveBankruptUser(user);
			} else if (canBeLiquidated) {
				if (this.isThrottled(userKey, auth)) {
					throttledUser++;
					continue;
				}

				const liquidateeUserAccount = user.getUserAccountOrThrow();

				// most attractive spot market liq
				const {
					bestIndex: depositMarketIndextoLiq,
					bestAmount: depositAmountToLiq,
					indexWithMaxAssets,
					indexWithOpenOrders,
				} = this.findBestSpotPosition(
					user,
					liquidateeUserAccount.spotPositions,
					false,
					this.maxPositionTakeoverPctOfCollateralNum,
					this.maxPositionTakeoverPctOfCollateralDenom
				);

				const {
					bestIndex: borrowMarketIndextoLiq,
					bestAmount: borrowAmountToLiq,
				} = this.findBestSpotPosition(
					user,
					liquidateeUserAccount.spotPositions,
					true,
					this.maxPositionTakeoverPctOfCollateralNum,
					this.maxPositionTakeoverPctOfCollateralDenom
				);

				let liquidateeHasSpotPos = false;
				if (borrowMarketIndextoLiq != -1 && depositMarketIndextoLiq != -1) {
					liquidateeHasSpotPos = true;
					const sent = await this.liqBorrow(
						depositMarketIndextoLiq,
						borrowMarketIndextoLiq,
						borrowAmountToLiq,
						user
					);
					if (sent) {
						liquidateSpotSent++;
					}
				}

				const usdcMarket = this.velocityClient.getSpotMarketAccount(
					QUOTE_SPOT_MARKET_INDEX
				);
				if (!usdcMarket) {
					throw new Error(
						`USDC spot market not loaded, misconfigured VelocityClient`
					);
				}

				// less attractive, perp / perp pnl liquidations
				let liquidateeHasPerpPos = false;
				let liquidateeHasUnsettledPerpPnl = false;
				let liquidateePerpIndexWithOpenOrders = -1;

				// shuffle user perp positions to get good position coverage in case some
				// positions put the liquidator into a bad state.
				const userPerpPositions = user.getActivePerpPositions();
				userPerpPositions.sort(() => Math.random() - 0.5);

				for (const liquidateePosition of userPerpPositions) {
					if (liquidateePosition.openOrders > 0) {
						liquidateePerpIndexWithOpenOrders = liquidateePosition.marketIndex;
					}

					const perpMarket = this.velocityClient.getPerpMarketAccount(
						liquidateePosition.marketIndex
					);
					if (!perpMarket) {
						throw new Error(
							`perpMarket not loaded for marketIndex ${liquidateePosition.marketIndex}, misconfigured VelocityClient`
						);
					}

					// TODO: use enum on new sdk release
					if (
						isOperationPaused(perpMarket.pausedOperations, 32 as PerpOperation)
					) {
						logger.info(
							`Skipping liquidation for ${userKey} on market ${perpMarket.marketIndex}, liquidation paused`
						);
						continue;
					}

					liquidateeHasUnsettledPerpPnl =
						liquidateePosition.baseAssetAmount.isZero() &&
						!liquidateePosition.quoteAssetAmount.isZero();
					liquidateeHasPerpPos =
						!liquidateePosition.baseAssetAmount.isZero() ||
						!liquidateePosition.quoteAssetAmount.isZero();

					const tryLiqPerp =
						liquidateeHasUnsettledPerpPnl &&
						// call liquidatePerpPnlForDeposit
						((liquidateePosition.quoteAssetAmount.lt(ZERO) &&
							depositMarketIndextoLiq > -1) ||
							// call liquidateBorrowForPerpPnl or settlePnl
							liquidateePosition.quoteAssetAmount.gt(ZERO));
					if (tryLiqPerp) {
						const sent = await this.liqPerpPnl(
							user,
							perpMarket,
							usdcMarket,
							liquidateePosition,
							depositMarketIndextoLiq,
							depositAmountToLiq,
							borrowMarketIndextoLiq,
							borrowAmountToLiq
						);
						if (sent) {
							liquidatePerpPnlForDepositSent++;
						}
					}

					const baseAmountToLiquidate =
						this.calculateBaseAmountToLiquidate(liquidateePosition);

					const subAccountToLiqPerp = this.getSubAccountIdToLiquidatePerp(
						liquidateePosition.marketIndex
					);
					if (subAccountToLiqPerp === undefined) {
						untrackedPerpMarket++;
						continue;
					}

					if (baseAmountToLiquidate.gt(ZERO)) {
						if (this.dryRun) {
							logger.warn('--dry run flag enabled - not sending liquidate tx');
							continue;
						}

						const sent = await this.liqPerp(
							user,
							liquidateePosition.marketIndex,
							subAccountToLiqPerp,
							baseAmountToLiquidate
						);
						if (sent) {
							liquidatePerpSent++;
						}
					} else if (!liquidateePosition.baseAssetAmount.isZero()) {
						// The size is below the market step size. The program
						// rejects such a size. Throttle the user until the
						// liquidator subaccount holds more collateral.
						logger.warn(
							`[${this.name}]: subaccount ${subAccountToLiqPerp} cannot size a liquidation of ${userKey} on perp market ${liquidateePosition.marketIndex} above the step size`
						);
						this.throttledUsers.set(userKey, Date.now());
					}
				}

				if (!liquidateeHasSpotPos && !liquidateeHasPerpPos) {
					logger.info(
						`${auth}-${user.userAccountPublicKey.toBase58()} can be liquidated but has no positions`
					);
					if (liquidateePerpIndexWithOpenOrders !== -1) {
						logger.info(
							`${auth}-${user.userAccountPublicKey.toBase58()} liquidatePerp with open orders in ${liquidateePerpIndexWithOpenOrders}`
						);
						this.throttledUsers.set(
							user.userAccountPublicKey.toBase58(),
							Date.now()
						);

						const subAccountToLiqPerp = this.getSubAccountIdToLiquidatePerp(
							liquidateePerpIndexWithOpenOrders
						);
						if (subAccountToLiqPerp === undefined) {
							untrackedPerpMarket++;
							continue;
						}

						const sent = await this.liqPerp(
							user,
							liquidateePerpIndexWithOpenOrders,
							subAccountToLiqPerp,
							ZERO
						);
						if (sent) {
							liquidatePerpSent++;
						}
					}
					if (indexWithOpenOrders !== -1) {
						logger.info(
							`${auth}-${user.userAccountPublicKey.toBase58()} liquidateSpot with assets in ${indexWithMaxAssets} and open orders in ${indexWithOpenOrders}`
						);
						this.throttledUsers.set(
							user.userAccountPublicKey.toBase58(),
							Date.now()
						);

						const subAccountToLiqSpot =
							this.getSubAccountIdToLiquidateSpot(indexWithOpenOrders);
						if (subAccountToLiqSpot === undefined) {
							untrackedSpotMarket++;
							continue;
						}
						const sent = await this.liqSpot(
							user,
							indexWithMaxAssets,
							indexWithOpenOrders,
							subAccountToLiqSpot,
							ZERO
						);
						if (sent) {
							liquidateSpotSent++;
						}
					}
				}
			} else if (user.isBeingLiquidated()) {
				if (this.isThrottled(userKey, auth)) {
					throttledUser++;
					continue;
				}

				logger.info(
					`[${
						this.name
					}]: user stuck in beingLiquidated status, need to clear it for ${user.userAccountPublicKey.toBase58()}`
				);

				const sent = await this.clearBeingLiquidatedStatus(user);
				liquidatePerpSent += sent.liquidatePerp;
				liquidatePerpPnlForDepositSent += sent.liquidatePerpPnlForDeposit;
				this.throttledUsers.set(userKey, Date.now());
			}
		}
		return {
			ran: true,
			checkedUsers,
			liquidatableUsers,
			tickSummary: {
				skipped: {
					untrackedPerpMarket,
					untrackedSpotMarket,
					throttledUser,
				},
				txsSent: {
					liquidatePerpPnlForDeposit: liquidatePerpPnlForDepositSent,
					liquidatePerp: liquidatePerpSent,
					liquidateSpot: liquidateSpotSent,
				},
			},
		};
	}

	/**
	 * iterates over users in userMap and checks:
	 * 		1. is user bankrupt? if so, resolve bankruptcy
	 * 		2. is user in liquidation? If so, endangered position is liquidated
	 */
	private async tryLiquidateStart() {
		const start = Date.now();
		/*
		  If there is more than one subAccount, then derisk and liquidate may try to
		  change it, so both need to be mutually exclusive.

		  If not, all we care about is avoiding two liquidation calls at the same time.
		 */
		// gated by bot state; skip if not idle
		if (this.botState === BOT_STATE.DERISKING) {
			logger.info(`${this.name} is DERISKING, skipping liquidation tick.`);
			return;
		}
		let ran = false;
		let checkedUsers = 0;
		let liquidatableUsers = 0;
		let tickSummary: any;
		try {
			this.setState(BOT_STATE.LIQUIDATING);
			({ ran, checkedUsers, liquidatableUsers, tickSummary } =
				await this.tryLiquidate());
		} catch (e) {
			console.error(e);
			if (e instanceof Error) {
				webhookMessage(
					`[${this.name}]: :x: uncaught error: \n${
						e.stack ? e.stack : e.message
					} `
				);
			}
		} finally {
			if (ran) {
				const totalTxsSent =
					tickSummary.txsSent.liquidatePerpPnlForDeposit +
					tickSummary.txsSent.liquidatePerp +
					tickSummary.txsSent.liquidateSpot;
				const totalSkipped =
					tickSummary.skipped.untrackedPerpMarket +
					tickSummary.skipped.untrackedSpotMarket +
					tickSummary.skipped.throttledUser;

				logger.info(
					`[${this.name}] Liquidation tick completed in ${
						Date.now() - start
					}ms:\n` +
						`  Users: ${checkedUsers} checked, ${liquidatableUsers} liquidatable\n` +
						`  Txs Sent: ${totalTxsSent} total (perpPnl: ${tickSummary.txsSent.liquidatePerpPnlForDeposit}, perp: ${tickSummary.txsSent.liquidatePerp}, spot: ${tickSummary.txsSent.liquidateSpot})\n` +
						`  Skipped: ${totalSkipped} total (untracked perp: ${tickSummary.skipped.untrackedPerpMarket}, untracked spot: ${tickSummary.skipped.untrackedSpotMarket}, throttled: ${tickSummary.skipped.throttledUser})`
				);
			}
			this.setState(BOT_STATE.IDLE);
		}
	}

	private recordHistogram(start: number, method: string) {
		if (this.sdkCallDurationHistogram) {
			this.sdkCallDurationHistogram!.record(Date.now() - start, {
				method: method,
			});
		}
	}

	private setState(next: BOT_STATE) {
		if (this.botState !== next) {
			logger.info(`${this.name} state transition: ${this.botState} -> ${next}`);
			this.botState = next;
		}
	}

	private initializeMetrics() {
		if (this.metricsInitialized) {
			logger.error('Tried to initilaize metrics multiple times');
			return;
		}
		this.metricsInitialized = true;

		const { endpoint: defaultEndpoint } = PrometheusExporter.DEFAULT_OPTIONS;
		this.exporter = new PrometheusExporter(
			{
				port: this.metricsPort,
				endpoint: defaultEndpoint,
			},
			() => {
				logger.info(
					`prometheus scrape endpoint started: http://localhost:${this.metricsPort}${defaultEndpoint}`
				);
			}
		);
		const meterName = this.name;
		const meterProvider = new MeterProvider({
			views: [
				new View({
					instrumentName: METRIC_TYPES.sdk_call_duration_histogram,
					instrumentType: InstrumentType.HISTOGRAM,
					meterName: meterName,
					aggregation: new ExplicitBucketHistogramAggregation(
						Array.from(new Array(20), (_, i) => 0 + i * 100),
						true
					),
				}),
			],
		});

		meterProvider.addMetricReader(this.exporter);
		this.meter = meterProvider.getMeter(meterName);

		this.runtimeSpecsGauge = this.meter.createObservableGauge(
			METRIC_TYPES.runtime_specs,
			{
				description: 'Runtime sepcification of this program',
			}
		);
		this.runtimeSpecsGauge.addCallback((obs) => {
			obs.observe(1, this.runtimeSpecs);
		});

		this.totalLeverage = this.meter.createObservableGauge(
			METRIC_TYPES.total_leverage,
			{
				description: 'Total leverage of the account',
			}
		);
		this.totalCollateral = this.meter.createObservableGauge(
			METRIC_TYPES.total_collateral,
			{
				description: 'Total collateral of the account',
			}
		);
		this.freeCollateral = this.meter.createObservableGauge(
			METRIC_TYPES.free_collateral,
			{
				description: 'Free collateral of the account',
			}
		);
		this.perpPositionValue = this.meter.createObservableGauge(
			METRIC_TYPES.perp_posiiton_value,
			{
				description: 'Value of account perp positions',
			}
		);
		this.perpPositionBase = this.meter.createObservableGauge(
			METRIC_TYPES.perp_posiiton_base,
			{
				description: 'Base asset value of account perp positions',
			}
		);
		this.perpPositionQuote = this.meter.createObservableGauge(
			METRIC_TYPES.perp_posiiton_quote,
			{
				description: 'Quote asset value of account perp positions',
			}
		);
		this.initialMarginRequirement = this.meter.createObservableGauge(
			METRIC_TYPES.initial_margin_requirement,
			{
				description: 'The account initial margin requirement',
			}
		);
		this.maintenanceMarginRequirement = this.meter.createObservableGauge(
			METRIC_TYPES.maintenance_margin_requirement,
			{
				description: 'The account maintenance margin requirement',
			}
		);
		this.unrealizedPnL = this.meter.createObservableGauge(
			METRIC_TYPES.unrealized_pnl,
			{
				description: 'The account unrealized PnL',
			}
		);
		this.unrealizedFundingPnL = this.meter.createObservableGauge(
			METRIC_TYPES.unrealized_funding_pnl,
			{
				description: 'The account unrealized funding PnL',
			}
		);

		this.sdkCallDurationHistogram = this.meter.createHistogram(
			METRIC_TYPES.sdk_call_duration_histogram,
			{
				description: 'Distribution of sdk method calls',
				unit: 'ms',
			}
		);

		this.userMapUserAccountKeysGauge = this.meter.createObservableGauge(
			METRIC_TYPES.user_map_user_account_keys,
			{
				description: 'number of user account keys in UserMap',
			}
		);
		this.userMapUserAccountKeysGauge.addCallback(async (obs) => {
			obs.observe(this.userMap!.size());
		});

		this.meter.addBatchObservableCallback(
			async (batchObservableResult: BatchObservableResult) => {
				// each subaccount is responsible for a market
				// record account specific metrics
				for (const [idx, user] of this.velocityClient.getUsers().entries()) {
					const accMarketIdx = idx;
					const userAccount = user.getUserAccountOrThrow();
					const oracle =
						this.velocityClient.getOracleDataForPerpMarket(accMarketIdx);

					batchObservableResult.observe(
						this.totalLeverage!,
						convertToNumber(user.getLeverage(), TEN_THOUSAND),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
					batchObservableResult.observe(
						this.totalCollateral!,
						convertToNumber(user.getTotalCollateral(), QUOTE_PRECISION),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
					batchObservableResult.observe(
						this.freeCollateral!,
						convertToNumber(user.getFreeCollateral(), QUOTE_PRECISION),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
					batchObservableResult.observe(
						this.perpPositionValue!,
						convertToNumber(
							user.getPerpPositionValue(accMarketIdx, oracle),
							QUOTE_PRECISION
						),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);

					const perpPosition = user.getPerpPosition(accMarketIdx);
					batchObservableResult.observe(
						this.perpPositionBase!,
						convertToNumber(
							perpPosition!.baseAssetAmount ?? ZERO,
							BASE_PRECISION
						),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
					batchObservableResult.observe(
						this.perpPositionQuote!,
						convertToNumber(
							perpPosition!.quoteAssetAmount ?? ZERO,
							QUOTE_PRECISION
						),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);

					batchObservableResult.observe(
						this.initialMarginRequirement!,
						convertToNumber(
							user.getInitialMarginRequirement(),
							QUOTE_PRECISION
						),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
					batchObservableResult.observe(
						this.maintenanceMarginRequirement!,
						convertToNumber(
							user.getMaintenanceMarginRequirement(),
							QUOTE_PRECISION
						),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
					batchObservableResult.observe(
						this.unrealizedPnL!,
						convertToNumber(user.getUnrealizedPNL(), QUOTE_PRECISION),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
					batchObservableResult.observe(
						this.unrealizedFundingPnL!,
						convertToNumber(user.getUnrealizedFundingPNL(), QUOTE_PRECISION),
						metricAttrFromUserAccount(user.userAccountPublicKey, userAccount)
					);
				}
			},
			[
				this.totalLeverage,
				this.totalCollateral,
				this.freeCollateral,
				this.perpPositionValue,
				this.perpPositionBase,
				this.perpPositionQuote,
				this.initialMarginRequirement,
				this.maintenanceMarginRequirement,
				this.unrealizedPnL,
				this.unrealizedFundingPnL,
			]
		);
	}
}
