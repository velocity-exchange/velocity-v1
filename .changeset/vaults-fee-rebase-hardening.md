---
'@velocity-exchange/vaults-sdk': patch
---

Vaults fee / rebase / share-accounting hardening (eleven Medium audit fixes). Program-internal behavior changes; no account-layout, IDL, or error-code change (reuses existing `InvalidVaultUpdate` / `InvalidVaultRebase`).

- **#96** — a protocol vault paying only a management fee rebased with `&mut None`, leaving `protocol_profit_and_fee_shares` in the old denomination. The management-fee-only path now passes the real `vault_protocol` into `apply_rebase` so protocol shares scale by the same divisor.
- **#97** — the timelocked fee-update path copied raw management-fee/profit-share/hurdle values, bypassing the init bounds. New shared `validate_fee_policy` is enforced when queueing (`manager_update_fees`) and at maturity (`apply_fee`, against live protocol state), so no update can install a policy init couldn't create.
- **#98** — a matured fee update stamped no epoch boundary, so a raised rate retroactively priced the pre-activation interval. `try_update_vault_fees` now stamps `last_fee_update_ts` to the activation instant (rate epochs).
- **#99** — `apply_rebase` skipped `VaultProtocol.last_protocol_withdraw_request.shares`, stranding a protocol request after a public rebase. It's now rebased by the same divisor.
- **#100** — `redeem_tokens`' conservation check compared inconsistent share domains (protocol shares miscounted after the provider was consumed). It now snapshots the complete domain (including protocol shares) and keeps the `VaultProtocol` provider alive across the before/after.
- **#101** — `transfer`/`tokenize`/`redeem` passed `&mut None` for the fee update, skipping a matured update (a basis-reset escape). They now thread the `FeeUpdate` PDA and apply a matured update, mirroring deposit/withdraw.
- **#102** — the protocol-vault combined-fee branch derived the manager slice from the uncapped total then capped only the total, so a long idle interval drove the fee-share denominator negative and froze all public actions. The combined fee is now capped to `equity - 1` before splitting.
- **#104** — a positive profit-share fee that floored to zero shares advanced the high-water mark while transferring nothing. `apply_profit_share` now rounds a positive fee up to at least one share.
- **#105** — `redeem_tokens` left `TokenizedVaultDepositor.last_vault_shares` at the pre-transfer balance, permanently breaking future `tokenize_shares`. The checkpoint is now refreshed to the post-transfer balance.
- **#106** — the signerless `apply_rebase` could floor a small depositor's shares (or a pending request's shares) to zero, freezing the position. The public path (`apply_rebase_public`) now rejects a rebase that would zero a nonzero claim; the depositor can still rebase via a signed action.
- **#107** — lifecycle paths rebased the depositor, then `apply_fee` could rebase the vault again, leaving the depositor at a stale base and aborting `InvalidVaultRebase`. Depositors (and pending requests) are now re-synced after `apply_fee`.

Note: **#95** (late reward → manager shares) is closed at the root by PR #307's revenue-share sweep block (no reward can reach the vault-owned User); its zero-supply-repair defense-in-depth is not added here.
