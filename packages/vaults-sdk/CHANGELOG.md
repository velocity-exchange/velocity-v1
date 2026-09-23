# @velocity-exchange/vaults-sdk

## 0.1.32

### Patch Changes

- Updated dependencies [[`f720e70`](https://github.com/velocity-exchange/velocity-v1/commit/f720e70641a87a3fed42a5164cca12a55c1d4bef)]:
  - @velocity-exchange/sdk@0.25.0

## 0.1.31

### Patch Changes

- Updated dependencies [[`60a173f`](https://github.com/velocity-exchange/velocity-v1/commit/60a173fd58e70d68e7d670523129621df267e2c8), [`ab33ee9`](https://github.com/velocity-exchange/velocity-v1/commit/ab33ee907bd02907266853856718f8715a570f97)]:
  - @velocity-exchange/sdk@0.24.0

## 0.1.30

### Patch Changes

- Updated dependencies [[`a655327`](https://github.com/velocity-exchange/velocity-v1/commit/a655327291a5ef9238bae929f19d06158db512a4)]:
  - @velocity-exchange/sdk@0.23.1

## 0.1.29

### Patch Changes

- [#499](https://github.com/velocity-exchange/velocity-v1/pull/499) [`0afc72e`](https://github.com/velocity-exchange/velocity-v1/commit/0afc72e8c1506ce834c1f57b764a5b1a6cce6713) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Read Agave 4.2 / SIMD-0385 transaction v1 on `getTransaction` paths.

  Bump `@solana/web3.js` to 1.99.0 (read-only v1), `@triton-one/yellowstone-grpc` to 6.0.0, and `helius-laserstream` to 0.8.5. SDK `engines.node` is now `>=20.18.0`. For the packages in this release the change is limited to `maxSupportedTransactionVersion: 1` on RPC reads. The `solana-*` 4.2 crate bump (Rust wire decode / send) is a follow-up; until it lands the Rust event poller walks a transaction's logs when `decode()` cannot read the v1 wire format, and decodes payloads only while the Velocity program is the executing program.

  `fetchLogs` now logs `getTransaction` batch errors instead of discarding them, and holds its `earliestTx`/`mostRecentTx` resume cursors behind any signature it failed to fetch so those transactions are retried rather than skipped. It returns `undefined` when no signature in the batch is safe to resume from, so keep the current cursor and retry in that case. `EventSubscriber.fetchPreviousTx` counts only transactions it has not already decoded toward `maxTx`, so the page re-read after a failed fetch no longer shortens a backfill.

- Updated dependencies [[`0afc72e`](https://github.com/velocity-exchange/velocity-v1/commit/0afc72e8c1506ce834c1f57b764a5b1a6cce6713)]:
  - @velocity-exchange/sdk@0.23.0

## 0.1.28

### Patch Changes

- Updated dependencies [[`d2ea4ff`](https://github.com/velocity-exchange/velocity-v1/commit/d2ea4ffd940d4498bb4d11a7983de650f0f4d886)]:
  - @velocity-exchange/sdk@0.22.0

## 0.1.27

### Patch Changes

- Updated dependencies [[`eaa0664`](https://github.com/velocity-exchange/velocity-v1/commit/eaa06645a8ae137ce4e8ca606b2a65d3a24980cd), [`033237b`](https://github.com/velocity-exchange/velocity-v1/commit/033237bb975692bcce5bd540b3b015aba29463f3), [`e2b86d3`](https://github.com/velocity-exchange/velocity-v1/commit/e2b86d3ddba2c3e903ce70da835314a16ebed8e3)]:
  - @velocity-exchange/sdk@0.21.0

## 0.1.26

### Patch Changes

- Updated dependencies [[`6c183e7`](https://github.com/velocity-exchange/velocity-v1/commit/6c183e7a9d45f4987055032efcd8267651a231a4)]:
  - @velocity-exchange/sdk@0.20.0

## 0.1.25

### Patch Changes

- Updated dependencies [[`a720d5b`](https://github.com/velocity-exchange/velocity-v1/commit/a720d5b5abdc6258fd6a171282c7e46c5378be4e), [`7e8ff7c`](https://github.com/velocity-exchange/velocity-v1/commit/7e8ff7ca876aad9e985d6fa0a1fd060614df4b8d), [`106aaeb`](https://github.com/velocity-exchange/velocity-v1/commit/106aaeb44eb4a3d0a6f1ad5f0c767b6f1e5adebe), [`be89e60`](https://github.com/velocity-exchange/velocity-v1/commit/be89e60ce61d33653817e35bcd2c640fc9204c6f)]:
  - @velocity-exchange/sdk@0.19.0

## 0.1.24

### Patch Changes

- Updated dependencies [[`4b55e4e`](https://github.com/velocity-exchange/velocity-v1/commit/4b55e4e6c7ae161b42d86f12a61da9d2c1003141), [`560a198`](https://github.com/velocity-exchange/velocity-v1/commit/560a198fa8a0f22ba7f3dc7f926164f8ca91dff5), [`48b8529`](https://github.com/velocity-exchange/velocity-v1/commit/48b85296c60250316ae30e3f980237af97591ec4)]:
  - @velocity-exchange/sdk@0.18.0

## 0.1.23

### Patch Changes

- Updated dependencies [[`079d579`](https://github.com/velocity-exchange/velocity-v1/commit/079d579d4fac0f3402a4a9f8fb1aeccf21ac32ba)]:
  - @velocity-exchange/sdk@0.17.0

## 0.1.22

### Patch Changes

- [#446](https://github.com/velocity-exchange/velocity-v1/pull/446) [`823724e`](https://github.com/velocity-exchange/velocity-v1/commit/823724e4a8ea0d34b5a79883512eec9cb40b6123) Thanks [@0xahzam](https://github.com/0xahzam)! - Extend perp and spot market accounts with reserved padding and retire the unused spot fee pool field.

- Updated dependencies [[`ce01885`](https://github.com/velocity-exchange/velocity-v1/commit/ce0188563670520bfcddb689866e37c1fa19ed00), [`823724e`](https://github.com/velocity-exchange/velocity-v1/commit/823724e4a8ea0d34b5a79883512eec9cb40b6123)]:
  - @velocity-exchange/sdk@0.16.0

## 0.1.21

### Patch Changes

- Updated dependencies [[`d3824be`](https://github.com/velocity-exchange/velocity-v1/commit/d3824be0f2261e709477e8a1aceedcd11da842c5), [`fe0adbd`](https://github.com/velocity-exchange/velocity-v1/commit/fe0adbd72d77eefaada569292bca2f5baf1e1e58)]:
  - @velocity-exchange/sdk@0.15.0

## 0.1.20

### Patch Changes

- [#390](https://github.com/velocity-exchange/velocity-v1/pull/390) [`a6bffcb`](https://github.com/velocity-exchange/velocity-v1/commit/a6bffcb20a909f98552ef8f3adee8b5665e4257a) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `addInsuranceFundStake`'s `amount` is now an upper bound rather than the staked amount. IF shares are
  indivisible, so the program transfers only the portion of the request that prices to whole shares and
  leaves the remainder — always less than one share price — in the token account.

  This completes the fix for the zero-shares High finding. Rejecting only the zero-share case bounded
  the loss instead of removing it: a request worth 1.5 shares minted 1 and donated the other half to
  existing shareholders, and because the share price is set off a donation-inflatable vault balance, an
  attacker could pick that fraction. Pricing the deposit exactly (shares floored, their cost ceiled, so
  the fund never sells a share below price) caps the residual at one token unit and makes the donation
  unprofitable.

  `IFDepositMintsZeroShares` (6360) now means the request was below the price of a single share. Read
  the staked amount from `InsuranceFundStakeRecord.amount` instead of assuming it equals the requested
  amount; with `fromSubaccount`, any remainder lands in the wallet's token account rather than returning
  to the sub-account.

  `VaultClient.addToInsuranceFundStake` inherits the same rule with one difference: the vaults program
  stakes the whole balance of the vault's IF token account, so a remainder from an earlier add is folded
  in and the staked amount can exceed `amount`.

- [#366](https://github.com/velocity-exchange/velocity-v1/pull/366) [`4227e3e`](https://github.com/velocity-exchange/velocity-v1/commit/4227e3e6fe3805cd0986f81ca6cc4a7513c0c460) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `cancel_request_remove_insurance_fund_stake` now settles any already-due revenue into the insurance
  fund vault before pricing the cancel's forfeiture (OtterSec #141). Previously a staker could order
  their signed cancel ahead of an already-due signerless settle, make the restake price against a stale
  vault, burn no shares, and keep revenue the anti-free-option rule assigns to the remaining stakers.

  **ABI change — the account list is reordered, not just appended.** The instruction now takes `state`
  (prepended), plus `spot_market_vault`, `velocity_signer` and `token_program`, matching
  `request_remove_insurance_fund_stake`. Both SDKs pass them for you
  (`VelocityClient.cancelRequestRemoveInsuranceFundStake`, `VaultClient.getCancelRequestRemoveInsuranceFundStakeIx`),
  so SDK callers need no change; anyone building the instruction manually must rebuild the account list.
  The `vaults` program's CPI wrapper gained the matching accounts.

- [#425](https://github.com/velocity-exchange/velocity-v1/pull/425) [`e8a894c`](https://github.com/velocity-exchange/velocity-v1/commit/e8a894c90edd03814330206b8f666591be72a774) Thanks [@0xahzam](https://github.com/0xahzam)! - Correct the slot-duration scaling in the off-chain mirrors and the staged-switch setter.

  - `activeSlotDurationFromState` is now applied wherever the Rust SDK, the swift server, and
    the account-list builder previously read the raw `State.slotDurationMs` base field. That
    field lags a staged switch until the following gate is staged, so the mirrors sized oracle
    staleness windows, the signed-order age limit, and the auction band check off the
    pre-switch duration.
  - `update_state_slot_duration_ms` commits an already-effective promotion when the gate
    schedule is exhausted instead of reverting it, so the base field never stays a step behind
    the live value. After the final 200ms switch, operators finalize the raw base field with one
    additional `set-slot-duration-ms 200` transaction.
  - Reference-price-offset smoothing accrues its budget per elapsed millisecond instead of per
    whole 400ms period. Flooring to whole periods zeroed the budget for any crank gap under
    400ms, which pinned the step to the minimum and made convergence slower the more often a
    market was cranked.
  - `math/time.ts` imports `BN` from the isomorphic entry point, keeping Anchor out of the
    browser bundle.
  - The SDK's oracle staleness allowance is a wall-clock duration rather than a fixed five
    slots.
  - Three velocity/jit-proxy instructions and three vaults instructions now take velocity's
    `State` so their oracle windows match the rest of the protocol. Hand-built transactions
    must add the account; the SDKs and CLI fill it in.
  - `velocity-admin exchange set-slot-duration-ms` previews the live duration instead of the
    base field.
  - `pythLazerCranker`'s post ceiling is a fixed wall-clock interval again, so the post rate
    does not double at each gate.

- [#372](https://github.com/velocity-exchange/velocity-v1/pull/372) [`76a7f0d`](https://github.com/velocity-exchange/velocity-v1/commit/76a7f0d9532e57e845c90f41187ba2a5986327b1) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `tokenizeShares` now fails with `InvalidTokenization` while the tokenized depositor's pooled value
  is below its pooled cost basis.

  A tokenized depositor carries one cost basis for every holder of its mint, and the profit-share fee
  is collected by shrinking the pool's shares — so it dilutes every token equally regardless of who
  accrued the loss. Minting into an under-water pool therefore handed the newcomer a slice of the
  existing holders' loss shelter (OtterSec #140). Clients should surface the new failure and can
  preflight it by comparing the depositor's value against `netDeposits + cumulativeProfitShareAmount`.

  Redeeming is unaffected — existing holders can always exit — but a pool that has been under water
  stays closed to _new_ tokenizations until the vault recovers past the pooled high-water mark.

  No SDK API change.

- [#371](https://github.com/velocity-exchange/velocity-v1/pull/371) [`bf83c36`](https://github.com/velocity-exchange/velocity-v1/commit/bf83c368e9921accdeeeb926433c2490c0124c4a) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Every vault instruction that snapshots NAV now CPIs velocity's
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

- [#405](https://github.com/velocity-exchange/velocity-v1/pull/405) [`fabc75c`](https://github.com/velocity-exchange/velocity-v1/commit/fabc75ce6daeb7faf75909ceacaac8ffac257bad) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - **`force_delete_user` could never succeed.** The handler bound `State` with a shared `load()` at the
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

- [#403](https://github.com/velocity-exchange/velocity-v1/pull/403) [`dbe7f37`](https://github.com/velocity-exchange/velocity-v1/commit/dbe7f370993bb36cd2ec24748ece9a373b29a887) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Grandfather the vaults fee policy per depositor, and settle the management fee before a rate change.

  Follow-up to the earlier `vaults-fee-rebase-hardening` release, replacing its OtterSec #98 fix. That fix stamped `last_fee_update_ts` to the activation instant, which forfeited the manager's pre-activation accrual and did nothing for profit share or the hurdle rate. Both are priced off a depositor's high-water mark rather than a clock, so no timestamp can slice them.

  **Management fee.** `apply_fee` now accrues the closing interval at the policy in force while it accrued, stamps `last_fee_update_ts`, and only then installs a matured update. `try_update_vault_fees` rejects an install on an unsettled vault, which makes `apply_fee` the single installer. The window between maturity and the first vault interaction is charged at the old rate. `managerUpdateFees` therefore settles through `apply_fee` instead of writing the new policy directly, so it takes a `velocityUser` account plus the spot market and its oracle in `remainingAccounts`. `getManagerUpdateFeesIx` passes them; protocol vaults still append `VaultProtocol`.

  **Profit share and hurdle rate.** `VaultDepositor` and `TokenizedVaultDepositor` gain `profitShareAtBasis` and `hurdleRateAtBasis`, which record the policy in force when the high-water mark was last set. Gain above that mark is priced at `min(vault.profitShare, profitShareAtBasis)` and sheltered by `max(vault.hurdleRate, hurdleRateAtBasis)`. A raised profit share or a lowered hurdle therefore never prices gain that was earned before it, and a policy that is better for the depositor still applies at once. A realization that leaves no unpriced gain advances both stamps to the live policy, so a manager moves depositors onto a new policy with `applyProfitShare`, which realizes their gain at the old policy first.

  Both new fields come from trailing padding, so `VaultDepositor` and `TokenizedVaultDepositor` keep their existing size. The IDL adds those fields and the `velocityUser` account. No error-code change.

- Updated dependencies [[`4124e93`](https://github.com/velocity-exchange/velocity-v1/commit/4124e9313dd70610a705817570bd9e428c8dea85), [`74786b4`](https://github.com/velocity-exchange/velocity-v1/commit/74786b44c1009369c98d920b10d8f322a2214e26), [`1004b31`](https://github.com/velocity-exchange/velocity-v1/commit/1004b31a45f0da9cf8faed18c5c82f2351730c75), [`48e9301`](https://github.com/velocity-exchange/velocity-v1/commit/48e930147f8110454ef83f13f27a8ce8b921791a), [`01a7131`](https://github.com/velocity-exchange/velocity-v1/commit/01a71316b0327acd32be6e90686edd296e592af6), [`7ee2feb`](https://github.com/velocity-exchange/velocity-v1/commit/7ee2febf9c4bfe9cb0e7361828a1aad087216df7), [`94bb6ce`](https://github.com/velocity-exchange/velocity-v1/commit/94bb6ce94aad1981e4ee7910a85ab9ffbfe1d7c3), [`b7b5ae8`](https://github.com/velocity-exchange/velocity-v1/commit/b7b5ae80040b66651e6553d16354cbd075113cbb), [`06fac9e`](https://github.com/velocity-exchange/velocity-v1/commit/06fac9ed1584d51a6599dfb673977c0a4626c943), [`4e29bc0`](https://github.com/velocity-exchange/velocity-v1/commit/4e29bc0f131ad278450042e2554fd64bac4315ee), [`02078e6`](https://github.com/velocity-exchange/velocity-v1/commit/02078e625eb89c3fd5798af8d07693a21268a30e), [`77499bb`](https://github.com/velocity-exchange/velocity-v1/commit/77499bb3c0644730d5d48e6e3b331988cc5c2b02), [`fccd4f6`](https://github.com/velocity-exchange/velocity-v1/commit/fccd4f63d7522eca86d79aa8ec93092af2b63b7f), [`a6bffcb`](https://github.com/velocity-exchange/velocity-v1/commit/a6bffcb20a909f98552ef8f3adee8b5665e4257a), [`4227e3e`](https://github.com/velocity-exchange/velocity-v1/commit/4227e3e6fe3805cd0986f81ca6cc4a7513c0c460), [`4872b4f`](https://github.com/velocity-exchange/velocity-v1/commit/4872b4f49942c0f2ef830d10214ac26f46464c38), [`aaec40f`](https://github.com/velocity-exchange/velocity-v1/commit/aaec40fe81268dcc5922f8bbdb1301ea635a6dfd), [`b808fbb`](https://github.com/velocity-exchange/velocity-v1/commit/b808fbb90c4fea6bc597929203b25b6b9cf415d5), [`a6bd667`](https://github.com/velocity-exchange/velocity-v1/commit/a6bd667c28c3216ac213556d160aaea8e459191f), [`d3ef5e5`](https://github.com/velocity-exchange/velocity-v1/commit/d3ef5e5ed17e0ac51e8b8eb2fd039c381e2cff30), [`15db231`](https://github.com/velocity-exchange/velocity-v1/commit/15db231101dd2ac6ed3a94d63d0b41e5acecceb3), [`ede187b`](https://github.com/velocity-exchange/velocity-v1/commit/ede187be1060f4790f03f459733e0485096aaf69), [`1b81121`](https://github.com/velocity-exchange/velocity-v1/commit/1b8112143db861aab3507df64028911285425827), [`1a6af18`](https://github.com/velocity-exchange/velocity-v1/commit/1a6af1819be7822e56009e444d82c7a2fa84aed9), [`ae71278`](https://github.com/velocity-exchange/velocity-v1/commit/ae7127876ef98465ab53d611ee3447db73b224a2), [`4d0946b`](https://github.com/velocity-exchange/velocity-v1/commit/4d0946b70b336cf71cfcdca202a47dc9d8c81e05), [`e8a894c`](https://github.com/velocity-exchange/velocity-v1/commit/e8a894c90edd03814330206b8f666591be72a774), [`193c357`](https://github.com/velocity-exchange/velocity-v1/commit/193c35720365eefac9bfe9fbf1b241cf809029ff), [`dbea9aa`](https://github.com/velocity-exchange/velocity-v1/commit/dbea9aae45f27f8800cc80480443974ce68c031d), [`64301e1`](https://github.com/velocity-exchange/velocity-v1/commit/64301e1f19257152bf3c51174f4549dfbbdc9009), [`6e34ce3`](https://github.com/velocity-exchange/velocity-v1/commit/6e34ce3a14292eb4f6ceecfc67cde2e15590bd35), [`fabc75c`](https://github.com/velocity-exchange/velocity-v1/commit/fabc75ce6daeb7faf75909ceacaac8ffac257bad), [`98e787d`](https://github.com/velocity-exchange/velocity-v1/commit/98e787decb6153bacf6ec7f25e867cdcf217b413)]:
  - @velocity-exchange/sdk@0.14.0

## 0.1.19

### Patch Changes

- Updated dependencies [[`872edd6`](https://github.com/velocity-exchange/velocity-v1/commit/872edd66c5d94a01d6c694a06b03c4ed20c2054c)]:
  - @velocity-exchange/sdk@0.13.0

## 0.1.18

### Patch Changes

- Updated dependencies [[`c08211e`](https://github.com/velocity-exchange/velocity-v1/commit/c08211e4c106f57a1092238adb5c6d14895734f5)]:
  - @velocity-exchange/sdk@0.12.0

## 0.1.17

### Patch Changes

- [#310](https://github.com/velocity-exchange/velocity-v1/pull/310) [`2c71959`](https://github.com/velocity-exchange/velocity-v1/commit/2c719596fda2aaedf6b13988ec13ddcc6293596e) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Vaults fee / rebase / share-accounting hardening (eleven Medium audit fixes). Program-internal behavior changes; no account-layout, IDL, or error-code change (reuses existing `InvalidVaultUpdate` / `InvalidVaultRebase`).

  - **#96** — a protocol vault paying only a management fee rebased with `&mut None`, leaving `protocol_profit_and_fee_shares` in the old denomination. The management-fee-only path now passes the real `vault_protocol` into `apply_rebase` so protocol shares scale by the same divisor.
  - **#97** — the timelocked fee-update path copied raw management-fee/profit-share/hurdle values, bypassing the init bounds. New shared `validate_fee_policy` is enforced when queueing (`manager_update_fees`) and on both maturity paths (`apply_fee` and the pending branch of `manager_update_fees`, each against live protocol state), so no update can install a policy init couldn't create. For protocol vaults, `manager_update_fees` with a pending update now takes the `VaultProtocol` account in `remaining_accounts` (the SDK's `getManagerUpdateFeesIx` appends it).
  - **#98** — a matured fee update stamped no epoch boundary, so a raised rate retroactively priced the pre-activation interval. `try_update_vault_fees` now stamps `last_fee_update_ts` to the activation instant (rate epochs).
  - **#99** — `apply_rebase` skipped `VaultProtocol.last_protocol_withdraw_request.shares`, stranding a protocol request after a public rebase. It's now rebased by the same divisor.
  - **#100** — `redeem_tokens`' conservation check compared inconsistent share domains (protocol shares miscounted after the provider was consumed). It now snapshots the complete domain (including protocol shares) and keeps the `VaultProtocol` provider alive across the before/after.
  - **#101** — `transfer`/`tokenize`/`redeem` passed `&mut None` for the fee update, skipping a matured update (a basis-reset escape). They now thread the `FeeUpdate` PDA and apply a matured update, mirroring deposit/withdraw.
  - **#102** — the protocol-vault combined-fee branch derived the manager slice from the uncapped total then capped only the total, so a long idle interval drove the fee-share denominator negative and froze all public actions. The combined fee is now capped to `equity - 1` before splitting.
  - **#104** — a positive profit-share fee that floored to zero shares advanced the high-water mark while transferring nothing; rounding it up to a whole share instead would confiscate value far exceeding the fee (unbounded at a high share price, e.g. after a rebase-then-recovery cycle), letting a manager-cranked crystallization capture ~100% of a small depositor's profit. `apply_profit_share` now defers such a fee — transfers nothing and rolls back the high-water mark / `profit_share_fee_paid` advance — so it is charged later once accrued profit makes it worth at least one share.
  - **#105** — `redeem_tokens` left `TokenizedVaultDepositor.last_vault_shares` at the pre-transfer balance, permanently breaking future `tokenize_shares`. The checkpoint is now refreshed to the post-transfer balance.
  - **#106** — the signerless `apply_rebase` could floor a small depositor's shares (or a pending request's shares) to zero, freezing the position. The public path (`apply_rebase_public`) now rejects a rebase that would zero a nonzero claim; the depositor can still rebase via a signed action.
  - **#107** — lifecycle paths rebased the depositor, then `apply_fee` could rebase the vault again, leaving the depositor at a stale base and aborting `InvalidVaultRebase`. Depositors (and pending requests) are now re-synced after `apply_fee`.

  Note: **#95** (late reward → manager shares) is closed at the root by PR #307's revenue-share sweep block (no reward can reach the vault-owned User); its zero-supply-repair defense-in-depth is not added here.

- Updated dependencies [[`2a73aa7`](https://github.com/velocity-exchange/velocity-v1/commit/2a73aa716aed4f5910893c0ad9012bd598f72844), [`6e29daf`](https://github.com/velocity-exchange/velocity-v1/commit/6e29daf0ef781986dbe3bbf79f1c9bd2e25eb646)]:
  - @velocity-exchange/sdk@0.11.0

## 0.1.16

### Patch Changes

- Updated dependencies [[`63a580e`](https://github.com/velocity-exchange/velocity-v1/commit/63a580ea3c31a21fb8820fe75075d799cc8dc3da), [`2219857`](https://github.com/velocity-exchange/velocity-v1/commit/2219857615aeb4cd11b5ace8a203279797f41c88)]:
  - @velocity-exchange/sdk@0.10.0

## 0.1.15

### Patch Changes

- [#307](https://github.com/velocity-exchange/velocity-v1/pull/307) [`0ac1f73`](https://github.com/velocity-exchange/velocity-v1/commit/0ac1f730d0bdc5420ae0efd0ec12a1eb017fa542) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Vault share-pricing hardening (four High audit fixes, #91/#92/#93/#94).

  Builder/referral rewards owed to a vault PDA accrue in arbitrary third-party escrows and only enter the vault-owned Velocity User's equity via a permissionless revenue-share sweep — at a time an attacker controls. Because the vault can't see or enumerate those rewards (and no legitimate flow has a vault earn revenue share — a third party can name a vault PDA as their builder with no signature), the reward is blocked at the source instead:

  - **#91/#92/#93** — new `UserStatus::VaultOwned` bit (`UserStatus.VAULT_OWNED = 32`) marks a vault-owned User; the vaults program sets it at `initialize_vault` via a new CPI to the new velocity instruction `update_user_vault_owned` (authority-gated, set-only). `sweep_completed_revenue_share_for_market` now skips crediting a builder/referral reward to a vault-owned User — draining the liability counter and clearing the row without transferring, so the reward stays in the market's PnL pool and can never enter vault NAV. This closes late-entrant dilution (#91), the stranded pending-withdrawer (#92), and the reward-donation-burns-a-canceller's-claim vector (#93) at the root.
  - **#93 (defense-in-depth)** — `VaultDepositor::deposit` now rejects a positive deposit that mints zero shares (mirrors `request_withdraw`'s guard and the IF `IFDepositMintsZeroShares` path); `WithdrawRequest::calculate_shares_lost` rejects a cancel that would floor a positive claim's retained shares to zero purely because equity rose.
  - **#94** — `Vault::calculate_equity` now fetches the denomination-market oracle with `get_price_data_and_validity` and gates it with `VelocityAction::MarginCalc` (rejecting NonPositive/TooVolatile/TooUncertain/StaleForMargin), instead of a raw unchecked `get_price_data`. Previously a stale-high denomination oracle could shrink NAV and overmint shares whenever the vault held no denomination position (so the margin walk never validated that oracle).

  SDK: adds `UserStatus.VAULT_OWNED` and the `updateUserVaultOwned` instruction to the IDL. No account-layout change (`VaultOwned` reuses a spare `status` bit; existing accounts read 0). `update_user_vault_owned` is CPI-only (called by the vaults program at vault init), not a client-facing builder.

- Updated dependencies [[`d142320`](https://github.com/velocity-exchange/velocity-v1/commit/d14232017da7b09be1a71af8c5f6ee889ccac745), [`25da8e1`](https://github.com/velocity-exchange/velocity-v1/commit/25da8e1e39ccbb8310de32dd0da29041f2a93a0c), [`c16315e`](https://github.com/velocity-exchange/velocity-v1/commit/c16315e594120afdeb10f832c64914da01cbddcb), [`e34c623`](https://github.com/velocity-exchange/velocity-v1/commit/e34c6233afa1e04c7a4ff4a3f088405508f85790), [`edfc846`](https://github.com/velocity-exchange/velocity-v1/commit/edfc8469b5b4058f8767f1e48075b384fb809b4f), [`5fac99b`](https://github.com/velocity-exchange/velocity-v1/commit/5fac99bb93343d93c2a58e776fdba888588f89a2), [`5fac99b`](https://github.com/velocity-exchange/velocity-v1/commit/5fac99bb93343d93c2a58e776fdba888588f89a2), [`0ac1f73`](https://github.com/velocity-exchange/velocity-v1/commit/0ac1f730d0bdc5420ae0efd0ec12a1eb017fa542)]:
  - @velocity-exchange/sdk@0.9.0

## 0.1.14

### Patch Changes

- [#252](https://github.com/velocity-exchange/velocity-v1/pull/252) [`fc86321`](https://github.com/velocity-exchange/velocity-v1/commit/fc86321e8b8323e95e2d8385a82fdc69ca00075f) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - IF request-remove settle (High audit fix): `requestRemoveInsuranceFundStake` now settles already-due protocol revenue into the insurance-fund vault before freezing the staker's withdraw value, mirroring `addInsuranceFundStake`. Previously the exit value was frozen against the pre-settle vault, so a public revenue settle between request and remove shifted the exiting staker's rightful share of already-due revenue to the remaining stakers. The `requestRemoveInsuranceFundStake` instruction gains `state`, `spotMarketVault`, `velocitySigner`, and `tokenProgram` accounts (plus transfer-hook remaining accounts); the SDK builder supplies them automatically, but manual instruction construction must include them. No on-chain account layout change. Because the pre-freeze settle is skipped while withdraws are paused, `requestRemoveInsuranceFundStake` now rejects during a withdraw pause (`ExchangePaused` for the exchange-wide status, `MarketWithdrawPaused` for the market-scoped `SpotOperation::Withdraw` bit) so an accepted request always freezes a post-settle exit value; request again once the pause lifts. Canceling a pending request stays allowed during a pause.

  The vaults program mirrors this: its `request_remove_insurance_fund_stake` instruction gains the same velocity CPI accounts (`velocity_state`, `velocity_spot_market_vault`, `velocity_signer`, `token_program`) and `cancel_request_remove_insurance_fund_stake` is split onto its own unchanged accounts struct. `VaultClient.getRequestRemoveInsuranceFundStakeIx` supplies the new accounts automatically (the spot-market vault auto-resolves from its PDA seeds).

- Updated dependencies [[`ac29aa1`](https://github.com/velocity-exchange/velocity-v1/commit/ac29aa129c2cac14099a92b4a484bf20d2974863), [`88642ad`](https://github.com/velocity-exchange/velocity-v1/commit/88642ad1784af54c9ca0df271535b32a69cbe517), [`1f866f1`](https://github.com/velocity-exchange/velocity-v1/commit/1f866f1c44def526aeef8927fe14d563f3a8ed7b), [`ce18d22`](https://github.com/velocity-exchange/velocity-v1/commit/ce18d22926ae6a18b98df8a60bbd2696e0d10dbc), [`4179772`](https://github.com/velocity-exchange/velocity-v1/commit/417977294e10ffc152a0e5230019671000d2185b), [`88a3c64`](https://github.com/velocity-exchange/velocity-v1/commit/88a3c647f609a5fe357e414ee8b4b630fd2dc68c), [`e1f45a3`](https://github.com/velocity-exchange/velocity-v1/commit/e1f45a3d9e9e54e42987ed71bff9f6eaa6eac623), [`fc86321`](https://github.com/velocity-exchange/velocity-v1/commit/fc86321e8b8323e95e2d8385a82fdc69ca00075f), [`8f8b1ef`](https://github.com/velocity-exchange/velocity-v1/commit/8f8b1efcd9323369e13b0166ea158d78bd49b2ad), [`70ec53e`](https://github.com/velocity-exchange/velocity-v1/commit/70ec53e9390f8da2dd5aeee752c2bf3d285a2697), [`3b9a07b`](https://github.com/velocity-exchange/velocity-v1/commit/3b9a07bbe8f72145006ab45837a1fc21858d8c73), [`2994a81`](https://github.com/velocity-exchange/velocity-v1/commit/2994a813ab3de23c44d11157f040f690e3ddf8d6), [`a0e111a`](https://github.com/velocity-exchange/velocity-v1/commit/a0e111a237245d4011b33ff263a2cc9237a66265), [`0e0654c`](https://github.com/velocity-exchange/velocity-v1/commit/0e0654cafc5a95855caccd1f7741e74089cb6007), [`943095b`](https://github.com/velocity-exchange/velocity-v1/commit/943095b975dff10791b0e287df462d2ecf176aea), [`2d4a32f`](https://github.com/velocity-exchange/velocity-v1/commit/2d4a32f1c74b28b123ad4bd47f734c2843b090e4), [`8761596`](https://github.com/velocity-exchange/velocity-v1/commit/87615967d775c435fa777a5fa9396a80bbb0a62f), [`633c5f1`](https://github.com/velocity-exchange/velocity-v1/commit/633c5f17c56b186e505a3a7bfad2a450a1e9a82e)]:
  - @velocity-exchange/sdk@0.8.0

## 0.1.13

### Patch Changes

- Updated dependencies [[`cec4fcb`](https://github.com/velocity-exchange/velocity-v1/commit/cec4fcbf440645ad55dd41ec8410a250ca96fdef), [`f03beee`](https://github.com/velocity-exchange/velocity-v1/commit/f03beeecea6f3c9cc6c0ad7e828e9fab639e9a1b), [`c85d802`](https://github.com/velocity-exchange/velocity-v1/commit/c85d80284fb61dac7a08e47fe7b78340bf1213cc)]:
  - @velocity-exchange/sdk@0.7.0

## 0.1.12

### Patch Changes

- Updated dependencies [[`35f1480`](https://github.com/velocity-exchange/velocity-v1/commit/35f1480f2a60bad00a96f2e254cb5e9b210067b1), [`00ebcd2`](https://github.com/velocity-exchange/velocity-v1/commit/00ebcd2068b03db30652d352e2995417e08d9b35)]:
  - @velocity-exchange/sdk@0.6.1

## 0.1.11

### Patch Changes

- Updated dependencies [[`00decfd`](https://github.com/velocity-exchange/velocity-v1/commit/00decfd93fff5668779255288a0f61242be99d07), [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab), [`c288314`](https://github.com/velocity-exchange/velocity-v1/commit/c2883143d813a5c608354d64c3d1a5b825c4f398), [`61cbeb2`](https://github.com/velocity-exchange/velocity-v1/commit/61cbeb2f70dcd3f6d4fab1209962132dea9d60fe), [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab), [`155ed06`](https://github.com/velocity-exchange/velocity-v1/commit/155ed0618d7012b9305f4c84786c4d4e96d86be8)]:
  - @velocity-exchange/sdk@0.6.0

## 0.1.10

### Patch Changes

- Updated dependencies [[`9854dfa`](https://github.com/velocity-exchange/velocity-v1/commit/9854dfa1c915938fe08495568f262285ffb6c933), [`6d58632`](https://github.com/velocity-exchange/velocity-v1/commit/6d58632540814c739fc3848e4110c2a24547722b), [`3042967`](https://github.com/velocity-exchange/velocity-v1/commit/304296799e8d5b8a6525cb18cfd8398637c00521)]:
  - @velocity-exchange/sdk@0.5.0

## 0.1.9

### Patch Changes

- Updated dependencies [[`dff8a47`](https://github.com/velocity-exchange/velocity-v1/commit/dff8a4754f6b736fed330b2ab4fa5685db4f8159), [`900c07d`](https://github.com/velocity-exchange/velocity-v1/commit/900c07d9da7e106c82fbe65b3d92226d090bdee9), [`8df28ac`](https://github.com/velocity-exchange/velocity-v1/commit/8df28ac6d113760ae4a8cdff4ad438cb25efce2c), [`bafd699`](https://github.com/velocity-exchange/velocity-v1/commit/bafd6990f8322f232d2f0d17042beb0e9c567164)]:
  - @velocity-exchange/sdk@0.4.0

## 0.1.8

### Patch Changes

- Updated dependencies [[`b7d15b9`](https://github.com/velocity-exchange/velocity-v1/commit/b7d15b970a74d267aeaf20bb644d5344b9aadc61), [`2f6c64d`](https://github.com/velocity-exchange/velocity-v1/commit/2f6c64d54f1146d8e7f9ee4ab556929c6bf8b920), [`3f148f8`](https://github.com/velocity-exchange/velocity-v1/commit/3f148f8b477e4176e11e0660adb0e67dd5163d3b)]:
  - @velocity-exchange/sdk@0.3.0

## 0.1.7

### Patch Changes

- Updated dependencies [[`d3b58ab`](https://github.com/velocity-exchange/velocity-v1/commit/d3b58ab7e3ad33f0e6634ff87e9b150915b3aa13)]:
  - @velocity-exchange/sdk@0.2.6

## 0.1.6

### Patch Changes

- Updated dependencies [[`15073bc`](https://github.com/velocity-exchange/velocity-v1/commit/15073bc0b740b2d1cad471126a00368e72655bd5)]:
  - @velocity-exchange/sdk@0.2.5

## 0.1.5

### Patch Changes

- Updated dependencies [[`4f8e7aa`](https://github.com/velocity-exchange/velocity-v1/commit/4f8e7aaef0e35b190fc0b91cd29314d902d1ccab)]:
  - @velocity-exchange/sdk@0.2.4

## 0.1.4

### Patch Changes

- Updated dependencies [[`ae78769`](https://github.com/velocity-exchange/velocity-v1/commit/ae78769ef58355202c030435c2796ef045fe30a0)]:
  - @velocity-exchange/sdk@0.2.3

## 0.1.3

### Patch Changes

- Updated dependencies [[`022a949`](https://github.com/velocity-exchange/velocity-v1/commit/022a949cb1802171ca57a61260f86c8908f94f34)]:
  - @velocity-exchange/sdk@0.2.2

## 0.1.2

### Patch Changes

- [`4fd7462`](https://github.com/velocity-exchange/velocity-v1/commit/4fd7462bfa3c55e31e3457b1b65f519cf052a6fa) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Testing new changelog based package publishing flow

- Updated dependencies [[`4fd7462`](https://github.com/velocity-exchange/velocity-v1/commit/4fd7462bfa3c55e31e3457b1b65f519cf052a6fa)]:
  - @velocity-exchange/sdk@0.2.1
