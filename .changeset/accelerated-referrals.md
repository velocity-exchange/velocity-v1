---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Referrer rewards split into a Standard and an Accelerated rate. Standard stays per-fee-tier
(`FeeTier.referrerRewardNumerator`, whose fresh default drops from 15% to 10%); Accelerated is
the fixed `ACCELERATED_REFERRER_REWARD_PERCENT` constant, independent of the tier. The referee
discount keeps reading the fee tier. `StateAccount.acceleratedReferralEnrollmentEnabled` and
`UserStatsAccount.acceleratedReferralStatus` mirror the new onchain fields, with the
`AcceleratedReferralStatus` flags, the `AcceleratedReferralStatusChange` action enum, and the
`AcceleratedReferralStatusChangedRecord` event (subscribed by default). `AdminClient` gains
`updateAcceleratedReferralEnrollment` and `updateUserAcceleratedReferralStatus`, wrapped by the
admin CLI as `exchange set-accelerated-referral-enrollment`, `user set-accelerated-referral`,
and `fees set-referral-rate`.

Fill instruction builders now append the referred taker's referrer `UserStats` (readonly) after
the taker's `RevenueShareEscrow`, which is what selects the Accelerated rate. The account is
optional onchain, so a client that omits it still fills at the Standard rate.
`getFillPerpOrderIx` takes a new trailing `takerReferrer` argument and `ReferrerMap` exposes
`getReferrerAuthority`; passing the referrer keeps the fill path free of an extra `UserStats`
fetch.
