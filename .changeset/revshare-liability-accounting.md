---
'@velocity-exchange/sdk': patch
---

Revenue-share accounting correctness (two related High audit fixes). Both change observable on-chain behavior but touch no SDK code — the SDK does not reimplement either path.

- **#88** — the fill-time builder-order lookup matched an escrow row on `(sub_account_id, order_id)` only, ignoring the market. Order ids are per-subaccount and reused across markets, and a builder row can outlive its order, so a stale market-A row could attach to a same-id fill in market B: the market-B taker was charged a builder fee that accrued to — and was later swept from — market A's PnL pool. The fill path now uses `find_builder_order_index`, which additionally requires the row's `market_index`/`market_type` to equal the fill's, that it still be `Open` (not a `Completed` row whose id is stale), and that it not be a referral row.
- **#90** — `calculate_perp_market_amm_summary_stats` (the balance-sheet recompute an `AmmCrank` commits into `amm.total_fee_minus_distributions`) subtracted `net_user_pnl` and the pending protocol/IF counters but not accrued-but-unswept builder/referrer revenue share, so it counted that PnL-pool liability as retained AMM equity and inflated the funding/curve budget by the owed amount. It now also subtracts `PerpMarket.pending_revenue_share`, matching the reservation the fee sweep already applies.

No instruction, account-layout, IDL, or error-code change.
