---
'@velocity-exchange/vaults-sdk': patch
---

Grandfather the vaults fee policy per depositor, and settle the management fee before a rate change.

Follow-up to the earlier `vaults-fee-rebase-hardening` release, replacing its OtterSec #98 fix. That fix stamped `last_fee_update_ts` to the activation instant, which forfeited the manager's pre-activation accrual and did nothing for profit share or the hurdle rate. Both are priced off a depositor's high-water mark rather than a clock, so no timestamp can slice them.

**Management fee.** `apply_fee` now accrues the closing interval at the policy in force while it accrued, stamps `last_fee_update_ts`, and only then installs a matured update. `try_update_vault_fees` rejects an install on an unsettled vault, which makes `apply_fee` the single installer. The window between maturity and the first vault interaction is charged at the old rate. `managerUpdateFees` therefore settles through `apply_fee` instead of writing the new policy directly, so it takes a `velocityUser` account plus the spot market and its oracle in `remainingAccounts`. `getManagerUpdateFeesIx` passes them; protocol vaults still append `VaultProtocol`.

**Profit share and hurdle rate.** `VaultDepositor` and `TokenizedVaultDepositor` gain `profitShareAtBasis` and `hurdleRateAtBasis`, which record the policy in force when the high-water mark was last set. Gain above that mark is priced at `min(vault.profitShare, profitShareAtBasis)` and sheltered by `max(vault.hurdleRate, hurdleRateAtBasis)`. A raised profit share or a lowered hurdle therefore never prices gain that was earned before it, and a policy that is better for the depositor still applies at once. A realization that leaves no unpriced gain advances both stamps to the live policy, so a manager moves depositors onto a new policy with `applyProfitShare`, which realizes their gain at the old policy first.

Both new fields come from trailing padding, so `VaultDepositor` and `TokenizedVaultDepositor` keep their existing size. The IDL adds those fields and the `velocityUser` account. No error-code change.
