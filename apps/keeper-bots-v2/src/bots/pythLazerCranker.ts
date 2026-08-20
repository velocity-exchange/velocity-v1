import { Bot } from '../types';
import { logger } from '../logger';
import { GlobalConfig, PythLazerCrankerBotConfig } from '../config';
import {
	BlockhashSubscriber,
	DevnetPerpMarkets,
	DevnetSpotMarkets,
	VelocityClient,
	getVariant,
	MainnetPerpMarkets,
	MainnetSpotMarkets,
	PriorityFeeSubscriber,
	TxSigAndSlot,
	PythLazerSubscriber,
	PythLazerPriceFeedArray,
	PriceUpdateAccount,
} from '@velocity-exchange/sdk';
import {
	AddressLookupTableAccount,
	ComputeBudgetProgram,
} from '@solana/web3.js';
import {
	chunks,
	getVersionedTransaction,
	simulateAndGetTxWithCUs,
	sleepMs,
} from '../utils';
import { Agent, setGlobalDispatcher } from 'undici';
import { Channel } from '@pythnetwork/pyth-lazer-sdk';
import { TxRecorder } from './common/txRecorder';

setGlobalDispatcher(
	new Agent({
		connections: 200,
	})
);

const SIM_CU_ESTIMATE_MULTIPLIER = 1.5;
const DEFAULT_INTEVAL_MS = 30000;
// Ceiling between posts when adaptive cranking is on. A wall-clock interval:
// no program-side gate for Pyth Lazer is slot-denominated, so this must not
// scale with slot time or the post rate (and the fee spend) doubles at each gate.
const DEFAULT_MAX_CRANK_INTERVAL_MS = 1600;
// A chunk with an unresolved send is not re-gated until this long has passed,
// so a hung sendTransaction cannot permanently wedge the chunk
const IN_FLIGHT_EXPIRY_MS = 10_000;

export class PythLazerCrankerBot implements Bot {
	private pythLazerClient?: PythLazerSubscriber;
	readonly decodeFunc: (name: string, data: Buffer) => PriceUpdateAccount;

	public name: string;
	public dryRun: boolean;
	public defaultIntervalMs?;

	private blockhashSubscriber: BlockhashSubscriber;
	private health: boolean = true;
	// Metrics
	private txRecorder: TxRecorder;

	// Post-tracking state, keyed by feed-chunk hash. lastPostMs/lastPostedPrices
	// reflect the last send that resolved successfully, inFlightSinceMs marks a
	// send whose outcome is still pending. Always maintained; only consulted by
	// the gate when crankDivergenceBps is set.
	private lastPostMs: Map<string, number> = new Map();
	private lastPostedPrices: Map<string, Map<number, number>> = new Map();
	private inFlightSinceMs: Map<string, number> = new Map();

	constructor(
		private globalConfig: GlobalConfig,
		private crankConfigs: PythLazerCrankerBotConfig,
		private velocityClient: VelocityClient,
		private priorityFeeSubscriber?: PriorityFeeSubscriber,
		private lookupTableAccounts: AddressLookupTableAccount[] = []
	) {
		this.name = crankConfigs.botId;
		this.dryRun = crankConfigs.dryRun;
		this.defaultIntervalMs = crankConfigs.intervalMs ?? DEFAULT_INTEVAL_MS;

		if (this.globalConfig.useJito) {
			throw new Error('Jito is not supported for pyth lazer cranker');
		}

		if (!this.globalConfig.lazerEndpoints || !this.globalConfig.lazerToken) {
			throw new Error('Missing lazerEndpoint or lazerToken in global config');
		}

		this.decodeFunc =
			this.velocityClient.program.account.pythLazerOracle.coder.accounts.decodeUnchecked.bind(
				this.velocityClient.program.account.pythLazerOracle.coder.accounts
			);

		this.blockhashSubscriber = new BlockhashSubscriber({
			connection: velocityClient.connection,
		});

		this.txRecorder = new TxRecorder(
			this.name,
			crankConfigs.metricsPort,
			false,
			20_000
		);
	}

