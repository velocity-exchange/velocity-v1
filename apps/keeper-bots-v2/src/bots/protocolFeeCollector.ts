import {
	AdminClient,
	BN,
	ZERO,
	getTokenAmount,
	SpotBalanceType,
	PriorityFeeSubscriberMap,
	VelocityMarketInfo,
} from '@velocity-exchange/sdk';
import {
	fetchRouterConfig,
	getDistributeIx,
	getRouterConfigPda,
} from '@velocity-exchange/revenue-router-sdk';
import { Mutex } from 'async-mutex';

import { getErrorCode } from '../error';
import { logger } from '../logger';
import { Bot } from '../types';
import { webhookMessage } from '../webhook';
import { BaseBotConfig } from '../config';
import {
	getVelocityPriorityFeeEndpoint,
	simulateAndGetTxWithCUs,
	sleepS,
} from '../utils';
import {
	AddressLookupTableAccount,
	ComputeBudgetProgram,
	PublicKey,
	TransactionInstruction,
} from '@solana/web3.js';

// withdraw ixs clamp the requested amount to the pool balance on-chain, so
// u64::MAX means "withdraw everything available"
const U64_MAX = new BN('18446744073709551615');

const errorCodesToSuppress = [
	6354, // InsufficientProtocolFees — pool drained between our balance check and the withdraw landing
];

// seconds to wait for sweep txs to confirm before simulating the withdrawals
// that consume their output
const SWEEP_CONFIRM_WAIT_S = 15;

/**
 * Sweeps and withdraws accrued protocol fees to the configured recipients.
 *
 * Per run: for each perp market, crank the permissionless
 * `sweepPerpMarketFees` (materializes pending fee-ledger carveouts into
 * `protocolFeePool`), then withdraw each non-empty perp `protocolFeePool` to
 * `state.protocolFeeRecipientPerp`'s ATA and each non-empty spot
 * `protocolFeePool` to `state.protocolFeeRecipientSpot`'s ATA. Withdrawals
 * require the wallet to hold `HotRole.FeeWithdraw` (or warm/cold admin); the
 * recipient is hard-locked on-chain so this key can only trigger payment to
 * the treasury, never redirect it. Intended to run daily (`runOnce: true`
 * under a CronJob, or the 24h interval loop).
 */
export class ProtocolFeeCollectorBot implements Bot {
	public readonly name: string;
	public readonly dryRun: boolean;
	public readonly runOnce: boolean;
	public readonly defaultIntervalMs: number = 24 * 60 * 60 * 1000; // 24 hours
	private priorityFeeSubscriberMap?: PriorityFeeSubscriberMap;

	private adminClient: AdminClient;
	private intervalIds: Array<NodeJS.Timer> = [];

	private watchdogTimerMutex = new Mutex();
	private watchdogTimerLastPatTime = Date.now();
	private lookupTableAccounts?: AddressLookupTableAccount[];
	private unhealthyReason?: string;

	constructor(adminClient: AdminClient, config: BaseBotConfig) {
		this.name = config.botId;
		this.dryRun = config.dryRun;
		this.runOnce = config.runOnce || false;
		this.adminClient = adminClient;
	}

	public async init() {
		logger.info(`${this.name} initing`);

		await this.adminClient.subscribe();

		const velocityMarkets: VelocityMarketInfo[] = [];
		for (const perpMarket of this.adminClient.getPerpMarketAccounts()) {
			velocityMarkets.push({
				marketType: 'perp',
				marketIndex: perpMarket.marketIndex,
			});
		}
		for (const spotMarket of this.adminClient.getSpotMarketAccounts()) {
			velocityMarkets.push({
				marketType: 'spot',
				marketIndex: spotMarket.marketIndex,
			});
		}

		this.priorityFeeSubscriberMap = new PriorityFeeSubscriberMap({
			// Prefer the configured endpoint (PRIORITY_FEE_ENDPOINT, e.g. the
			// in-cluster dlob-server) over the hardcoded public dlob fallback.
			velocityPriorityFeeEndpoint:
				process.env.PRIORITY_FEE_ENDPOINT ??
				getVelocityPriorityFeeEndpoint('mainnet-beta'),
			velocityMarkets,
			frequencyMs: 10_000,
		});
		await this.priorityFeeSubscriberMap!.subscribe();

		// no getUser().exists() check: the fee-withdraw hot key signs the
		// sweep/withdraw ixs directly and needs no velocity user account

		this.lookupTableAccounts =
			await this.adminClient.fetchAllLookupTableAccounts();
	}

