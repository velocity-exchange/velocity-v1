import { Keypair } from '@solana/web3.js';
import { BN } from '../isomorphic/anchor';
import nacl from 'tweetnacl';
import { decodeUTF8 } from 'tweetnacl-util';
import WebSocket from 'ws';

const SEND_INTERVAL = 500;
const MAX_BUFFERED_AMOUNT = 20 * 1024; // 20 KB as worst case scenario

type Quote = {
	bidPrice: BN | null;
	askPrice: BN | null;
	bidBaseAssetAmount: BN | null;
	askBaseAssetAmount: BN | null;
	marketIndex: number;
	isOracleOffset?: boolean;
};

type WsMessage = {
	channel: string;
	nonce?: string;
	message?: string;
};

export class IndicativeQuotesSender {
	private heartbeatTimeout: ReturnType<typeof setTimeout> | null = null;
	private sendQuotesInterval: ReturnType<typeof setTimeout> | null = null;

	private readonly heartbeatIntervalMs = 60000;
	/**
	 * Reconnect backoff bounds. Jittered, so that a fleet of senders knocked off
	 * the same server pod does not retry in lockstep.
	 */
	private readonly reconnectBaseDelayMs = 500;
	private readonly reconnectMaxDelayMs = 30000;
	private reconnectAttempts = 0;
	/**
	 * Handle of the pending reconnect, and the guard against scheduling a
	 * second one: a single disconnect normally emits both `close` and `error`,
	 * and the heartbeat timer can fire on top of them.
	 */
	private reconnectTimeout: ReturnType<typeof setTimeout> | null = null;
	private ws: WebSocket | null = null;
	private connected = false;

	private quotes: Map<number, Quote[]> = new Map();

	constructor(
		private endpoint: string,
		private keypair: Keypair
	) {}

	generateChallengeResponse(nonce: string): string {
		const messageBytes = decodeUTF8(nonce);
		const signature = nacl.sign.detached(messageBytes, this.keypair.secretKey);
		const signatureBase64 = Buffer.from(signature).toString('base64');
		return signatureBase64;
	}

	handleAuthMessage(message: WsMessage): void {
		if (message['channel'] === 'auth' && message['nonce'] != null) {
			const signatureBase64 = this.generateChallengeResponse(message['nonce']);
			this.ws?.send(
				JSON.stringify({
					pubkey: this.keypair.publicKey.toBase58(),
					signature: signatureBase64,
				})
			);
		}

		if (
			message['channel'] === 'auth' &&
			message['message']?.toLowerCase() === 'authenticated'
		) {
			this.connected = true;
			// Reset here rather than on `open`: a pod that is mid-shutdown still
			// completes the TCP handshake, so a successful auth is the first real
			// proof the connection is usable.
			this.reconnectAttempts = 0;
		}
	}

