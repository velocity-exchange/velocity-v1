---
'@velocity-exchange/sdk': patch
---

`User`'s VIP fee tier calculation now reads its volume thresholds from the exported
`VIP_FEE_TIER_ONE_VOLUME_QUOTE` and `VIP_FEE_TIER_TWO_VOLUME_QUOTE` constants instead of
duplicating the `5,000,000` / `80,000,000` quote values inline, so SDK consumers can import
and compare against the same thresholds the tier logic uses.