	private buildFeedIdChunks(): PythLazerPriceFeedArray[] {
		let feedIdChunks: PythLazerPriceFeedArray[] = [];

		if (
			!this.crankConfigs.pythLazerIds &&
			!this.crankConfigs.pythLazerIdsByChannel
		) {
			const spotMarkets =
				this.globalConfig.velocityEnv === 'mainnet-beta'
					? MainnetSpotMarkets
					: DevnetSpotMarkets;
			const perpMarkets =
				this.globalConfig.velocityEnv === 'mainnet-beta'
					? MainnetPerpMarkets
					: DevnetPerpMarkets;

			const allFeedIds: number[] = [];
			for (const market of [...spotMarkets, ...perpMarkets]) {
				if (
					(this.crankConfigs.onlyCrankUsedOracles &&
						!getVariant(market.oracleSource).toLowerCase().includes('lazer')) ||
					market.pythLazerId == undefined
				)
					continue;
				if (
					this.crankConfigs.ignorePythLazerIds?.includes(market.pythLazerId!)
				) {
					continue;
				}

				// Check on-chain market status using velocityClient
				let marketStatus: string | undefined;
				try {
					if ('baseAssetSymbol' in market) {
						// It's a perp market
						const perpMarketAccount = this.velocityClient.getPerpMarketAccount(
							market.marketIndex
						);
						if (perpMarketAccount) {
							marketStatus = getVariant(perpMarketAccount.status);
							// Skip markets that are not active (e.g., initialized, delisted, etc.)
							if (marketStatus !== 'active') {
								logger.info(
									`Skipping pyth lazer id ${market.pythLazerId} for perp market ${market.marketIndex} (status: ${marketStatus})`
								);
								continue;
							}
						}
					} else {
						// It's a spot market
						const spotMarketAccount = this.velocityClient.getSpotMarketAccount(
							market.marketIndex
						);
						if (spotMarketAccount) {
							marketStatus = getVariant(spotMarketAccount.status);
							// Skip markets that are not active
							if (marketStatus !== 'active') {
								logger.info(
									`Skipping pyth lazer id ${market.pythLazerId} for spot market ${market.marketIndex} (status: ${marketStatus})`
								);
								continue;
							}
						}
					}
				} catch (e) {
					logger.warn(
						`Could not get market status for market ${market.marketIndex}, including feed ${market.pythLazerId} anyway: ${e}`
					);
				}

				allFeedIds.push(market.pythLazerId!);
				logger.info(
					`Adding pyth lazer id ${market.pythLazerId!} for market ${
						market.marketIndex
					} (status: ${marketStatus ?? 'unknown'})`
				);
			}
			const allFeedIdsSet = new Set(allFeedIds);
			feedIdChunks = chunks(Array.from(allFeedIdsSet), 11).map((ids) => {
				return {
					priceFeedIds: ids,
					channel: 'fixed_rate@200ms',
				};
			});
		} else if (this.crankConfigs.pythLazerIdsByChannel) {
			for (const key of Object.keys(
				this.crankConfigs.pythLazerIdsByChannel
			) as Channel[]) {
				const ids = this.crankConfigs.pythLazerIdsByChannel[key];
				if (!ids || ids.length === 0) {
					continue;
				}
				for (const idChunk of chunks(ids, 11)) {
					feedIdChunks.push({
						priceFeedIds: idChunk,
						channel: key as Channel,
					});
				}
			}
		} else {
			feedIdChunks = chunks(
				Array.from(this.crankConfigs.pythLazerIds!),
				11
			).map((ids) => {
				return {
					priceFeedIds: ids,
					channel: 'real_time',
				};
			});
		}

		logger.info(`Feed ID chunks: ${JSON.stringify(feedIdChunks)}`);
		return feedIdChunks;
	}

	async init(): Promise<void> {
		logger.info(`Initializing ${this.name} bot`);
		await this.blockhashSubscriber.subscribe();
		this.lookupTableAccounts.push(
			...(await this.velocityClient.fetchAllLookupTableAccounts())
		);

		// Build feed ID chunks after velocityClient is subscribed so we can check market statuses
		const feedIdChunks = this.buildFeedIdChunks();

		if (feedIdChunks.length === 0) {
			throw new Error('No valid pyth lazer feeds to subscribe to');
		}

		logger.info(
			`pythLazerChannel config: ${this.crankConfigs.pythLazerChannel}`
		);
		if (this.crankConfigs.feedProperties?.length) {
			logger.info(
				`pythLazerFeedProperties override: ${JSON.stringify(
					this.crankConfigs.feedProperties
				)}`
			);
		}
		this.pythLazerClient = new PythLazerSubscriber(
			this.globalConfig.lazerEndpoints!,
			this.globalConfig.lazerToken!,
			feedIdChunks,
			this.globalConfig.velocityEnv,
			undefined,
			undefined,
			this.crankConfigs.feedProperties
		);

		await this.pythLazerClient.subscribe();
	}

	async reset(): Promise<void> {
		logger.info(`Resetting ${this.name} bot`);
		this.blockhashSubscriber.unsubscribe();
		await this.velocityClient.unsubscribe();
		this.pythLazerClient?.unsubscribe();
	}

