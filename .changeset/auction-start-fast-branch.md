---
'@velocity-exchange/sdk': patch
---

`getTriggerAuctionStartPrice` now matches the program on both branches of `get_perp_baseline_start_price_offset`: past 50bps of fast/slow TWAP divergence it uses the 5-minute mark/oracle offset alone, and inside the band it blends with the AMM's cached `longSpread`/`shortSpread` instead of half of `baseSpread`.
