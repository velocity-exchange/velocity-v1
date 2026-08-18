---
'@velocity-exchange/sdk': minor
---

Perp fills charge a builder fee only while the taker meets initial margin. `User` gains
`isBuilderFeeCharged()`, and `User.calculatePerpTakerFee` and `VelocityClient.getMarketFees` consult
it, so a predicted fee for a taker below initial margin no longer includes the builder fee
(OtterSec #83).

A builder fee is an additive debit on the taker that the builder later claims into its own account,
and the taker is the party that approves the builder. The fee is therefore a transfer out of the
account, and it must clear the gate a withdrawal clears. A position-decreasing fill is otherwise
checked against maintenance margin alone, which lets an under-margined taker reduce the position in
slices and route out value the initial-margin gate holds in the account. The 1% cap on the fee rate
bounds one fill, not the sequence.

The program's gate reads the same oracle rules a withdrawal reads: strict (TWAP-bounded) prices,
no collateral for a deposit with an invalid oracle, and every liability oracle valid.
`isBuilderFeeCharged()` applies the strict prices but does not model oracle validity, so it is an
estimate — it can report `true` where the program waives the fee.

The program waives the fee, not the fill: the taker still closes the position and the builder is
paid nothing for that fill. A client that shows a builder fee before a close must read
`isBuilderFeeCharged()` to predict the charge for an under-margined account.
