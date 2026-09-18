import { AdminClient } from './adminClient';
import { ConfirmOptions, Signer, Transaction } from '@solana/web3.js';
import { TxSigAndSlot } from './tx/types';
import { PollingVelocityClientAccountSubscriber } from './accounts/pollingVelocityClientAccountSubscriber';
import { VelocityClientConfig } from './velocityClientConfig';

export class TestClient extends AdminClient {
	public constructor(config: VelocityClientConfig) {
		config.txVersion = 'legacy';
		if (config.accountSubscription?.type !== 'polling') {
			throw new Error('Test client must be polling');
		}
		// Blockhash caching keys its TTL off the wall clock, which is incompatible
		// with LiteSVM's simulated clock: within a single 2s wall-clock window the
		// LiteSVM runtime advances many slots (and its blockhash), so a cached
		// blockhash goes stale and sequential builds collide into identical
		// transactions ("already processed") or reference an expired blockhash
		// ("Blockhash not found"). There is also no real RPC to save under LiteSVM.
		// Force fresh fetches unless a test explicitly opts back in.
		config.txHandlerConfig = {
			...config.txHandlerConfig,
			blockhashCachingEnabled:
				config.txHandlerConfig?.blockhashCachingEnabled ?? false,
		};
		super(config);
	}

	async sendTransaction(
		tx: Transaction,
		additionalSigners?: Array<Signer>,
		opts?: ConfirmOptions,
		preSigned?: boolean
	): Promise<TxSigAndSlot> {
		const { txSig, slot } = await super.sendTransaction(
			tx,
			additionalSigners,
			opts,
			preSigned
		);

		let lastFetchedSlot = (
			this.accountSubscriber as PollingVelocityClientAccountSubscriber
		).accountLoader.mostRecentSlot;
		await this.fetchAccounts();
		while (slot !== undefined && lastFetchedSlot < slot) {
			await this.fetchAccounts();
			lastFetchedSlot = (
				this.accountSubscriber as PollingVelocityClientAccountSubscriber
			).accountLoader.mostRecentSlot;
		}

		return { txSig, slot };
	}
}
