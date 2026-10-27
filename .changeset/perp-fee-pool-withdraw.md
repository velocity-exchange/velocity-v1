---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Add `AdminClient.withdrawFromPerpMarketFeePool` and `getWithdrawFromPerpMarketFeePoolIx` for the
new cold-admin `withdraw_from_perp_market_fee_pool` instruction. It sends an exact amount from a
perp market's fee pool to the quote ATA of a recipient wallet the caller passes, and fails if the market,
oracle, bankruptcy reserve or AMM equity checks do not pass. Add the `PerpFeePoolWithdrawRecordV0`
event type. The admin CLI gains `perp-market withdraw-fee-pool <market> <amount> --recipient <pubkey>`, whose
`--dry-run` prints the pool and ledger values and simulates the instruction as the signer.
