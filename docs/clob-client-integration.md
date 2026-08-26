# Trading the CLOB from a client

What a client needs to place, cancel, modify and display orders that rest on a book. The design
behind these choices is [`clob-client-surface.md`](./clob-client-surface.md), whose **Still to do**
section lists what is not closed yet — read it before building on the feed. This is the working
reference.

The one thing to internalise: **a resting CLOB order has no `User.orders` slot.** `place_clob_order`
reserves the account's open-order aggregates and the order itself lives on the book. So
`user.getOpenOrders()` does not return it, and never will. Everything below follows from that.

## Order identity

A book order's id is minted from `User.next_order_id` — the same counter the account's DLOB orders
draw from. So:

- ids are unique per user across both venues, and a client keeps one `orderId: number` handle;
- the order-history stream names the order by that id, in the records it already carries;
- nothing has to map between velocity's ids and the book's.

The book keeps its own `u64` order id as well. It orders the book and breaks placement-slot ties,
and a client sees it only as half of the cancel handle below. Do not display it and do not key on
it.

**The handle.** Cancel and modify take a `ClobOrderRefV0`:

```ts
type ClobOrderRefV0 = { nodeIndex: number; orderId: BN }; // orderId here is the *book's*
```

It is a hint the book verifies against the order id and fails closed on: a stale one (the node was
freed or reused) reverts rather than cancelling whichever order took the slot. A client never has to
construct one — `placeClobOrder` returns it as transaction return data, and every row of the
user-orders feed carries it.

## Placing

```ts
await velocityClient.placeClobOrder({
  marketIndex,
  direction,          // PositionDirection.LONG rests as a bid
  price,              // PRICE_PRECISION
  baseAssetAmount,    // BASE_PRECISION
  maxTs,              // BN(0) = good-till-cancelled
  activationDelaySlots: null,   // null = the book's default speed bump
  rejectIfCrossed: false,       // true = post-only
});
```

Every account is resolved from `PerpMarket.clobQuoter`, so a client passes a market and nothing
else. `getPlaceClobOrderIx` gives the instruction if it is being bundled.

**The speed bump.** A new order is not matchable until its activation slot — by default one slot
after placement. This is what replaced JIT. Asking for less than the book's default requires the
transaction to be co-signed by the flow authority, which is what swift's `POST /attest` provides;
pass the instructions sysvar when doing so. A UI that shows an order as live the instant the
transaction confirms is showing something slightly ahead of the truth; `activationSlot` on the feed
row is the honest answer.

**`rejectIfCrossed` is not what makes the order a maker.** A resting CLOB order settles at its own
price on the maker fee schedule in every path that can consume it — a router taker takes it there,
and a crossed pair settles through the cross crank, which runs the protocol `User` as the taker on
both legs and pays it out of the spread. What the flag buys is the *placement* behaviour a maker
wants: refuse rather than rest through the other side. Wire it to the post-only toggle; do not
describe it as a fee setting.

## Cancelling and modifying

```ts
await velocityClient.cancelClobOrder({ marketIndex, orderRef });
await velocityClient.cancelAllClobOrders({ marketIndex, sides: CancelSidesV0.BOTH });
await velocityClient.modifyClobOrder({
  marketIndex,
  orderRef,
  price: newPrice,        // null keeps the resting price
  baseAssetAmount: null,  // null keeps the *remaining* size, not the original
  maxTs: null,            // null keeps the resting expiry
  activationDelaySlots: null,
  rejectIfCrossed: false,
});
```

**Cancel always works.** It is deliberately ungated on the quoter entry's active/approved flags: a
maker must be able to pull orders off a killed or delisted book.

**A modify keeps the order's id** and loses its queue position — the book has no in-place mutation,
so the order is removed and replaced. A UI can treat it as one order at new terms, which is what the
records say too. A `rejectIfCrossed` modify that is refused leaves the maker with **no order**,
because the original is already off the book when the replacement is refused.

**A sweep can stop early.** `cancelAllClobOrders` is capped per call. The instruction reports whether
it finished; when it did not, the user still has resting orders and repeating the call is safe.

## Reading a user's resting orders

This is what replaces `user.getOpenOrders()` for book orders. It comes from dlob-server, not from
the chain, because neither the `User` account nor the book can answer it — the book answers about
orders a caller already names, or about the depth a taker of a given size would reach, and an order
deeper than that size is simply not in the answer.

```ts
const client = new UserClobOrdersClient(dlobServerUrl, dlobServerWsUrl);

const orders = await client.fetch(userAccountPublicKey);         // every market
const unsubscribe = client.subscribe(userAccountPublicKey, (orders) => { ... });
```

Note it is keyed by the **`User` account**, not the authority — one subscription per sub-account.

Raw HTTP, for a non-SDK consumer:

```
GET /userOrders?userPubkey=<User PDA>[&marketIndexes=0,1]
→ { "user": "...", "slot": 123, "orders": [ <row>, ... ] }
```

Websocket, same channel machinery as the book feeds:

```
→ { "type": "subscribe", "channel": "user_orders", "user": "<User PDA>" }
← { "user": "...", "marketIndex": 0, "marketType": "perp",
    "slot": 123, "ts": 1730000000000, "orders": [ <row>, ... ] }
```

One row — the shape is pinned by `packages/sdk/tests/fixtures/userClobOrderRow.json`, which the
publisher asserts it emits and the SDK asserts it reads, so the two cannot drift:

