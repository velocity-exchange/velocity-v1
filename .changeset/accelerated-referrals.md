---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Referrer rewards split into a Standard and an Accelerated rate. Standard stays per-fee-tier
(`FeeTier.referrerRewardNumerator`, whose fresh default drops from 15% to 10%); Accelerated is
the fixed `ACCELERATED_REFERRER_REWARD_PERCENT` constant, independent of the tier. The referee
discount keeps reading the fee tier. `UserStatsAccount.acceleratedReferralStatus` mirrors the
new onchain field, with the `AcceleratedReferralStatus` flags, the
`AcceleratedReferralStatusChange` action enum, and the
`AcceleratedReferralStatusChangedRecord` event (subscribed by default). `AdminClient` gains
`updateUserAcceleratedReferralStatus`, wrapped by the admin CLI as
`user set-accelerated-referral` alongside `fees set-referral-rate`. Automatic enrollment is
gated by a beta-scoped program constant rather than a state field, so there is no client
surface to toggle it.

Fill instruction builders now append the referred taker's referrer `UserStats` (readonly) after
the taker's `RevenueShareEscrow`, which is what selects the Accelerated rate. The account is
optional onchain, so a client that omits it still fills at the Standard rate.
`getFillPerpOrderIx` takes a new trailing `takerReferrer` argument and `ReferrerMap` exposes
`getReferrerAuthority`; passing the referrer keeps the fill path free of an extra `UserStats`
fetch.
