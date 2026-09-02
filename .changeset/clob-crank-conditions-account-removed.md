---
'@velocity-exchange/sdk': minor
---

The CLOB placement and taker routes drop their optional `crankConditions` account. The book
hosts its own crank wakes, so `placeAndTakePerpOrderV1`, `placeAndMakePerpOrderV1`,
`cancelOrdersV1`, `fillLegacyDlobOrder` and `placeSignedMsgTakerOrder` never read it; the
`clobAccounts` parameter shapes lose the field.
