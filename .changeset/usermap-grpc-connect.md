---
"@velocity-exchange/usermap-server": patch
---

Fix usermap publisher crash under gRPC. Call `client.connect()` before `subscribe()` (yellowstone-grpc 5.x requires an explicit dial, otherwise it throws "Client not connected. Call connect() first"). Also stop retrying the whole `main()` on failure — only the subscription is retried now, so a transient failure no longer re-runs `server.listen(:5001)` and crashes with `EADDRINUSE` (an unhandled `'error'` event that bypassed the retry). Fatal setup errors now exit the process for a clean pod restart.