```jsonc
{
  "orderId": 41,                  // velocity's — the handle to display and key on
  "nodeIndex": 7,                 // with clobOrderId, the cancel/modify handle
  "clobOrderId": "18446744073709551615",
  "marketIndex": 3,
  "direction": "long",            // or "short"
  "price": "99000000",            // PRICE_PRECISION, decimal string
  "baseAssetAmount": "500000000", // BASE_PRECISION remaining, not original
  "maxTs": "0",                   // 0 = good-till-cancelled
  "activationSlot": "1234",       // not matchable before this
  "placedSlot": "1233",
  "takerOrigin": false,
  "venue": "clob"
}
```

Three things to design around:

- **A callback carries the user's whole current set for that market**, not a delta. Replace what you
  hold; do not reconcile.
- **`baseAssetAmount` is what is left**, not what was placed. To show "filled 3 of 10" a client needs
  the original from its own record of the placement or from order history.
- **Silence means nothing changed.** The publisher republishes a user only when that user's own rows
  move, so a quiet feed is not a stale feed. The one write a client must not miss is the empty list
  that says the last order left.

The path from publisher tick to websocket has not been run end to end against a live Redis yet — the
two sides are unit-tested and the row shape is pinned across them, but the whole loop is unproven.
Expect to shake it out rather than to find it working first time.

## Merging the two venues in one list

Not every order can rest on a book. These stay in `User.orders` on the DLOB, and will for as long as
the DLOB lives:

| Order | Where it rests |
| --- | --- |
| Limit, fixed price | CLOB |
| Limit with an oracle offset | DLOB — no fixed price to rest at |
| Reduce-only limit | DLOB — enforced at placement against state the book does not hold |
| Trigger-limit, armed | `User.orders`, not matchable until it fires |
| Trigger-limit, fired | CLOB, with a shadow slot left behind (see below) |
| Trigger-market | `User.orders`, then the taker flow |
| Market / signed-msg taker | Not resting; an unfilled restable remainder migrates to the CLOB |

So the open-orders view is the union of `user.getOpenOrders()` and the feed. The row's `venue` field
is what tells them apart, and it decides which cancel path an order takes — `cancelOrder(orderId)`
for a DLOB order, `cancelClobOrder({ marketIndex, orderRef })` for a book one.

**Placed triggers are the trap.** When a trigger-limit fires, its live order goes onto the book and
its `User.orders` slot becomes a *shadow*: it keeps the trigger parameters and the book handle, and
it deliberately reads as untriggered so DLOB matching ignores it. Two consequences:

- the shadow will appear in `user.getOpenOrders()` output — filter on `OrderBitFlag.PlacedOnClob`
  (`64`) and do not render it as a second open order beside the feed row;
- `cancelOrder` on it fails with `OrderPlacedOnClob`. Cancelling a placed trigger goes through
  `cancelClobOrder`, which frees the shadow in the same instruction.

An evicted trigger re-arms rather than dying — the slot flips back to armed, edge-gated on the price
crossing the trigger again.

## Order states a UI has to name

Beyond open/filled/cancelled, a book order has states the DLOB never had:

- **Pending activation** — placed, not yet matchable (`activationSlot > current slot`).
- **Evicted** — the side hit its capacity threshold and the worst-priced order was cranked off.
  Nobody asked for it, and a placed trigger re-arms on it, so it should not read as "you cancelled
  this". The record's explanation is `ClobOrderEvicted`.
- **Expired** — `maxTs` passed and a crank reclaimed it. Explanation `OrderExpired`.
- **Culled** — a fill left a remainder below the market's minimum order size, so the book removed it
  with the fill. Explanation `ClobRemainderCulled`. It always accompanies a fill.
- **Force-cancelled** — the account failed its margin or equity floor and a keeper reclaimed its
  risk-increasing orders. Explanation `InsufficientFreeCollateral`.

## Order history

Velocity emits the records the history pipeline already reads: `OrderRecord` when an order starts
resting, `OrderActionRecord` with `OrderAction::Cancel` when it stops. A book order's `OrderRecord`
carries a synthesized `Order` with `bitFlags` including `PlacedOnClob`, which is how a reader tells
it from a DLOB order in the same id space.

Two gaps the client should know are gaps rather than bugs:

- **A maker's fill record names the order but not its size.** `makerOrderId` is set whenever the
  fill's balance change names exactly one order; `makerOrderBaseAssetAmount` and the cumulative
  totals are absent, because velocity holds no per-order state for a book order and a wrong size is
  worse than none. A reader has both from the place record.
- **`cancelAllClobOrders` emits no velocity record per order.** A sweep takes up to 128 orders and a
  record is 480 bytes, which no transaction's log budget holds. The per-order detail is on the
  book's own `OrdersCancelRecordV0`, which lists velocity's ids — so it joins the same stream, but
  it needs a decoder for the CLOB program's events, which does not exist in TypeScript yet. That is
  indexer work, tracked in the surface doc's **Still to do**.

## The order book display

Nothing to do. The book publisher writes the same Redis keys and the same document shape as before;
CLOB and PropAMM depth arrive as two new keys in each level's existing `sources` dictionary, and
`liquiditySource` in the SDK now includes `'clob'` and `'propamm'`. A client that only knows
`vamm`/`dlob` keeps working and simply attributes less of the book.

The L3 document gains a `source` per row and a `quotedSize` on the document. That size is
load-bearing for PropAMM rows: a PropAMM row is a quote at the size it was asked for, not standing
depth, and it has no queue position and cannot be cancelled. Only `clob` rows are orders.
