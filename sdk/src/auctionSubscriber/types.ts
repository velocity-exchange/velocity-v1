import { GrpcConfigs } from '../accounts/types';
import { VelocityClient } from '../velocityClient';
import { UserAccount } from '../types';
import { ConfirmOptions, PublicKey } from '@solana/web3.js';

type AuctionSubscriberConfigBase = {
	opts?: ConfirmOptions;
	resubTimeoutMs?: number;
	logResubMessages?: boolean;
	grpcConfigs?: GrpcConfigs;
};

export type AuctionSubscriberConfig = AuctionSubscriberConfigBase &
	(
		| {
				velocityClient: VelocityClient;
				/** @deprecated use velocityClient */ driftClient?: VelocityClient;
		  }
		| {
				velocityClient?: VelocityClient;
				/** @deprecated use velocityClient */ driftClient: VelocityClient;
		  }
	);

export interface AuctionSubscriberEvents {
	onAccountUpdate: (
		account: UserAccount,
		pubkey: PublicKey,
		slot: number
	) => void;
}
