---
'@velocity-exchange/sdk': patch
---

Stop trusting JSON-RPC batch response order in `fetchLogs` and `BulkAccountLoader`.

JSON-RPC lets a server return batch responses in any order, and `connection._rpcBatchRequest` hands them back uncorrelated, so reading them by position can attribute a response to the wrong request.

`fetchTransactionLogs` blamed the wrong signature for a failed `getTransaction` and let the resume cursor advance past the one that actually failed, dropping its events for good. Errors are now attributed by elimination (a result carries its own signature, so whatever the batch returned no result for is what failed), and logs are returned in requested order rather than response order.

`BulkAccountLoader.loadChunk` matched each `getMultipleAccounts` response to a chunk by position. Those results carry no pubkeys, so a reordered batch wrote account data under the wrong keys silently. It now sends the batch with ids it owns and matches responses back by id.
