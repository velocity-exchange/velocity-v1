---
'@velocity-exchange/sdk': patch
---

Stop `EventSubscriber.fetchPreviousTx` from dropping logs it already fetched.

`fetchLogs` held its `earliestTx`/`mostRecentTx` resume cursors behind a failed `getTransaction` and returned `undefined` when neither end of the page was safe to resume from, which the backfill could not tell apart from "nothing to fetch". It now returns the fetched logs with only the unsafe cursor field left `undefined`, so `fetchPreviousTx` delivers that page's events before it stops and `PollingLogProvider` still skips the tick and retries with the cursor it already has.
