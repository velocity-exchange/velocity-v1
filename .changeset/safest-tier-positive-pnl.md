---
'@velocity-exchange/sdk': patch
---

`User.getSafestTiers()` no longer counts a zero-base perp position whose only exposure is positive unsettled pnl as the user's safest perp liability, mirroring the program fix in `calculate_user_safest_position_tiers`: a positive pnl claim is a claim on the market's pnl pool, not a liability, and counting it made liquidator pre-flight tier checks skip `liquidatePerpPnlForDeposit` liquidations the program now accepts.
