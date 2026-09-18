import { PublicKey } from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import { PositionDirection, UserClobOrder } from '../types';

// `WebSocket` is a global in a browser and in node 22. The SDK supports node
// 20, where it is not, so the `ws` package supplies it there.
let WebSocketImpl: typeof WebSocket;
if (typeof globalThis !== 'undefined' && (globalThis as any).WebSocket) {
	WebSocketImpl = (globalThis as any).WebSocket;
} else {
	WebSocketImpl = require('ws');
}

/** How long to wait before rebuilding a socket that closed. */
const RECONNECT_DELAY_MS = 1_000;

/**
 * A user's resting CLOB orders, read from the dlob-server.
 *
 * # Why this is not read from the chain
 *
 * A CLOB order has no `User.orders` slot, because the order lives on the book.
 * The account a client already subscribes to therefore cannot report what the
 * user rests. The book cannot report it either. The book answers about orders a
 * caller already names, and about the depth a taker of a given size reaches. A
 * user's order deeper than that size is absent from the answer. The server
 * indexes the book's account and serves the result.
 *
 * # What the feed costs
 *
 * The feed is cheap to hold open. Books are re-quoted continuously because
 * prices move continuously. A user's resting set changes only when that user
 * places an order, cancels one, or gets filled. The publisher therefore
 * republishes a user only when that user's own rows change, and a subscriber
 * that hears nothing still rests what it had.
 *
 * Every row carries the handle a cancel or a modify takes, so acting on an
 * order needs no further lookup.
 */
type UserSubscription = {
	/** The user's rows, per market. A market with no rows is absent. */
	byMarket: Map<number, UserClobOrder[]>;
	/**
	 * Markets a live message has already written. The seed fetch runs
	 * concurrently with the feed, so it must not overwrite fresher rows.
	 */
	live: Set<number>;
	listeners: Set<(orders: UserClobOrder[]) => void>;
};

export class UserClobOrdersClient {
	private readonly url: string;
	private readonly wsUrl?: string;
	private socket?: WebSocket;
	private reconnectTimer?: ReturnType<typeof setTimeout>;
	/** Set by `unsubscribeAll`, so a deliberate close does not reconnect. */
	private closed = false;
	private subscribers = new Map<string, UserSubscription>();

	/**
	 * @param url - dlob-server base URL, e.g. `https://dlob.velocity.trade`.
	 * @param wsUrl - Websocket URL for live updates. Omit it to leave only `fetch`.
	 */
	constructor(url: string, wsUrl?: string) {
		this.url = url.replace(/\/$/, '');
		this.wsUrl = wsUrl;
	}

	/**
	 * Every CLOB order the user is resting, across markets.
	 *
	 * @param userAccountPublicKey - The `User` account, not its authority.
	 * @param marketIndexes - Restrict to these perp markets. Omit it for every market.
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
	 * Watch one user's resting orders. The callback receives the user's whole
	 * current set across every market, and never a delta. A client can therefore
	 * replace what it holds instead of reconciling it. An empty array means the
	 * user rests nothing.
	 *
	 * The publisher writes one document per market, and republishes a market only
	 * when that market's rows change. This client holds the user's rows per
	 * market and re-emits the union, so an update in one market leaves the
	 * others standing. The set is seeded from `fetch`, because the feed alone
	 * never names a market that has not changed since the subscription opened.
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
		const existing = this.subscribers.get(user);
		const subscription: UserSubscription = existing ?? {
			byMarket: new Map(),
			live: new Set(),
			listeners: new Set(),
		};
		subscription.listeners.add(onUpdate);
		this.subscribers.set(user, subscription);
		this.closed = false;
		this.connect();
		this.send({ type: 'subscribe', channel: 'user_orders', user });
		if (!existing) {
			this.seed(userAccountPublicKey, subscription);
		} else {
			onUpdate(currentOrders(subscription));
		}

		return () => {
			subscription.listeners.delete(onUpdate);
			if (subscription.listeners.size === 0) {
				this.subscribers.delete(user);
				this.send({ type: 'unsubscribe', channel: 'user_orders', user });
			}
		};
	}

	public unsubscribeAll(): void {
		this.closed = true;
		if (this.reconnectTimer) {
			clearTimeout(this.reconnectTimer);
			this.reconnectTimer = undefined;
		}
		this.subscribers.clear();
		this.socket?.close();
		this.socket = undefined;
	}

	/**
	 * The user's current set, so the first callback is whole rather than
	 * whatever market publishes next. A market the feed has already written is
	 * left alone, because the feed is the fresher of the two.
	 */
	private seed(
		userAccountPublicKey: PublicKey,
		subscription: UserSubscription
	): void {
		this.fetch(userAccountPublicKey)
			.then((orders) => {
				if (!this.subscribers.has(userAccountPublicKey.toString())) {
					return;
				}
				const byMarket = new Map<number, UserClobOrder[]>();
				for (const order of orders) {
					const rows = byMarket.get(order.marketIndex) ?? [];
					rows.push(order);
					byMarket.set(order.marketIndex, rows);
				}
				for (const [marketIndex, rows] of byMarket) {
					if (!subscription.live.has(marketIndex)) {
						subscription.byMarket.set(marketIndex, rows);
					}
				}
				emit(subscription);
			})
			.catch(() => {
				// The feed still fills the set, one market at a time, as those
				// markets change. Emitting what is held keeps a subscriber that
				// rests nothing from waiting on a callback that never comes.
				emit(subscription);
			});
	}

