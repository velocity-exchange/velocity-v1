---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': patch
---

`initialize_spot_market` and `initialize_perp_market` take an args struct

The two init instructions now take a single `InitializeSpotMarketArgs` /
`InitializePerpMarketArgs` struct in place of twenty and twenty-eight positional
arguments, and the `_v2` instructions added alongside them are gone. Instruction
names, discriminators and account lists are unchanged, so the break is the
argument encoding: a client built from an older IDL fails to deserialize.

The spot struct carries `minBorrowRate` and `maxTokenDeposits`, which the
positional form hardcoded to 0, and init accepts `curveUpdateIntensity` up to
200. Both removed a follow-up instruction from a listing.

SDK: `getInitializeSpotMarketIx(args, mint, oracle, marketIndex?)` and
`getInitializePerpMarketIx(args, priceOracle)` take the struct and return one
instruction. `initializeSpotMarketV2`, `getInitializeSpotMarketV2Ix`,
`initializePerpMarketV2` and `getInitializePerpMarketV2Ix` are removed.
`AdminClient.initializeSpotMarket` and `initializePerpMarket` keep their
positional signatures as a convenience form and fill the struct, so callers of
those are unaffected, but they pass 0 for the two new spot fields.
