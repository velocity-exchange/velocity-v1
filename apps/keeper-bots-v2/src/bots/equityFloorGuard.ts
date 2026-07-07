import {
	VelocityClient,
	User,
	UserMap,
	getUserStatsAccountPublicKey,
	ZERO,
	BN,
} from '@velocity-exchange/sdk';
import { Mutex } from 'async-mutex';

import { logger } from '../logger';
import { Bot } from '../types';
import { BaseBotConfig } from '../config';
import {
	AddressLookupTableAccount,
	ComputeBudgetProgram,
} from '@solana/web3.js';
import { simulateAndGetTxWithCUs } from '../utils';

const DEFAULT_WARN_BUFFER_BPS = 500; // warn when headroom < 5% of the floor

export interface EquityFloorGuardConfig extends BaseBotConfig {
	/** warn when equity headroom above the floor falls below floor * bps / 10000 */
	warnBufferBps?: number;
}

/**
 * Watches every subaccount with an `equityFloor` set. Logs a warning when
 * equity headroom above the floor runs low, and on breach fires the
 * permissionless `tripEquityFloorBreaker` instruction, freezing all of the
 * authority's subaccounts on-chain.
 *
 * Detection and escalation only: this bot holds no privileged key. The
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
	private warnBufferBps: number;

	/** subaccounts already warned this episode, to avoid log spam */
	private warnedUsers = new Set<string>();
	/** authorities whose breaker this bot already attempted to trip */
	private trippedAuthorities = new Set<string>();

	private watchdogTimerMutex = new Mutex();
	private watchdogTimerLastPatTime = Date.now();

	constructor(velocityClient: VelocityClient, config: EquityFloorGuardConfig) {
		this.name = config.botId;
		this.dryRun = config.dryRun;
		this.runOnce = config.runOnce || false;
		this.velocityClient = velocityClient;
		this.warnBufferBps = config.warnBufferBps ?? DEFAULT_WARN_BUFFER_BPS;
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

	private async checkFlooredUsers() {
		try {
			for (const user of this.userMap.values()) {
				const userAccount = user.getUserAccountOrThrow();
				if (userAccount.equityFloor.lte(ZERO)) {
					continue;
				}

				const userKey = user.getUserAccountPublicKey().toBase58();
				const authorityKey = userAccount.authority.toBase58();

				if (user.isBelowEquityFloor()) {
					logger.error(
						`${this.name}: BREACH user ${userKey} (authority ${authorityKey}) below equity floor ${userAccount.equityFloor}`
					);
					await this.tryTripBreaker(user, authorityKey);
					continue;
				}

				const headroom = user.getEquityAboveFloor();
				const warnThreshold = userAccount.equityFloor
					.mul(new BN(this.warnBufferBps))
					.div(new BN(10_000));
				if (headroom !== null && headroom.lt(warnThreshold)) {
					if (!this.warnedUsers.has(userKey)) {
						this.warnedUsers.add(userKey);
						logger.warn(
							`${this.name}: WARNING user ${userKey} headroom ${headroom} below buffer ${warnThreshold}`
						);
					}
				} else {
					this.warnedUsers.delete(userKey);
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
		if (this.trippedAuthorities.has(authorityKey)) {
			return;
		}

		const userAccount = user.getUserAccountOrThrow();

		// skip if already tripped on chain (by us earlier or anyone else)
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
				logger.error(
					`${this.name}: trip sim error for ${authorityKey}: ${JSON.stringify(
						simResult.simError
					)}`
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
			logger.info(
				`${this.name}: breaker tripped for ${authorityKey}: https://solscan.io/tx/${txSig}`
			);
		} catch (e) {
			logger.error(`${this.name}: failed to trip breaker: ${e}`);
		}
	}
}
