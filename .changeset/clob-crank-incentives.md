---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

CLOB crank incentive loop (relay program-keeper mode): new `ClobCrankConditionsV0` account (relay condition block + keeper-payment reservoir), simulation-only `resolve_clob_crank_evict` / `resolve_clob_crank_remove_expired` resolvers, dual-mode `crank_clob_evict` / `crank_clob_remove_expired` (protocol-`User` filler selects an unsigned reservoir-lamport payout), an optional `crank_conditions` account on `place_clob_order`, and `withdraw_protocol_user_deposit` (FeeWithdraw hot role) in the IDL. `update_perp_market_clob_quoter` changed signature — it now takes `(keeperPaymentLamports, expireFallbackSlots)` plus the conditions account and stands the cranks up as part of the attach. SDK adds `ClobCrankConditionsV0Account` / `ProtocolUserWithdrawRecordV0` mirrors, `getClobCrankConditionsPublicKey`, and `adminClient.withdrawProtocolUserDeposit`; admin CLI gains `quoter set-market-clob` and `fees withdraw-protocol-user`.
