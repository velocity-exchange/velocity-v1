---
'@velocity-exchange/sdk': minor
---

`fillPerpOrderV1`: a keeper fill's restable remainder migrates to the market's CLOB instead of resting in `User.orders`. A signed-message taker order cannot be IOC, so its leftover rests — and until now a keeper-driven fill had no CLOB accounts, so it rested on the DLOB. Pass `clobAccounts` (including `marketIndex`) to `buildFillPerpOrderInstruction` to select the route. Market-order remainders are deliberately not migrated; see `docs/taker-remainder-auction.md`.
