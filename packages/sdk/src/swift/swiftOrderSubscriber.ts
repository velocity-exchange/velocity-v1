import {
	DevnetPerpMarkets,
	MainnetPerpMarkets,
} from '../constants/perpMarkets';
import { VelocityClient } from '../velocityClient';
import { VelocityEnv } from '../config';
import {
	getUserAccountPublicKey,
	getUserStatsAccountPublicKey,
} from '../addresses/pda';
import {
	MarketType,
	OptionalOrderParams,
	PostOnlyParams,
	SignedMsgOrderParamsDelegateMessage,
	SignedMsgOrderParamsMessage,
	UserAccount,
} from '../types';
import { Keypair, PublicKey, TransactionInstruction } from '@solana/web3.js';
import nacl from 'tweetnacl';
import { decodeUTF8 } from 'tweetnacl-util';
import WebSocket from 'ws';
import { sha256 } from '@noble/hashes/sha256';

// In practice, this for now is just an OrderSubscriber or a UserMap
export interface AccountGetter {
	mustGetUserAccount(publicKey: string): Promise<UserAccount>;
}

type SwiftOrderSubscriberConfigBase = {
	userAccountGetter?: AccountGetter;
	endpoint?: string;
	marketIndexes: number[];
	/**
		In the future, this will be used for verifying $VELOCITY stake as we add
		authentication for delegate signers
		For now, pass a new keypair or a keypair to an empty wallet
	*/
	keypair: Keypair;
};

export type SwiftOrderSubscriberConfig = SwiftOrderSubscriberConfigBase & {
	velocityEnv: VelocityEnv;
} & { velocityClient: VelocityClient };

/**
 * Swift order message received from WebSocket
 */
export interface SwiftOrderMessage {
	/** Hex string of the order message */
	order_message: string;
	/** Base58 string of taker authority */
	taker_authority: string;
	/** Base58 string of signing authority */
	signing_authority: string;
	/** Base64 string containing the order signature */
	order_signature: string;
	/** Swift order UUID */
	uuid: string;
	/** Whether the order auction params are likely to be sanitized on submission to program */
	will_sanitize?: boolean;
	/** Base64 string of a prerequisite deposit tx. The swift order_message should be bundled
	 * after the deposit when present  */
	depositTx?: string;
	/** order market index */
	market_index: number;
	/** order timestamp in unix ms */
	ts: number;
}

export class SwiftOrderSubscriber {
	private heartbeatTimeout: ReturnType<typeof setTimeout> | null = null;
	private readonly heartbeatIntervalMs = 60000;
	/**
	 * Reconnect backoff bounds. Jittered, so that a fleet of subscribers
	 * knocked off the same server pod does not retry in lockstep.
	 */
	private readonly reconnectBaseDelayMs = 500;
	private readonly reconnectMaxDelayMs = 30000;
	private reconnectAttempts = 0;
	/**
	 * Handle of the pending reconnect, and the guard against scheduling a
	 * second one: a single disconnect normally emits both `close` and `error`,
	 * and the heartbeat timer can fire on top of them, so without it one
	 * eviction would spawn two or three sockets.
	 */
	private reconnectTimeout: ReturnType<typeof setTimeout> | null = null;
	/** Set by `unsubscribe()` so a pending reconnect timer cannot undo it. */
	private stopped = false;
	private ws: WebSocket | null = null;
	private velocityClient: VelocityClient;
	public userAccountGetter?: AccountGetter; // In practice, this for now is just an OrderSubscriber or a UserMap
	public onOrder?: (
		orderMessageRaw: SwiftOrderMessage,
		signedMessage:
			| SignedMsgOrderParamsMessage
			| SignedMsgOrderParamsDelegateMessage,
		isDelegateSigner?: boolean
	) => Promise<void>;

	subscribed = false;

	/**
	 * Retained so a reconnect resubscribes with the caller's original options.
	 * The previous implementation re-entered `subscribe(onOrder)` with no
	 * further arguments, so after any disconnect a subscriber silently reverted
	 * to rejecting sanitized orders and deposit trades.
	 */
	private acceptSanitized = false;
	private acceptDepositTrade = false;

	constructor(private config: SwiftOrderSubscriberConfig) {
		// Type-system guarantees at least one of the two is supplied.
		this.velocityClient = config.velocityClient!;
		this.userAccountGetter = config.userAccountGetter;
	}

	unsubscribe() {
		this.stopped = true;
		if (this.reconnectTimeout) {
			clearTimeout(this.reconnectTimeout);
			this.reconnectTimeout = null;
		}
		if (this.heartbeatTimeout) {
			clearTimeout(this.heartbeatTimeout);
			this.heartbeatTimeout = null;
		}
		this.teardownSocket();
		this.subscribed = false;
	}

