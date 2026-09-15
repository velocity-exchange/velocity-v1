---
'@velocity-exchange/sdk': minor
---

JupiterClient now defaults to the Jupiter v2 swap API (`GET /swap/v2/build`): `getQuote` requires `userPublicKey` and returns a wallet-bound quote that carries its own instructions, so `getRouteInstructions` / `getSwapTransaction` make no further HTTP request and reject the quote if swapped by a different wallet.

Pass `apiVersion: 'v1'` (or `jupiterApiVersion: 'v1'` on `UnifiedSwapClient`) to keep the previous behaviour. That is also required for four things v2 does not support, each of which now throws rather than being silently ignored:

- `autoSlippage`
- `swapMode: 'ExactOut'` — `/swap/v2/build` is ExactIn-only, and sent an ExactOut amount it spends that amount as the *input*. `getJupiterSwapIx` / `getLpJupiterSwapIx` / `getJupiterLiquidateSpotWithSwapIxV6` all forward `swapMode`, so ExactOut callers on those paths must switch to `apiVersion: 'v1'`.
- `onlyDirectRoutes` — v2 accepts the param and still returns multi-hop routes.
- `computeUnitLimit` on `getSwapTransaction` is v2-only in the other direction: v1's `POST /swap` sizes the compute budget itself.

A custom `url` is assumed to already carry its version segment, so a client constructed with one now requests `<url>/build` instead of `<url>/quote`. Point it at a v2-capable base or pass `apiVersion: 'v1'`.
