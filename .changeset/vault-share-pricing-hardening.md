---
'@velocity-exchange/sdk': patch
'@velocity-exchange/vaults-sdk': patch
---

Vault share-pricing hardening (four High audit fixes, #91/#92/#93/#94).

Builder/referral rewards owed to a vault PDA accrue in arbitrary third-party escrows and only enter the vault-owned Velocity User's equity via a permissionless revenue-share sweep — at a time an attacker controls. Because the vault can't see or enumerate those rewards (and no legitimate flow has a vault earn revenue share — a third party can name a vault PDA as their builder with no signature), the reward is blocked at the source instead:

- **#91/#92/#93** — new `UserStatus::VaultOwned` bit (`UserStatus.VAULT_OWNED = 32`) marks a vault-owned User; the vaults program sets it at `initialize_vault` via a new CPI to the new velocity instruction `update_user_vault_owned` (authority-gated, set-only). `sweep_completed_revenue_share_for_market` now skips crediting a builder/referral reward to a vault-owned User — draining the liability counter and clearing the row without transferring, so the reward stays in the market's PnL pool and can never enter vault NAV. This closes late-entrant dilution (#91), the stranded pending-withdrawer (#92), and the reward-donation-burns-a-canceller's-claim vector (#93) at the root.
- **#93 (defense-in-depth)** — `VaultDepositor::deposit` now rejects a positive deposit that mints zero shares (mirrors `request_withdraw`'s guard and the IF `IFDepositMintsZeroShares` path); `WithdrawRequest::calculate_shares_lost` rejects a cancel that would floor a positive claim's retained shares to zero purely because equity rose.
- **#94** — `Vault::calculate_equity` now fetches the denomination-market oracle with `get_price_data_and_validity` and gates it with `VelocityAction::MarginCalc` (rejecting NonPositive/TooVolatile/TooUncertain/StaleForMargin), instead of a raw unchecked `get_price_data`. Previously a stale-high denomination oracle could shrink NAV and overmint shares whenever the vault held no denomination position (so the margin walk never validated that oracle).

SDK: adds `UserStatus.VAULT_OWNED` and the `updateUserVaultOwned` instruction to the IDL. No account-layout change (`VaultOwned` reuses a spare `status` bit; existing accounts read 0). `update_user_vault_owned` is CPI-only (called by the vaults program at vault init), not a client-facing builder.
