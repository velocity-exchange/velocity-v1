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

		// Blockhash caching keys its TTL off the wall clock, but LiteSVM advances many
		// slots per wall-clock window, so a cached blockhash goes stale and collides
		// ("already processed") or expires ("Blockhash not found"). Force fresh
		// fetches unless a test explicitly opts back in.
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