	async connect(): Promise<void> {
		const ws = new WebSocket(
			this.endpoint + '?pubkey=' + this.keypair.publicKey.toBase58()
		);
		this.ws = ws;

		// Registered before `open`, never inside it. A socket that dies during the
		// handshake - server pod evicted, connection refused, reset - emits
		// `error` without ever emitting `open`, and node throws on an unhandled
		// 'error' event, taking the whole process down instead of retrying.
		ws.on('error', (error: Error) => {
			console.error('Indicative quotes WebSocket error:', error);
			this.scheduleReconnect();
		});

		ws.on('close', (code: number, reason: Buffer) => {
			console.log(
				`Disconnected from indicative quotes server: code=${code} reason=${reason?.toString()}`
			);
			this.scheduleReconnect();
		});

		ws.on('unexpected-response', (_request, response) => {
			console.error(
				'Unexpected response from indicative quotes server:',
				response?.statusCode
			);
			this.scheduleReconnect();
		});

		ws.on('open', async () => {
			console.log('Connected to the server');

			ws.on('message', async (data: WebSocket.Data) => {
				let message: WsMessage;
				try {
					message = JSON.parse(data.toString());
				} catch (e) {
					console.warn('Failed to parse json message: ', data.toString());
					return;
				}
				this.startHeartbeatTimer();

				if (message['channel'] === 'auth') {
					this.handleAuthMessage(message);
				}

				if (
					message['channel'] === 'auth' &&
					message['message']?.toLowerCase() === 'authenticated'
				) {
					this.sendQuotesInterval = setInterval(() => {
						if (this.connected) {
							for (const [marketIndex, quotes] of this.quotes.entries()) {
								const message = {
									market_index: marketIndex,
									market_type: 'perp',
									quotes: quotes.map((quote) => {
										return {
											bid_price: quote.bidPrice
												? quote.bidPrice.toString()
												: null,
											ask_price: quote.askPrice
												? quote.askPrice.toString()
												: null,
											bid_size: quote.bidBaseAssetAmount
												? quote.bidBaseAssetAmount.toString()
												: null,
											ask_size: quote.askBaseAssetAmount
												? quote.askBaseAssetAmount.toString()
												: null,
											is_oracle_offset: quote.isOracleOffset,
										};
									}),
								};
								try {
									if (
										this.ws?.readyState === WebSocket.OPEN &&
										this.ws?.bufferedAmount < MAX_BUFFERED_AMOUNT
									) {
										this.ws.send(JSON.stringify(message));
									}
								} catch (err) {
									console.error('Error sending quote:', err);
								}
							}
						}
					}, SEND_INTERVAL);
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

	setQuote(newQuotes: Quote | Quote[]): void {
		if (!this.connected) {
			console.warn('Setting quote before connected to the server, ignoring');
		}
		const quotes = Array.isArray(newQuotes) ? newQuotes : [newQuotes];
		const newQuoteMap = new Map<number, Quote[]>();
		for (const quote of quotes) {
			if (
				quote.marketIndex == null ||
				quote.bidPrice == null ||
				quote.askPrice == null ||
				quote.bidBaseAssetAmount == null ||
				quote.askBaseAssetAmount == null
			) {
				console.warn(
					'Received incomplete quote, ignoring and deleting old quote',
					quote
				);
				if (quote.marketIndex != null) {
					this.quotes.delete(quote.marketIndex);
				}
				return;
			}
			if (!newQuoteMap.has(quote.marketIndex)) {
				newQuoteMap.set(quote.marketIndex, []);
			}
			newQuoteMap.get(quote.marketIndex)?.push(quote);
		}
		newQuoteMap.forEach((quotes, marketIndex) => {
			this.quotes.set(marketIndex, quotes);
		});
	}

	/**
	 * Tear the current socket down and queue a fresh `connect()`.
	 *
	 * Idempotent per disconnect, and safe to call from any of the socket's
	 * failure paths.
	 */
	private scheduleReconnect() {
		if (this.reconnectTimeout) {
			return;
		}

		// Listeners go first: `terminate()` on a live socket emits `close`, which
		// would otherwise re-enter this method.
		if (this.ws) {
			this.ws.removeAllListeners();
			// terminate() on a CONNECTING socket emits `error` on next tick; unhandled, it crashes.
			this.ws.on('error', () => {});
			this.ws.terminate();
			this.ws = null;
		}
		if (this.heartbeatTimeout) {
			clearTimeout(this.heartbeatTimeout);
			this.heartbeatTimeout = null;
		}
		if (this.sendQuotesInterval) {
			clearInterval(this.sendQuotesInterval);
			this.sendQuotesInterval = null;
		}
		this.connected = false;

		const delayMs = this.nextReconnectDelayMs();
		console.log(
			`Reconnecting to indicative quotes WebSocket in ${delayMs}ms...`
		);
		this.reconnectTimeout = setTimeout(() => {
			// Cleared before reconnecting, not in the `open` handler: the
			// replacement socket may itself fail before opening, and that failure
			// has to be able to schedule the next attempt.
			this.reconnectTimeout = null;
			this.connect().catch((error) => {
				console.error('Indicative quotes reconnect failed:', error);
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