	public async reset() {
		await this.priorityFeeSubscriberMap!.unsubscribe();
		await this.adminClient.unsubscribe();
		for (const intervalId of this.intervalIds) {
			clearInterval(intervalId as NodeJS.Timeout);
		}
		this.intervalIds = [];
	}

	public async startIntervalLoop(intervalMs?: number): Promise<void> {
		logger.info(`${this.name} Bot started!`);
		if (this.runOnce) {
			await this.tryCollectProtocolFees();
		} else {
			const intervalId = setInterval(
				this.tryCollectProtocolFees.bind(this),
				intervalMs
			);
			this.intervalIds.push(intervalId);
		}
	}

	public async healthCheck(): Promise<boolean> {
		if (this.unhealthyReason !== undefined) {
			logger.error(`${this.name} unhealthy: ${this.unhealthyReason}`);
			return false;
		}
		let healthy = false;
		await this.watchdogTimerMutex.runExclusive(async () => {
			healthy =
				this.watchdogTimerLastPatTime > Date.now() - 2 * this.defaultIntervalMs;
		});
		return healthy;
	}

	/**
	 * Simulate and send one collection ix. `sent` is false on dry run, sim
	 * error, or send failure. `confirmedSlot` is undefined when the send
	 * returned without a confirmed slot, which means the money it moves may
	 * not have landed yet.
	 */
	private async sendIx(
		ix: TransactionInstruction,
		marketType: 'perp' | 'spot',
		marketIndex: number,
		label: string
	): Promise<{ sent: boolean; confirmedSlot?: number }> {
		try {
			const pfs = this.priorityFeeSubscriberMap!.getPriorityFees(
				marketType,
				marketIndex
			);
			// Guard NaN: /batchPriorityFees returns level-less entries for markets
			// with no published fees (see fundingRateUpdater.ts) — pfs.medium is
			// then undefined and Math.floor(NaN) throws at BigInt conversion.
			let microLamports = 10_000;
			if (pfs && Number.isFinite(pfs.medium)) {
				microLamports = Math.floor(pfs.medium);
			}
			const ixs = [
				ComputeBudgetProgram.setComputeUnitLimit({
					units: 1_400_000, // simulateAndGetTxWithCUs will overwrite
				}),
				ComputeBudgetProgram.setComputeUnitPrice({
					microLamports,
				}),
				ix,
			];

			const recentBlockhash =
				await this.adminClient.connection.getLatestBlockhash('confirmed');
			const simResult = await simulateAndGetTxWithCUs({
				ixs,
				connection: this.adminClient.connection,
				payerPublicKey: this.adminClient.wallet.publicKey,
				lookupTableAccounts: this.lookupTableAccounts!,
				// generous headroom: these ixs are tiny (~25-40k CUs) but on-chain
				// execution can diverge from the simulated path (e.g. an oracle
				// going stale between sim and execution burns extra CUs on logging)
				cuLimitMultiplier: 2,
				doSimulation: true,
				recentBlockhash: recentBlockhash.blockhash,
			});
			logger.info(
				`${label} on ${marketType} market ${marketIndex} estimated to take ${simResult.cuEstimate} CUs.`
			);
			if (simResult.simError !== null) {
				// e.g. InsufficientProtocolFees: the pending fee-ledger carveout we
				// counted as available couldn't (fully) materialize into the pool
				// this run — expected, retried next run
				const simErrorCode = (simResult.simError as any)?.InstructionError?.[1]
					?.Custom;
				if (errorCodesToSuppress.includes(simErrorCode)) {
					logger.info(
						`${label} for ${marketType} market ${marketIndex} skipped: sim returned suppressed error code ${simErrorCode}`
					);
				} else {
					logger.error(
						`Sim error: ${JSON.stringify(simResult.simError)}\n${
							simResult.simTxLogs ? simResult.simTxLogs.join('\n') : ''
						}`
					);
				}
			} else if (this.dryRun) {
				logger.info(
					`[DRY RUN] would send ${label} for ${marketType} market ${marketIndex}`
				);
			} else {
				const sendTxStart = Date.now();
				const txSig = await this.adminClient.txSender.sendVersionedTransaction(
					simResult.tx,
					[],
					this.adminClient.opts
				);
				logger.info(
					`${label} for ${marketType} market ${marketIndex} tx sent in ${
						Date.now() - sendTxStart
					}ms: https://solana.fm/tx/${txSig.txSig}`
				);
				return { sent: true, confirmedSlot: txSig.slot };
			}
		} catch (e: any) {
			const err = e as Error;
			const errorCode = getErrorCode(err);
			logger.error(
				`Error code: ${errorCode} while sending ${label} for ${marketType} marketIndex=${marketIndex}: ${err.message}`
			);
			console.error(err);

			if (errorCode && !errorCodesToSuppress.includes(errorCode)) {
				await webhookMessage(
					`[${
						this.name
					}]: :x: Error code: ${errorCode} while sending ${label} for ${marketType} marketIndex=${marketIndex}:\n${
						e.logs ? (e.logs as Array<string>).join('\n') : ''
					}\n${err.stack ? err.stack : err.message}`
				);
			}
		}
		return { sent: false };
	}

