---
'@velocity-exchange/sdk': patch
---

TxHandler now caches recent blockhashes by default (2s TTL), collapsing the per-build `getLatestBlockhash` RPC call into at most one fetch per window. Consumers building many transactions in quick succession (e.g. crankers) no longer hit RPC on every build. Set `txHandlerConfig.blockhashCachingEnabled: false` to restore the previous fetch-fresh-every-build behavior.
