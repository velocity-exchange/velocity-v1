---
'@velocity-exchange/sdk': minor
---

Liquidations via relay: `liquidatePerp`/`liquidatePerpWithFill` gained an optional trailing `crankConditions` account (the SDK emits a program-id placeholder automatically) and their `authority` is now a payout account in program-keeper mode — the signed keeper path is unchanged, and plain `liquidatePerp` rejects the protocol `User` (no protocol inventory). New IDL surface: `syncLiqConditions`, the simulation-only `resolveSyncLiqConditions` / `resolveLiquidatePerpWithFill`, and the `LiqConditionsV0` account.
