---
'@velocity-exchange/sdk': patch
---

Apply strict (TWAP-bounded) spot pricing to swap sizing, matching the program.

`User.getMaxSwapAmount` built its in/out `StrictOraclePrice` with no TWAP argument, so both legs of a swap were valued at the live oracle price and strict mode was unconditionally off. `handle_end_swap` in the program builds both strict prices from each market's `historical_oracle_data.last_oracle_price_twap_5min` with `strict = true` before calling `select_margin_type_for_swap`, and the resulting margin check runs with `strict = true` whenever the swap worsens free collateral (`meets_withdraw_margin_requirement_swap`). The SDK therefore over-valued the bought asset (deposits should price at `min(oracle, twap5min)`) and under-valued any borrow the swap opens (liabilities should price at `max(oracle, twap5min)`), so a UI-computed max swap could exceed what the program allows. The base `getFreeCollateral()` the search starts from is already strict, so the mismatch also made the search's free-collateral delta inconsistent with its own starting point.

The free-collateral search now passes the relevant spot market's stored `lastOraclePriceTwap5Min` for both the in and out sides, unconditionally, as the handler does.

Strict pricing is deliberately confined to that sizing path. The `leverage` field `getMaxSwapAmount` returns, and everything `accountLeverageAfterSwap` returns, are deltas applied to the non-strict live-oracle baseline from `getLeverageComponents()`; pricing those deltas strictly would blend two price bases inside one figure and yield a valuation matching neither the oracle nor the TWAP. Both leverage readouts therefore stay on the live oracle price — unchanged from before this release, and comparable to the account leverage `getLeverage()` reports.
