---
'@velocity-exchange/sdk': patch
'@velocity-exchange/vaults-sdk': patch
---

Perp insurance fees swept into the quote revenue pool now remain attributable to their source
market until they reach the insurance fund vault. `PerpMarketAccount` gains
`insuranceFundRevenueReceivable`, and `SpotMarketAccount` gains the corresponding
`perpMarketIfRevenueReceivable` aggregate. Generic revenue settlement, spot bankruptcy and spot
withdrawal accounting reserve the aggregate. A source market bankruptcy consumes its own
receivable before shared insurance capital. The aggregate is not part of insurance fund NAV, so
creating a source claim does not admit value to IF stakers before normal revenue settlement.

The receivable records the token amount represented by the revenue pool after scaled balance
rounding. This keeps every recorded claim fully backed even when cumulative deposit interest is not
at unit precision.

`VelocityClient.settlePerpMarketIfRevenueToInsuranceFund` is a permissionless settlement path for
moving one market's receivable from the quote spot vault into the insurance fund vault. Generic
revenue and source market claims consume one shared allowance for each quote market settlement
period. A partial source settlement therefore leaves the unused allowance available to other
sources or generic revenue in the same period. Settlement clears the paid amount from the source
claim and aggregate while leaving the remaining claim source owned. Insurance fund NAV increases
only by the amount physically settled, apart from token precision rounding.

Spot bankruptcy socializes residual loss only across unreserved deposits. It rescales the revenue
pool after the interest haircut so source market claims remain fully backed and cannot be spent by
another spot market liability. A market whose settlement snapshot is still zero seeds the snapshot
without consuming its source claims, matching the generic revenue bootstrap behavior.

`PerpMarket` grows from 1304 to 1560 bytes and `SpotMarket` grows from 808 to 1064 bytes. Each gets
248 bytes of reserved tail space on `PerpMarket`. `SpotMarket` uses a second `u64` for the shared
settlement allowance and retains 240 bytes of reserved tail space. Existing accounts must be
extended with the account extension crank immediately after the program upgrade.

`PerpMarketAccount` and `SpotMarketAccount` also mirror the reserved `paddingFuture` tail that the
IDL declares, so the remaining space in each struct is visible to consumers.