	/**
	 * Detach listeners, then drop the socket. Order matters: `terminate()` on a
	 * live socket emits `close`, which would otherwise re-enter
	 * `scheduleReconnect()`.
	 */
	private teardownSocket() {
		if (this.ws) {
			this.ws.removeAllListeners();
			this.ws.terminate();
			this.ws = null;
		}
	}

	getSymbolForMarketIndex(marketIndex: number): string {
		const env = this.config.velocityEnv;
		const markets = env === 'devnet' ? DevnetPerpMarkets : MainnetPerpMarkets;
		return markets[marketIndex].symbol;
	}

	generateChallengeResponse(nonce: string): string {
		const messageBytes = decodeUTF8(nonce);
		const signature = nacl.sign.detached(
			messageBytes,
			this.config.keypair.secretKey
		);
		const signatureBase64 = Buffer.from(signature).toString('base64');
		return signatureBase64;
	}

	handleAuthMessage(message: any): void {
		if (message['channel'] === 'auth' && message['nonce'] != null) {
			const signatureBase64 = this.generateChallengeResponse(message['nonce']);
			this.ws?.send(
				JSON.stringify({
					pubkey: this.config.keypair.publicKey.toBase58(),
					signature: signatureBase64,
				})
			);
		}

		if (
			message['channel'] === 'auth' &&
			message['message']?.toLowerCase() === 'authenticated'
		) {
			this.subscribed = true;
			// Reset here rather than on `open`: a pod that is mid-shutdown still
			// completes the TCP handshake, so a successful auth is the first real
			// proof the connection is usable.
			this.reconnectAttempts = 0;
			this.config.marketIndexes.forEach(async (marketIndex) => {
				this.ws?.send(
					JSON.stringify({
						action: 'subscribe',
						market_type: 'perp',
						market_name: this.getSymbolForMarketIndex(marketIndex),
					})
				);
				await new Promise((resolve) => setTimeout(resolve, 100));
			});
		}
	}

	async subscribe(
		onOrder: (
			orderMessageRaw: SwiftOrderMessage,
			signedMessage:
				| SignedMsgOrderParamsMessage
				| SignedMsgOrderParamsDelegateMessage,
			isDelegateSigner?: boolean
		) => Promise<void>,
		acceptSanitized = false,
		acceptDepositTrade = false
	): Promise<void> {
		this.onOrder = onOrder;
		this.acceptSanitized = acceptSanitized;
		this.acceptDepositTrade = acceptDepositTrade;
		this.stopped = false;

		const env = this.config.velocityEnv;
		const endpoint =
			this.config.endpoint ??
			(env === 'devnet'
				? 'wss://swift.master.velocity.exchange/ws'
				: 'wss://swift.velocity.exchange/ws');
		const ws = new WebSocket(
			endpoint + '?pubkey=' + this.config.keypair.publicKey.toBase58()
		);
		this.ws = ws;

		// Registered before `open`, never inside it. A socket that dies during the
		// handshake - server pod evicted, connection refused, reset - emits
		// `error` without ever emitting `open`, and node throws on an unhandled
		// 'error' event, taking the whole process down instead of retrying.
		ws.on('error', (error: Error) => {
			console.error('Swift WebSocket error:', error);
			this.scheduleReconnect();
		});

		ws.on('close', (code: number, reason: Buffer) => {
			console.log(
				`Disconnected from swift server: code=${code} reason=${reason?.toString()}`
			);
			this.scheduleReconnect();
		});

		ws.on('unexpected-response', (_request, response) => {
			console.error(
				'Unexpected response from swift server:',
				response.statusCode
			);
			this.scheduleReconnect();
		});

		ws.on('open', async () => {
			console.log('Connected to the server');

			ws.on('message', async (data: WebSocket.Data) => {
				const message = JSON.parse(data.toString());
				this.startHeartbeatTimer();

				if (message['channel'] === 'auth') {
					this.handleAuthMessage(message);
				}

				if (message['order']) {
					const order = message['order'] as SwiftOrderMessage;
					// ignore likely sanitized orders by default
					if (order.will_sanitize === true && !acceptSanitized) {
						return;
					}
					// order has a prerequisite deposit tx attached
					if (message['deposit']) {
						order.depositTx = message['deposit'];
						if (!acceptDepositTrade) {
							return;
						}
					}
					const signedMsgOrderParamsBuf = Buffer.from(
						order.order_message,
						'hex'
					);
					const isDelegateSigner = signedMsgOrderParamsBuf
						.slice(0, 8)
						.equals(
							Uint8Array.from(
								Buffer.from(
									sha256('global' + ':' + 'SignedMsgOrderParamsDelegateMessage')
								).slice(0, 8)
							)
						);
					const signedMessage =
						this.velocityClient.decodeSignedMsgOrderParamsMessage(
							signedMsgOrderParamsBuf,
							isDelegateSigner
						);

					if (!signedMessage.signedMsgOrderParams.price) {
						console.error(
							`order has no price: ${JSON.stringify(
								signedMessage.signedMsgOrderParams
							)}`
						);
						return;
					}

					onOrder(order, signedMessage, isDelegateSigner);
				}
			});
		});
	}

