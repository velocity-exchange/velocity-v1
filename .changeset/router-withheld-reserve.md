---
'@velocity-exchange/sdk': minor
---

A quoter now reports depth it could not reach, and the router keeps worse prices off it.

`quote_v0` returns `withheldPrice` and `withheldBase` alongside its ladder: liquidity a book holds at a better price than it quoted, passed over because the transaction does not carry the accounts of the user who owns it. A fill reserves that depth instead of handing it to a worse-priced source, so the taker keeps that base unfilled and it rests where the book's price can reach it. Immediate-or-cancel orders take the worse price, since their remainder cancels rather than rests.

`splitAcrossQuoters` takes an optional `reserve` for this, and `RouterQuoterBook` carries the `withheld` level a quoter reported. A client predicting a fill without it will predict liquidity the chain declines to route.

`ClobCancelSides` is `CancelSidesV0` in the IDL, matching the declaration in `quoter-spec` that every program on the wire now shares. Variants and their tags are unchanged, so nothing re-encodes.
