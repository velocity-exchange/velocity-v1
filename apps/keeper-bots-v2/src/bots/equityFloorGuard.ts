import {
	VelocityClient,
	User,
	UserMap,
	getUserStatsAccountPublicKey,
	getEquityFloorLevel,
	EquityFloorLevel,
	ZERO,
	BN,
	QUOTE_PRECISION,
} from '@velocity-exchange/sdk';
import { Mutex } from 'async-mutex';

import { logger } from '../logger';
import { Bot } from '../types';
import { BaseBotConfig } from '../config';
import { webhookMessage } from '../webhook';
import {
	AddressLookupTableAccount,
	ComputeBudgetProgram,
} from '@solana/web3.js';
import { simulateAndGetTxWithCUs } from '../utils';
import { PrometheusExporter } from '@opentelemetry/exporter-prometheus';
import { Counter, Meter, ObservableGauge } from '@opentelemetry/api';
import { MeterProvider } from '@opentelemetry/sdk-metrics-base';
import { metricAttrFromUserAccount, RuntimeSpec } from '../metrics';

const DEFAULT_WARNING_BUFFER_MULTIPLE = 2; // warn inside floor + 2x buffer
const DEFAULT_HEADROOM_DROP_ALERT_PCT = 10; // alert when headroom falls 10% between checks

enum METRIC_TYPES {
	runtime_specs = 'runtime_specs',
	equity_floor_headroom = 'equity_floor_headroom',
	equity_floor_buffered_headroom = 'equity_floor_buffered_headroom',
	equity_floor_level = 'equity_floor_level',
	equity_floor_level_transitions = 'equity_floor_level_transitions',
	equity_floor_headroom_drops = 'equity_floor_headroom_drops',
	equity_floor_trip_attempts = 'equity_floor_trip_attempts',
	equity_floor_trips_confirmed = 'equity_floor_trips_confirmed',
}

/** numeric encoding for the level gauge, higher = worse */
const LEVEL_VALUES: Record<EquityFloorLevel, number> = {
	disabled: -1,
	healthy: 0,
	warning: 1,
	critical: 2,
	breached: 3,
};

export interface EquityFloorGuardConfig extends BaseBotConfig {
	/** warn when equity is inside floor + this multiple of the buffer (default 2) */
	warningBufferMultiple?: number;
	/** alert when a subaccount's headroom above the floor drops by this percent between checks (default 10) */
	headroomDropAlertPct?: number;
	metricsPort?: number;
}

type TrackedUserState = {
	level: EquityFloorLevel;
	/** headroom above the raw floor at the last check, QUOTE_PRECISION */
	headroom: BN;
};

/**
 * Watches every subaccount with an `equityFloor` set. Publishes per-subaccount
 * headroom/level metrics, logs and webhooks level transitions and sharp
 * headroom drops, and on breach fires the permissionless
 * `tripEquityFloorBreaker` instruction, freezing all of the authority's
 * subaccounts on-chain.
 *
 * The levels mirror the SDK's `getEquityFloorLevel` and use the same net-equity
 * metric the onchain checks use. `breached` is below the floor and trippable.
 * `critical` is below the floor plus the buffer, where risk-increasing actions
 * are rejected. `warning` is inside `warningBufferMultiple` buffers of the
 * floor. `healthy` is above all of those.
 *
 * The trip attempt is gated on the point-value level, or on the SDK's
 * `provesEquityFloorBreach` mirror of the onchain trip proof. The point value
 * prices every position at the live oracle with no validity check, so an
 * invalid oracle printing high can hide a breach that the program's concession
 * walk still proves.
 *
 * This bot only detects and escalates, and it holds no privileged key. The
 * per-subaccount enforcement is automatic inside the program, and position
 * unwind / floor reset are multisig operations handled by humans.
 */
export class EquityFloorGuardBot implements Bot {
	public readonly name: string;
	public readonly dryRun: boolean;
	public readonly runOnce: boolean;
	public readonly defaultIntervalMs: number = 10_000;

	private velocityClient: VelocityClient;
	private lookupTableAccounts?: AddressLookupTableAccount[];
	private intervalIds: Array<NodeJS.Timer> = [];
	private userMap: UserMap;
	private warningBufferMultiple: number;
	private headroomDropAlertPct: number;

	/** last observed level + headroom per subaccount, for transition/drop triggers */
	private trackedUsers = new Map<string, TrackedUserState>();
	/** authorities whose breaker this bot already attempted to trip */
	private trippedAuthorities = new Set<string>();
	/** Authorities whose trip is blocked by an invalid oracle. It debounces the
	 * webhook. */
	private oracleBlockedAuthorities = new Set<string>();

