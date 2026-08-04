---
'@velocity-exchange/sdk': minor
---

`placeAndMakePerpOrderV1`: a maker's unmatched remainder rests on the market's CLOB instead of being cancelled. `place_and_make` is IOC post-only, so v0 must throw away whatever the named taker order did not consume; v1 keeps it working on the book. Pass `clobAccounts` to `buildPlaceAndMakePerpOrderInstruction` to select the route, mirroring the take builder.
