---
'@velocity-exchange/sdk': minor
---

MM oracle freshness fixes, mirroring the program:

- `getOracleValidity` resolves an unset (`oracleSlotDelayOverride < 0`) immediate-fill staleness
  threshold by price source, via a new optional `isMmSourcedPrice` parameter: an MM-oracle-sourced
  price gets `MM_ORACLE_MIN_SLOT_GAP` (the program will not accept MM-oracle writes closer together
  than that, so a tighter threshold is unsatisfiable), while an exchange-sourced price keeps the
  strict zero threshold. `0` still means "no immediate AMM fills on this market", and explicit
  positive thresholds are unchanged. `MMOraclePriceData` gains an `isMMSourcedPrice` flag populated
  by `getMMOracleDataForPerpMarket`.

- `updateMmOracleNative` / `getUpdateMmOracleNativeIx` take a new required `oracleSourceSlot`
  parameter (breaking): the slot the price was observed at, which the program now requires in the
  payload and checks against the landing slot (`MM_ORACLE_MAX_SOURCE_AGE_SLOTS`, exported), so a
  late-landing update cannot make an old observation read as fresh. The builder also validates its
  inputs (positive price fitting `i64`, `u64` sequence id and source slot); the program now
  hard-errors on any non-positive price, not just exact zero.

- Exports the `MM_ORACLE_MIN_SLOT_GAP` and `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` constants.