	// metrics
	private metricsPort?: number;
	private metricsInitialized = false;
	private exporter?: PrometheusExporter;
	private meter?: Meter;
	private runtimeSpec: RuntimeSpec;
	private bootTimeMs?: number;
	private runtimeSpecsGauge?: ObservableGauge;
	private headroomGauge?: ObservableGauge;
	private bufferedHeadroomGauge?: ObservableGauge;
	private levelGauge?: ObservableGauge;
	private levelTransitionCounter?: Counter;
	private headroomDropCounter?: Counter;
	private tripAttemptCounter?: Counter;
	private tripConfirmedCounter?: Counter;
	/** latest gauge observations, keyed by user pubkey */
	private gaugeObservations = new Map<
		string,
		{ attrs: any; headroom: number; bufferedHeadroom: number; level: number }
	>();

	private watchdogTimerMutex = new Mutex();
	private watchdogTimerLastPatTime = Date.now();

	constructor(
		velocityClient: VelocityClient,
		runtimeSpec: RuntimeSpec,
		config: EquityFloorGuardConfig
	) {
		this.name = config.botId;
		this.dryRun = config.dryRun;
		this.runOnce = config.runOnce || false;
		this.velocityClient = velocityClient;
		this.runtimeSpec = runtimeSpec;
		this.warningBufferMultiple =
			config.warningBufferMultiple ?? DEFAULT_WARNING_BUFFER_MULTIPLE;
		this.headroomDropAlertPct =
			config.headroomDropAlertPct ?? DEFAULT_HEADROOM_DROP_ALERT_PCT;
		this.metricsPort = config.metricsPort;
		if (this.metricsPort) {
			this.initializeMetrics();
		}
		this.userMap = new UserMap({
			velocityClient: this.velocityClient,
			subscriptionConfig: {
				type: 'polling',
				frequency: 10_000,
				commitment: this.velocityClient.opts?.commitment,
			},
			skipInitialLoad: false,
			includeIdle: true,
		});
	}

