---
'@velocity-exchange/sdk': patch
---

Mirror the program's fix to the immediate-fill oracle staleness threshold in `isOracleValid`.

`oracleSlotDelayOverride` bounds the oracle delay tolerated by immediate (JIT / auction-skipping)
AMM fills. A negative value means unset, and both the program and this mirror previously clamped it
to zero, requiring the price to have been written in the same slot as the fill. That is
unsatisfiable for an MM-oracle-sourced price, because the program will not accept MM-oracle writes
closer together than `MM_ORACLE_MIN_SLOT_GAP` slots. Unset now resolves to that gap instead of to
zero. `0` still means "no immediate AMM fills on this market", and explicit positive thresholds are
unchanged.

Also exports the new `MM_ORACLE_MIN_SLOT_GAP` constant.
