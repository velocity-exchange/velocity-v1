---
'@velocity-exchange/sdk': minor
---

CLOB order surface: a client can place, cancel, modify and sweep resting book orders, and read back
what it is resting.

`VelocityClient` gains `placeClobOrder`, `cancelClobOrder`, `modifyClobOrder` and
`cancelAllClobOrders` with their `get*Ix` builders. A caller names a market and nothing else — the
book's program, its account and the PDAs resolve from `PerpMarket.clobQuoter`.

An order's id is minted from `User.next_order_id`, the same counter the account's DLOB orders draw
from, so a client names an order the same way wherever it rests. `placeClobOrder` returns the
order's `ClobOrderRefV0`, which is what a cancel or a modify takes; the book verifies it against the
order id and fails closed on a stale one. A modify keeps the id and loses the queue position.

`rejectIfCrossed` refuses a placement that would rest crossed with the opposite side — what a
post-only order asks for. It is not what makes the order a maker: a resting CLOB order settles at
its own price on the maker fee schedule either way.

`UserClobOrdersClient` reads a user's resting book orders from the dlob-server, over
`GET /userOrders` or the `user_orders` websocket channel. Book orders have no `User.orders` slot, so
this is what replaces `user.getOpenOrders()` for them; every row carries the order's handle.

`liquiditySource` — and so `L2Level['sources']` — gains `'clob'` and `'propamm'`, the two sources
the book publisher adds to each price level.