	private connect(): void {
		if (this.socket) {
			return;
		}
		this.socket = new WebSocketImpl(this.wsUrl!);
		this.socket.onopen = () => {
			// A reconnect must repeat what it was watching. The server keeps no
			// record of a socket that closed.
			for (const user of this.subscribers.keys()) {
				this.send({ type: 'subscribe', channel: 'user_orders', user });
			}
		};
		this.socket.onclose = () => {
			this.socket = undefined;
			this.scheduleReconnect();
		};
		// A socket that errors closes after it, and `onclose` reconnects. The
		// handler exists so an error does not reach the process as an unhandled
		// event.
		this.socket.onerror = () => undefined;
		this.socket.onmessage = (event) => this.handleMessage(event.data);
	}

	/**
	 * Rebuild the socket after it closed with subscriptions still open.
	 * Without this a network drop ends every live subscription silently.
	 */
	private scheduleReconnect(): void {
		if (this.closed || this.reconnectTimer || this.subscribers.size === 0) {
			return;
		}
		this.reconnectTimer = setTimeout(() => {
			this.reconnectTimer = undefined;
			if (this.closed || this.subscribers.size === 0) {
				return;
			}
			this.connect();
		}, RECONNECT_DELAY_MS);
		// A node timer that outlives the work keeps the process alive.
		(this.reconnectTimer as any)?.unref?.();
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
		const subscription = this.subscribers.get(message?.user);
		if (!subscription || !Array.isArray(message.orders)) {
			return;
		}
		const orders = message.orders.map(deserializeUserClobOrder);
		// The document covers one market. Its index is on the document, and on
		// every row it carries. An empty document names the market it emptied
		// only on the document, so that is what this reads.
		const marketIndex = Number(message.marketIndex ?? orders[0]?.marketIndex);
		if (!Number.isInteger(marketIndex)) {
			return;
		}
		subscription.live.add(marketIndex);
		if (orders.length === 0) {
			subscription.byMarket.delete(marketIndex);
		} else {
			subscription.byMarket.set(marketIndex, orders);
		}
		emit(subscription);
	}

	private send(payload: Record<string, unknown>): void {
		if (this.socket?.readyState === WebSocketImpl.OPEN) {
			this.socket.send(JSON.stringify(payload));
		}
	}
}

/** Every market's rows as one list, which is what a subscriber is handed. */
function currentOrders(subscription: UserSubscription): UserClobOrder[] {
	const orders: UserClobOrder[] = [];
	for (const rows of subscription.byMarket.values()) {
		orders.push(...rows);
	}
	return orders;
}

function emit(subscription: UserSubscription): void {
	const orders = currentOrders(subscription);
	for (const listener of subscription.listeners) {
		listener(orders);
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