	async startIntervalLoop(intervalMs = this.defaultIntervalMs): Promise<void> {
		logger.info(`Starting ${this.name} bot with interval ${intervalMs} ms`);
		if (this.crankConfigs.crankDivergenceBps !== undefined) {
			const maxCrankIntervalMs =
				this.crankConfigs.maxCrankIntervalMs ??
				DEFAULT_MAX_CRANK_INTERVAL_MS;
			logger.info(
				`Adaptive cranking enabled: posting at most every ${maxCrankIntervalMs}ms or on >=${this.crankConfigs.crankDivergenceBps}bps divergence`
			);
			if (intervalMs !== undefined && intervalMs >= maxCrankIntervalMs) {
				logger.warn(
					`intervalMs (${intervalMs}) >= maxCrankIntervalMs (${maxCrankIntervalMs}): every tick will post via the max-interval branch and divergence gating will never apply. Set intervalMs below maxCrankIntervalMs so it acts as the condition poll rate.`
				);
			}
		} else if (this.crankConfigs.maxCrankIntervalMs !== undefined) {
			logger.warn(
				`maxCrankIntervalMs is set but crankDivergenceBps is not: adaptive cranking is disabled and maxCrankIntervalMs is ignored (posting every ${intervalMs}ms tick)`
			);
		}
		await sleepMs(5000);
		await this.runCrankLoop();

		setInterval(async () => {
			await this.runCrankLoop();
		}, intervalMs);
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

	/**
	 * Snapshot of the parsed prices for a chunk's feeds, taken at gate time so
	 * the divergence baseline matches the priceMessage being posted. Feeds with
	 * missing or non-finite prices are omitted.
	 */
	private snapshotChunkPrices(feedIds: number[]): Map<number, number> {
		const prices = new Map<number, number>();
		for (const feedId of feedIds) {
			const price = this.pythLazerClient!.feedIdToPrice.get(feedId);
			if (price !== undefined && Number.isFinite(price) && price > 0) {
				prices.set(feedId, price);
			}
		}
		return prices;
	}

	/**
	 * Decides whether a feed chunk should be posted this tick. Always posts when
	 * adaptive cranking is disabled (crankDivergenceBps unset). Otherwise posts
	 * when maxCrankIntervalMs has elapsed since the chunk's last successful
	 * post, when any feed in the chunk has moved >= crankDivergenceBps from its
	 * last posted price, or when a feed's price is unknown (posting is the safe
	 * default). A chunk with an unresolved in-flight send is skipped.
	 */
	private shouldPostChunk(
		feedIdsStr: string,
		feedIds: number[],
		currentPrices: Map<number, number>,
		nowMs: number
	): string | undefined {
		const divergenceBps = this.crankConfigs.crankDivergenceBps;
		if (divergenceBps === undefined) {
			return 'interval';
		}

		const inFlightSinceMs = this.inFlightSinceMs.get(feedIdsStr);
		if (
			inFlightSinceMs !== undefined &&
			nowMs - inFlightSinceMs < IN_FLIGHT_EXPIRY_MS
		) {
			return undefined;
		}

		const lastPostMs = this.lastPostMs.get(feedIdsStr);
		if (lastPostMs === undefined) {
			return 'first post';
		}

		const maxCrankIntervalMs =
			this.crankConfigs.maxCrankIntervalMs ??
			DEFAULT_MAX_CRANK_INTERVAL_MS;
		if (nowMs - lastPostMs >= maxCrankIntervalMs) {
			return `max interval (${
				nowMs - lastPostMs
			}ms >= ${maxCrankIntervalMs}ms)`;
		}

		const lastPrices = this.lastPostedPrices.get(feedIdsStr);
		for (const feedId of feedIds) {
			const currentPrice = currentPrices.get(feedId);
			if (currentPrice === undefined) {
				return `no live parsed price for feed ${feedId}`;
			}
			const lastPrice = lastPrices?.get(feedId);
			if (lastPrice === undefined) {
				return `no last posted price for feed ${feedId}`;
			}
			const moveBps = (Math.abs(currentPrice - lastPrice) / lastPrice) * 10_000;
			if (moveBps >= divergenceBps) {
				return `feed ${feedId} moved ${moveBps.toFixed(
					1
				)}bps >= ${divergenceBps}bps`;
			}
		}

		return undefined;
	}

	private recordPostedChunk(
		feedIdsStr: string,
		postedPrices: Map<number, number>
	) {
		this.lastPostMs.set(feedIdsStr, Date.now());
		this.lastPostedPrices.set(feedIdsStr, postedPrices);
		this.inFlightSinceMs.delete(feedIdsStr);
	}

	private recordFailedChunk(feedIdsStr: string) {
		this.inFlightSinceMs.delete(feedIdsStr);
	}

	async runCrankLoop() {
		if (!this.pythLazerClient) {
			logger.warn('pythLazerClient not initialized, skipping crank loop');
			return;
		}

		for (const [
			feedIdsStr,
			priceMessage,
		] of this.pythLazerClient.feedIdChunkToPriceMessage.entries()) {
			const feedIds = this.pythLazerClient.getPriceFeedIdsFromHash(feedIdsStr);
			const nowMs = Date.now();
			const chunkPrices = this.snapshotChunkPrices(feedIds);
			const postReason = this.shouldPostChunk(
				feedIdsStr,
				feedIds,
				chunkPrices,
				nowMs
			);
			if (postReason === undefined) {
				continue;
			}
			this.inFlightSinceMs.set(feedIdsStr, nowMs);
			const cus = Math.max(0, feedIds.length - 3) * 6_000 + 30_000;
			const ixs = [
				ComputeBudgetProgram.setComputeUnitLimit({
					units: cus,
				}),
			];
			const priorityFees = Math.floor(
				(this.priorityFeeSubscriber?.getCustomStrategyResult() || 0) *
					this.velocityClient.txSender.getSuggestedPriorityFeeMultiplier()
			);
			logger.info(
				`Priority fees to use: ${priorityFees} with multiplier: ${this.velocityClient.txSender.getSuggestedPriorityFeeMultiplier()}`
			);
			ixs.push(
				ComputeBudgetProgram.setComputeUnitPrice({
					microLamports: priorityFees,
				})
			);
			const pythLazerIxs =
				await this.velocityClient.getPostPythLazerOracleUpdateIxs(
					feedIds,
					priceMessage,
					ixs
				);
			ixs.push(...pythLazerIxs);

			if (!this.crankConfigs.skipSimulation) {
				ixs[0] = ComputeBudgetProgram.setComputeUnitLimit({
					units: 1_400_000,
				});
				const simResult = await simulateAndGetTxWithCUs({
					ixs,
					connection: this.velocityClient.connection,
					payerPublicKey: this.velocityClient.wallet.publicKey,
					lookupTableAccounts: this.lookupTableAccounts,
					cuLimitMultiplier: SIM_CU_ESTIMATE_MULTIPLIER,
					doSimulation: true,
					recentBlockhash: await this.getBlockhashForTx(),
				});
				if (simResult.simError) {
					logger.error(
						`Error simulating pyth lazer oracles for ${feedIds}: ${simResult.simTxLogs}`
					);
					this.recordFailedChunk(feedIdsStr);
					continue;
				}
				const startTime = Date.now();
				this.velocityClient
					.sendTransaction(simResult.tx)
					.then((txSigAndSlot: TxSigAndSlot) => {
						this.recordPostedChunk(feedIdsStr, chunkPrices);
						const duration = Date.now() - startTime;
						this.txRecorder.send(duration);
						logger.info(
							`Posted pyth lazer oracles for ${feedIds} (${postReason}) update atomic tx: ${txSigAndSlot.txSig}, took ${duration}ms, skippedSim: false`
						);
					})
					.catch((e) => {
						this.recordFailedChunk(feedIdsStr);
						logger.error(
							`Error sending pyth lazer oracle update for ${feedIds}: ${e}`
						);
					});
			} else {
				const startTime = Date.now();
				const tx = getVersionedTransaction(
					this.velocityClient.wallet.publicKey,
					ixs,
					this.lookupTableAccounts,
					await this.getBlockhashForTx()
				);
				this.velocityClient
					.sendTransaction(tx)
					.then((txSigAndSlot: TxSigAndSlot) => {
						this.recordPostedChunk(feedIdsStr, chunkPrices);
						const duration = Date.now() - startTime;
						this.txRecorder.send(duration);
						logger.info(
							`Posted pyth lazer oracles for ${feedIds} (${postReason}) update atomic tx: ${txSigAndSlot.txSig}, took ${duration}ms, skippedSim: true`
						);
					})
					.catch((e) => {
						this.recordFailedChunk(feedIdsStr);
						logger.error(
							`Error sending pyth lazer oracle update for ${feedIds}: ${e}`
						);
					});
			}
		}
	}

	async healthCheck(): Promise<boolean> {
		const txRecorderHealthy = this.txRecorder.isHealthy();
		if (!txRecorderHealthy) {
			logger.warn(`${this.name} bot tx recorder is unhealthy`);
		}
		this.health = txRecorderHealthy;
		return this.health;
	}
}
