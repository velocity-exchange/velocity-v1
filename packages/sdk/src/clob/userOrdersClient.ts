import { PublicKey } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import { PositionDirection, UserClobOrder } from '../types';

/**
 * A user's resting CLOB orders, read from the dlob-server.
 *
 * # Why this is not read from the chain
 *
 * A CLOB order has no `User.orders` slot — the order lives on the book — so
 * the account a client already subscribes to cannot answer "what am I
 * resting". Nor can the book: it answers about orders a caller already names,
 * or about the depth a taker of some size would reach, and a user's order
 * deeper than that size is simply not in the answer. The server indexes the
 * book's account and serves the result.
 *
 * # What the feed costs
 *
 * Almost nothing to hold open. Books are re-quoted continuously because prices
 * move continuously, but a user's resting set changes only when that user
 * places, cancels or gets filled — so the publisher republishes a user only
 * when their own rows actually change, and a subscriber that hears nothing is
 * resting what it was.
 *
 * Every row carries the handle a cancel or a modify takes, so acting on an
 * order needs no further lookup.
 */
export class UserClobOrdersClient {
	private readonly url: string;
	private readonly wsUrl?: string;
	private socket?: WebSocket;
	private subscribers = new Map<
		string,
		Set<(orders: UserClobOrder[]) => void>
	>();

	/**
	 * @param url - dlob-server base URL, e.g. `https://dlob.velocity.trade`.
	 * @param wsUrl - Websocket URL for live updates. Omitted, only `fetch` works.
	 */
	constructor(url: string, wsUrl?: string) {
		this.url = url.replace(/\/$/, '');
		this.wsUrl = wsUrl;
	}

	/**
	 * Every CLOB order the user is resting, across markets.
	 *
	 * @param userAccountPublicKey - The `User` account, not its authority.
	 * @param marketIndexes - Restrict to these perp markets; every market when omitted.
	 */
	public async fetch(
		userAccountPublicKey: PublicKey,
		marketIndexes?: number[]
	): Promise<UserClobOrder[]> {
		const query = new URLSearchParams({
			userPubkey: userAccountPublicKey.toString(),
		});
		if (marketIndexes?.length) {
			query.set('marketIndexes', marketIndexes.join(','));
		}
		const response = await fetch(`${this.url}/userOrders?${query}`);
		if (!response.ok) {
			throw new Error(
				`userOrders request failed: ${response.status} ${response.statusText}`
			);
		}
		const body = await response.json();
		return (body.orders ?? []).map(deserializeUserClobOrder);
	}

	/**
	 * Watch one user's resting orders. The callback fires with the user's whole
	 * current set — not a delta — so a client can replace what it holds rather
	 * than reconcile. An empty array means the user rests nothing.
	 *
	 * @returns An unsubscribe function.
	 */
	public subscribe(
		userAccountPublicKey: PublicKey,
		onUpdate: (orders: UserClobOrder[]) => void
	): () => void {
		if (!this.wsUrl) {
			throw new Error('UserClobOrdersClient was built without a websocket URL');
		}
		const user = userAccountPublicKey.toString();
		const listeners = this.subscribers.get(user) ?? new Set();
		listeners.add(onUpdate);
		this.subscribers.set(user, listeners);
		this.connect();
		this.send({ type: 'subscribe', channel: 'user_orders', user });

		return () => {
			listeners.delete(onUpdate);
			if (listeners.size === 0) {
				this.subscribers.delete(user);
				this.send({ type: 'unsubscribe', channel: 'user_orders', user });
			}
		};
	}

	public unsubscribeAll(): void {
		this.subscribers.clear();
		this.socket?.close();
		this.socket = undefined;
	}

	private connect(): void {
		if (this.socket) {
			return;
		}
		this.socket = new WebSocket(this.wsUrl!);
		this.socket.onopen = () => {
			// A reconnect has to re-say what it was watching; the server holds
			// no memory of a socket that went away.
			for (const user of this.subscribers.keys()) {
				this.send({ type: 'subscribe', channel: 'user_orders', user });
			}
		};
		this.socket.onclose = () => {
			this.socket = undefined;
		};
		this.socket.onmessage = (event) => this.handleMessage(event.data);
	}

	private handleMessage(raw: unknown): void {
		if (typeof raw !== 'string') {
			return;
		}
		let message: any;
		try {
			message = JSON.parse(raw);
			// The server wraps the publisher's document as a string.
			if (typeof message.data === 'string') {
				message = JSON.parse(message.data);
			}
		} catch {
			return;
		}
		const listeners = this.subscribers.get(message?.user);
		if (!listeners || !Array.isArray(message.orders)) {
			return;
		}
		const orders = message.orders.map(deserializeUserClobOrder);
		for (const listener of listeners) {
			listener(orders);
		}
	}

	private send(payload: Record<string, unknown>): void {
		if (this.socket?.readyState === WebSocket.OPEN) {
			this.socket.send(JSON.stringify(payload));
		}
	}
}

/** The wire form (decimal strings in on-chain precision) as SDK types. */
export function deserializeUserClobOrder(row: any): UserClobOrder {
	return {
		orderId: Number(row.orderId),
		nodeIndex: Number(row.nodeIndex),
		clobOrderId: new BN(row.clobOrderId),
		marketIndex: Number(row.marketIndex),
		direction:
			row.direction === 'long'
				? PositionDirection.LONG
				: PositionDirection.SHORT,
		price: new BN(row.price),
		baseAssetAmount: new BN(row.baseAssetAmount),
		maxTs: new BN(row.maxTs),
		activationSlot: new BN(row.activationSlot),
		placedSlot: new BN(row.placedSlot),
		takerOrigin: !!row.takerOrigin,
		venue: 'clob',
	};
}
