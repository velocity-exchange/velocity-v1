---
'@velocity-exchange/dlob-server': patch
---

Stop a Redis blip from killing the publisher, and make the slot-diff kill-switch usable with HTTP health probes.

Redis writes on the publish path were fire-and-forget. When the client reconnected, `assertConnected` rejected a promise nobody handled and Node 24 terminated the process. Those writes now log and carry on, and every entrypoint installs an `unhandledRejection` guard so a future un-awaited call degrades the service instead of killing it.

The health check also changes in two ways so `httpGet /health` can replace the `tcpSocket` probes the mainnet publishers run today:

- A slot of 0 means no poll has returned yet and is reported as healthy, so a publisher starting against an empty book no longer crash-loops.
- The slot-diff kill-switch latches only after a market stays behind the oracle for `KILL_SWITCH_SUSTAIN_MS` (default 60s) instead of on a single sample, and a transient stall now clears itself rather than pinning the pod unhealthy forever.
