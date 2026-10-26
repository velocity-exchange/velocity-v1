---
'@velocity-exchange/sdk': minor
'@velocity-exchange/vaults-sdk': patch
'@velocity-exchange/jit-proxy': patch
'@velocity-exchange/admin-cli': patch
---

Default commitment is now `finalized` instead of `confirmed`. This covers `DEFAULT_CONFIRMATION_OPTS`
(preflight and confirmation), the fallback commitment in the account subscribers, user maps, event
subscriber, blockhash and slot subscribers, one-shot fetches, and the gRPC and Laserstream
subscribers' default `commitmentLevel`. Under Alpenglow (mainnet since 2026-10-21) `confirmed` is
only signalled once a slot is finalized, so the two levels return the same slot on devnet and
mainnet, and Anza plans to remove `confirmed`. Callers that pass a commitment explicitly are
unaffected. On a local validator without Alpenglow, `finalized` still lags about 32 slots, so pass
`'confirmed'` there if tests need the old speed.
