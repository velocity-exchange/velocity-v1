---
'@velocity-exchange/sdk': minor
'@velocity-exchange/vaults-sdk': patch
'@velocity-exchange/admin-cli': patch
'@velocity-exchange/jit-proxy': patch
---

Read Agave 4.2 / SIMD-0385 transaction v1 on `getTransaction` paths.

Bump `@solana/web3.js` to 1.99.0 (read-only v1), `@triton-one/yellowstone-grpc` to 6.0.0, and `helius-laserstream` to 0.8.5. SDK `engines.node` is now `>=20.18.0`. For the packages in this release the change is limited to `maxSupportedTransactionVersion: 1` on RPC reads. The `solana-*` 4.2 crate bump (Rust wire decode / send) is a follow-up; until it lands the Rust event poller walks a transaction's logs when `decode()` cannot read the v1 wire format, and decodes payloads only while the Velocity program is the executing program.

`fetchLogs` now logs `getTransaction` batch errors instead of discarding them, and holds its `earliestTx`/`mostRecentTx` resume cursors behind any signature it failed to fetch so those transactions are retried rather than skipped. It returns `undefined` when no signature in the batch is safe to resume from, so keep the current cursor and retry in that case. `EventSubscriber.fetchPreviousTx` counts only transactions it has not already decoded toward `maxTx`, so the page re-read after a failed fetch no longer shortens a backfill.
