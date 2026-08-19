---
'@velocity-exchange/vaults-sdk': patch
'@velocity-exchange/sdk': patch
---

**`force_delete_user` could never succeed.** The handler bound `State` with a shared `load()` at the
top and called `load_mut()` at the bottom. `Ref` implements `Drop`, so the first borrow lived to the
end of the scope and the shadowing `let` did not end it. Every call reverted with
`AccountBorrowFailed` — after the account's deposits had already moved to the keeper. Nothing
covered the success path, so the revert went unnoticed. The borrow is now released explicitly, and
`tests/velocity/equityFloorOracle.ts` covers the success path.

Every vault instruction that snapshots NAV now books the lending interest of **every** spot market
that prices the vault's equity, not just the denomination market.

`Vault::calculate_equity` delegates to velocity's `calculate_user_equity`, which converts every held
spot position through that position's own market's cumulative index. Refreshing one market left the
rest priced off whatever index the last unrelated crank had written. For a borrow the sign flips: a
stale `cumulative_borrow_interest` understates the liability, so NAV reads high and a withdrawer is
overpaid out of the vault rather than out of another depositor.

**New velocity instruction `refresh_spot_market_interest`.** It books up to sixteen spot markets in
one call. Accounts: `state`, plus the markets as writable accounts in remaining accounts. Argument:
`market_indexes: Vec<u16>`. Permissionless, like the single-market
`update_spot_market_cumulative_interest` crank beside it, which is unchanged and stays the crank
that keeps a spot market's oracle EMA fresh. SDK: `VelocityClient.refreshSpotMarketInterest` and
`refreshSpotMarketInterestIx`.

**The refresh passes no oracle.** `calculate_equity` gates the denomination oracle on
`is_oracle_valid_for_action(MarginCalc)`, whose `TooVolatile` arm measures the live price against
`last_oracle_price_twap`. The previous refresh advanced that TWAP toward the live price immediately
before the check read it.

**A delisted denomination market no longer blocks every vault instruction.** The refresh carries no
`spot_market_valid` guard, so the paths that move no tokens keep working: `request_withdraw`,
`cancel_withdraw_request`, `apply_rebase`, `apply_profit_share` and `liquidate`. Delisting is a
terminal state, so the previous behavior had no recovery at all. The token-moving paths
(`withdraw`, `force_withdraw`, `manager_withdraw`) still fail, because velocity's own withdraw
admits only `Active`, `ReduceOnly` and `Settlement` — that gate is unchanged and out of scope here.
Nothing about delisted markets changes: `deposit`, `force_delete_user` and `resolve_spot_bankruptcy`
already book interest on one.

**Isolated perp positions are covered too.** Such a position holds collateral that prices through
its perp market's quote spot market, which the position itself does not name. The market list picks
those up from the perp market accounts already present for the equity walk, and does that walk only
when the user holds an isolated position, so an ordinary vault pays nothing for it.

**ABI change — 20 instructions, accounts removed.** `velocity_spot_market` and `velocity_oracle` are
removed from all of them, and `velocity_spot_market_vault` from the thirteen that do not need it for
a deposit or withdraw CPI of their own. Each keeps `velocity_state` and `velocity_program`.
`manager_update_fees` joins the list, because installing a matured fee update snapshots NAV.
Affected: `deposit`, `manager_deposit`, `withdraw`, `manager_withdraw`, `protocol_withdraw`,
`force_withdraw`, `request_withdraw`, `manager_request_withdraw`, `protocol_request_withdraw`,
`cancel_withdraw_request`, `manager_cancel_withdraw_request`, `protocol_cancel_withdraw_request`,
`apply_rebase`, `apply_rebase_tokenized_depositor`, `apply_profit_share`, `tokenize_shares`,
`redeem_tokens`, `transfer_vault_depositor_shares`, `liquidate`, `manager_update_fees`.

`VaultClient` builds every affected instruction, so SDK callers need no change. Anyone hand-rolling
account lists must drop the removed accounts and must mark every spot market in the remaining
accounts writable — velocity fails the load with `SpotMarketWrongMutability` when it is asked to
refresh a market it was handed read-only.
