---
'@velocity-exchange/sdk': patch
---

A market order's unfilled remainder now rests on the CLOB from `placeAndTakePerpOrderV1`, not just from the keeper fill route.

It rests at the order's `auctionEndPrice` — the worst fill it already agreed to — and the CLOB `OrderRef` comes back as the transaction's return data, same as a limit remainder. Code that read a market order's leftover out of `User.orders` after a place-and-take should read the return data and treat it as a book order.
