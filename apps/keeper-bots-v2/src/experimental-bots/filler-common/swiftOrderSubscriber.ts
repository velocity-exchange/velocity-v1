import {
	DevnetPerpMarkets,
	VelocityEnv,
	loadKeypair,
	MainnetPerpMarkets,
} from '@velocity-exchange/sdk';
import { Keypair } from '@solana/web3.js';
import nacl from 'tweetnacl';
import { decodeUTF8 } from 'tweetnacl-util';
import WebSocket from 'ws';
import { sleepMs } from '../../utils';
import dotenv from 'dotenv';
import parseArgs from 'minimist';

export type SwiftOrderSubscriberConfig = {
	velocityEnv: VelocityEnv;
	endpoint: string;
	marketIndexes: number[];
	keypair: Keypair;
};

export class SwiftOrderSubscriber {
	private heartbeatTimeout: NodeJS.Timeout | null = null;
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
	private reconnectTimeout: NodeJS.Timeout | null = null;
	private ws: WebSocket | null = null;
	subscribed: boolean = false;

	constructor(private config: SwiftOrderSubscriberConfig) {}

	getSymbolForMarketIndex(marketIndex: number) {
		const markets =
			this.config.velocityEnv === 'devnet'
				? DevnetPerpMarkets
				: MainnetPerpMarkets;
		return markets[marketIndex].symbol;
	}

	generateChallengeResponse(nonce: string) {
		const messageBytes = decodeUTF8(nonce);
		const signature = nacl.sign.detached(
			messageBytes,
			this.config.keypair.secretKey
		);
		const signatureBase64 = Buffer.from(signature).toString('base64');
		return signatureBase64;
	}

	handleAuthMessage(message: any) {
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
				await sleepMs(100);
			});
		}
	}

	async subscribe() {
		const ws = new WebSocket(
			this.config.endpoint +
				'?pubkey=' +
				this.config.keypair.publicKey.toBase58()
		);
		this.ws = ws;

		// Registered before `open`, never inside it. A socket that dies during the
		// handshake - server pod evicted, connection refused, reset - emits
		// `error` without ever emitting `open`. Node throws on an unhandled
		// 'error' event, so with the handlers nested inside `open` this process
		// died instead of retrying, which is exactly what a rolling swift
		// ws-server produces.
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
					const order = message['order'];
					if (typeof process.send === 'function') {
						process.send({
							type: 'signedMsgOrderParamsMessage',
							data: {
								type: 'signedMsgOrderParamsMessage',
								signedMsgOrder: order,
								marketIndex: order.market_index,
								uuid: this.convertUuidToNumber(order.uuid),
							},
						});
					}
				}
			});
		});
	}

	private startHeartbeatTimer() {
		if (this.heartbeatTimeout) {
			clearTimeout(this.heartbeatTimeout);
		}
		this.heartbeatTimeout = setTimeout(() => {
			console.warn(
				`No heartbeat received within ${this.heartbeatIntervalMs}ms, reconnecting...`
			);
			this.scheduleReconnect();
		}, this.heartbeatIntervalMs);
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

	/**
	 * Tear the current socket down and queue a fresh `subscribe()`.
	 *
	 * Idempotent per disconnect, and safe to call from any of the socket's
	 * failure paths.
	 */
	private scheduleReconnect() {
		if (this.reconnectTimeout) {
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
			this.subscribe().catch((error) => {
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

	private convertUuidToNumber(uuid: string): number {
		return uuid
			.split('')
			.reduce(
				(n, c) =>
					n * 64 +
					'_~0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ'.indexOf(
						c
					),
				0
			);
	}
}

async function main() {
	process.on('disconnect', () => process.exit());

	dotenv.config();

	const args = parseArgs(process.argv.slice(2));
	const velocityEnv = args['velocity-env'] ?? 'devnet';
	const marketIndexesStr = String(args['market-indexes']);
	const marketIndexes = marketIndexesStr.split(',').map(Number);

	const endpoint = process.env.ENDPOINT;
	const privateKey = process.env.KEEPER_PRIVATE_KEY;

	if (!endpoint || !privateKey) {
		throw new Error('ENDPOINT and KEEPER_PRIVATE_KEY must be provided');
	}

	const keypair = loadKeypair(privateKey);
	// SWIFT_WS_ENDPOINT lets a deployment point at its own swift ws-server (velocity
	// runs swift-ws-server-app in-cluster). Falls back to the public hosts otherwise.
	const swiftOrderSubscriberConfig: SwiftOrderSubscriberConfig = {
		velocityEnv,
		endpoint:
			process.env.SWIFT_WS_ENDPOINT ??
			(velocityEnv === 'devnet'
				? 'wss://swift.master.velocity.exchange/ws'
				: 'wss://swift.velocity.exchange/ws'),
		marketIndexes,
		keypair,
	};

	const swiftOrderSubscriber = new SwiftOrderSubscriber(
		swiftOrderSubscriberConfig
	);
	await swiftOrderSubscriber.subscribe();
}

main();
