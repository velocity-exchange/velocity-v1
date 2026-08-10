---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Rework the perp fee schedule. Fee tiers cut from 6 to 3 with new 30d-volume thresholds ($5M / $80M) and new defaults (4/3/2bps taker, flat -0.25bp maker rebate); `getUserFeeTier` mirrors the new thresholds, projects the rolling-volume decay to now (demotion tracks the live trailing window), and applies the new promotional tier floor. New onchain knobs with SDK/CLI surface: per-market additive taker-fee add-on (`PerpMarketAccount.takerFeeAddonTenthBps`, applied by `getMarketFees` before `feeAdjustment` and floored at zero; `AdminClient.updatePerpMarketTakerFeeAddon`, `velocity-admin fees set-taker-addon`) and the promo fee-tier floor (`StateAccount.promoFeeTier`, effective tier = max(volume tier, promo tier), 0 = off; `AdminClient.updatePromoFeeTier`, `velocity-admin fees set-promo-tier`).
