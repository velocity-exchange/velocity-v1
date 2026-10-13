import { Commitment, Connection, SYSVAR_CLOCK_PUBKEY } from '@solana/web3.js';
import { EventEmitter } from 'events';
import StrictEventEmitter from 'strict-event-emitter-types/types/src';
import { BN } from '../isomorphic/anchor';
import { promiseTimeout } from '../util/promiseTimeout';

// A half-open websocket never answers the unsubscribe, so the teardown promise
// can pend forever. Abandon it rather than let the resubscribe chain wait.
const UNSUBSCRIBE_TIMEOUT_MS = 10_000;

// eslint-disable-next-line @typescript-eslint/ban-types
type ClockSubscriberConfig = {
	commitment: Commitment;
	resubTimeoutMs?: number;
};

/** Events emitted on `ClockSubscriber.eventEmitter`. */
export interface ClockSubscriberEvent {
	/** Fired with the new on-chain unix timestamp (seconds) whenever a strictly-newer `Clock` sysvar update is observed. */
	clockUpdate: (ts: number) => void;
}

/**
 * ClockSubscriber — tracks the on-chain unix timestamp by subscribing to the
 * `Clock` sysvar via `connection.onAccountChange`, with an optional
 * stall-detection resubscribe. Useful for reading "on-chain now" without a
 * per-call RPC round-trip (e.g. for evaluating time-based order/auction
 * conditions the same way the program would). Nothing is populated until
 * `subscribe()` completes its first update — `currentTs`/`getUnixTs()` return
 * `undefined` until then.
 */
export class ClockSubscriber {
	private _latestSlot?: number;
	private _currentTs?: number;
	private subscriptionId?: number;
	commitment: Commitment;
	eventEmitter: StrictEventEmitter<EventEmitter, ClockSubscriberEvent>;

	/** Slot of the most recently observed `Clock` sysvar update, or `undefined` before the first update. */
	public get latestSlot(): number | undefined {
		return this._latestSlot;
	}

	/** On-chain unix timestamp (seconds) as of the most recently observed `Clock` sysvar update, or `undefined` before the first update. */
	public get currentTs(): number | undefined {
		return this._currentTs;
	}

	// Reconnection
	private timeoutId?: ReturnType<typeof setTimeout>;
	private resubTimeoutMs?: number;
	private isUnsubscribing = false;
	private receivingData = false;

	/**
	 * @param connection RPC connection to subscribe on.
	 * @param config.commitment Commitment for the account-change subscription; defaults to `'confirmed'`.
	 * @param config.resubTimeoutMs If set to a positive value, resubscribe when no update arrives for this many ms; `0` (or unset) disables the resubscribe watchdog. Positive values below 1000ms log a warning but are still honored.
	 */
	public constructor(
		private connection: Connection,
		config?: ClockSubscriberConfig
	) {
		this.eventEmitter = new EventEmitter();
		this.resubTimeoutMs = config?.resubTimeoutMs;
		this.commitment = config?.commitment || 'confirmed';
		if (this.resubTimeoutMs !== undefined && this.resubTimeoutMs < 1000) {
			console.log(
				'resubTimeoutMs should be at least 1000ms to avoid spamming resub'
			);
		}
	}

	/** Subscribes to `Clock` sysvar account changes. Unlike `SlotSubscriber`/`SlothashSubscriber`, does not perform an initial RPC fetch — `currentTs`/`latestSlot` stay `undefined` until the first account-change notification arrives. Idempotent while already subscribed. */
	public async subscribe(): Promise<void> {
		if (this.subscriptionId != null) {
			return;
		}

		this.subscriptionId = this.connection.onAccountChange(
			SYSVAR_CLOCK_PUBKEY,
			(acctInfo, context) => {
				if (!this.latestSlot || this.latestSlot < context.slot) {
					if (this.resubTimeoutMs && !this.isUnsubscribing) {
						this.receivingData = true;
						clearTimeout(this.timeoutId);
						this.setTimeout();
					}
					this._latestSlot = context.slot;
					const currentTs = new BN(
						acctInfo.data.subarray(32, 39),
						undefined,
						'le'
					).toNumber();
					this._currentTs = currentTs;
					this.eventEmitter.emit('clockUpdate', currentTs);
				}
			},
			this.commitment
		);

		if (this.resubTimeoutMs) {
			this.receivingData = true;
			this.setTimeout();
		}
	}

	private setTimeout(): void {
		this.timeoutId = setTimeout(async () => {
			if (this.isUnsubscribing) {
				// If we are in the process of unsubscribing, do not attempt to resubscribe
				return;
			}

			if (!this.receivingData) {
				return;
			}

			console.log(
				`No new slot in ${this.resubTimeoutMs}ms, clock subscriber resubscribing`
			);
			try {
				await this.unsubscribe(true);
				this.receivingData = false;
				await this.subscribe();
			} catch (e) {
				console.error('Clock subscriber resubscribe failed', e);
			} finally {
				// subscribe() arms the next timeout on success. If anything above
				// threw, nothing is armed and receivingData is false, so the
				// watchdog chain would silently end here.
				if (this.resubTimeoutMs && this.timeoutId === undefined) {
					this.receivingData = true;
					this.setTimeout();
				}
			}
		}, this.resubTimeoutMs);
	}

	/** @returns The on-chain unix timestamp (seconds) as of the last update, or `undefined` if `subscribe()` hasn't received an update yet. Equivalent to the `currentTs` getter. */
	public getUnixTs(): number | undefined {
		return this.currentTs;
	}

	/**
	 * Removes the `onAccountChange` listener and cancels the resub timeout.
	 * @param onResub Internal flag used when unsubscribing as part of an automatic resubscribe cycle; when `false`, also permanently disables future auto-resubscribe by clearing `resubTimeoutMs`.
	 */
	public async unsubscribe(onResub = false): Promise<void> {
		if (!onResub) {
			this.resubTimeoutMs = undefined;
		}
		this.isUnsubscribing = true;
		clearTimeout(this.timeoutId);
		this.timeoutId = undefined;

		if (this.subscriptionId != null) {
			try {
				const removed = await promiseTimeout(
					this.connection
						.removeAccountChangeListener(this.subscriptionId)
						.then(() => true),
					UNSUBSCRIBE_TIMEOUT_MS
				);
				if (!removed) {
					console.error(
						`Clock subscriber unsubscribe timed out after ${UNSUBSCRIBE_TIMEOUT_MS}ms, forcing cleanup`
					);
				}
			} catch (e) {
				console.error(
					'Clock subscriber unsubscribe failed, forcing cleanup',
					e
				);
			}
			this.subscriptionId = undefined;
		}
		this.isUnsubscribing = false;
	}
}
