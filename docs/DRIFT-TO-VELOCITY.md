# Migrating from Drift Protocol v2 to Velocity

This repo is a fork of [`velocity-exchange/protocol-v2`](https://github.com/velocity-exchange/protocol-v2)
(fork point: `0ae3e3b1d`, SDK `v2.163.0-beta.0`, April 2026). The original Drift program is
**paused**; Velocity is an **entirely new program deployment** with a new program ID, a
reduced feature set, and a renamed SDK.

This document tracks everything that changed between the two repos from an integrator's
point of view. It reflects the current state of `master` — every PR referenced below is
**merged**.

---

## 1. At a glance

|                   | Drift (old)                                   | Velocity (new)                                                   |
| ----------------- | --------------------------------------------- | ---------------------------------------------------------------- |
| Program ID        | `dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH` | `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P` (devnet & mainnet) |
| npm package       | `@drift-labs/sdk` `2.163.0-beta.0`            | `@velocity-exchange/sdk` `0.2.x` (version reset)                 |
| Main client class | `DriftClient`                                 | `VelocityClient` — **no back-compat aliases**                    |
| Anchor            | 0.29.0                                        | 1.0 (`@anchor-lang/core@1.0.1`), new IDL format                  |
| IDL               | `drift.json`                                  | `velocity.json`                                                  |
| Rust crate        | `drift` (`programs/drift/`)                   | `velocity` (`programs/velocity/`)                                |
| Package manager   | yarn                                          | bun                                                              |

Because the program ID is new, **every PDA address changes** (seed strings are unchanged,
but the program ID input to derivation is different) and **no on-chain state carries
over** — users, markets, and balances start fresh on Velocity. Anchor account and
instruction discriminators are derived from names, not the program ID, so the
discriminators for surviving accounts/instructions (`User`, `PerpMarket`,
`place_perp_order`, …) are byte-identical to Drift's — but the account **layouts**
behind them changed (see §5), so old decoders must not be pointed at Velocity accounts.

---

## 2. Feature removals

These Drift features do not exist on Velocity. Integrations touching them must be removed
or reworked.

| Feature                                                       | Removed in | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ------------------------------------------------------------- | ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Spot DLOB trading**                                         | #6         | `place_spot_order`, `place_and_take_spot_order`, `place_and_make_spot_order`, `fill_spot_order` deleted. New error `SpotDlobTradingDisabled` (6350). Spot markets still exist for collateral/borrow-lend, but cannot be traded on the order book.                                                                                                                                                                                                                                                                                                                  |
| **External spot fulfillment (Serum / Phoenix / OpenBook v2)** | #36        | All `*_fulfillment_config` instructions and SDK subscribers (`serumSubscriber`, `phoenixSubscriber`, `openbookV2Subscriber`, fulfillment config maps) deleted.                                                                                                                                                                                                                                                                                                                                                                                                   |
| **Fuel (points/incentives)**                                  | #36        | All `*_fuel` instructions, `User.last_fuel_bonus_update_ts`, `PerpMarket.fuel_boost_*`, SDK `math/fuel`, `FuelSeasonRecord`, `FuelSweepRecord` deleted.                                                                                                                                                                                                                                                                                                                                                                                                          |
| **vAMM LP ("BAMM" LP shares)**                                | #36        | `PerpPosition.lp_shares` and friends removed; `LPRecord`/`LPAction` types deleted. Replaced by the new VLP module (§3).                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **Protected maker mode**                                      | #38        | All `protected_maker_*` instructions (`update_user_protected_maker_orders`, `update_perp_market_protected_maker_params`, `initialize_protected_maker_mode_config`, `update_protected_maker_mode_config`), the `ProtectedMakerModeConfig` on-chain account (PDA seed `protected_maker_mode_config`), the `UserStatus::ProtectedMakerOrders` bit, and SDK `math/protectedMakerParams` / `math/userStatus` / `getProtectedMakerModeConfigPublicKey` / `AdminClient.initializeProtectedMakerModeConfig` / `updateProtectedMakerModeConfig` / `VelocityClient.updateUserProtectedMakerOrders` deleted. `PerpMarket.protected_maker_*` fields replaced in place by padding; `InvalidProtectedMakerModeConfig` error preserved as a `@deprecated` stub. |
| **High leverage mode**                                        | #2, #47    | `enable_user_high_leverage_mode`, `disable_user_high_leverage_mode`, `initialize_high_leverage_mode_config`, `update_high_leverage_mode_config`, `update_perp_market_high_leverage_margin_ratio` deleted. `User.margin_mode` (a `MarginMode` enum) replaced in place by `padding_former_margin_mode: u8`; the `MarginMode` enum is deleted. `PerpMarket.high_leverage_margin_ratio_initial`/`_maintenance` replaced by `padding_former_hlm: [u8; 4]`. `InvalidHighLeverageModeConfig` / `CouldNotDeserializeHighLeverageModeConfig` renamed to `Deprecated*` stubs (numeric codes preserved). SDK `PollingHighLeverageModeConfigAccountSubscriber` / `WebSocketHighLeverageModeConfigAccountSubscriber` deleted. PR #47 removed the residual `HIGH_LEVERAGE_MIN_MARGIN_RATIO` constant. |
| **Prediction markets**                                        | #13        | `initialize_prediction_market` deleted; `ContractType::Prediction` renamed to `ContractType::DeprecatedPrediction` (discriminant preserved, not reused). `InvalidPredictionMarketOrder` renamed to `DepreciatedPredictionMarketOrder` (code 6284). The SDK `ContractType` no longer exposes a `PREDICTION` static.                                                                                                                                                                                                                                                |
| **Pyth pull/push (legacy)**                                   | #7         | Program instructions `initialize_pyth_pull_oracle`, `update_pyth_pull_oracle`, `post_pyth_pull_oracle_update_atomic`, `post_multi_pyth_pull_oracle_updates_atomic` deleted — keepers posting pull oracle updates must stop calling these. SDK `pythPullClient`, `pythOracleUtils` deleted. `AdminClient.initializePerpMarket` / `initializeSpotMarket` default `oracleSource` changed from `OracleSource.PYTH` to `OracleSource.PYTH_LAZER`. Pyth Lazer is the supported Pyth path. The pull `OracleSource` variants (`PythPull`, `Pyth1KPull`, `Pyth1MPull`, `PythStableCoinPull`) **keep their original names** (marked `@deprecated` in doc-comments only — they were _not_ renamed to `Deprecated*`). |
| **Switchboard oracles**                                       | #14        | Both classic and on-demand removed from SDK (`oracles/switchboardClient`, `oracles/switchboardOnDemandClient`); `OracleSource` discriminants preserved as `Deprecated*`.                                                                                                                                                                                                                                                                                                                                                                                          |
| **Legacy referrer-reward fee path**                           | #67        | Removed the legacy epoch-capped referrer-reward path routed through `UserStats`. Deleted: `UserStats.fees.total_referrer_reward`, `UserStats.fees.current_epoch_referrer_reward`, `UserStats.next_epoch_ts`; `FeeStructure.referrer_reward_epoch_upper_bound` → `padding` (offset/size preserved, IDL field name changed); the `FeatureBitFlags::BuilderReferral` bit and `State.builder_referral_enabled()`; the `MAX_REFERRER_REWARD_EPOCH_UPPER_BOUND` constant. `RevenueShareEscrowAccount` lost four fields (`referrerBoostExpireTs`, `referrerRewardOffset`, `refereeFeeNumeratorOffset`, `referrerBoostNumerator`; `reservedFixed` grew 17→24 bytes). SDK: `referrerInfo?: ReferrerInfo` removed from `placeAndMakePerpOrder`, `placeAndMakeSignedMsgPerpOrders`, `fillPerpOrder` and related ix-builders. Referrer rewards now flow exclusively through the escrow-based path (#73). |
| **Gov-token (DRIFT) stake fee discount**                      | #80        | Staking the governance token in the spot-market-15 insurance fund no longer grants a fee discount: perp fee tiers are now determined by 30-day volume only. Instructions `update_user_gov_token_insurance_stake` and `update_delegate_user_gov_token_insurance_stake` deleted; `UserStats.if_staked_gov_token_amount` replaced by padding. Spot market 15 has no special treatment anymore (the gov-specific IF revenue-settle APR cap was removed; the general cap applies).                                                                                       |
| **Protocol-owned insurance fund shares & IF rebalance**       | #75        | The IF is 100% staker-owned. Deleted: `admin_withdraw_from_insurance_fund_vault`, `transfer_protocol_if_shares_to_revenue_pool`, `begin/end_insurance_fund_swap`, `initialize/update_if_rebalance_config`, `initialize/update_protocol_if_shares_transfer_config`, `deposit_into_insurance_fund_stake`, the `IfRebalanceConfig` / `ProtocolIfSharesTransferConfig` accounts, and `HotRole::IfRebalance` (+ `State.hot_if_rebalance`). `InsuranceFund.total_factor`/`user_factor` are replaced by a single `if_fee_factor` (lending-yield carveout to stakers). Protocol revenue no longer flows through IF shares at all. |

## 3. Feature additions

| Feature                                          | Added in       | Integrator impact                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ------------------------------------------------ | -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **VLP module** (`programs/velocity/src/vlp/`)    | #65, #66       | New AMM + hedge architecture. `PerpMarket` gains `hedge_config: HedgeConfig` and `market_stats: MarketStats`. The five flat LP-pool config fields on `PerpMarketAccount` (`lpPoolId`, `lpStatus`, `lpPausedOperations`, `lpFeeTransferScalar`, `lpExchangeFeeExcluscionScalar`) were replaced by a single `hedgeConfig` sub-object (`{ poolId, status, pausedOperations, exchangeFeeExclusionScalar, feeTransferScalar }`).                                                                                                                                                                                                                                                                                                                                                                       |
| **Tiered admin keys**                            | #36, #63       | `State.admin` replaced by cold/warm/hot key model (`cold_admin`, `warm_admin`, `hot_*` keys). Anyone reading `State.admin` directly must update. In #76, `update_spot_market_oracle` and `update_spot_market_expiry` were promoted from warm-admin to cold-admin-only (swapping an oracle re-prices the withdraw-guard notional cap).                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **Native fast-path entrypoint**                  | fork; #63      | Keeper instructions with discriminator `[0xFF, 0xFF, 0xFF, 0xFF, opcode]` bypass Anchor dispatch (e.g. MM oracle update = opcode 0). These do not appear in the IDL. Inherited from the Drift fork; #63 refactored the native admin handlers to a zero-copy struct cast. The handlers now re-establish Anchor's account guarantees before trusting any byte: `State` and `PerpMarket` are loaded via `AccountLoader` (program ownership + discriminator) and the slot comes from the `Clock` sysvar (see the `native-path` change-log row).                                                                                                                                                                                                                                                                |
| **`transfer_deposit_by_delegate` + `update_user_allow_delegate_transfer`** | #45 | Delegates can transfer spot deposits between subaccounts once the authority opts in. Before a delegate can call `transfer_deposit_by_delegate`, the owner must call `update_user_allow_delegate_transfer(true)` to set the `AllowDelegateTransfer` bit in `UserStats.delegate_permissions`. SDK: `VelocityClient.updateUserAllowDelegateTransfer(...)` and `transferDepositByDelegate(...)`. `UserStatsAccount` gains a `delegatePermissions: number` field (1 byte carved from trailing padding; size and all other offsets unchanged at 240 bytes).                                                                                                                                                                                                                                              |
| **`transfer_fee_and_pnl_pool`**                  | #1             | Admin instruction to rebalance tokens between a perp market's AMM fee pool and PnL pool (same or cross-market). Requires the **warm admin** key. SDK: `AdminClient.transferFeeAndPnlPool(perpMarketIndexWithFeePool, perpMarketIndexWithPnlPool, amount, direction)` and `getTransferFeeAndPnlPoolIx(...)`. Direction via the new `TransferFeeAndPnlPoolDirection` export (`.FEE_TO_PNL_POOL` / `.PNL_TO_FEE_POOL`). Emits a `TransferFeeAndPnlPoolRecord` event (`ts`, `slot`, `perp_market_index_with_fee_pool`, `perp_market_index_with_pnl_pool`, `direction`, `amount`).                                                                                                                                                                                                                       |
| **Funding rate clamp + floor increase**          | #12            | Funding floor raised from 7.3% to 10.95% annualized (`FUNDING_RATE_OFFSET_DENOMINATOR` 5000 → 3333). Dead-zone clamp added: when `\|mark_twap − oracle_twap\| ≤ 0.05%` of oracle price (`FUNDING_RATE_CLAMP_DENOMINATOR = 2000`), the funding premium is suppressed to the offset-only floor value. Changes funding dynamics vs Drift for low-divergence markets. (Superseded by the per-market continuous dead zone in #94.)                                                                                                                                                                                                                                                                                                                                                                    |
| **Continuous funding dead zone (per-market)**    | #94            | Replaces #12's global hard cutoff with a per-market continuous ramp. Two new `AMM` fields (occupying the 8 bytes previously `_padding_funding_twap`): `funding_clamp_threshold: u32` (noise band, BPS_PRECISION; default 5 bps) and `funding_ramp_slope: u32` (PERCENTAGE_PRECISION; default 1.0×). New admin instruction `update_perp_market_funding_dead_zone(funding_clamp_threshold, funding_ramp_slope)`; SDK `AdminClient.updatePerpMarketFundingDeadZone(...)` / `getUpdatePerpMarketFundingDeadZoneIx(...)`. `PerpMarketAccount` gains `fundingClampThreshold` / `fundingRampSlope` (replacing `paddingFundingTwap`). `PerpMarket` size unchanged (1304).                                                                                                                                |
| **MM oracle validation**                         | #60            | Slot-monotonicity, minimum 2-slot gap, and a 1% per-write step cap added to the existing MM oracle native handler.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| **Special user status**                          | #17            | New `User.special_user_status: u8` bitmask field (replaces 1 byte of padding; account size unchanged); new `SpecialUserStatus` SDK enum (`VammHedger = 1`). Two new instructions: `update_special_user_status(status)` (admin/hot-wallet — `AdminClient.updateSpecialUserStatus` / `getUpdateSpecialUserStatusIx`) and `special_transfer_perp_position_to_vamm(market_index, amount)` (user-callable, authority signs, only when `special_user_status == VammHedger` — `VelocityClient.specialTransferPerpPositionToVamm` / `getSpecialTransferPerpPositionToVammIx`). New error `InvalidTransferPerpPosition` (6312).                                                                                                                                                                            |
| **Builder codes**                                | #68            | Optional `builder_idx` / `builder_fee_tenth_bps` on `OrderParams`; new `change_approved_builder` instruction and `RevenueShareEscrow` account. Existing order placements are unaffected (fields are optional).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| **Revenue-share fill enforcement**               | #68, #73       | Perp fills fail with `UnableToLoadRevenueShareAccount` (6324 / `0x18b4`) unless the taker's `RevenueShareEscrow` is passed in remaining accounts when (a) the taker order carries a builder code, or (b) the taker's `UserStats.referrer_status` has the `BuilderReferral` bit (escrow exists with a referrer). Liquidation fills and the feature-flag-off state are exempt. Fillers must attach the escrow for any taker that has one with a referrer — see §4.4. Referral rewards also no longer accrue (and referral slots are no longer created) for escrows without a referrer.                                                                                                                                                                                                              |
| **Funding bias spread widening**                 | #77            | New `AMM.funding_bias_sensitivity` field widens the vAMM's paying-side spread while it pays funding, up to `1 + sensitivity/100` at the funding offset floor. New admin instruction `update_perp_market_funding_bias_sensitivity`; SDK gains `AdminClient.updatePerpMarketFundingBiasSensitivity`. Default 0 = off, no quote change until enabled. Alongside this, `last_funding_oracle_twap` moved from `PerpMarket` to `MarketStats` (carved out of `MarketStats.padding`); the old `PerpMarket` slot became `_padding_funding_twap` and was later repurposed as the live `funding_clamp_threshold` + `funding_ramp_slope` fields by #94 — all offsets and sizes are unchanged and existing accounts need no migration. SDK: `PerpMarketAccount.lastFundingOracleTwap` is now `marketStats.lastFundingOracleTwap`. |
| **Withdraw guard notional cap**                  | #76            | `update_withdraw_guard_threshold` now requires the spot market's `oracle` account and rejects any threshold worth more than $10k notional (`MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL`, priced at the max of live price and 5-min TWAP). New error `WithdrawGuardThresholdNotionalTooLarge` (6352 / `0x18D0`). SDK `AdminClient.updateWithdrawGuardThreshold(spotMarketIndex, withdrawGuardThreshold, oracle?)` / `getUpdateWithdrawGuardThresholdIx(...)` gain an optional trailing `oracle?` (auto-resolved from the subscription cache or on-chain when omitted; manual instruction construction must include it).                                                                                                                                                                                  |
| **`VelocityCore` SDK module**                    | #21            | Subscription-free instruction-building API (`packages/sdk/src/core/`, exported via `export * from './core'`). Static helpers for PDAs, account decoding, remaining-accounts construction, signed-msg helpers, and pure instruction builders for deposit / withdraw / orders / fill / trigger / settlement / perp liquidation / place-cancel-modify / funding-rate updates — without a subscribed `VelocityClient`. See §4.5.                                                                                                                                                                                                                                                                                                                                                                    |
| **Vaults program + SDK**                         | #83, #85       | The drift-vaults program and its TS client are now first-party in this repo: the `vaults` program (`programs/vaults/`, ID `vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR`) and the `@velocity-exchange/vaults-sdk` package (`packages/vaults-sdk/`). Renames vs upstream drift-vaults: Rust crate/dir `drift_vaults` → `vaults`; the CPI dep on the core program resolves by its real crate name `velocity` (not the `drift` alias); SDK type `DriftVaults` → `Vaults`; IDL `drift_vaults.json` → `vaults.json` (generated from the program via `bun run program:idl:vaults`, never hand-edited). The program ID is unchanged.                                                                                                                                                                       |
| **Fee redesign + AMM isolation**                 | #75            | Explicit per-fill three-way fee split (`FeeStructure.amm_fee_numerator` / `if_fee_numerator`; protocol = residual). Per-market `PerpMarket.fee_ledger: FeeLedger` tracks gross fees + pending carveouts. Protocol fees accrue to a withdrawable `protocol_fee_pool` (perp + spot) and exit via `withdraw_protocol_fees_perp/spot` (new `HotRole::FeeWithdraw` key; pays the ATA of `State.protocol_fee_recipient_perp` / `_spot` (separately configurable treasuries), created on demand). Streaming sweep (`sweep_perp_market_fees`, permissionless) materializes carveouts out of the pnl pool; emits `PerpMarketFeeSweepRecord`. The AMM's books contain only its own money — its configurable fee provision is clawed back in bankruptcy as the backstop of last resort. Liquidations gain a `protocol_liquidation_fee` cut (new `protocol_fee` field on liquidation records). New errors `InvalidProtocolFeeRecipient` (6353) / `InsufficientProtocolFees` (6354). Full design doc: [`FEES.md`](../FEES.md). |
| **Build-gated features: isolated positions + VLP hedge** | #201 | Two opt-in cargo features, `isolated-position` and `vlp-hedge`, gate the instruction surface of isolated perp positions (`deposit_into_isolated_perp_position`, `transfer_isolated_perp_position_deposit`, `withdraw_from_isolated_perp_position`, plus the signed-msg `isolated_position_deposit` field, rejected with `IsolatedPositionDisabled` (6357) when gated) and the VLP hedge/LP-pool component (pool/constituent init + config, swap, add/remove liquidity, program-vault, AUM/target-base cranks, and `settle_perp_to_lp_pool`). **Mainnet builds compile these out until audited**; devnet/test builds keep them (`anchor-test` implies both). Five admin config instructions stay in every build because they share `#[derive(Accounts)]` structs with ungated admin instructions (anchor's cpi codegen cannot mix gated/ungated users of one struct): `update_perp_market_lp_pool_id`, `update_perp_market_lp_pool_paused_operations`, and the three `update_feature_bit_flags_*_lp_pool` toggles — all inert config writes whose readers are compiled out (the `hedge_config.status` activator itself is gated). Account layouts and the IDL are identical across builds: `PerpPosition.isolated_position_scaled_balance`, `PerpMarket.hedge_config`, and all LP-pool account types remain. Calling a gated instruction on a mainnet deployment fails with Anchor's fallback (instruction not found). Enabling later = add the features to the mainnet build invocation and upgrade in place. |
| **Per-user equity floor**                        | equity-floor   | New `User.equity_floor: u64` (QUOTE_PRECISION; carved from tail padding, `User` size unchanged at 4496) sets a minimum cross-margin total collateral for the account. While total collateral is below the floor, the program rejects risk-increasing order placement and fills (taker and maker), withdrawals, deposit transfers out (`transfer_deposit`, `transfer_deposit_by_delegate`), and `transfer_perp_position` from-side, all with `EquityBelowFloor` (6358 / `0x18D6`); reduce-only activity stays allowed, and keepers may force-cancel risk-increasing resting orders below the floor. Set only via the new **warm-admin** instruction `update_user_equity_floor(equity_floor)` (`AdminClient.updateUserEquityFloor` / `getUpdateUserEquityFloorIx`; admin CLI `velocity-admin user set-equity-floor`); the account's authority/delegate cannot change it. `0` disables (default for all existing accounts). The floor is per sub-account, but `transfer_deposit_by_delegate` gained an `equity_floor_delta: u64` arg (**signature change** vs the #45 form) that atomically moves floor along with funds between same-authority sub-accounts: the debited side must stay at/above its reduced floor, the credited side's post-transfer collateral must back its increased floor (else `InvalidEquityFloorTransfer`, 6359 / `0x18D7`), and the sum of floors is preserved — a delegate can rebalance capital freely while total equity across floored sub-accounts can never drop below the sum of floors. An authority-wide breaker escalates the per-subaccount freeze: the permissionless `trip_equity_floor_breaker` proves one subaccount is below its floor and sets `UserStats.equity_breaker_tripped` (1 byte carved from trailing padding; `UserStats` size unchanged at 240) — while set, **every** subaccount of the authority rejects risk-increasing fills, withdrawals and transfers out; only the warm-admin `reset_equity_floor_breaker` clears it (SDK `VelocityClient.tripEquityFloorBreaker` / `AdminClient.resetEquityFloorBreaker`; admin CLI `user reset-equity-breaker`). SDK: `UserAccount.equityFloor: BN`, `UserStatsAccount.equityBreakerTripped: number`, `User.isBelowEquityFloor()` / `getEquityAboveFloor()`, `getWithdrawalLimit` caps by equity above the floor, and `transferDepositByDelegate(..., equityFloorDelta?)` (accepts `'auto'` on the quote market to move the minimal floor the debited side needs). |
| **Bulk order margin enforcement (per risk-scope)** | #135         | `place_orders`/`place_scale_orders` now run the initial-margin check once per **risk scope** touched by the batch (cross-margin, plus each isolated market with a risk-increasing order), instead of a single end-of-batch check. Previously an early risk-increasing order's exposure wasn't accumulated into that check, and the check could be skipped altogether if the final order in the batch was a no-op — a batch could slip a risk-increasing order past a weaker or absent margin gate. Batches that previously succeeded may now be rejected with `InsufficientCollateral`. No SDK-side logic mirror exists; this is a program-only enforcement tightening. |
| **Auction-duration floor on requested spread**   | auction-floor-client-spread | Order sanitization's duration floor (`max(client duration, spread% × tier slots-per-pct)`) now measures the narrower of the client-requested and post-sanitize price ranges. Start-price improvements toward baseline no longer inflate auction durations — most visibly on tail-tier (C and below) markets with wide baseline spreads, where fully-specified signed-msg orders were floored to the baseline spread regardless of the requested duration. Expect materially shorter auctions for tight-spread orders on tail-tier markets; genuinely wide requested spreads are floored exactly as before. Program-only behavior change; no layout/IDL change (§6). |

---

## 4. SDK surface changes

### 4.1 Package and tooling

```bash
# old
npm install @drift-labs/sdk
# new
npm install @velocity-exchange/sdk
```

- Versioning reset: `2.163.0-beta.0` → `0.x` (changesets manage releases from this repo; release-please was removed).
- Anchor dependency: `@coral-xyz/anchor@0.29.0` → `@anchor-lang/core@1.0.1` (aliased as
  `@coral-xyz/anchor`). The IDL is Anchor-1.0 format and will not load in 0.29 clients.
- **IDL account name casing**: Anchor 1.0 emits camelCase account names in the IDL. Any code
  that passes an account name as a string to a coder method (e.g.
  `program.coder.accounts.decodeUnchecked('PerpMarket', …)`) must change PascalCase →
  camelCase: `'PerpMarket'` → `'perpMarket'`, `'SpotMarket'` → `'spotMarket'`, `'User'` →
  `'user'`, etc.
- Repo tooling moved from yarn to bun (only matters if you build from source).
- **jit-proxy**: `@drift-labs/jit-proxy` → `@velocity-exchange/jit-proxy`. The client is now
  vendored in-repo, ported to Anchor 1.0, and built against `@velocity-exchange/sdk`. The
  upstream package was built against `@drift-labs/sdk` and assumed Drift account layouts (e.g.
  it read `perpMarketAccount.amm.minOrderSize`, which Velocity removed from perp markets),
  which crashed the JIT maker on perp taker updates. Same `JitProxyClient` / `JitterSniper` /
  `JitterShotgun` API; the `driftClient` constructor fields now take a `VelocityClient`. The
  program is Velocity's own deployment at `J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ`
  (devnet & mainnet), replacing Drift's `J1TnP8zvVxbtF5KFp5xRmWuvG9McnhzmBd9XGfCyuxFP`;
  the SDK config presets' `JIT_PROXY_PROGRAM_ID` point at the new id.
- Anchor imports inside the SDK go through an isomorphic layer (`sdk/src/isomorphic/anchor`)
  with separate node/browser builds.

### 4.2 Renames (no deprecated aliases — find/replace required)

PR #37 originally shipped `@deprecated` Drift aliases; they have since been **removed**.
The old names no longer exist.

| Old                                         | New                                                                 |
| ------------------------------------------- | ------------------------------------------------------------------- |
| `DriftClient`                               | `VelocityClient`                                                    |
| `DriftClientConfig`                         | `VelocityClientConfig`                                              |
| `DriftClientSubscriptionConfig`             | `VelocityClientSubscriptionConfig`                                  |
| `DriftEnv`                                  | `VelocityEnv`                                                       |
| `DRIFT_PROGRAM_ID`                          | `VELOCITY_PROGRAM_ID`                                               |
| `DRIFT_ORACLE_RECEIVER_ID`                  | `VELOCITY_ORACLE_RECEIVER_ID` (same pubkey)                         |
| `USDC_MINT_ADDRESS`                         | `QUOTE_MINT_ADDRESS` (#18; devnet value changed to the dUSDT placeholder `GqmEqYsy8EyvofDpmtFxK8zhYrgWgNokAtYoduQdL7v6`, mainnet USDC unchanged) |
| `WebSocketDriftClientAccountSubscriber(V2)` | `WebSocketVelocityClientAccountSubscriber(V2)`                      |
| `pollingDriftClientAccountSubscriber`       | `pollingVelocityClientAccountSubscriber`                            |
| `grpcDriftClientAccountSubscriber(V2)`      | `grpcVelocityClientAccountSubscriber(V2)`                           |
| `Program<Drift>`                            | `Program<Velocity>` (alias `VelocityProgram`)                       |

### 4.3 Removed exports

Importing any of these now fails at build time:

`math/fuel`, `serum/*`, `phoenix/*`, `openbook/*`, `oracles/pythPullClient`,
`oracles/switchboardClient`, `oracles/switchboardOnDemandClient`, `util/pythOracleUtils`,
`math/userStatus`, `math/protectedMakerParams`,
`accounts/*HighLeverageModeConfigAccountSubscriber`, `util/tps` (and its `estimateTps`
helper), `getProtectedMakerModeConfigPublicKey`,
`AdminClient.initializeProtectedMakerModeConfig` / `updateProtectedMakerModeConfig`,
`VelocityClient.updateUserProtectedMakerOrders`,
plus types `LPRecord`, `LPAction`, `FuelSeasonRecord`, `FuelSweepRecord`,
`ProtectedMakerModeConfig`,
`SpotFulfillmentType`, `SpotFulfillmentStatus`, `SpotFulfillmentConfigStatus`.

Config fields `SERUM_V3`, `PHOENIX`, `OPENBOOK`, `SERUM_LOOKUP_TABLE`,
`PYTH_PULL_ORACLE_LOOKUP_TABLE` were dropped from the env config object.

Mainnet `MARKET_LOOKUP_TABLE` / `MARKET_LOOKUP_TABLES` now point at the
relaunch lookup table `4E971nER9Jn4JjT8mKEX1nvkfg8Qycp7zNEcCq2nT8ZY`
(state, signer, spot markets 0-1 with oracles/mints/vaults/IF vaults, perp
markets 0-3 with oracles, token/ATA/system programs). Drift's original
tables (`Fpys8…`, `EiWSs…`) reference pre-relaunch accounts and must not be
used against the Velocity program.

Gov-token stake fee discount removal (#80): `VelocityClient.updateUserGovTokenInsuranceStake`
/ `getUpdateUserGovTokenInsuranceStakeIx`,
`AdminClient.updateDelegateUserGovTokenInsuranceStake` /
`getUpdateDelegateUserGovTokenInsuranceStakeIx`, and constants
`GOV_SPOT_MARKET_INDEX` and `MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT_GOV`
(the `constants/insuranceFund` module) were removed.

Dead-export cleanup (#82): the following previously-exported symbols had no consumer
inside the SDK, its tests, or any velocity-exchange org repository and were removed.
The module `tx/forwardOnlyTxSender` was deleted (`ForwardOnlyTxSender` class) —
**but later restored in #89** (see §4.6).
Removed `math` functions: `builderCodesEnabled`, `builderReferralEnabled`,
`calculateAvailablePerpLiquidity`, `calculateBudgetedK` (the non-`BN` variant;
`calculateBudgetedKBN` is unaffected), `calculateCollateralValueOfDeposit`,
`calculateLiquidationPrice` (`calculateLiquidationPriceAfterPerpTrade` is unaffected),
`calculateMaxSpread`, `calculateNewMarketAfterTrade`,
`calculateOraclePriceForPerpMargin`, `calculateOracleReserveSpread`,
`calculatePerpMarketBaseLiquidatorFee`, `calculatePositionFundingPNL`,
`calculateUserMaxPerpOrderSize`, `fetchMSolMetrics`, `isOrderReduceOnly`,
`isOrderRiskIncreasing`, `isOrderRiskIncreasingInSameDirection`, `isTakingOrder`,
`trimVaaSignatures`. Also removed: `memcmp` helper `getUserThatHasBeenLP`, constants
`MAX_I64` / `TEN_MILLION`, type `MSOL_METRICS_ENDPOINT_RESPONSE`, and the deep-import-only
`PYTH_SOLANA_RECEIVER_IDL` (`pyth/types`). The misspelled constant `PTYH_LAZER_PROGRAM_ID`
was renamed to the correctly-spelled `PYTH_LAZER_PROGRAM_ID`.

(`calculateMaxRemainingDeposit` was in this removal batch but was restored in #89 — see §4.6.)

Legacy referrer migration removal (#149): `VelocityClient.migrateReferrer` /
`getMigrateReferrerIx` were removed. These wrapped the `migrate_referrer` program
instruction, which backfilled `RevenueShareEscrow.referrer` from `UserStats.referrer`
for escrows created before that copy was folded into escrow initialization. The
instruction's entrypoint had already been removed with the legacy referral model, so it
was absent from the IDL and the SDK methods threw at runtime; escrow initialization now
copies the referrer unconditionally, making the migration redundant.

### 4.4 Type-level breaking changes

- **`oraclePriceOffset` is now `BN`** (was `number`) on `Order` and `OrderParams` —
  widened to i64 on-chain in #51. Code passing raw numbers must wrap in `new BN(...)`.
- **`Order.quoteAssetAmount` removed.** This field never existed on the on-chain `Order`
  struct (which only has `quoteAssetAmountFilled`); it was a vestigial SDK-type member that
  the decoder always populated with `0`. The TS `Order` type now matches the IDL. Read
  filled quote from `quoteAssetAmountFilled` instead.
- `PerpMarketAccount`: oracle fields (`oracle`, `oracleSource`, …) moved from `amm.*` to
  the top level; aggregate position/funding stats moved into the market; new
  `marketStats` and `hedgeConfig` sub-structs (the latter replacing the flat `lp*` fields,
  #66); fuel/PMM/HLM/LP fields removed. `lastFundingOracleTwap` now lives at
  `marketStats.lastFundingOracleTwap` (#77); `fundingClampThreshold` / `fundingRampSlope`
  replace `paddingFundingTwap` (#94).
- `PerpPosition`: `lpShares`, `lastQuoteAssetAmountPerLp`, `perLpBase` removed.
- `StateAccount`: single `admin` replaced by the cold/warm/hot key set.
- `UserStatsAccount`: `ifStakedGovTokenAmount` removed (gov-stake fee discount removal, #80);
  `getUserFeeTier` no longer applies a stake-based discount. New `delegatePermissions: number`
  field (#45) — set/cleared by `update_user_allow_delegate_transfer`, gates whether a delegate
  may call `transfer_deposit_by_delegate`.
- `CurveRecord` event → `AmmCurveChanged` (fields changed too).
- **Revenue-share escrow on fills** (#68, #73): `ReferrerStatus` enum gains
  `BuilderReferral = 4`; new `isBuilderReferral(userStats)`, `escrowHasReferrer(escrow)`,
  and `hasBuilderParams(orderParams)` helpers in `math/builder`.
  `fillPerpOrder` / `getFillPerpOrderIx`, `placeAndTakePerpOrder` /
  `getPlaceAndTakePerpOrderIx`, `placeAndMakePerpOrder` /
  `getPlaceAndMakePerpOrderIx`, and `getPlaceAndMakeSignedMsgPerpOrderIxs` accept an
  optional trailing `takerEscrow` (the taker's decoded `RevenueShareEscrowAccount`,
  e.g. from a `RevenueShareEscrowMap`) so the taker's escrow is attached when the
  taker is referred (required by the program's fill-time enforcement — see §3). The
  builders validate `takerEscrow.authority` against the taker's authority. **Note:** #68
  originally took a `revenueShareEscrowMap?: RevenueShareEscrowMap` on
  `placeAndTakePerpOrder` / `getPlaceAndTakePerpOrderIx`; #73 replaced that with the
  decoded `takerEscrow?` — callers passing a map must switch to the decoded escrow account.
  The settle-PnL builders keep their map-based `revenueShareEscrowMap` param.
- **`PerpOperation` bit values**: 8 flags total, with `AMM_IMMEDIATE_FILL = 64` and
  `SETTLE_REV_POOL = 128`. Code hardcoding these numeric values instead of referencing the
  `PerpOperation` enum must update.
- **`OrderBitFlag` gains `HasBuilder = 16` and `IsIsolatedPosition = 32`**, matching the full
  6-bit on-chain flag set. Code reading `OrderRecord.order.bitFlags` /
  `OrderActionRecord.bitFlags` can detect builder-fee and isolated-margin orders directly via
  the shared enum instead of redefining local flag constants.
- **Fee redesign** (#75):
  - `PerpMarketAccount`: `totalExchangeFee` / `totalLiquidationFee` moved into a new
    nested `feeLedger: FeeLedger` (with `pendingProtocolFee`, `pendingIfFee`,
    `ammProtocolFeesReceived`, `pendingAmmProvision`); new `protocolFeePool`,
    `protocolLiquidationFee`, `feePoolBufferTarget` fields.
  - `SpotMarketAccount`: new `protocolFeePool`, `protocolLiquidationFee`,
    `protocolFeeFactor`; `insuranceFund.totalFactor`/`userFactor` → `ifFeeFactor`.
  - `StateAccount`: new `protocolFeeRecipientPerp` / `protocolFeeRecipientSpot`
    (two separately configurable treasury keys, one for perp, one for spot) / `hotFeeWithdraw`;
    `FeeStructure` gains `ammFeeNumerator` / `ifFeeNumerator` (carved from reserved padding).
  - `calculateUpdatedAMM` / `calculateBidAskPrice` / `calculateUpdatedAMMSpreadReserves` /
    `calculateOptimalPegAndBudget` / `calculateNewAmm` dropped their `totalExchangeFee`
    parameter (the AMM no longer has a fee floor).
  - `updatePerpMarketAmmSummaryStats` dropped `excludeTotalLiqFee`.
- **Strict null-checking surfaced on some accessors** (#74/#78, when the SDK turned
  on `"strict": true`). A few public signatures were widened to expose the `undefined`
  the runtime already returned:
  - `DLOBNode.getPrice(...)` now returns `BN | undefined` (was `BN`). It always could
    return `undefined` for orders without a resolvable limit price (e.g. post-auction
    market orders); the type now admits it. A new `getPriceOrThrow(...)` is provided for
    call sites that structurally require a defined price.
  - `BlockhashSubscriber.getLatestBlockHeight()` now returns `number | undefined` (was
    `number`) — `undefined` before any blockhash has been fetched, as the runtime
    already did.
  - `nextRevenuePoolSettleApr(spotMarket, vaultBalance, amount)`'s third positional
    `amount: BN` is now required (was `amount?: BN`); the function always dereferenced it,
    so omitting it already produced `NaN`/threw at runtime.
  - `BasicUserAccountSubscriber.getUserAccountAndSlot()` and
    `BasicUserStatsAccountSubscriber.getUserStatsAccountAndSlot()` now return
    `DataAndSlot<T> | undefined` (was the non-optional `DataAndSlot<T>`), matching the
    `UserAccountSubscriber` / `UserStatsAccountSubscriber` interface — they return
    `undefined` until an account is loaded, as the runtime already did. Relatedly, the
    `{ data, slot }` pair these and the polling subscribers store is now **atomic**: a
    loaded account always carries a real `slot` (`number`, never `undefined`; seeded
    accounts use `0` as an oldest-possible sentinel), so `DataAndSlot.slot` can be relied
    on as defined. `doesAccountExist()` on these subscribers is now a type predicate.
  - `User.getUserAccountAndSlot()` (and `VelocityClient.getUserAccountAndSlot()`) keep
    their `DataAndSlot<UserAccount> | undefined` return — `undefined` until the account
    loads, as the runtime already did. A new `User.getUserAccountAndSlotOrThrow()` is
    provided for call sites that structurally require a loaded account.
  - **`UserAccountSubscriber` "not subscribed" contract is now uniform.** Every
    implementation's `getUserAccountAndSlot()` throws `NotSubscribedError` when called
    before `subscribe()` — the WebSocket and polling subscribers already did, and the
    gRPC-multi and WebSocket-program subscribers now match. Consequently
    `User.getUserAccount()` **throws** when not subscribed and returns `undefined` only
    when subscribed but the account was not found on chain (since `subscribe()` awaits
    the initial fetch, `undefined` means "not found", not "still loading"). The
    `getUserAccountOrThrow()` / `getUserAccountAndSlotOrThrow()` error message changed
    from `User account not loaded: <pubkey>` to `User account not found: <pubkey>`;
    both still propagate `NotSubscribedError` when called before subscribing. Consumers
    that matched on the old message string should update.

### 4.5 New: `VelocityCore` (#21)

A subscription-free instruction-building module (`export * from './core'`) for
integrators who only need to construct instructions (PDAs, remaining accounts, deposit /
withdraw / order / fill / liquidation builders) without running a full subscribed client.

### 4.6 New exports

These public exports were **added** (or restored) relative to the fork point:

- `TransferFeeAndPnlPoolDirection` enum-class (`FEE_TO_PNL_POOL` / `PNL_TO_FEE_POOL`),
  `AdminClient.transferFeeAndPnlPool` / `getTransferFeeAndPnlPoolIx` (#1).
- `SpecialUserStatus` enum (`VammHedger = 1`); `AdminClient.updateSpecialUserStatus` /
  `getUpdateSpecialUserStatusIx`; `VelocityClient.specialTransferPerpPositionToVamm` /
  `getSpecialTransferPerpPositionToVammIx` (#17).
- `VelocityClient.updateUserAllowDelegateTransfer` / `transferDepositByDelegate` (#45).
- `AdminClient.updatePerpMarketFundingDeadZone` / `getUpdatePerpMarketFundingDeadZoneIx` (#94).
- `AdminClient.updatePerpMarketFundingBiasSensitivity` (#77).
- `AdminClient.updateWithdrawGuardThreshold` / `getUpdateWithdrawGuardThresholdIx` gained an
  optional trailing `oracle?` arg (#76).
- `VelocityCore` module (#21, see §4.5).
- **Restored in #89** (had been removed in #82): `ForwardOnlyTxSender` (`tx/forwardOnlyTxSender`)
  and `calculateMaxRemainingDeposit` (`math/spotMarket`).
- `PriceUpdateAccount` is now re-exported from the package root (#97); previously it was only
  reachable via a subpath import.
- `AdminClient.updatePauseAdmin` / `getUpdatePauseAdminIx` — cold-admin rotation of the
  emergency `pause_admin` key (`StateAccount.pauseAdmin`), sitting alongside
  `updateWarmAdmin` / `updateHotAdmin`.
- `isIsolatedPositionBankrupt(user, marketIndex)` and `hasIsolatedMarginBankrupt(user)`
  (`math/bankruptcy`) — mirror the isolated half of the program's bankruptcy routing
  (`is_isolated_margin_bankrupt` + `has_isolated_margin_bankrupt`). Needed because
  `User.isBankrupt()` reads only the account-level `UserStatus.BANKRUPT` bit, which is never
  set for isolated-only bankruptcies, and `isUserBankrupt` (cross) deliberately skips isolated
  positions. `isIsolatedPositionBankrupt` throws `InvalidPerpPosition` on a non-isolated index.
- `calculatePerpIfFee` / `calculateSpotIfFee` (`math/liquidation`) — port the margin-shortage-aware
  insurance-fund fee caps; feed their output (not the raw `if + protocol` sum) into the
  covering-amount helpers. `calculateMaxPctToLiquidate` gained an `isIsolatedPosition` param
  (returns 100% in one shot for isolated positions, per `IsolatedMarginLiquidatePerpMode`).
- `User.calculateFeeForQuoteAmount` was **renamed to `User.calculatePerpTakerFee`** (the old
  name is gone — update call sites). It also gained an optional trailing `builderInfo`
  (`Pick<OrderParams, 'builderIdx' | 'builderFeeTenthBps'>`) arg; when present the builder fee
  (`quoteAmount * builderFeeTenthBps / 100_000`) is added on top of the tiered fee.
  `VelocityClient.getMarketFees` now also applies the **referee discount** to the taker fee
  (previously omitted on this path) — referred users get a lower predicted fee.
- `MMOraclePriceData` gained optional `isMMOracleEnabled` / `isMMOracleAsRecent` /
  `isMMExchangeDiffBpsHigh` fields (populated by `getMMOracleDataForPerpMarket`).
  `isFallbackAvailableLiquiditySource` now mirrors `amm_fill_gates_ok` fully — it additionally
  suppresses AMM fallback on market drawdown and on MM-vs-exchange oracle volatility (>1% diff
  while the MM oracle is enabled and as-recent).
- `TRIGGER_PRICE_LAST_FILL_MAX_AGE` (`constants/numericConstants`) — max age of the last fill
  before `getTriggerPrice` treats the last-fill leg as absent (oracle price substitutes).
- Several types were added by the `types.ts` ↔ IDL reconciliation — see §4.7.

### 4.7 SDK type reconciliation (`types.ts` ↔ IDL)

The hand-maintained TypeScript mirrors in `sdk/src/types.ts` are not generated from the IDL
(the SDK does not use Anchor's `IdlAccounts`/`IdlTypes`/`IdlEvents` helpers), and had drifted
from the generated `idl/velocity.json`. This batch realigns them. Integrators who decoded
accounts/events with the previous TS shapes should note:

- **Added fields** (present in the IDL / emitted on-chain all along, missing from the TS type):
  - Account structs: `StateAccount.pauseAdmin` + `lpPoolFeatureBitFlags`; `PerpMarketAccount.poolId`;
    `SpotMarketAccount.expiryTs`; `UserStatsAccount.disableUpdatePerpBidAskTwap` + `pausedOperations`;
    `InsuranceFundStake.lastValidTs`; `AmmCache.bump`;
    `LPPoolAccount.targetOracleDelayFeeBpsPer10Slots` + `targetPositionDelayFeeBpsPer10Slots`.
  - `AMM`: the bid/ask reserve set (`askBaseAssetReserve`, `askQuoteAssetReserve`,
    `bidBaseAssetReserve`, `bidQuoteAssetReserve`), `lastOracleReservePriceSpreadPct`,
    `lastSpreadUpdateSlot`, `longSpread`, `shortSpread`, `referencePriceOffset`.
  - Event/record types: `DepositRecord` (`signer?`, `userTokenAmountAfter`);
    `OrderActionRecord` (`triggerPrice`, `builderIdx`, `builderFee`); `LiquidationRecord` (`bitFlags`);
    `LiquidatePerpRecord` + `LiquidateSpotRecord` (`protocolFee`).
- **Corrected field types** (no on-chain change — the TS type was wrong):
  - `LiquidationRecord.canceledOrderIds`: `BN[]` → `number[]`.
  - `LiquidatePerpRecord.userOrderId` / `liquidatorOrderId`: `BN` → `number`.
  - `OrderFillerRewardStructure.rewardNumerator` / `rewardDenominator`: `BN` → `number`.
  - `RevenueShareSettleRecord.ts`: `number` → `BN`.
- **Removed phantom fields** (never existed on-chain): `LPSwapRecord.outMint` / `inMint`,
  `LPMintRedeemRecord.lpMint`.
- **New exported types**: `PrelaunchOracleParams`, `PythLazerOracle`,
  `UpdatePerpMarketSummaryStatsParams`, `SignedMsgWsDelegatesAccount`, `PerpMarketFeeSweepRecord`,
  `ProtocolFeeWithdrawRecord`, `TransferFeeAndPnlPoolRecord`.
- **Events wired into `EventSubscriber`**: `PerpMarketFeeSweepRecord`, `ProtocolFeeWithdrawRecord`,
  `RevenueShareSettleRecord`, `TransferFeeAndPnlPoolRecord`, and `LPBorrowLendDepositRecord` are
  now registered in `EventMap` / `VelocityEvent` / the default `eventTypes` list
  (`events/types.ts`). All five have long been emitted on-chain with IDL entries and TS types,
  but `parseEventsFromLogs` silently dropped any event not in that registration list — they are
  now subscribable like any other record type.

---

## 5. On-chain layout & ABI notes

- **Account discriminators unchanged** for surviving accounts (`User`, `UserStats`,
  `State`, `PerpMarket`, `SpotMarket`, …) — Anchor derives them from the account name.
  Same for surviving instruction discriminators.
- **`request_remove_insurance_fund_stake` account list changed** (if-request-remove-settle):
  the instruction now settles already-due revenue before freezing the exit value, so its
  `#[derive(Accounts)]` gained `state`, `spot_market_vault`, `velocity_signer`, and
  `token_program` (order: `state`, `spot_market`, `insurance_fund_stake`, `user_stats`,
  `authority`, `spot_market_vault`, `insurance_fund_vault`, `velocity_signer`,
  `token_program`), plus optional transfer-hook `remaining_accounts` + token-mint like the
  add path. The discriminator is unchanged, but a manual (non-SDK) builder must now supply
  these accounts. `cancel_request_remove_insurance_fund_stake` was split onto its own
  `CancelRequestRemoveInsuranceFundStake` struct with the **same** 5 accounts it always had
  (`spot_market`, `insurance_fund_stake`, `user_stats`, `authority`, `insurance_fund_vault`) —
  no change for cancel callers. No on-chain account layout change.
- **Layouts changed**: `User` is 4376 → 4496 bytes. `PerpMarket` grew across several PRs:
  1216 → 1240 (#16, Anchor-1.0 16-byte `PoolBalance` alignment), reorganized through the
  AMM decoupling (#65) and `HedgeConfig` addition down to 1224 (#66), then 1224 → 1304
  (#75, embedded `FeeLedger` + protocol fee fields). The current size is **1304 bytes**,
  with u128/i128 fields front-loaded for alignment. Any custom (non-IDL) decoder must be
  rebuilt against `sdk/src/idl/velocity.json`.
- **Error codes are ABI-stable**: removed variants were renamed to `Deprecated*` stubs
  in place (numeric codes preserved); new variants are appended at the end. The tail of the
  enum is now `SpotDlobTradingDisabled` (6350), `InvalidAdminTier` (6351),
  `WithdrawGuardThresholdNotionalTooLarge` (6352), `InvalidProtocolFeeRecipient` (6353),
  `InsufficientProtocolFees` (6354), `InvalidNativeStateAccount` (6355),
  `InvalidNativePerpMarketAccount` (6356), `IsolatedPositionDisabled` (6357),
  `EquityBelowFloor` (6358), `InvalidEquityFloorTransfer` (6359). Decode errors by
  code as before, but expect `Deprecated*` names for retired features.
- **`User` layout**: `equity_floor: u64` was carved from the tail padding after
  `special_user_status` (3 padding bytes, the 8-byte field at offset 4472, then 8 more
  padding bytes). Account size is unchanged at 4496 bytes and no other offset moved —
  existing accounts stay valid (`equity_floor` reads as 0 = disabled), but custom
  decoders must add the field.
- **PDA seed strings unchanged** (`drift_state`, `user`, `spot_market_vault`, …) — only
  the program ID changed, so all derived addresses differ from Drift's.
- **`UserStats` layout preserved** across the gov-stake fee discount removal (#80) and the
  delegate-permissions addition (#45): `if_staked_gov_token_amount` was replaced in place by
  padding, and `delegate_permissions: u8` (#45) and `equity_breaker_tripped: u8`
  (equity-floor breaker) were carved from the trailing padding. The account size
  (240 bytes) and every other field offset are unchanged — existing accounts stay
  valid, but custom decoders must account for the new `delegate_permissions` and
  `equity_breaker_tripped` bytes.
  The `update_user_gov_token_insurance_stake` and
  `update_delegate_user_gov_token_insurance_stake` instructions no longer exist.
- **`MarketStatus` discriminants shifted** (#5): the deprecated `FundingPaused`, `AmmPaused`,
  `FillPaused`, `WithdrawPaused` variants were removed, so the surviving variants are now
  `Initialized` (0), `Active` (1), `ReduceOnly` (2), `Settlement` (3), `Delisted` (4) —
  vs Drift's `ReduceOnly` (6), `Settlement` (7), `Delisted` (8). `MarketStatus` is stored
  directly in `PerpMarket.status` / `SpotMarket.status`, so any custom (non-IDL) decoder
  built against the old Drift discriminants will silently misread these states.
- **Oracle support**: Pyth (push), Pyth Lazer, Prelaunch, QuoteAsset. Switchboard is a
  `Deprecated*` enum stub; the legacy Pyth pull variants (`PythPull`, `Pyth1KPull`,
  `Pyth1MPull`, `PythStableCoinPull`) keep their **original** names (not `Deprecated*`).
  All deprecated/removed sources return `InvalidOracle` if used.
- **`OracleSource` Switchboard variants renamed to their deprecated keys** (`Switchboard` →
  `DeprecatedSwitchboard`, `SwitchboardOnDemand` → `DeprecatedSwitchboardOnDemand`;
  discriminants preserved). The SDK's `OracleSource` class mirrors this with
  `DEPRECATED_SWITCHBOARD` / `DEPRECATED_SWITCHBOARD_ON_DEMAND` (Borsh keys
  `deprecatedSwitchboard` / `deprecatedSwitchboardOnDemand`). Any code still matching on the
  old `switchboard` / `switchboardOnDemand` keys will fail to decode these oracle sources.
- **`LiquidationRecord.bankrupt` is now state-derived, not a constant** (#174):
  the top-level `bankrupt` flag on `LiquidationRecord` (a sibling of the nested
  `perpBankruptcy` / `spotBankruptcy` sub-records — those sub-records have no `bankrupt`
  field of their own) now reflects whether the user still holds a bankrupting liability
  **after** the resolve call completes, rather than always being `true`. Read it as
  `record.bankrupt`, not `record.perpBankruptcy.bankrupt`. The wire type is unchanged
  (still a `bool`), so this is invisible to type-checkers — indexers and downstream
  consumers that assumed `bankrupt == true` on every emitted record must re-check the
  field's value instead of treating its presence as the signal.
- **Mainnet `initialize` requires a fixed signer** (#158): the one-time global `State`
  creation now locks the `admin` account to `state_init_authority`
  (`prpHJmuXnqdaz92tBVdwsqmqyhqPLuq5Km35a5QWco3`) on real mainnet builds only, to prevent
  front-running of genesis; devnet/localnet and the integration-test build are unaffected.
- **`PerpMarket.bankruptcy_if_floor_pct: u32`** replaces the 4-byte trailing padding
  immediately before `market_stats`. Account size (1304 bytes) and every other field
  offset are unchanged — existing accounts stay valid (the field reads as 0 = floor
  disabled until the admin sets it; new markets initialize to 10 bps), but custom
  decoders must add the field. The fee sweep's IF drain leaves this fraction of
  open-interest notional (valued at the market's oracle TWAP) behind in
  `fee_ledger.pending_if_fee`, keeping a standing first-loss tranche available to
  `resolve_perp_bankruptcy` that a permissionless sweep cannot clear ahead of a
  resolution. New warm-admin instruction `update_perp_market_bankruptcy_if_floor_pct`.

---

## 6. Change log vs upstream (merged PRs)

| PR        | Change                                                                                                                                                                                                                                                                                                                                                                       |
| --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| #1        | `transfer_fee_and_pnl_pool` instruction (warm-admin); rebalance AMM fee pool ↔ PnL pool. SDK `AdminClient.transferFeeAndPnlPool` / `getTransferFeeAndPnlPoolIx`; new `TransferFeeAndPnlPoolDirection` export; emits `TransferFeeAndPnlPoolRecord` event                                                                                                                       |
| #2, #47   | Remove high leverage mode: instructions, `User.margin_mode`/`MarginMode`, `PerpMarket` HLM fields, HLM config subscribers, `HIGH_LEVERAGE_MIN_MARGIN_RATIO` (#47); error variants → `Deprecated*` stubs                                                                                                                                                                       |
| #5        | `MarketStatus` refactor: extract into own module; remove deprecated `FundingPaused`/`AmmPaused`/`FillPaused`/`WithdrawPaused`; discriminants for `ReduceOnly`/`Settlement`/`Delisted` shift 6/7/8 → 2/3/4                                                                                                                                                                      |
| #6        | Disable spot DLOB trading (`SpotDlobTradingDisabled` = 6350)                                                                                                                                                                                                                                                                                                                  |
| #7        | Remove legacy Pyth pull/push (program instructions + SDK clients); default `oracleSource` → `PYTH_LAZER`                                                                                                                                                                                                                                                                      |
| #12       | Funding floor raised 7.3% → 10.95% annualized (`FUNDING_RATE_OFFSET_DENOMINATOR` 5000 → 3333) + 0.05% dead-zone clamp (`FUNDING_RATE_CLAMP_DENOMINATOR` = 2000)                                                                                                                                                                                                                |
| #13       | Remove prediction markets (`ContractType::Prediction` → `DeprecatedPrediction`; `InvalidPredictionMarketOrder` → `DepreciatedPredictionMarketOrder`, 6284)                                                                                                                                                                                                                    |
| #14       | Remove Switchboard oracle support (classic + on-demand)                                                                                                                                                                                                                                                                                                                       |
| #16       | Anchor 0.29 → 1.0; `PerpMarket::SIZE` 1216 → 1240 (16-byte `PoolBalance` alignment); IDL account names now camelCase (affects string-keyed coder calls)                                                                                                                                                                                                                       |
| #17       | Special user account status: `User.special_user_status` bitmask + `SpecialUserStatus` enum; new `update_special_user_status` (admin) + `special_transfer_perp_position_to_vamm` (user); new error `InvalidTransferPerpPosition` (6312)                                                                                                                                        |
| #18       | SDK quote-mint cleanup: `USDC_MINT_ADDRESS` → `QUOTE_MINT_ADDRESS`; devnet value → dUSDT placeholder, mainnet unchanged                                                                                                                                                                                                                                                       |
| #21       | SDK core (`VelocityCore`) expansion, isomorphic Anchor build, perp instruction delegation                                                                                                                                                                                                                                                                                    |
| #26       | New program ID + devnet deployment                                                                                                                                                                                                                                                                                                                                           |
| #36       | Remove fuel, vAMM LP, Serum/Phoenix orderbooks; add admin commands                                                                                                                                                                                                                                                                                                           |
| #37       | SDK rename Drift → Velocity (aliases since removed)                                                                                                                                                                                                                                                                                                                           |
| #38       | Remove protected maker mode (instructions, `ProtectedMakerModeConfig` account + PDA helper, SDK math/admin/client methods, `PerpMarket` fields)                                                                                                                                                                                                                               |
| #39       | Yarn → Bun                                                                                                                                                                                                                                                                                                                                                                   |
| #45       | `transfer_deposit_by_delegate` + `update_user_allow_delegate_transfer`; `UserStats.delegate_permissions` field (carved from padding, size unchanged)                                                                                                                                                                                                                          |
| #51       | `oracle_price_offset` widened to i64                                                                                                                                                                                                                                                                                                                                         |
| #52–#59   | release-please publishing for SDK (`0.0.x`) — later replaced by changesets                                                                                                                                                                                                                                                                                                    |
| #60       | MM oracle validation — strict slot-monotonicity, min 2-slot gap, and 1% per-write step cap on the existing MM oracle native handler                                                                                                                                                                                                                                          |
| #63       | Zero-copy native admin handlers (refactor of the inherited native fast-path entrypoint)                                                                                                                                                                                                                                                                                      |
| #65       | Decouple AMM from rest of codebase                                                                                                                                                                                                                                                                                                                                           |
| #66       | VLP module (vAMM + hedge): flat `lp*` `PerpMarketAccount` fields restructured into `hedgeConfig`; `PerpMarket::SIZE` → 1224                                                                                                                                                                                                                                                   |
| #67       | Remove legacy referrer-reward fee path (`UserStats` epoch fields, `FeeStructure.referrer_reward_epoch_upper_bound`, `FeatureBitFlags::BuilderReferral`, four `RevenueShareEscrowAccount` fields, SDK `referrerInfo?` params)                                                                                                                                                   |
| #68       | Builder codes on non-swift orders; fill-time enforcement of builder + referral revenue share (escrow required when taker has a builder order or a referred escrow)                                                                                                                                                                                                            |
| #70       | Rebrand program crate drift → velocity                                                                                                                                                                                                                                                                                                                                       |
| #71       | This migration guide                                                                                                                                                                                                                                                                                                                                                         |
| #73       | Enforce referral revenue share at fill time: `fill_perp_order` rejects (`UnableToLoadRevenueShareAccount`) when taker has `BuilderReferral` but no escrow supplied; SDK `placeAndTakePerpOrder` param `revenueShareEscrowMap` → `takerEscrow`                                                                                                                                  |
| #74, #78  | Enable TypeScript `strict` mode in the SDK. No runtime behavior change; a few public accessor signatures widened to expose already-possible `undefined` (`DLOBNode.getPrice`, `BlockhashSubscriber.getLatestBlockHeight`, the basic/polling user(-stats) subscribers' `get…AndSlot()`) and `nextRevenuePoolSettleApr`'s `amount` made required. The user(-stats) subscribers' stored `{ data, slot }` pair is now atomic (§4.4) |
| #75       | Fee redesign (explicit per-fill carveouts, withdrawable protocol fees via `protocolFeeRecipientPerp`/`protocolFeeRecipientSpot`, 100% staker-owned IF) + AMM isolation; `PerpMarket::SIZE` 1224 → 1304; new errors 6353/6354                                                                                                                                                   |
| #76       | Withdraw guard threshold notional cap: `update_withdraw_guard_threshold` now requires an `oracle` account; rejects > $10k notional; new error `WithdrawGuardThresholdNotionalTooLarge` (6352); `update_spot_market_oracle`/`_expiry` promoted to cold admin; SDK `updateWithdrawGuardThreshold` gains optional `oracle?`                                                       |
| #77       | Funding bias spread widening: `AMM.funding_bias_sensitivity` + `update_perp_market_funding_bias_sensitivity` admin ix; `last_funding_oracle_twap` moved `PerpMarket` → `MarketStats` (offset-preserving)                                                                                                                                                                       |
| #80       | Remove gov-token (DRIFT) stake fee discount: gov stake-sync instructions, `UserStats.if_staked_gov_token_amount` (→ padding), gov IF revenue-settle APR cap, `GOV_SPOT_MARKET_INDEX`                                                                                                                                                                                          |
| #82       | Remove 27 unused SDK exports (see §4.3 dead-export cleanup); rename misspelled `PTYH_LAZER_PROGRAM_ID` → `PYTH_LAZER_PROGRAM_ID`                                                                                                                                                                                                                                               |
| #83, #85  | Vendor drift-vaults into the monorepo as the `vaults` program + `@velocity-exchange/vaults-sdk` (renames `drift_vaults` → `vaults`, `DriftVaults` → `Vaults`; CPI dep resolves as `velocity`)                                                                                                                                                                                 |
| #89       | Restore `ForwardOnlyTxSender` (`tx/forwardOnlyTxSender`) and `calculateMaxRemainingDeposit` (`math/spotMarket`) to the SDK public API (both removed in #82)                                                                                                                                                                                                                   |
| #94       | Continuous funding dead zone: per-market `funding_clamp_threshold` + `funding_ramp_slope` (recycle `_padding_funding_twap`) replace #12's global hard cutoff; `update_perp_market_funding_dead_zone` ix; `AdminClient.updatePerpMarketFundingDeadZone`; `PerpMarketAccount.fundingClampThreshold`/`fundingRampSlope` replace `paddingFundingTwap`                              |
| #97       | Re-export `PriceUpdateAccount` from the `@velocity-exchange/sdk` package root; migrate dlob-server + keeper-bots-v2 to the workspace SDK                                                                                                                                                                                                                                       |
| #127 | Reconcile hand-written `sdk/src/types.ts` mirrors with the generated IDL: add previously-missing account/event fields, correct `BN`↔`number` field types, drop phantom (never-on-chain) `*Mint` record fields, export new param/record types (§4.7). No on-chain layout change                                                                                |
| native-path | Harden the native fast-path admin handlers (`update_mm_oracle_native`, `update_amm_spread_adjustment_native`): authenticate against the program-owned `State` account loaded via `AccountLoader` (owner + discriminator), require the market slot to hold a program-owned `PerpMarket` (replaces an unchecked `bytemuck` cast), and read the slot from the `Clock` sysvar instead of a caller-supplied account. New errors `InvalidNativeStateAccount` (6355) / `InvalidNativePerpMarketAccount` (6356). The `update_amm_spread_adjustment_native` ix now requires the `State` account at index 2 (SDK `getUpdateAmmSpreadAdjustmentNativeIx` is now async and adds it) |
| #139      | `transfer_deposit` / `transfer_deposit_by_delegate` now enforce the same admission checks as direct deposit/withdraw: the recipient credit requires active spot-market status for a positive deposit balance (`MarketActionPaused`) and respects `max_token_deposits` (`MaxDeposit`); the source debit honors direct-withdraw's reduce-only cap (`ReduceOnlyWithdrawIncreasedRisk`). Transfers that previously succeeded into a capped/non-active/reduce-only market now revert. No ABI/layout change |
| #149      | Remove the dead `migrate_referrer` program instruction (handler + accounts struct; entrypoint already removed with the legacy referral model, so no IDL/ABI change) and its non-functional SDK wrappers `VelocityClient.migrateReferrer` / `getMigrateReferrerIx` (§4.3)                                                                                       |
| #155 | Uniform `UserAccountSubscriber` "not subscribed" contract: gRPC-multi and WebSocket-program subscribers' `getUserAccountAndSlot()` now throw `NotSubscribedError` before `subscribe()` (matching WebSocket/polling), so `User.getUserAccount()` throws when not subscribed and returns `undefined` only when not found; `getUserAccount(AndSlot)OrThrow` message `User account not loaded` → `User account not found` (§4.4)                                                            |
| #TBD | Compile-time gate isolated positions (`isolated-position` feature) and the VLP hedge component (`vlp-hedge` feature) out of mainnet builds pending audit; devnet/test builds keep both (`anchor-test` implies them). New error `IsolatedPositionDisabled` (6357) rejects signed-msg orders carrying `isolated_position_deposit` on gated builds. Five lp-pool admin config ixs sharing accounts structs with ungated admin ixs stay in all builds (inert config writes; see §3). Layouts/IDL unchanged; the `update_initial_amm_cache_info` / `override_amm_cache_info` handlers moved from `vlp/hedge/admin.rs` to `vlp/amm/admin.rs` (shared amm-cache maintenance, stays on mainnet); dead `ResetAmmCache` accounts struct removed (§3) |
| #172 | Decouple solvency-repair from withdrawals: new `State.solvency_status` (1 B carved from padding, size unchanged) + `SolvencyStatus` bitflag; `resolve_perp_pnl_deficit`/`resolve_perp_bankruptcy`/`resolve_spot_bankruptcy` now gated by `solvency_repair_not_paused` instead of `WithdrawPaused`; new `update_solvency_status` instruction (cold-admin only); SDK `SolvencyStatus` enum, `StateAccount.solvencyStatus`, `solvencyRepairPaused()` helper, `AdminClient.updateSolvencyStatus` |
| jit-proxy | Vendor the jit-proxy client into the monorepo as `@velocity-exchange/jit-proxy` (replacing `@drift-labs/jit-proxy`), ported to Anchor 1.0 and built against `@velocity-exchange/sdk`. Fixes a JIT-maker crash where the jitter read `perpMarketAccount.amm.minOrderSize` (removed on Velocity perps); the perp dust guard and synthetic `Order` (no `quoteAssetAmount`) now match Velocity's layout. Same `JitProxyClient`/`JitterSniper`/`JitterShotgun` API; constructor `driftClient` fields take a `VelocityClient` (§4.1) |
| jit-proxy-id | Deploy the vendored jit-proxy program under Velocity's own id `J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ` (devnet & mainnet; a create-with-seed vanity address — see `deploy-scripts/deploy-jit-proxy.sh`), replacing Drift's `J1TnP8zvVxbtF5KFp5xRmWuvG9McnhzmBd9XGfCyuxFP` in `declare_id!`, the generated IDL, and the SDK config presets' `JIT_PROXY_PROGRAM_ID` (§4.1). Integrators must point jitters/JIT makers at the new program id |
| #134      | Fix `liquidate_spot_with_swap_begin`/`_end`: a stale fixed account-index/count guard (13 vs the actual 11) made every real call fail with `InvalidLiquidateSpotWithSwap`. The instruction was non-functional prior to this fix and is now operational; keepers that shelved this ix should re-verify their integration. SDK builder (`velocityClient.ts`) was already correct — no SDK change |
| #135      | Bulk `place_orders`/`place_scale_orders` now enforce the initial-margin check once per risk scope touched by the batch (cross-margin, plus each isolated market with a risk-increasing order), rather than a single check after the last order — closing a gap where an early risk-increasing order's exposure wasn't accumulated into the check, and the check could be skipped entirely if the final order in the batch was a no-op. Batches that previously succeeded may now be rejected with `InsufficientCollateral` (§3) |
| #137      | Direct `deposit()` now respects the per-market `SpotOperation::Deposit` pause bit (`MarketActionPaused`), independent of the pre-existing global deposit-pause and aggregate `max_token_deposits` cap checks. Deposits into a market with only the per-market deposit bit paused now revert |
| #158      | Mainnet `initialize` (one-time global `State` creation) now requires a fixed admin signer (`state_init_authority` = `prpHJmuXnqdaz92tBVdwsqmqyhqPLuq5Km35a5QWco3`) to prevent front-running of genesis; devnet/localnet and the integration-test build are unaffected (§5) |
| #174      | `LiquidationRecord.bankrupt` (the top-level flag, not a field of the nested `perpBankruptcy`/`spotBankruptcy` sub-records) now reflects whether the user still holds a bankrupting liability after the resolve call, instead of always being `true`. Wire type unchanged — consumers assuming `bankrupt == true` must update (§5) |
| #182      | AMM JIT no longer participates in a DLOB match fill when a hard AMM-fill gate (pause / drawdown / MM-oracle volatility / oracle invalidity) is active; match fills can now be smaller or route entirely to the resting DLOB maker under those conditions |
| equity-floor | New warm-admin instruction `update_user_equity_floor(equity_floor)` sets `User.equity_floor: u64` (QUOTE_PRECISION; tail padding, size unchanged): a minimum cross-margin total collateral below which risk-increasing order placement/fills, withdrawals, deposit/position transfers out revert with `EquityBelowFloor` (6358) and keepers may force-cancel risk-increasing resting orders; reduce-only activity unaffected, `0` disables. `transfer_deposit_by_delegate` gains an `equity_floor_delta` arg (signature change) that atomically moves floor with the funds between same-authority sub-accounts, preserving the sum of floors (`InvalidEquityFloorTransfer`, 6359). Authority-wide breaker: permissionless `trip_equity_floor_breaker` proves one sub-account below its floor and sets `UserStats.equity_breaker_tripped` (padding byte, size unchanged), freezing all of the authority's sub-accounts until warm-admin `reset_equity_floor_breaker`. SDK `AdminClient.updateUserEquityFloor`/`resetEquityFloorBreaker`, `VelocityClient.tripEquityFloorBreaker`, `UserAccount.equityFloor`, `UserStatsAccount.equityBreakerTripped`, `User.isBelowEquityFloor`/`getEquityAboveFloor`, floor-capped `getWithdrawalLimit`, `transferDepositByDelegate(..., equityFloorDelta?)`; admin CLI `user set-equity-floor`, `user reset-equity-breaker` (§3, §5) |
| bankruptcy-if-floor | Fix a High audit finding: the permissionless fee sweep (`sweep_perp_market_fees` and the inline sweep on every `settle_pnl`) could drain `fee_ledger.pending_if_fee` — `resolve_perp_bankruptcy`'s first-loss tranche — ahead of a bankruptcy resolution, converting a tranche-covered loss into a shared-IF draw or socialized funding loss. New `PerpMarket.bankruptcy_if_floor_pct: u32` (repurposed trailing padding before `market_stats`; size/offsets unchanged) makes the sweep's IF drain leave that fraction of OI notional (at the oracle TWAP) behind as a standing tranche; new markets default to 10 bps, existing markets read 0 (disabled) until set via the new warm-admin `update_perp_market_bankruptcy_if_floor_pct` (SDK `AdminClient.updatePerpMarketBankruptcyIfFloorPct`, CLI `perp-market set-bankruptcy-if-floor`); `PerpMarketAccount.bankruptcyIfFloorPct` added (§5) |
| bid-ask-twap-hardening | Harden `update_perp_bid_ask_twap`: (1) `update_funding_rate` is no longer called from the crank — refreshing the caller-curated DLOB mark TWAP and applying funding in one instruction let the just-written TWAP feed funding at zero elapsed time; funding now runs only via its own `update_funding_rate` crank and on fills. Integrators/keepers relying on the funding side-effect must call `update_funding_rate` separately (the keeper-bots `fundingRateUpdater` already does). (2) The oracle-divergence filter is now symmetric — DLOB levels are kept only within `oracle ± BID_ASK_TWAP_MAX_ORACLE_DIVERGENCE_PERCENT` (15%) on both sides, so caller-supplied depth can no longer push mark TWAP past the band via high bids / low asks. (3) `keeper_stats` is bound to the signer (`has_one = authority`); a caller can no longer point at a third party's staked `UserStats` to pass the IF-stake gate. No layout/IDL change |
| spot-bankruptcy-revenue-pool | `resolve_spot_bankruptcy` now consumes the spot market's `revenue_pool` as a first-loss tranche before the staker-owned IF vault and before socializing any remainder to depositors (replacing upstream's unimplemented `todo`; mirrors the perp-bankruptcy tranche order where in-transit IF revenue pays before the vault). The draw is counter-only (no token movement), bypasses the periodic revenue-settle timer and staker APR cap, and shrinks `revenue_pool.scaled_balance`/`deposit_balance` in place. `SpotBankruptcyRecord` is unchanged — `if_payment` still records only the IF-vault draw; the revenue-pool tranche is program-log only, and `cumulative_deposit_interest_delta`/`total_social_loss` now reflect the smaller post-tranche socialized loss. No layout/IDL change |
| trigger-price-staleness | Trigger-price last-fill leg gains a staleness guard: `PerpMarket::get_trigger_price` (median trigger price, `MedianTriggerPrice` feature) substitutes the oracle price for `last_fill_price` when the market's last fill (`market_stats.last_trade_ts`) is older than `TRIGGER_PRICE_LAST_FILL_MAX_AGE` (5 min); previously a fill from hours ago voted in the median indefinitely on quiet markets. Zero-fill fulfillment steps no longer stamp `last_trade_ts` or update the volume rolling sums (real fills only). SDK `getTriggerPrice` mirrors the guard; new export `TRIGGER_PRICE_LAST_FILL_MAX_AGE`. No layout/IDL change |
| if-request-remove-settle | Fix a High audit finding: `request_remove_insurance_fund_stake` now settles already-due protocol revenue into the IF vault **before** freezing the staker's `last_withdraw_request_value`, mirroring `add_insurance_fund_stake`. Previously the exit value was frozen against the pre-settle vault, so a public revenue settle between request and remove shifted the exiting staker's rightful share of that already-due revenue to the remaining stakers. **Instruction accounts changed (ABI):** `request_remove_insurance_fund_stake` gains `state`, `spot_market_vault`, `velocity_signer`, `token_program` (and accepts the transfer-hook `remaining_accounts` + token-mint like the add path); `cancel_request_remove_insurance_fund_stake` moves to its own unchanged accounts struct (`CancelRequestRemoveInsuranceFundStake`, same 5 accounts as before). Integrators constructing the request-remove ix manually must pass the new accounts; SDK `VelocityClient.requestRemoveInsuranceFundStake` handles them automatically. No on-chain account layout change (§5) |
| order-amm-correctness | Fix three OtterSec audit findings in the perp order/fill path (no layout/IDL/ABI change). (1) A `ReduceOnly` perp market now forces every order it fills to be risk-reducing: `fill_perp_order` (taker) and `get_maker_orders_info` (makers) re-derive the market's reduce-only status at fill time and stamp `order.reduce_only`, so a legacy order placed while the market was `Active` can no longer increase exposure after the market is flipped to `ReduceOnly` (previously the fill keyed only off the flag stored at placement). (2) The AMM fallback-price premium (`AMM::get_fallback_price`) now clamps the seconds-to-expiry operand before multiplying, so an order with an unbounded `max_ts` (e.g. `i64::MAX`) no longer overflows and aborts every fill routed through it; the divisor is unchanged for all in-range expiries. (3) The perp DLOB matcher now builds its `QuoteContext` with the real (safe) oracle instead of a zero-price default, so oracle-offset resting makers are requoted at the same price maker discovery froze them at rather than reverting in `validate_fill_price`. SDK unaffected (the TS DLOB matcher already threads the oracle consistently; the fallback-price math is not mirrored) |
| auction-floor-client-spread | Auction-duration floor in order sanitization (`update_perp_auction_params`, market/oracle and crossing-limit variants) now paces the **narrower of the requested and sanitized price ranges** instead of always the sanitized range. Previously, pulling the start price toward baseline (tail-tier markets, and any non-signed market order) widened the range and inflated the duration floor — a tier-C signed-msg order asking a 0.2% spread / 20 slots could be floored to 60–100+ slots by the market's baseline spread. Orders whose auction prices are left untouched, or whose end price is sanitized inward, keep today's durations; orders whose start is improved toward baseline keep the client-requested duration when within the 10-slot signed-msg grace. Applies to signed-msg and regular orders alike; fully-derived auctions (no client prices) are unaffected. Program-only behavior change (the swift server's `will_sanitize` simulation calls the program function and inherits it); no SDK logic mirror exists, no layout/IDL change |
| lazer-max-staleness | Fix a High audit finding: `post_pyth_lazer_oracle_update` validated a signed Lazer message only for signer trust and a monotonic (non-decreasing) feed timestamp versus the cached account — never against `Clock::unix_timestamp` — yet always stamped `posted_slot` to the current slot, and downstream oracle staleness is derived solely from that slot. An authentic-but-stale or replayed message was therefore treated as slot-fresh for AMM/margin/liquidation/settlement (and because the monotonic check is strict `<`, the same message could be re-posted each slot to peg a stale price as perpetually fresh). The handler now rejects (skips) any feed whose message timestamp lags `Clock::unix_timestamp` by more than the new `PYTH_LAZER_MAX_STALENESS_SECONDS` (15s). Keepers/integrators posting Lazer updates must post reasonably promptly (legit updates are sub-second, so unaffected); a message older than 15s no longer updates the cache. No account-layout or IDL change (the constant is not IDL-exposed) |

---

## 7. Migration checklist

1. **Swap the dependency**: `@drift-labs/sdk` → `@velocity-exchange/sdk`.
2. **Find/replace renames** (§4.2). There are no runtime aliases; TypeScript will surface
   every site as a compile error.
3. **Update the program ID** everywhere it is hardcoded:
   `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P`.
4. **Re-derive all PDAs / cached addresses.** Nothing derived against the Drift program
   ID is valid on Velocity. User accounts must be re-initialized; balances do not migrate.
5. **Replace the IDL** if you load it yourself: use `sdk/src/idl/velocity.json`
   (Anchor 1.0 format) and an Anchor 1.0 client. Update any string-keyed coder calls to the
   new camelCase account names (§4.1).
6. **Wrap `oraclePriceOffset` values in `BN`.**
7. **Delete integrations with removed features** (§2): spot DLOB orders, Serum/Phoenix/
   OpenBook fulfillment, fuel, LP shares, protected maker, high leverage mode,
   prediction markets, Switchboard/Pyth-pull oracles, gov-token stake fee discount.
8. **Update account decoders/indexers** to the new `User` / `PerpMarket` / `State` /
   `UserStats` layouts and the shifted `MarketStatus` discriminants (§5); discriminators
   match Drift's, so guard by program ID, not discriminator.
9. **Re-test error handling**: codes are stable, but retired codes now decode to
   `Deprecated*` names and new codes exist past the old end of the enum (through 6354).
10. **Adopt builder codes** (optional): approve builders via `changeApprovedBuilder(...)`
    and set `builderIdx` / `builderFeeTenthBps` on `OrderParams`. No action needed if you
    don't use builders. If you are a filler, attach the taker's `RevenueShareEscrow` in
    remaining accounts when the taker has a builder order or a referred escrow (see §3
    fill-time enforcement).
11. **Re-pull the IDL and types** for the fee redesign — `PerpMarket` is now 1304 bytes and
    fee fields live in `feeLedger` (§4.4). IF stakers receive 100% of settled revenue (no
    protocol share mint). If you index fees, the authoritative flow description is
    [`FEES.md`](../FEES.md).
