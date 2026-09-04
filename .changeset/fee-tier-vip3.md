---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Add the fourth perp fee tier, VIP 3, at $200M trailing-30d volume.

The program's tier ladder is now Regular / VIP 1 / VIP 2 / VIP 3 (indices 0-3, breakpoints $5M / $80M / $200M). The SDK exports the new breakpoint as `VIP_FEE_TIER_THREE_VOLUME_QUOTE` and includes it in `PERP_FEE_TIER_VOLUME_THRESHOLDS`, so `getPerpFeeTierIndex`, `User.getUserFeeTier` and `VelocityClient.getMarketFees` select tier 3 above $200M and `PERP_FEE_TIER_MAX_INDEX` is 3. A `promoFeeTier` of 3 puts every account on the top tier.

Admin CLI: `fees set-schedule` takes four tier fees (`<t0bp> <t1bp> <t2bp> <t3bp>`; tiers 4-9 mirror tier 3), `fees set-promo-tier` accepts 3, and `show fees` prints the VIP 3 row.
