---
'@velocity-exchange/vaults-sdk': patch
---

Every vault instruction that snapshots NAV now CPIs velocity's
`update_spot_market_cumulative_interest` for the vault's denomination spot market **before** pricing
shares (OtterSec #136, #137). `Vault::calculate_equity` values the vault's velocity deposit off the
market's _stored_ `cumulative_deposit_interest`; only velocity may write that account, so the vaults
program has to refresh it by CPI. Previously `deposit` refreshed it only afterwards (as a side effect
of the deposit CPI), so an entrant minted shares against a stale index and captured part of the lender
interest the incumbents had already earned; the withdraw-request and cancel paths never refreshed at
all, leaking pre-request interest to the remaining shareholders and letting request-window interest
escape the cancellation share-forfeiture rule.

**ABI change — 19 instructions gained accounts (appended, nothing reordered or removed).** The new
accounts are `velocity_spot_market` (writable, PDA-pinned to `vault.spot_market_index`),
`velocity_oracle`, and — where the instruction did not already have them — `velocity_spot_market_vault`,
`velocity_state` and `velocity_program`:

`deposit`, `manager_deposit`, `withdraw`, `manager_withdraw`, `protocol_withdraw`, `force_withdraw`,
`request_withdraw`, `manager_request_withdraw`, `protocol_request_withdraw`, `cancel_request_withdraw`,
`manger_cancel_withdraw_request`, `protocol_cancel_withdraw_request`, `apply_rebase`,
`apply_rebase_tokenized_depositor`, `apply_profit_share`, `tokenize_shares`, `redeem_tokens`,
`transfer_vault_depositor_shares`, `liquidate`.

`VaultClient` fills all of them in, so callers that build instructions through the SDK need no change.
Anyone hand-rolling account lists must append them — and must pass `velocity_spot_market` /
`velocity_spot_market_vault` **explicitly**: their seeds derive from a field of the `vault` account and
Anchor's TypeScript PDA resolver does not resolve that, it silently substitutes the default pubkey.

Behavioral note: because the refresh runs inside velocity, these instructions now also inherit
velocity's `exchange_not_paused` / `spot_market_valid` / spot-market-vault-solvency checks. A paused
exchange or a delisted denomination market now blocks withdraw-request and cancel too, not just the
paths that already CPI'd `deposit`/`withdraw`.
