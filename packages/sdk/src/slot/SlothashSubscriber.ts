import {
	Commitment,
	Connection,
	SYSVAR_SLOT_HASHES_PUBKEY,
} from '@solana/web3.js';
import bs58 from 'bs58';
import { BN } from '../isomorphic/anchor';
import { promiseTimeout } from '../util/promiseTimeout';

// A half-open websocket never answers the unsubscribe, so the teardown promise
// can pend forever. Abandon it rather than let the resubscribe chain wait.
const UNSUBSCRIBE_TIMEOUT_MS = 10_000;

// eslint-disable-next-line @typescript-eslint/ban-types
type SlothashSubscriberConfig = {
	resubTimeoutMs?: number;
	commitment?: Commitment;
}; // for future customization

/** A slot and its corresponding blockhash, decoded from the `SysvarS1otHashes111111111111111111111111111` account. */
export type Slothash = {
	slot: number;
	/** Base58-encoded blockhash for that slot. */
	hash: string;
};

/**
 * SlothashSubscriber — tracks the most recent entry of the `SlotHashes`
 * sysvar via `connection.onAccountChange`, with an optional stall-detection
 * resubscribe. Updates whose slot is not strictly greater than the currently
 * tracked one are ignored.
 */
export class SlothashSubscriber {
	private _currentSlothash?: Slothash;
	private get currentSlothash(): Slothash {
		if (!this._currentSlothash) {
			throw new Error(
				'SlothashSubscriber: slothash accessed before subscribe()'
			);
		}
		return this._currentSlothash;
	}
	subscriptionId?: number;
	commitment: Commitment;

	// Reconnection
	timeoutId?: ReturnType<typeof setTimeout>;
	resubTimeoutMs?: number;
	isUnsubscribing = false;
	receivingData = false;

	/**
	 * @param connection RPC connection to subscribe on.
	 * @param config.commitment Commitment for both the initial fetch and the account-change subscription; defaults to `'processed'`.
	 * @param config.resubTimeoutMs If set to a positive value, resubscribe when no update arrives for this many ms; `0` (or unset) disables the resubscribe watchdog. Positive values below 1000ms log a warning but are still honored.
	 */
	public constructor(
		private connection: Connection,
		config?: SlothashSubscriberConfig
	) {
		this.resubTimeoutMs = config?.resubTimeoutMs;
		this.commitment = config?.commitment ?? 'processed';
		if (this.resubTimeoutMs != null && this.resubTimeoutMs < 1000) {
			console.log(
				'resubTimeoutMs should be at least 1000ms to avoid spamming resub'
			);
		}
	}

	/**
	 * Fetches the `SlotHashes` sysvar once via RPC, then subscribes to
	 * `onAccountChange` for live updates. Idempotent while already subscribed.
	 * @throws If the sysvar account can't be fetched on the initial load.
	 */
	public async subscribe(): Promise<void> {
		if (this.subscriptionId != null) {
			return;
		}

		const currentAccountData = await this.connection.getAccountInfo(
			SYSVAR_SLOT_HASHES_PUBKEY,
			this.commitment
		);
		if (currentAccountData == null) {
			throw new Error('Failed to retrieve current slot hash');
		}
		this._currentSlothash = deserializeSlothash(currentAccountData.data);

		this.subscriptionId = this.connection.onAccountChange(
			SYSVAR_SLOT_HASHES_PUBKEY,
			(slothashInfo, context) => {
				if (
					!this._currentSlothash ||
					this._currentSlothash.slot < context.slot
				) {
					if (this.resubTimeoutMs && !this.isUnsubscribing) {
						this.receivingData = true;
						clearTimeout(this.timeoutId);
						this.setTimeout();
					}
					this._currentSlothash = deserializeSlothash(slothashInfo.data);
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
				`No new slot in ${this.resubTimeoutMs}ms, slothash subscriber resubscribing`
			);
			try {
				await this.unsubscribe(true);
				this.receivingData = false;
				await this.subscribe();
			} catch (e) {
				console.error('Slothash subscriber resubscribe failed', e);
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

	/**
	 * @returns The most recently observed `Slothash`.
	 * @throws If called before `subscribe()` has completed its initial fetch.
	 */
	public getSlothash(): Slothash {
		return this.currentSlothash;
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
						`Slothash subscriber unsubscribe timed out after ${UNSUBSCRIBE_TIMEOUT_MS}ms, forcing cleanup`
					);
				}
			} catch (e) {
				console.error(
					'Slothash subscriber unsubscribe failed, forcing cleanup',
					e
				);
			}
			this.subscriptionId = undefined;
		}
		this.isUnsubscribing = false;
	}
}

function deserializeSlothash(data: Buffer): Slothash {
	const slotNumber = new BN(data.subarray(8, 16), 10, 'le');
	const hash = bs58.encode(data.subarray(16, 48));
	return {
		slot: slotNumber.toNumber(),
		hash,
	};
}
