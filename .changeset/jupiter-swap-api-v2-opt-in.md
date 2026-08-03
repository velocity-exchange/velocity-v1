---
'@velocity-exchange/sdk': minor
---

JupiterClient: opt-in support for Jupiter Swap API v2 (`GET /swap/v2/build`) via `apiVersion: 'v2'` — one HTTP round trip for quote + instructions instead of `/quote` then `POST /swap` plus a transaction deserialization. v2 quotes are wallet-bound, so `getQuote` requires `userPublicKey` and rejects the quote if it is later swapped by a different wallet; `autoSlippage` is not supported on v2 (the API silently ignores it and returns zero slippage tolerance) and throws, directing callers to `apiVersion: 'v1'`. `UnifiedSwapClient` accepts a matching `jupiterApiVersion` option. The default remains `'v1'`, so nothing changes unless you opt in.
