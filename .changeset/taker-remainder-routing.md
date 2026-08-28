---
'@velocity-exchange/sdk': minor
---

Signed-message orders route at placement and rest on the CLOB

A signed-message order is routed when it is placed, and whatever it cannot fill rests on the
market's book as a taker-origin remainder instead of on the DLOB. The activation-slot auction then
decides who fills it on price rather than on who lands a transaction first.
`place_and_make_signed_msg_perp_order` is removed: it existed only to match a signed-message order
already resting in `User.orders`.

The route digest moves off `Order` onto `SignedMsgUserOrders`, next to the CLOB order id the
remainder rests under, and widens from five bytes to eight. `getRouteDigest` mirrors the new width.
`Order.route_digest` is retired to padding, so `Order` stays 104 bytes.

A fired trigger rests taker-origin too. It came to trade, so a cross settles at the counterparty's
price rather than picking it off at its own. Its owner cannot cancel it inside the activation
window; liquidation force-cancel stays exempt and `max_ts` still bounds its life.

A partly filled remainder keeps its order id and its queue position. The book gained `fill_v0`, so
velocity reports the base it settled and the order shrinks in place rather than being cancelled and
re-placed at the back of its price level.

`L3RowV0` carries `node_index` and `placed_slot` and is 72 bytes rather than 64.
