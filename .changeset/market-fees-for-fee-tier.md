---
'@velocity-exchange/sdk': minor
---

Add `getMarketFeesForFeeTier` and a fee-tier override on `VelocityClient.getMarketFees`, so a caller can price a market at a tier the account is not on.

`getMarketFees` computed the fee for whichever tier the account was on and applied the market surcharge, `feeAdjustment`, referee discount and builder fee on the way. There was no way to ask what the same market would charge at a different tier, which is what a UI needs to show the saving a fee promotion is making against someone's own volume tier. Deriving the second figure from the raw tier rate instead gives two numbers computed by different formulas, so the difference between them is not the saving.

The modifier pipeline is now the exported `getMarketFeesForFeeTier(feeTier, marketType, marketAccount?, { isReferee, builderFeeTenthBps })`, and `getMarketFees` resolves the tier and the account-derived inputs and delegates to it. Passing `feeTierOverride` (the new fifth argument) prices that tier with everything else unchanged; the referee discount comes from the tier being priced, as it does on chain. No behaviour change for existing calls.
