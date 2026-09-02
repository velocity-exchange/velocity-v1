---
'@velocity-exchange/sdk': minor
---

Apply the `State.promoFeeTier` floor everywhere the SDK selects a perp fee tier, and export the selection rule.

`VelocityClient.getMarketFees` read `feeTiers[0]` unconditionally when called without a `user`, so while a promo is active the generic schedule quoted the undiscounted entry tier and disagreed with the same call made with a user. Consumers that price a market before a wallet connects (a trade form's fee row, a market list) were showing a fee no account actually pays. `DLOB.getMakerRebate` had the same gap: it prices one rebate for the whole book, and a promo floor moves the tier that rebate comes from. It stays an approximation of what the program charges each maker individually, exact only while the tiers share a maker rebate, as they do today at a flat -0.25bp; the method now documents that condition.

The volume ladder and the promo floor now have one definition, `getPerpFeeTierIndex` (`math/fees`), which `User.getUserFeeTier`, `getMarketFees` and `DLOB.getMakerRebate` all select through. `PERP_FEE_TIER_VOLUME_THRESHOLDS`, `PERP_FEE_TIER_MAX_INDEX` and `User.getUserPerpFeeTierIndex` are exported alongside it, so surfaces that rank the tier itself (highlighting the active row of a fee schedule, progress toward the next tier) can stop mirroring the program's ladder by hand.
