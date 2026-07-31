---
'@velocity-exchange/sdk': minor
---

`PriorityFeeSubscriber` accepts an optional `fetchSolanaPriorityFee` override in its config, defaulting to the built-in `getRecentPrioritizationFees` RPC call. Lets an upstream route SOLANA fee sampling through its own cache/proxy instead of hitting the RPC directly.
