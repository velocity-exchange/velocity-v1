---
'@velocity-exchange/dlob-server': patch
---

Stop a Redis blip from killing the publisher, and make the slot-diff kill-switch usable with HTTP health probes.

Redis writes on the publish path were fire-and-forget. When the client reconnected, `assertConnected` rejected a promise nobody handled and Node 24 terminated the process. Those writes now log and carry on, and every entrypoint installs an `unhandledRejection` guard so a future un-awaited call degrades the service instead of killing it.

The health check also changes in two ways so `httpGet /health` can replace the `tcpSocket` probes the mainnet publishers run today:

- A slot of 0 means no poll has returned yet, and is reported as healthy for the first `STARTUP_GRACE_MS` (default 180s) so a publisher starting against an empty book no longer crash-loops. Past that window a process still reporting no slot is treated as wedged rather than starting, and `/health` returns unhealthy.
- The slot-diff kill-switch latches only after a market stays behind the oracle continuously for `KILL_SWITCH_SUSTAIN_MS` (default 60s) instead of on a single sample. A gap in sampling longer than `KILL_SWITCH_SAMPLE_GAP_MS` (default 10s) restarts the window, so elapsed time across a stalled publish loop cannot latch it on its own.
- A transient stall clears itself rather than pinning the pod unhealthy forever. Only `Restart` stays latched, and it now survives the status write that `/health` performs on every probe.

All three durations fall back to their defaults if the environment override is not a finite positive number.
