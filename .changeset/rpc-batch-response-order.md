---
'@velocity-exchange/sdk': minor
---

Stop trusting JSON-RPC batch response order in `fetchLogs` and `BulkAccountLoader`.

JSON-RPC lets a server return batch responses in any order and requires matching them by `id`. `connection._rpcBatchRequest` generates the ids internally and hands them back uncorrelated, so reading the array by position can attribute a response to the wrong request. Both batch call sites now go through a new `rpcBatchRequest` helper that sends the batch with ids it owns and returns the responses aligned to the requests, rejecting if any request goes unanswered.

`fetchTransactionLogs` blamed the wrong signature for a failed `getTransaction` and let the resume cursor advance past the one that actually failed, dropping its events for good. `BulkAccountLoader.loadChunk` matched each `getMultipleAccounts` response to a chunk by position; those results carry no pubkeys, so a reordered batch wrote account data under the wrong keys silently. `loadChunk` also read results back against the unfiltered chunk while requesting only accounts with live callbacks, shifting every account after an unsubscribed one onto the wrong data.

This is a minor rather than a patch because the batch transport moved from `Connection._rpcBatchRequest` to `Connection._rpcClient`: any test double or connection proxy that implements only the former needs updating.
