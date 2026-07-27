# @velocity-exchange/vaults-sdk

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