	/**
	 * Confirm the router is initialized and cranked by this wallet. Returning
	 * false skips only the distribute step; the withdrawals still run and the
	 * money waits in the router ATA.
	 */
	private async checkRouterConfig(): Promise<boolean> {
		const routerConfigPda = getRouterConfigPda();
		let config;
		try {
			config = await fetchRouterConfig(this.adminClient.connection);
		} catch (e: any) {
			logger.error(
				`${
					this.name
				}: could not read RouterConfig ${routerConfigPda.toBase58()}: ${
					e.message
				}`
			);
			return false;
		}
		if (config === null) {
			logger.error(
				`${
					this.name
				}: RouterConfig ${routerConfigPda.toBase58()} does not exist, skipping distribute`
			);
			return false;
		}
		if (config.cranker.equals(PublicKey.default)) {
			logger.error(
				`${this.name}: RouterConfig.cranker is unset, skipping distribute`
			);
			return false;
		}
		if (!config.cranker.equals(this.adminClient.wallet.publicKey)) {
			logger.error(
				`${
					this.name
				}: RouterConfig.cranker is ${config.cranker.toBase58()}, not this wallet ${this.adminClient.wallet.publicKey.toBase58()}, skipping distribute`
			);
			return false;
		}
		return true;
	}

	/** Own method so tests can drive the run without an RPC. */
	private async buildDistributeIx(): Promise<TransactionInstruction> {
		return await getDistributeIx({
			connection: this.adminClient.connection,
			cranker: this.adminClient.wallet.publicKey,
			payer: this.adminClient.wallet.publicKey,
		});
	}

