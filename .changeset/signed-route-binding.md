---
'@velocity-exchange/sdk': minor
---

Retail takers route, and a signed route binds the filler.

`placeAndTakePerpOrderV1` now fills through the router, so a taker signing their own transaction reaches CLOB and PropAMM liquidity rather than only the vAMM and the DLOB makers they passed. Its remaining accounts gain the quoter section.

`fillPerpOrder` takes a third argument, `signedRoute` — the `QuoterV0` entries the order's signer chose. It is checked against a digest stamped on the order, and every entry must be present in the transaction, so a filler can neither misreport the route nor drop a quoter the taker picked. `buildFillPerpOrderInstruction` accepts `signedRoute` (defaults to `[]`); `Order` gains `routeDigest`.
