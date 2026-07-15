---
'@velocity-exchange/sdk': patch
---

Pyth Lazer max-staleness check (High audit fix): `post_pyth_lazer_oracle_update` validated a signed Lazer message only for signer trust and a monotonic feed timestamp versus the cached account — never against `Clock::unix_timestamp` — while always stamping `posted_slot` to the current slot, from which all downstream staleness is derived. An authentic-but-stale or replayed message was thus treated as slot-fresh (and a strict-`<` monotonic check let the same message be re-posted each slot to peg a stale price as fresh). The handler now skips any feed whose message timestamp lags the wall clock by more than `PYTH_LAZER_MAX_STALENESS_SECONDS` (15s). Keepers must post Lazer updates promptly (legit updates are sub-second, so this is a no-op for them). No on-chain layout or IDL change.