	private async tryCollectProtocolFees() {
		this.unhealthyReason = undefined;
		try {
			const state = this.adminClient.getStateAccount();
			const perpRecipientSet = !state.protocolFeeRecipientPerp.equals(
				PublicKey.default
			);
			const spotRecipientSet = !state.protocolFeeRecipientSpot.equals(
				PublicKey.default
			);

			// 1) sweep: permissionless, materializes pending fee-ledger carveouts
			// into protocolFeePool (also runs inline on every pnl settle)
			let sweepsSent = 0;
			for (const perpMarket of this.adminClient.getPerpMarketAccounts()) {
				if (perpMarket.feeLedger.pendingProtocolFee.eq(ZERO)) {
					logger.info(
						`${this.name}: skipping sweep for perp market ${perpMarket.marketIndex}: no pending protocol fees`
					);
					continue;
				}
				const result = await this.sendIx(
					await this.adminClient.getSweepPerpMarketFeesIx(
						perpMarket.marketIndex
					),
					'perp',
					perpMarket.marketIndex,
					'sweepPerpMarketFees'
				);
				if (result.sent) {
					sweepsSent++;
				}
			}

			// the withdraw sims read on-chain pool balances, so give any sweeps a
			// moment to confirm before withdrawing what they materialized
			if (sweepsSent > 0) {
				logger.info(
					`${this.name}: waiting ${SWEEP_CONFIRM_WAIT_S}s for ${sweepsSent} sweep tx(s) to confirm`
				);
				await sleepS(SWEEP_CONFIRM_WAIT_S);
			}

			// 2) perp withdrawals (quote-denominated pools)
			const routerOk = await this.checkRouterConfig();
			if (
				routerOk &&
				!state.protocolFeeRecipientPerp.equals(getRouterConfigPda())
			) {
				logger.warn(
					`${
						this.name
					}: state.protocolFeeRecipientPerp is ${state.protocolFeeRecipientPerp.toBase58()}, not the router config ${getRouterConfigPda().toBase58()}`
				);
			}
			let perpWithdrawalsUnconfirmed = 0;
			if (!perpRecipientSet) {
				logger.info(
					`${this.name}: state.protocolFeeRecipientPerp is unset, skipping perp withdrawals`
				);
			} else {
				for (const perpMarket of this.adminClient.getPerpMarketAccounts()) {
					const quoteSpotMarket = this.adminClient.getSpotMarketAccountOrThrow(
						perpMarket.quoteSpotMarketIndex
					);
					// the cached perp market account may predate this run's sweep, so
					// count the pending carveout as available too; the on-chain clamp
					// (and suppressed InsufficientProtocolFees) covers any residual gap
					const available = getTokenAmount(
						perpMarket.protocolFeePool.scaledBalance,
						quoteSpotMarket,
						SpotBalanceType.DEPOSIT
					).add(perpMarket.feeLedger.pendingProtocolFee);
					if (available.eq(ZERO)) {
						logger.info(
							`${this.name}: skipping withdraw for perp market ${perpMarket.marketIndex}: protocol fee pool is empty`
						);
						continue;
					}
					logger.info(
						`${
							this.name
						}: withdrawing ~${available.toString()} quote base units of protocol fees from perp market ${
							perpMarket.marketIndex
						}`
					);
					const result = await this.sendIx(
						await this.adminClient.getWithdrawProtocolFeesPerpIx(
							perpMarket.marketIndex,
							U64_MAX
						),
						'perp',
						perpMarket.marketIndex,
						'withdrawProtocolFeesPerp'
					);
					if (result.sent && result.confirmedSlot === undefined) {
						perpWithdrawalsUnconfirmed++;
					}
				}
			}

			// 3) spot withdrawals (per-market token pools; lending fees accrue here
			// directly and need no sweep)
			if (!spotRecipientSet) {
				logger.info(
					`${this.name}: state.protocolFeeRecipientSpot is unset, skipping spot withdrawals`
				);
			} else {
				for (const spotMarket of this.adminClient.getSpotMarketAccounts()) {
					const available = getTokenAmount(
						spotMarket.protocolFeePool.scaledBalance,
						spotMarket,
						SpotBalanceType.DEPOSIT
					);
					if (available.eq(ZERO)) {
						logger.info(
							`${this.name}: skipping withdraw for spot market ${spotMarket.marketIndex}: protocol fee pool is empty`
						);
						continue;
					}
					logger.info(
						`${
							this.name
						}: withdrawing ~${available.toString()} token base units of protocol fees from spot market ${
							spotMarket.marketIndex
						}`
					);
					await this.sendIx(
						await this.adminClient.getWithdrawProtocolFeesSpotIx(
							spotMarket.marketIndex,
							U64_MAX
						),
						'spot',
						spotMarket.marketIndex,
						'withdrawProtocolFeesSpot'
					);
				}
			}

			// 4) split whatever reached the router ATA between the recovery pool
			// and the treasury
			if (routerOk) {
				if (perpWithdrawalsUnconfirmed > 0) {
					logger.info(
						`${this.name}: skipping distribute: ${perpWithdrawalsUnconfirmed} withdrawal(s) without a confirmed slot; funds wait in the router ATA`
					);
				} else {
					// marketType/marketIndex only pick a priority fee bucket here
					const firstPerpMarket =
						this.adminClient.getPerpMarketAccounts()[0]?.marketIndex ?? 0;
					const result = await this.sendIx(
						await this.buildDistributeIx(),
						'perp',
						firstPerpMarket,
						'distribute'
					);
					if (!result.sent && !this.dryRun) {
						this.unhealthyReason = 'distribute failed';
						await webhookMessage(
							`[${this.name}]: :x: router distribute failed; fees are stranded in the router ATA until the next run`
						);
					}
				}
			}
		} catch (e: any) {
			console.error(e);
			const err = e as Error;
			if (
				!err.message.includes('Transaction was not confirmed') &&
				!err.message.includes('Blockhash not found')
			) {
				const errorCode = getErrorCode(err);
				await webhookMessage(
					`[${
						this.name
					}]: :x: Protocol fee collector error: Error code: ${errorCode}:\n${
						e.logs ? (e.logs as Array<string>).join('\n') : ''
					}\n${err.stack ? err.stack : err.message}`
				);
			}
		} finally {
			logger.info('Protocol fee collection finished');
			await this.watchdogTimerMutex.runExclusive(async () => {
				this.watchdogTimerLastPatTime = Date.now();
			});
		}
	}
}
