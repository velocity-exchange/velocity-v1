import { DataAndSlot, UserAccountEvents, UserAccountSubscriber } from './types';
import { PublicKey } from '@solana/web3.js';
import StrictEventEmitter from 'strict-event-emitter-types';
import { EventEmitter } from 'events';
import { UserAccount } from '../types';

/**
 * Basic implementation of UserAccountSubscriber. It will only take in UserAccount
 * data during initialization and will not fetch or subscribe to updates.
 */
export class BasicUserAccountSubscriber implements UserAccountSubscriber {
	isSubscribed: boolean;
	eventEmitter: StrictEventEmitter<EventEmitter, UserAccountEvents>;
	userAccountPublicKey: PublicKey;

	callbackId?: string;
	errorCallbackId?: string;

	user: DataAndSlot<UserAccount | undefined>;

	public constructor(
		userAccountPublicKey: PublicKey,
		data?: UserAccount,
		slot?: number
	) {
		this.isSubscribed = true;
		this.eventEmitter = new EventEmitter();
		this.userAccountPublicKey = userAccountPublicKey;
		this.user = { data, slot: slot ?? 0 };
	}

	async subscribe(_userAccount?: UserAccount): Promise<boolean> {
		return true;
	}

	async addToAccountLoader(): Promise<void> {}

	async fetch(): Promise<void> {}

	doesAccountExist(): boolean {
		return this.user !== undefined;
	}

	async unsubscribe(): Promise<void> {}

	assertIsSubscribed(): void {}

	public getUserAccountAndSlot(): DataAndSlot<UserAccount> {
		const { data, slot } = this.user;
		if (data === undefined) {
			throw new Error(
				'UserAccount data not available: no account data has been provided yet'
			);
		}
		return { data, slot };
	}

	public updateData(userAccount: UserAccount, slot: number): void {
		if (!this.user || slot >= (this.user.slot ?? 0)) {
			this.user = { data: userAccount, slot };
			this.eventEmitter.emit('userAccountUpdate', userAccount);
			this.eventEmitter.emit('update');
		}
	}
}