	async getPlaceAndMakeSignedMsgOrderIxs(
		orderMessageRaw: SwiftOrderMessage,
		signedMsgOrderParamsMessage:
			| SignedMsgOrderParamsMessage
			| SignedMsgOrderParamsDelegateMessage,
		makerOrderParams: OptionalOrderParams
	): Promise<TransactionInstruction[]> {
		if (!this.userAccountGetter) {
			throw new Error('userAccountGetter must be set to use this function');
		}

		const signedMsgOrderParamsBuf = Buffer.from(
			orderMessageRaw.order_message,
			'hex'
		);

		const isDelegateSigner = signedMsgOrderParamsBuf
			.slice(0, 8)
			.equals(
				Uint8Array.from(
					Buffer.from(
						sha256('global' + ':' + 'SignedMsgOrderParamsDelegateMessage')
					).slice(0, 8)
				)
			);
		const signedMessage = this.velocityClient.decodeSignedMsgOrderParamsMessage(
			signedMsgOrderParamsBuf,
			isDelegateSigner
		);

		const takerAuthority = new PublicKey(orderMessageRaw.taker_authority);
		const signingAuthority = new PublicKey(orderMessageRaw.signing_authority);
		const takerUserPubkey = isDelegateSigner
			? (signedMessage as SignedMsgOrderParamsDelegateMessage).takerPubkey
			: await getUserAccountPublicKey(
					this.velocityClient.program.programId,
					takerAuthority,
					(signedMessage as SignedMsgOrderParamsMessage).subAccountId
			  );
		const takerUserAccount = await this.userAccountGetter.mustGetUserAccount(
			takerUserPubkey.toString()
		);
		const ixs = await this.velocityClient.getPlaceAndMakeSignedMsgPerpOrderIxs(
			{
				orderParams: signedMsgOrderParamsBuf,
				signature: Buffer.from(orderMessageRaw.order_signature, 'base64'),
			},
			decodeUTF8(orderMessageRaw.uuid),
			{
				taker: takerUserPubkey,
				takerUserAccount,
				takerStats: getUserStatsAccountPublicKey(
					this.velocityClient.program.programId,
					takerUserAccount.authority
				),
				signingAuthority: signingAuthority,
			},
			Object.assign({}, makerOrderParams, {
				postOnly: PostOnlyParams.MUST_POST_ONLY,
				immediateOrCancel: true,
				marketType: MarketType.PERP,
			})
		);
		return ixs;
	}

	private startHeartbeatTimer() {
		if (this.heartbeatTimeout) {
			clearTimeout(this.heartbeatTimeout);
		}
		if (!this.onOrder) {
			throw new Error('onOrder callback function must be set');
		}
		this.heartbeatTimeout = setTimeout(() => {
			console.warn(
				`No heartbeat received within ${this.heartbeatIntervalMs}ms, reconnecting...`
			);
			this.scheduleReconnect();
		}, this.heartbeatIntervalMs);
	}

	/**
	 * Tear the current socket down and queue a fresh `subscribe()`.
	 *
	 * Idempotent per disconnect, and safe to call from any of the socket's
	 * failure paths.
	 */
	private scheduleReconnect() {
		if (this.stopped || this.reconnectTimeout) {
			return;
		}

		if (this.heartbeatTimeout) {
			clearTimeout(this.heartbeatTimeout);
			this.heartbeatTimeout = null;
		}
		this.teardownSocket();
		this.subscribed = false;

		const delayMs = this.nextReconnectDelayMs();
		console.log(`Reconnecting to swift WebSocket in ${delayMs}ms...`);
		this.reconnectTimeout = setTimeout(() => {
			// Cleared before resubscribing, not in the `open` handler: the
			// replacement socket may itself fail before opening, and that failure
			// has to be able to schedule the next attempt.
			this.reconnectTimeout = null;
			this.subscribe(
				this.onOrder!,
				this.acceptSanitized,
				this.acceptDepositTrade
			).catch((error) => {
				console.error('Swift resubscribe failed:', error);
				this.scheduleReconnect();
			});
		}, delayMs);
	}

	/** Exponential backoff with full jitter, capped at `reconnectMaxDelayMs`. */
	private nextReconnectDelayMs(): number {
		const ceiling = Math.min(
			this.reconnectMaxDelayMs,
			this.reconnectBaseDelayMs * 2 ** this.reconnectAttempts
		);
		this.reconnectAttempts++;
		return Math.floor(Math.random() * ceiling);
	}
}
