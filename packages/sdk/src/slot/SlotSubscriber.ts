import { Connection } from '@solana/web3.js';
import { EventEmitter } from 'events';
import StrictEventEmitter from 'strict-event-emitter-types/types/src';
import { promiseTimeout } from '../util/promiseTimeout';

// A half-open websocket never answers the unsubscribe, so the teardown promise
// can pend forever. Abandon it rather than let the resubscribe chain wait.
const UNSUBSCRIBE_TIMEOUT_MS = 10_000;

// eslint-disable-next-line @typescript-eslint/ban-types
type SlotSubscriberConfig = {
	resubTimeoutMs?: number;
}; // for future customization

/** Events emitted on `SlotSubscriber.eventEmitter`. */
export interface SlotSubscriberEvents {
	/** Fired whenever a new, strictly-greater slot is observed (including the initial slot fetched by `subscribe()`). */
	newSlot: (newSlot: number) => void;
}

/**
 * SlotSubscriber — tracks the current slot via `connection.onSlotChange`,
 * with an optional stall-detection resubscribe. Slot updates that are not
 * strictly greater than the currently tracked slot are ignored (protects
 * against out-of-order delivery).
 */
export class SlotSubscriber {
	currentSlot = 0;
	subscriptionId?: number;
	eventEmitter: StrictEventEmitter<EventEmitter, SlotSubscriberEvents>;

	// Reconnection
	timeoutId?: ReturnType<typeof setTimeout>;
	resubTimeoutMs?: number;
	isUnsubscribing = false;
	receivingData = false;

	/**
	 * @param connection RPC connection to subscribe on.
	 * @param config.resubTimeoutMs If set to a positive value, resubscribe when no slot update arrives for this many ms; `0` (or unset) disables the resubscribe watchdog. Positive values below 1000ms log a warning (too aggressive) but are still honored.
	 */
	public constructor(
		private connection: Connection,
		config?: SlotSubscriberConfig
	) {
		this.eventEmitter = new EventEmitter();
		this.resubTimeoutMs = config?.resubTimeoutMs;
		if (this.resubTimeoutMs !== undefined && this.resubTimeoutMs < 1000) {
			console.log(
				'resubTimeoutMs should be at least 1000ms to avoid spamming resub'
			);
		}
	}

	/** Fetches the current slot once via RPC, then subscribes to `onSlotChange` for live updates. Idempotent while already subscribed. */
	public async subscribe(): Promise<void> {
		if (this.subscriptionId != null) {
			return;
		}

		const newSlot = await this.connection.getSlot('confirmed');
		this.updateCurrentSlot(newSlot);

		this.subscriptionId = this.connection.onSlotChange((slotInfo) => {
			const newSlot = slotInfo.slot;

			if (!this.currentSlot || this.currentSlot < newSlot) {
				if (this.resubTimeoutMs && !this.isUnsubscribing) {
					this.receivingData = true;
					clearTimeout(this.timeoutId);
					this.setTimeout();
				}
				this.updateCurrentSlot(newSlot);
			}
		});

		if (this.resubTimeoutMs) {
			this.receivingData = true;
			this.setTimeout();
		}
	}

	private updateCurrentSlot(slot: number) {
		this.currentSlot = slot;
		this.eventEmitter.emit('newSlot', slot);
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
				`No new slot in ${this.resubTimeoutMs}ms, slot subscriber resubscribing`
			);
			try {
				await this.unsubscribe(true);
				this.receivingData = false;
				await this.subscribe();
			} catch (e) {
				console.error('Slot subscriber resubscribe failed', e);
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

	/** @returns The most recently observed slot, or `0` if `subscribe()` hasn't completed yet. */
	public getSlot(): number {
		return this.currentSlot;
	}

	/**
	 * Removes the `onSlotChange` listener and cancels the resub timeout.
	 * @param onResub Internal flag used when unsubscribing as part of an automatic resubscribe cycle; when `false` (a caller-initiated unsubscribe), also permanently disables future auto-resubscribe by clearing `resubTimeoutMs`.
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
						.removeSlotChangeListener(this.subscriptionId)
						.then(() => true),
					UNSUBSCRIBE_TIMEOUT_MS
				);
				if (!removed) {
					console.error(
						`Slot subscriber unsubscribe timed out after ${UNSUBSCRIBE_TIMEOUT_MS}ms, forcing cleanup`
					);
				}
			} catch (e) {
				console.error('Slot subscriber unsubscribe failed, forcing cleanup', e);
			}
			this.subscriptionId = undefined;
		}
		this.isUnsubscribing = false;
	}
}
