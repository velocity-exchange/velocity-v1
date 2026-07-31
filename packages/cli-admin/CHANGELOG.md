# @velocity-exchange/admin-cli

## 0.10.1

### Patch Changes

- Updated dependencies [[`c08211e`](https://github.com/velocity-exchange/velocity-v1/commit/c08211e4c106f57a1092238adb5c6d14895734f5)]:
  - @velocity-exchange/sdk@0.12.0

## 0.10.0

### Minor Changes

- [#325](https://github.com/velocity-exchange/velocity-v1/pull/325) [`2a73aa7`](https://github.com/velocity-exchange/velocity-v1/commit/2a73aa716aed4f5910893c0ad9012bd598f72844) Thanks [@0xahzam](https://github.com/0xahzam)! - Account extension: new `extend_account` instruction grows a zero-copy account (`User`, `UserStats`, `PerpMarket`, `SpotMarket`, `State`, ...) to the size the deployed program compiles in for its discriminator's type, as the migration crank for a future upgrade that appends fields to an account struct (see `docs/ACCOUNT-EXTENSION.md`). Gated on the new `HotRole.AccountExtension` (cold, warm, or the configured hot key; `StateAccount` gains `hotAccountExtension`, carved from tail padding with the account size unchanged). Grow-only with the target compiled in, payer covers the rent-exempt shortfall, tail zero-filled by the runtime, no-op when already at size; borsh accounts and unknown discriminators are rejected with the new `InvalidAccountExtension` error (6367). A devnet/test-only `extend_account_devnet(new_len)` (compiled out of production mainnet builds, kept by `anchor-test`; same role gate) grows to an arbitrary larger size so the flow can be exercised before a real extension exists. SDK: `VelocityClient.extendAccount`/`getExtendAccountIx` and the devnet variants. Admin CLI: new `extend-account` command, single-account mode plus a `--type <t>` batch crank that scans by discriminator, skips at-size accounts, and supports `--dry-run` and `--batch-size`; assign the role with `auth set-hot-admin accountExtension <pubkey>`.

### Patch Changes

- Updated dependencies [[`2a73aa7`](https://github.com/velocity-exchange/velocity-v1/commit/2a73aa716aed4f5910893c0ad9012bd598f72844), [`6e29daf`](https://github.com/velocity-exchange/velocity-v1/commit/6e29daf0ef781986dbe3bbf79f1c9bd2e25eb646)]:
  - @velocity-exchange/sdk@0.11.0

## 0.9.1

### Patch Changes

- Updated dependencies [[`63a580e`](https://github.com/velocity-exchange/velocity-v1/commit/63a580ea3c31a21fb8820fe75075d799cc8dc3da), [`2219857`](https://github.com/velocity-exchange/velocity-v1/commit/2219857615aeb4cd11b5ace8a203279797f41c88)]:
  - @velocity-exchange/sdk@0.10.0

## 0.9.0

### Minor Changes

- [#322](https://github.com/velocity-exchange/velocity-v1/pull/322) [`c16315e`](https://github.com/velocity-exchange/velocity-v1/commit/c16315e594120afdeb10f832c64914da01cbddcb) Thanks [@0xahzam](https://github.com/0xahzam)! - Equity floor buffer: `User.equity_floor_buffer` (carved from the last 8 tail-padding bytes, account size unchanged) adds admin-set headroom above the equity floor. Every risk-increasing gate (order placement/fills, withdrawals, swaps, deposit/position transfers out, trigger activation, floor-transfer to-side) now enforces `total_collateral >= equity_floor + equity_floor_buffer`, while the permissionless breaker still trips at the raw floor — so no permitted action can leave a subaccount trippable; only a passive drawdown through the whole buffer can arm the breaker. `updateUserEquityFloor(user, equityFloor, equityFloorBuffer)` sets both (breaking signature change, on-chain and in `AdminClient`). SDK: `UserAccount.equityFloorBuffer`, `User.isBelowBufferedEquityFloor`/`getBufferedEquityFloor`/`getEquityAboveBufferedFloor`, pure `calculateEquityFloorAutoDelta` and `getEquityFloorLevel` helpers, and a new `EquityFloorManager` that abstracts the per-subaccount mechanics for delegates: aggregate status + levels, haircut-padded `transferQuote`/`planQuoteTransfer`, `getMaxWithdrawable`/`getMaxQuoteTransferable`, and proportional-to-equity `rebalanceFloors` via zero-amount floor moves. `transferDepositByDelegate` `'auto'` now targets `floor + buffer`. Admin CLI: `user set-equity-floor <user> <floor> <buffer>` (breaking), new `user equity-floor-status <authority>` and `user close-positions` closure sweep.

### Patch Changes

- Updated dependencies [[`d142320`](https://github.com/velocity-exchange/velocity-v1/commit/d14232017da7b09be1a71af8c5f6ee889ccac745), [`25da8e1`](https://github.com/velocity-exchange/velocity-v1/commit/25da8e1e39ccbb8310de32dd0da29041f2a93a0c), [`c16315e`](https://github.com/velocity-exchange/velocity-v1/commit/c16315e594120afdeb10f832c64914da01cbddcb), [`e34c623`](https://github.com/velocity-exchange/velocity-v1/commit/e34c6233afa1e04c7a4ff4a3f088405508f85790), [`edfc846`](https://github.com/velocity-exchange/velocity-v1/commit/edfc8469b5b4058f8767f1e48075b384fb809b4f), [`5fac99b`](https://github.com/velocity-exchange/velocity-v1/commit/5fac99bb93343d93c2a58e776fdba888588f89a2), [`5fac99b`](https://github.com/velocity-exchange/velocity-v1/commit/5fac99bb93343d93c2a58e776fdba888588f89a2), [`0ac1f73`](https://github.com/velocity-exchange/velocity-v1/commit/0ac1f730d0bdc5420ae0efd0ec12a1eb017fa542)]:
  - @velocity-exchange/sdk@0.9.0

## 0.8.0

### Minor Changes

- [#185](https://github.com/velocity-exchange/velocity-v1/pull/185) [`1f866f1`](https://github.com/velocity-exchange/velocity-v1/commit/1f866f1c44def526aeef8927fe14d563f3a8ed7b) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Add per-market configurable withdraw circuit breaker and a daily deposit rate
  cap.

  The previously-hardcoded 25% daily withdraw circuit breaker is now configurable
  per spot market via `SpotMarketAccount.withdrawCircuitBreakerBps` (basis points,
  10000 = 100%; `0` keeps the default 25% = 2500 bps). A new daily deposit rate
  cap mirrors the withdraw side: `depositGuardThreshold` (no cap below it) and
  `maxDepositBpsPerDay` (basis points; `0` disables) bound how far resulting
  deposits may exceed the 24h deposit TWAP. It is enforced on the direct `deposit`
  instruction and on the shared spot-credit path, so `transfer_pools` and
  `end_swap` deposit credits are bounded too; it reverts with the new
  `DailyDepositLimit` (6364) program error.

  All three fields are carved from the existing 13-byte alignment gap before
  `protocol_fee_pool`, so `SpotMarket` stays 808 bytes with every other field
  offset unchanged and no account migration. Existing markets read the repurposed
  bytes as 0 (default 25% breaker, disabled deposit cap). The two percentage
  fields are `u16` basis points rather than `u32` PERCENTAGE_PRECISION so the set
  fits the gap; `depositGuardThreshold` stays `u64` (token amount).

  SDK: `SpotMarketAccount` gains `withdrawCircuitBreakerBps`,
  `depositGuardThreshold`, and `maxDepositBpsPerDay`; new
  `AdminClient.updateSpotMarketWithdrawCircuitBreaker` /
  `updateSpotMarketDepositCap` (and their `getUpdate…Ix` builders); new math
  helpers `calculateMaxDepositTokenAmount` / `checkDepositLimits`; the existing
  `calculateWithdrawLimit` now honors the configurable breaker (all in basis
  points).

  Admin CLI: new `spot-market set-withdraw-breaker <market> <pct>` and
  `spot-market set-deposit-cap <market> <threshold> <pctPerDay>` commands (pct in
  basis points).

- [#291](https://github.com/velocity-exchange/velocity-v1/pull/291) [`ded5391`](https://github.com/velocity-exchange/velocity-v1/commit/ded5391eaeeed7c401eda6a0070072ba97d40e17) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `program upgrade` command: propose a BPFLoaderUpgradeable upgrade from an existing on-chain buffer (validates the buffer and prints its executable hash before proposing).

- [#284](https://github.com/velocity-exchange/velocity-v1/pull/284) [`ad91962`](https://github.com/velocity-exchange/velocity-v1/commit/ad91962bddd83efc5e39076aeb229e1adbb71cc9) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `spot-market set-scale-initial-asset-weight-start` command: sets the deposit-notional threshold (QUOTE_PRECISION, 1e6) above which a spot market's initial asset weight scales down. `0` disables scaling. Warm/cold admin; maintenance weight is unaffected.

- [#297](https://github.com/velocity-exchange/velocity-v1/pull/297) [`373edfc`](https://github.com/velocity-exchange/velocity-v1/commit/373edfc398386942124656d85609060e12445a1e) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Add `show fees`: read-only command printing every fee a user can pay — perp fee tiers (by 30d volume) and the spot tier, filler reward, the trade-fee remainder split, and per-market fee adjustments, liquidation fees, and spot interest carveouts.

- [#279](https://github.com/velocity-exchange/velocity-v1/pull/279) [`605e0be`](https://github.com/velocity-exchange/velocity-v1/commit/605e0be12a96c2c6c5a21b2a98c83083b3fa37fe) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `user init` command: initializes UserStats (if missing) and sequential named sub-accounts for a given authority (or a Squads vault PDA via `--multisig`/`--vault-index`). On mainnet the program requires the authority to sign creation, so with `--multisig` the instructions are batched into one vault transaction proposal with the vault as rent payer; otherwise the signer pays and the transaction is sent directly. Idempotent across reruns. Prints the planned accounts and expected rent up front; `--dry-run` stops there.

- [#279](https://github.com/velocity-exchange/velocity-v1/pull/279) [`605e0be`](https://github.com/velocity-exchange/velocity-v1/commit/605e0be12a96c2c6c5a21b2a98c83083b3fa37fe) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `user set-delegate` command: sets the delegate wallet on an authority's sub-accounts (optionally toggling the authority-wide `allowDelegateTransfer` flag), batched into a single Squads vault transaction proposal with `--multisig`. `sendOrPropose` now accepts a vault index so proposals can execute from vaults other than 0, and `user deposit`/`user withdraw` gain a `--vault-index` option. `set-delegate`, `deposit` and `withdraw` also gain `--dry-run`, printing the instruction list and expected proposal rent/fees without sending.

### Patch Changes

- [#292](https://github.com/velocity-exchange/velocity-v1/pull/292) [`633c5f1`](https://github.com/velocity-exchange/velocity-v1/commit/633c5f17c56b186e505a3a7bfad2a450a1e9a82e) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Fix warm-gated spot-market admin commands failing when proposed through the warm-admin Squads multisig. `getUpdateSpotMarketStatusIx`, `getUpdateWithdrawGuardThresholdIx`, `getUpdateSpotMarketIfFactorIx`, and `getUpdateSpotMarketScaleInitialAssetWeightStartIx` now accept an optional `admin` override, and `velocity-admin spot-market` commands resolve the admin signer to the executing authority (the multisig's vault 0 PDA with `--multisig`, else the local keypair) instead of always embedding `state.coldAdmin`.

- Updated dependencies [[`ac29aa1`](https://github.com/velocity-exchange/velocity-v1/commit/ac29aa129c2cac14099a92b4a484bf20d2974863), [`88642ad`](https://github.com/velocity-exchange/velocity-v1/commit/88642ad1784af54c9ca0df271535b32a69cbe517), [`1f866f1`](https://github.com/velocity-exchange/velocity-v1/commit/1f866f1c44def526aeef8927fe14d563f3a8ed7b), [`ce18d22`](https://github.com/velocity-exchange/velocity-v1/commit/ce18d22926ae6a18b98df8a60bbd2696e0d10dbc), [`4179772`](https://github.com/velocity-exchange/velocity-v1/commit/417977294e10ffc152a0e5230019671000d2185b), [`88a3c64`](https://github.com/velocity-exchange/velocity-v1/commit/88a3c647f609a5fe357e414ee8b4b630fd2dc68c), [`e1f45a3`](https://github.com/velocity-exchange/velocity-v1/commit/e1f45a3d9e9e54e42987ed71bff9f6eaa6eac623), [`fc86321`](https://github.com/velocity-exchange/velocity-v1/commit/fc86321e8b8323e95e2d8385a82fdc69ca00075f), [`8f8b1ef`](https://github.com/velocity-exchange/velocity-v1/commit/8f8b1efcd9323369e13b0166ea158d78bd49b2ad), [`70ec53e`](https://github.com/velocity-exchange/velocity-v1/commit/70ec53e9390f8da2dd5aeee752c2bf3d285a2697), [`3b9a07b`](https://github.com/velocity-exchange/velocity-v1/commit/3b9a07bbe8f72145006ab45837a1fc21858d8c73), [`2994a81`](https://github.com/velocity-exchange/velocity-v1/commit/2994a813ab3de23c44d11157f040f690e3ddf8d6), [`a0e111a`](https://github.com/velocity-exchange/velocity-v1/commit/a0e111a237245d4011b33ff263a2cc9237a66265), [`0e0654c`](https://github.com/velocity-exchange/velocity-v1/commit/0e0654cafc5a95855caccd1f7741e74089cb6007), [`943095b`](https://github.com/velocity-exchange/velocity-v1/commit/943095b975dff10791b0e287df462d2ecf176aea), [`2d4a32f`](https://github.com/velocity-exchange/velocity-v1/commit/2d4a32f1c74b28b123ad4bd47f734c2843b090e4), [`8761596`](https://github.com/velocity-exchange/velocity-v1/commit/87615967d775c435fa777a5fa9396a80bbb0a62f), [`633c5f1`](https://github.com/velocity-exchange/velocity-v1/commit/633c5f17c56b186e505a3a7bfad2a450a1e9a82e)]:
  - @velocity-exchange/sdk@0.8.0

## 0.7.0

### Minor Changes

- [#258](https://github.com/velocity-exchange/velocity-v1/pull/258) [`075913d`](https://github.com/velocity-exchange/velocity-v1/commit/075913d6643d2ee0218b3b807deb6fd68854394c) Thanks [@0xahzam](https://github.com/0xahzam)! - New `feature-flags median-trigger-price <true|false>` command wrapping `updateFeatureBitFlagsMedianTriggerPrice` (bit 2 of `State.featureBitFlags`; enabling requires the cold admin).

### Patch Changes

- [#225](https://github.com/velocity-exchange/velocity-v1/pull/225) [`a874748`](https://github.com/velocity-exchange/velocity-v1/commit/a874748e268183f1cea12353ca404da8439cb88d) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `--multisig` now only creates a Squads proposal when the multisig's vault 0 PDA is actually a required signer of the instructions being dispatched. When the vault does not need to sign (e.g. the wallet itself is the required authority), the CLI prints a notice and sends the transaction directly instead of creating a pointless proposal.

- Updated dependencies [[`cec4fcb`](https://github.com/velocity-exchange/velocity-v1/commit/cec4fcbf440645ad55dd41ec8410a250ca96fdef), [`f03beee`](https://github.com/velocity-exchange/velocity-v1/commit/f03beeecea6f3c9cc6c0ad7e828e9fab639e9a1b), [`c85d802`](https://github.com/velocity-exchange/velocity-v1/commit/c85d80284fb61dac7a08e47fe7b78340bf1213cc)]:
  - @velocity-exchange/sdk@0.7.0

## 0.6.1

### Patch Changes

- [#245](https://github.com/velocity-exchange/velocity-v1/pull/245) [`35f1480`](https://github.com/velocity-exchange/velocity-v1/commit/35f1480f2a60bad00a96f2e254cb5e9b210067b1) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Bankruptcy IF-fee floor (High audit fix): the permissionless fee sweep now leaves `bankruptcyIfFloorPct` of open-interest notional behind in `feeLedger.pendingIfFee`, so a sweep front-running a `resolvePerpBankruptcy` can no longer strip the first-loss tranche up to the floor. `PerpMarketAccount` gains `bankruptcyIfFloorPct` (repurposed padding — layout size unchanged; existing markets read 0 = disabled, new markets default to 10 bps), `AdminClient` gains `updatePerpMarketBankruptcyIfFloorPct`, and the admin CLI gains `perp-market set-bankruptcy-if-floor <market> <pct>`.

- Updated dependencies [[`35f1480`](https://github.com/velocity-exchange/velocity-v1/commit/35f1480f2a60bad00a96f2e254cb5e9b210067b1), [`00ebcd2`](https://github.com/velocity-exchange/velocity-v1/commit/00ebcd2068b03db30652d352e2995417e08d9b35)]:
  - @velocity-exchange/sdk@0.6.1

## 0.6.0

### Minor Changes

- [#220](https://github.com/velocity-exchange/velocity-v1/pull/220) [`155ed06`](https://github.com/velocity-exchange/velocity-v1/commit/155ed0618d7012b9305f4c84786c4d4e96d86be8) Thanks [@0xahzam](https://github.com/0xahzam)! - Per-user equity floor: new warm-admin instruction `update_user_equity_floor` sets `User.equityFloor` (QUOTE_PRECISION), a minimum cross-margin total collateral below which the program rejects risk-increasing order placement and fills, withdrawals, and transfers out of the account with `EquityBelowFloor` (6358); reduce-only activity stays allowed and 0 disables. `transferDepositByDelegate` gains an `equityFloorDelta` argument (instruction signature change) that atomically moves floor along with funds between same-authority subaccounts, preserving the sum of floors (`InvalidEquityFloorTransfer`, 6359). An authority-wide breaker escalates the freeze: the permissionless `trip_equity_floor_breaker` proves one subaccount below its floor and sets `UserStats.equityBreakerTripped`, freezing all of the authority's subaccounts until the warm-admin `reset_equity_floor_breaker` clears it. SDK adds `AdminClient.updateUserEquityFloor` / `getUpdateUserEquityFloorIx` / `resetEquityFloorBreaker`, `VelocityClient.tripEquityFloorBreaker`, `UserAccount.equityFloor`, `UserStatsAccount.equityBreakerTripped`, `User.isBelowEquityFloor` / `getEquityAboveFloor`, floor-aware `getWithdrawalLimit`, and an `'auto'` floor-delta mode on `transferDepositByDelegate` (quote market) that computes the minimal floor to carry. Admin CLI adds `velocity-admin user reset-equity-breaker <userStats>`. Admin CLI adds `velocity-admin user set-equity-floor <user> <floor>`.

### Patch Changes

- Updated dependencies [[`00decfd`](https://github.com/velocity-exchange/velocity-v1/commit/00decfd93fff5668779255288a0f61242be99d07), [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab), [`c288314`](https://github.com/velocity-exchange/velocity-v1/commit/c2883143d813a5c608354d64c3d1a5b825c4f398), [`61cbeb2`](https://github.com/velocity-exchange/velocity-v1/commit/61cbeb2f70dcd3f6d4fab1209962132dea9d60fe), [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab), [`155ed06`](https://github.com/velocity-exchange/velocity-v1/commit/155ed0618d7012b9305f4c84786c4d4e96d86be8)]:
  - @velocity-exchange/sdk@0.6.0

## 0.5.0

### Minor Changes

- [#205](https://github.com/velocity-exchange/velocity-v1/pull/205) [`9854dfa`](https://github.com/velocity-exchange/velocity-v1/commit/9854dfa1c915938fe08495568f262285ffb6c933) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `user deposit`, `user withdraw`, and `if stake` commands to the admin CLI, usable directly or through a Squads V4 multisig (`--multisig` defaults the authority to the vault 0 PDA so the proposal executes with the vault as signer). SDK: `getWithdrawIx`, `getInitializeInsuranceFundStakeIx`, and `getAddInsuranceFundStakeIx` now accept an optional `overrides.authority`, matching `getDepositInstruction`, so instructions can be built for an authority other than the wallet (e.g. a multisig vault PDA).

### Patch Changes

- Updated dependencies [[`9854dfa`](https://github.com/velocity-exchange/velocity-v1/commit/9854dfa1c915938fe08495568f262285ffb6c933), [`6d58632`](https://github.com/velocity-exchange/velocity-v1/commit/6d58632540814c739fc3848e4110c2a24547722b), [`3042967`](https://github.com/velocity-exchange/velocity-v1/commit/304296799e8d5b8a6525cb18cfd8398637c00521)]:
  - @velocity-exchange/sdk@0.5.0

## 0.4.0

### Minor Changes

- [#189](https://github.com/velocity-exchange/velocity-v1/pull/189) [`8df28ac`](https://github.com/velocity-exchange/velocity-v1/commit/8df28ac6d113760ae4a8cdff4ad438cb25efce2c) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Program↔SDK parity fixes from the 2026-07-02 audit: renamed deprecated Switchboard
  OracleSource keys to match the IDL (fixes a decode crash on affected markets), applied
  the $100 initial-margin unrealized-PnL cap, standardized auction/limit prices to order
  tick size across the DLOB, isolated-position handling in bankruptcy/liquidation math,
  corrected MM-oracle validity gating, referrer_status memcmp offset, PerpOperation and
  OrderBitFlag bit values, wired five missing event records into EventSubscriber, fixed
  withdraw-limit divisors, multi-pool margin segregation, referee/builder fee estimation,
  and added AdminClient.updatePauseAdmin plus admin CLI commands for pause-admin rotation
  and fee-pool transfers. Also fixed withdrawFromIsolatedPerpPosition's withdraw-all path:
  it substituted the MIN_I64 sentinel into the instruction's unsigned u64 amount (serializing
  as 2^63, so full withdrawals always failed on-chain with InsufficientCollateral); it now
  clamps the request to the position's deposit plus claimable PnL.

  Follow-up completeness fixes: DLOBSubscriber.getL2/getL3 (and the dlob-server publisher) now
  thread orderTickSize so the public book view is tick-standardized like on-chain; added
  hasIsolatedMarginBankrupt and wired isolated-only bankruptcy detection into keeper resolution
  (isIsolatedPositionBankrupt now guards against non-isolated indices); getMarketFees applies the
  referee discount and calculateFeeForQuoteAmount accepts builder params so both public fee-prediction
  entry points match on-chain; and isFallbackAvailableLiquiditySource now fully mirrors
  amm_fill_gates_ok, adding the market-drawdown and MM-vs-exchange oracle volatility gates.

  Low-risk parity follow-ups: MarginCategory now includes 'Fill' as a single shared type, handled
  across perp margin ratio / unrealized-asset-weight and spot asset/liability weights (the
  integer-averaged midpoint of initial and maintenance, mirroring get_margin_ratio /
  get_asset_weight / get_liability_weight) instead of throwing or returning undefined; the
  worst-tier taker-fee estimate in calculateEntriesEffectOnFreeCollateral now ceil-divides to match
  calculate_taker_fee; MM-oracle validity is computed with the raw exchange confidence (matching
  get_mm_oracle_price_data) while the returned MM price keeps its diff-adjusted confidence; and
  corrected the OracleSourceNum doc (it is an SDK-internal oracle-id encoding, not the on-chain
  Borsh discriminant).

  Visible/breaking API changes in this release: `OracleSource.SWITCHBOARD` /
  `OracleSource.SWITCHBOARD_ON_DEMAND` (and the corresponding `OracleSourceNum` entries) are renamed
  to `DEPRECATED_SWITCHBOARD` / `DEPRECATED_SWITCHBOARD_ON_DEMAND` with no aliases kept for the old
  names; `ContractType.FUTURE` is renamed to `DEPRECATED_FUTURE`; and `FeatureBitFlags.BUILDER_REFERRAL`
  is removed outright, since no such on-chain flag exists. Separately, `getLimitPrice` gained a new
  optional trailing `tickSize` parameter — the existing `fallbackPrice` parameter stays in its original
  4th position, so old 4-argument call sites keep working unchanged.

### Patch Changes

- Updated dependencies [[`dff8a47`](https://github.com/velocity-exchange/velocity-v1/commit/dff8a4754f6b736fed330b2ab4fa5685db4f8159), [`900c07d`](https://github.com/velocity-exchange/velocity-v1/commit/900c07d9da7e106c82fbe65b3d92226d090bdee9), [`8df28ac`](https://github.com/velocity-exchange/velocity-v1/commit/8df28ac6d113760ae4a8cdff4ad438cb25efce2c), [`bafd699`](https://github.com/velocity-exchange/velocity-v1/commit/bafd6990f8322f232d2f0d17042beb0e9c567164)]:
  - @velocity-exchange/sdk@0.4.0

## 0.3.0

### Minor Changes

- [#172](https://github.com/velocity-exchange/velocity-v1/pull/172) [`b7d15b9`](https://github.com/velocity-exchange/velocity-v1/commit/b7d15b970a74d267aeaf20bb644d5344b9aadc61) Thanks [@0xahzam](https://github.com/0xahzam)! - Decouple solvency-repair from the withdraw pause. The `resolve_perp_pnl_deficit`,
  `resolve_perp_bankruptcy`, and `resolve_spot_bankruptcy` instructions are now gated by a
  new `State.solvencyStatus` bitfield instead of `WithdrawPaused`, so user withdrawals can
  be halted while solvency repair keeps running (or repair can be frozen on its own). Adds
  the `SolvencyStatus` enum, `StateAccount.solvencyStatus`, a `solvencyRepairPaused()`
  helper, `AdminClient.updateSolvencyStatus`, and the `exchange set-solvency-status` admin
  CLI command.

### Patch Changes

- Updated dependencies [[`b7d15b9`](https://github.com/velocity-exchange/velocity-v1/commit/b7d15b970a74d267aeaf20bb644d5344b9aadc61), [`2f6c64d`](https://github.com/velocity-exchange/velocity-v1/commit/2f6c64d54f1146d8e7f9ee4ab556929c6bf8b920), [`3f148f8`](https://github.com/velocity-exchange/velocity-v1/commit/3f148f8b477e4176e11e0660adb0e67dd5163d3b)]:
  - @velocity-exchange/sdk@0.3.0

## 0.2.1

### Patch Changes

- Updated dependencies [[`d3b58ab`](https://github.com/velocity-exchange/velocity-v1/commit/d3b58ab7e3ad33f0e6634ff87e9b150915b3aa13)]:
  - @velocity-exchange/sdk@0.2.6

## 0.2.0

### Minor Changes

- [`ec502c6`](https://github.com/velocity-exchange/velocity-v1/commit/ec502c6535e3ce2105f3df5b94bb005b7364c555) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Add `perp-market set-oracle-slot-delay <market> <slots>` command to set a perp
  market's `oracle_slot_delay_override`. Lets operators raise the "stale for amm
  immediate" tolerance above the default `-1` (which clamps to a 0-slot threshold
  and makes a healthy multi-slot oracle crank read as perpetually stale).

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

- Updated dependencies [[`4fd7462`](https://github.com/velocity-exchange/velocity-v1/commit/4fd7462bfa3c55e31e3457b1b65f519cf052a6fa)]:
  - @velocity-exchange/sdk@0.2.1
