---
'@velocity-exchange/sdk': patch
---

BlockhashSubscriber: derive the current block height from `getLatestBlockhashAndContext().value.lastValidBlockHeight - 150` instead of a paired `getBlockHeight` RPC call. This halves the per-poll RPC load of the subscriber (removing one `getBlockHeight` request per interval) with no change to `getLatestBlockHeight()` semantics, matching the Rust subscriber which never issued the extra call.