	private initializeMetrics() {
		if (this.metricsInitialized) {
			logger.error('Tried to initialize metrics multiple times');
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
		const meterProvider = new MeterProvider();
		meterProvider.addMetricReader(this.exporter);
		this.meter = meterProvider.getMeter(this.name);

		this.bootTimeMs = Date.now();
		this.runtimeSpecsGauge = this.meter.createObservableGauge(
			METRIC_TYPES.runtime_specs,
			{
				description: 'Runtime specification of this program',
			}
		);
		this.runtimeSpecsGauge.addCallback((obs) => {
			obs.observe(this.bootTimeMs!, this.runtimeSpec as any);
		});

		this.headroomGauge = this.meter.createObservableGauge(
			METRIC_TYPES.equity_floor_headroom,
			{
				description:
					'Equity above the raw floor (trip threshold) per subaccount, quote units',
			}
		);
		this.headroomGauge.addCallback((obs) => {
			for (const { attrs, headroom } of this.gaugeObservations.values()) {
				obs.observe(headroom, attrs);
			}
		});
		this.bufferedHeadroomGauge = this.meter.createObservableGauge(
			METRIC_TYPES.equity_floor_buffered_headroom,
			{
				description:
					'Equity above floor + buffer (action gate) per subaccount, quote units',
			}
		);
		this.bufferedHeadroomGauge.addCallback((obs) => {
			for (const {
				attrs,
				bufferedHeadroom,
			} of this.gaugeObservations.values()) {
				obs.observe(bufferedHeadroom, attrs);
			}
		});
		this.levelGauge = this.meter.createObservableGauge(
			METRIC_TYPES.equity_floor_level,
			{
				description:
					'Floor level per subaccount: 0 healthy, 1 warning, 2 critical, 3 breached',
			}
		);
		this.levelGauge.addCallback((obs) => {
			for (const { attrs, level } of this.gaugeObservations.values()) {
				obs.observe(level, attrs);
			}
		});
		this.levelTransitionCounter = this.meter.createCounter(
			METRIC_TYPES.equity_floor_level_transitions,
			{ description: 'Count of level transitions, labeled from/to' }
		);
		this.headroomDropCounter = this.meter.createCounter(
			METRIC_TYPES.equity_floor_headroom_drops,
			{
				description: `Count of headroom drops >= ${this.headroomDropAlertPct}% between checks`,
			}
		);
		this.tripAttemptCounter = this.meter.createCounter(
			METRIC_TYPES.equity_floor_trip_attempts,
			{ description: 'Count of tripEquityFloorBreaker sends attempted' }
		);
		this.tripConfirmedCounter = this.meter.createCounter(
			METRIC_TYPES.equity_floor_trips_confirmed,
			{ description: 'Count of breaker trips confirmed landed' }
		);
	}

	public async init() {
		logger.info(`${this.name} initing`);
		await this.velocityClient.subscribe();
		await this.userMap.subscribe();
		this.lookupTableAccounts =
			await this.velocityClient.fetchAllLookupTableAccounts();
	}

	public async reset() {
		for (const intervalId of this.intervalIds) {
			clearInterval(intervalId as NodeJS.Timeout);
		}
		this.intervalIds = [];
		await this.userMap?.unsubscribe();
	}

	public async startIntervalLoop(intervalMs?: number): Promise<void> {
		logger.info(`${this.name} Bot started!`);
		if (this.runOnce) {
			await this.checkFlooredUsers();
		} else {
			const intervalId = setInterval(
				this.checkFlooredUsers.bind(this),
				intervalMs ?? this.defaultIntervalMs
			);
			this.intervalIds.push(intervalId);
		}
	}

	public async healthCheck(): Promise<boolean> {
		let healthy = false;
		await this.watchdogTimerMutex.runExclusive(async () => {
			healthy =
				this.watchdogTimerLastPatTime > Date.now() - 5 * this.defaultIntervalMs;
		});
		return healthy;
	}

	private quoteNumber(value: BN): number {
		return Number(value.toString()) / Number(QUOTE_PRECISION.toString());
	}

	private async checkFlooredUsers() {
		try {
			// The trip-proof mirror classifies oracle staleness against this slot.
			const slot = new BN(await this.velocityClient.connection.getSlot());
			const seen = new Set<string>();
			for (const user of this.userMap.values()) {
				const userAccount = user.getUserAccountOrThrow();
				if (userAccount.equityFloor.lte(ZERO)) {
					continue;
				}

				const userKey = user.getUserAccountPublicKey().toBase58();
				const authorityKey = userAccount.authority.toBase58();
				seen.add(userKey);

				// The onchain floor checks read getFloorNetEquity. getNetUsdValue has
				// a different composition and no oracle-validity verdict.
				const equity = user.getFloorNetEquity(slot).value;
				const headroom = equity.sub(userAccount.equityFloor);
				const bufferedHeadroom = headroom.sub(userAccount.equityFloorBuffer);
				const level = getEquityFloorLevel(
					equity,
					userAccount.equityFloor,
					userAccount.equityFloorBuffer,
					this.warningBufferMultiple
				);
				const attrs = metricAttrFromUserAccount(
					user.getUserAccountPublicKey(),
					userAccount
				);
				this.gaugeObservations.set(userKey, {
					attrs,
					headroom: this.quoteNumber(headroom),
					bufferedHeadroom: this.quoteNumber(bufferedHeadroom),
					level: LEVEL_VALUES[level],
				});

				const previous = this.trackedUsers.get(userKey);

				if (previous && previous.level !== level) {
					const worsened = LEVEL_VALUES[level] > LEVEL_VALUES[previous.level];
					const message =
						`${this.name}: user ${userKey} (authority ${authorityKey}) ` +
						`${previous.level} -> ${level}, headroom ${this.quoteNumber(
							headroom
						).toFixed(2)}, buffered headroom ${this.quoteNumber(
							bufferedHeadroom
						).toFixed(2)}`;
					if (worsened) {
						logger.warn(message);
						if (level === 'critical' || level === 'breached') {
							await webhookMessage(message);
						}
					} else {
						logger.info(message);
					}
					this.levelTransitionCounter?.add(1, {
						...attrs,
						from: previous.level,
						to: level,
					});
				}

				// sharp drop trigger: headroom fell by >= headroomDropAlertPct
				// since the last check while a real cushion existed
				if (
					previous &&
					previous.headroom.gt(ZERO) &&
					headroom.lt(
						previous.headroom
							.mul(new BN(100 - this.headroomDropAlertPct))
							.div(new BN(100))
					)
				) {
					const message =
						`${this.name}: user ${userKey} (authority ${authorityKey}) headroom dropped ` +
						`${this.quoteNumber(previous.headroom).toFixed(
							2
						)} -> ${this.quoteNumber(headroom).toFixed(2)} ` +
						`(>= ${this.headroomDropAlertPct}% in one interval)`;
					logger.warn(message);
					await webhookMessage(message);
					this.headroomDropCounter?.add(1, attrs);
				}

				this.trackedUsers.set(userKey, { level, headroom });

				// The trip decision mirrors the onchain predicate rather than the
				// point value alone. An invalid oracle printing high can make
				// the point value look healthy while the program's concession
				// walk still proves the breach. Either signal attempts the trip,
				// and the simulation classifies the chain's answer.
				const provesBreach = user.provesEquityFloorBreach(slot);
				if (level === 'breached' || provesBreach) {
					logger.error(
						`${this.name}: BREACH user ${userKey} (authority ${authorityKey}) ` +
							(level === 'breached'
								? `below equity floor ${userAccount.equityFloor}`
								: `provable breach of equity floor ${userAccount.equityFloor} despite a healthy point value (invalid oracle conceded)`)
					);
					await this.tryTripBreaker(user, authorityKey);
				}
			}

			// drop state and gauges for users that lost their floor or vanished
			for (const key of [...this.trackedUsers.keys()]) {
				if (!seen.has(key)) {
					this.trackedUsers.delete(key);
					this.gaugeObservations.delete(key);
				}
			}
		} catch (err) {
			logger.error(`${this.name}: ${err}`);
		} finally {
			await this.watchdogTimerMutex.runExclusive(async () => {
				this.watchdogTimerLastPatTime = Date.now();
			});
		}
	}

	private async tryTripBreaker(
		user: User,
		authorityKey: string
	): Promise<void> {
		// A dry run never sets the onchain flag, so the cache alone debounces it.
		if (this.dryRun && this.trippedAuthorities.has(authorityKey)) {
			return;
		}

		const userAccount = user.getUserAccountOrThrow();

		// Skip an authority already tripped on chain, whether by this bot or by
		// anyone else. The cache is trusted only while the onchain flag backs it.
		// A warm-admin reset clears the flag, the entry is evicted, and a later
		// breach trips again instead of being skipped until the bot restarts.
		try {
			const stats = await (
				this.velocityClient.program.account as any
			).userStats.fetch(
				getUserStatsAccountPublicKey(
					this.velocityClient.program.programId,
					userAccount.authority
				)
			);
			if (stats.equityBreakerTripped !== 0) {
				this.trippedAuthorities.add(authorityKey);
				return;
			}
			this.trippedAuthorities.delete(authorityKey);
		} catch (e) {
			logger.error(`${this.name}: failed to fetch user stats: ${e}`);
			return;
		}

		if (this.dryRun) {
			logger.info(
				`${this.name}: DRY RUN would trip breaker for ${authorityKey}`
			);
			this.trippedAuthorities.add(authorityKey);
			return;
		}

		try {
			this.tripAttemptCounter?.add(1, { authority: authorityKey });
			const ixs = [
				ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }),
				await this.velocityClient.getTripEquityFloorBreakerIx(
					user.getUserAccountPublicKey(),
					userAccount
				),
			];

			const recentBlockhash = (
				await this.velocityClient.connection.getLatestBlockhash({
					commitment: 'finalized',
				})
			).blockhash;

			const simResult = await simulateAndGetTxWithCUs({
				ixs,
				connection: this.velocityClient.connection,
				payerPublicKey: this.velocityClient.wallet.publicKey,
				lookupTableAccounts: this.lookupTableAccounts!,
				cuLimitMultiplier: 1.2,
				doSimulation: true,
				recentBlockhash,
			});

			if (simResult.simError !== null) {
				const simErrorText = JSON.stringify(simResult.simError);
				// InvalidOracle, code 6035 or 0x1793, does not mean the account
				// is healthy. The account is below its floor, and the trip
				// cannot prove it until the oracle recovers. During an oracle
				// outage this is the state worth alerting on, so it gets its
				// own branch rather than the generic sim-error path.
				if (
					simErrorText.includes('"Custom":6035') ||
					simErrorText.includes('0x1793')
				) {
					const message =
						`${this.name}: trip for ${authorityKey} blocked by invalid oracle; ` +
						`account is breached but unprovable until the feed recovers, retrying`;
					logger.warn(message);
					// Send one webhook per outage rather than one per cycle.
					if (!this.oracleBlockedAuthorities.has(authorityKey)) {
						this.oracleBlockedAuthorities.add(authorityKey);
						await webhookMessage(message);
					}
					return;
				}
				this.oracleBlockedAuthorities.delete(authorityKey);
				logger.error(
					`${this.name}: trip sim error for ${authorityKey}: ${simErrorText}`
				);
				return;
			}

			const { txSig } =
				await this.velocityClient.txSender.sendVersionedTransaction(
					simResult.tx,
					[],
					this.velocityClient.opts
				);
			this.trippedAuthorities.add(authorityKey);
			this.oracleBlockedAuthorities.delete(authorityKey);
			this.tripConfirmedCounter?.add(1, { authority: authorityKey });
			const message = `${this.name}: breaker tripped for ${authorityKey}: https://solscan.io/tx/${txSig}`;
			logger.info(message);
			await webhookMessage(message);
		} catch (e) {
			logger.error(`${this.name}: failed to trip breaker: ${e}`);
		}
	}
}
