# @velocity-exchange/admin-cli

## 0.16.0

### Minor Changes

- [#523](https://github.com/velocity-exchange/velocity-v1/pull/523) [`f720e70`](https://github.com/velocity-exchange/velocity-v1/commit/f720e70641a87a3fed42a5164cca12a55c1d4bef) Thanks [@0xahzam](https://github.com/0xahzam)! - Make `--dry-run` global, validate `call` and `batch` payloads against the IDL, and decode
  instructions in the dry-run output.

  Each command declared `--dry-run` itself, so 13 had it and the rest did not. `call`,
  `spot-market set-max-token-deposits` and `perp-market deposit-fee-pool` offered no way to preview
  a proposal. It is now a global option that `sendOrPropose` checks, and `sendOrPropose` is the only
  code path that signs or proposes. Gating it there covers every state-changing command, so no
  command can accept the flag and still send. This removes the 13 local declarations. Commands that
  print a fuller preview, such as `wallet swap` with its quote and `lut extend` with its account
  diff, return before dispatch and behave as before.

  `buildIxFromPayload` now rejects unknown and missing args. It used to pass `undefined` to Anchor,
  which serialized a missing numeric arg as `0`. The IDL names fields in snake_case while the Anchor
  client exposes them in camelCase, so a payload written from the IDL proposed
  `initializePythLazerOracle` with `feedId` 0, and a seeds constraint caught it only later. The
  check runs in both directions. An `{ option: T }` arg may still be absent.

  The dry run now decodes each instruction and prints its name, arguments and named accounts,
  instead of a program id and an account count. A wrong argument shows up before the proposal
  exists. It still prints the account count for every instruction, including ones it cannot decode,
  because that count is the only warning that a Jupiter route is large enough to overflow the Squads
  executor.

### Patch Changes

- Updated dependencies [[`f720e70`](https://github.com/velocity-exchange/velocity-v1/commit/f720e70641a87a3fed42a5164cca12a55c1d4bef)]:
  - @velocity-exchange/sdk@0.25.0

## 0.15.3

### Patch Changes

- [#511](https://github.com/velocity-exchange/velocity-v1/pull/511) [`60a173f`](https://github.com/velocity-exchange/velocity-v1/commit/60a173fd58e70d68e7d670523129621df267e2c8) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `perp-market deposit-fee-pool` and `perp-market sync-amm-summary-stats` to the admin CLI, for
  recovering a market whose `total_fee_minus_distributions` has gone negative. Both SDK instruction
  builders (`getDepositIntoPerpMarketFeePoolIx`, `getUpdatePerpMarketAmmSummaryStatsIx`) now take an
  optional `admin` override so the hot role that actually signs can be passed, as the other hot-role
  builders already allow.
- Updated dependencies [[`60a173f`](https://github.com/velocity-exchange/velocity-v1/commit/60a173fd58e70d68e7d670523129621df267e2c8), [`ab33ee9`](https://github.com/velocity-exchange/velocity-v1/commit/ab33ee907bd02907266853856718f8715a570f97)]:
  - @velocity-exchange/sdk@0.24.0

## 0.15.2

### Patch Changes

- Updated dependencies [[`a655327`](https://github.com/velocity-exchange/velocity-v1/commit/a655327291a5ef9238bae929f19d06158db512a4)]:
  - @velocity-exchange/sdk@0.23.1

## 0.15.1

### Patch Changes

- [#499](https://github.com/velocity-exchange/velocity-v1/pull/499) [`0afc72e`](https://github.com/velocity-exchange/velocity-v1/commit/0afc72e8c1506ce834c1f57b764a5b1a6cce6713) Thanks [@ChesterSim](https://github.com/ChesterSim)! - Read Agave 4.2 / SIMD-0385 transaction v1 on `getTransaction` paths.

  Bump `@solana/web3.js` to 1.99.0 (read-only v1), `@triton-one/yellowstone-grpc` to 6.0.0, and `helius-laserstream` to 0.8.5. SDK `engines.node` is now `>=20.18.0`. For the packages in this release the change is limited to `maxSupportedTransactionVersion: 1` on RPC reads. The `solana-*` 4.2 crate bump (Rust wire decode / send) is a follow-up; until it lands the Rust event poller walks a transaction's logs when `decode()` cannot read the v1 wire format, and decodes payloads only while the Velocity program is the executing program.

  `fetchLogs` now logs `getTransaction` batch errors instead of discarding them, and holds its `earliestTx`/`mostRecentTx` resume cursors behind any signature it failed to fetch so those transactions are retried rather than skipped. It returns `undefined` when no signature in the batch is safe to resume from, so keep the current cursor and retry in that case. `EventSubscriber.fetchPreviousTx` counts only transactions it has not already decoded toward `maxTx`, so the page re-read after a failed fetch no longer shortens a backfill.

- Updated dependencies [[`0afc72e`](https://github.com/velocity-exchange/velocity-v1/commit/0afc72e8c1506ce834c1f57b764a5b1a6cce6713)]:
  - @velocity-exchange/sdk@0.23.0

## 0.15.0

### Minor Changes

- [#491](https://github.com/velocity-exchange/velocity-v1/pull/491) [`d2ea4ff`](https://github.com/velocity-exchange/velocity-v1/commit/d2ea4ffd940d4498bb4d11a7983de650f0f4d886) Thanks [@0xahzam](https://github.com/0xahzam)! - Add the fourth perp fee tier, VIP 3, at $200M trailing-30d volume.

  The program's tier ladder is now Regular / VIP 1 / VIP 2 / VIP 3 (indices 0-3, breakpoints $5M / $80M / $200M). The SDK exports the new breakpoint as `VIP_FEE_TIER_THREE_VOLUME_QUOTE` and includes it in `PERP_FEE_TIER_VOLUME_THRESHOLDS`, so `getPerpFeeTierIndex`, `User.getUserFeeTier` and `VelocityClient.getMarketFees` select tier 3 above $200M and `PERP_FEE_TIER_MAX_INDEX` is 3. A `promoFeeTier` of 3 puts every account on the top tier.

  Admin CLI: `fees set-schedule` takes four tier fees (`<t0bp> <t1bp> <t2bp> <t3bp>`; tiers 4-9 mirror tier 3), `fees set-promo-tier` accepts 3, and `show fees` prints the VIP 3 row.

### Patch Changes

- Updated dependencies [[`d2ea4ff`](https://github.com/velocity-exchange/velocity-v1/commit/d2ea4ffd940d4498bb4d11a7983de650f0f4d886)]:
  - @velocity-exchange/sdk@0.22.0

## 0.14.2

### Patch Changes

- [#482](https://github.com/velocity-exchange/velocity-v1/pull/482) [`a7fecad`](https://github.com/velocity-exchange/velocity-v1/commit/a7fecadb09b37c0fd247eba376d11e705429db3e) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `velocity-admin show state`: dumps every field of the singleton State account, with the `exchangeStatus`, `featureBitFlags`, `lpPoolFeatureBitFlags` and `solvencyStatus` bitmasks decoded to their bit names. The `feature-flags` subcommands only write bits; there was no way to read the current ones back.

- Updated dependencies [[`eaa0664`](https://github.com/velocity-exchange/velocity-v1/commit/eaa06645a8ae137ce4e8ca606b2a65d3a24980cd), [`033237b`](https://github.com/velocity-exchange/velocity-v1/commit/033237bb975692bcce5bd540b3b015aba29463f3), [`e2b86d3`](https://github.com/velocity-exchange/velocity-v1/commit/e2b86d3ddba2c3e903ce70da835314a16ebed8e3)]:
  - @velocity-exchange/sdk@0.21.0

## 0.14.1

### Patch Changes

- Updated dependencies [[`6c183e7`](https://github.com/velocity-exchange/velocity-v1/commit/6c183e7a9d45f4987055032efcd8267651a231a4)]:
  - @velocity-exchange/sdk@0.20.0

## 0.14.0

### Minor Changes

- [#465](https://github.com/velocity-exchange/velocity-v1/pull/465) [`6f4deee`](https://github.com/velocity-exchange/velocity-v1/commit/6f4deeec6997c868403bb7138dd847ead6176b11) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `lut show` and `lut extend`: inspect and extend the market address lookup table. The market account set is derived from `State`'s spot and perp market counts rather than a hardcoded list, so it cannot go stale when a market is added, and addresses already present are skipped. Defaults to the environment's configured table, refuses a frozen table, and checks the resulting size against the 256 entry limit.

### Patch Changes

- [#467](https://github.com/velocity-exchange/velocity-v1/pull/467) [`be89e60`](https://github.com/velocity-exchange/velocity-v1/commit/be89e60ce61d33653817e35bcd2c640fc9204c6f) Thanks [@0xahzam](https://github.com/0xahzam)! - Authorize the `VammQuoteManagement` hot role for scoped vAMM quoting setters, enforce protocol wide safety bounds for every hot role value, and keep oracle, MM reset, and formulaic k controls on warm/cold admin. Adds a direct `perp-market set-spread-adjustment` admin CLI command, tightens every `perp-market` positional to a strict decimal-integer parse (previously `Number('')`/`parseInt('0x10', 10)` silently resolved to market 0, and `new BN(' ')` hung the process), and lets `getUpdatePerpMarketAmmSpreadAdjustmentIx` / `getUpdatePerpMarketFundingBiasSensitivityIx` take an explicit `admin` authority so the CLI can route these setters through the hot role Squads vault instead of defaulting to cold admin.

- Updated dependencies [[`a720d5b`](https://github.com/velocity-exchange/velocity-v1/commit/a720d5b5abdc6258fd6a171282c7e46c5378be4e), [`7e8ff7c`](https://github.com/velocity-exchange/velocity-v1/commit/7e8ff7ca876aad9e985d6fa0a1fd060614df4b8d), [`106aaeb`](https://github.com/velocity-exchange/velocity-v1/commit/106aaeb44eb4a3d0a6f1ad5f0c767b6f1e5adebe), [`be89e60`](https://github.com/velocity-exchange/velocity-v1/commit/be89e60ce61d33653817e35bcd2c640fc9204c6f)]:
  - @velocity-exchange/sdk@0.19.0

## 0.13.0

### Minor Changes

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`c982bc8`](https://github.com/velocity-exchange/velocity-v1/commit/c982bc8749ee54d0b8abc80aa927589a56f6836a) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `batch` command: build several IDL-driven instructions from one payload file ({ instructions: [{ ix, args, accounts }, ...] }, each entry the `call` payload shape plus the instruction name) and dispatch them as a single transaction or vault proposal, so related admin changes share one approval round and one timelock.

- [#458](https://github.com/velocity-exchange/velocity-v1/pull/458) [`76e8a12`](https://github.com/velocity-exchange/velocity-v1/commit/76e8a12ccec2a508bbefd469c864604939cc6f83) Thanks [@0xahzam](https://github.com/0xahzam)! - Connection profiles and authority introspection. `config init` builds named profiles (keypair, env, optional multisig, optional own RPC) interactively and verifies every ingredient against the live cluster before saving: RPC classified by genesis hash, keypair loaded, multisig matched against the onchain State admins. RPC URLs are shared per cluster (`config set-rpc`); profiles without their own url inherit the shared one for their env. Select a profile with `-p/--profile`, `VELOCITY_ADMIN_PROFILE`, or the configured default, with explicit flags always overriding. Every chain-touching command prints a context header (cluster by genesis hash, profile, signer, dispatch mode) and dies on a declared env that contradicts the RPC's actual cluster; when env is not declared it is adopted from the chain. Mainnet direct sends ask for interactive confirmation (`--yes` skips, non-TTY implies it). New `whoami` reports which State admin tiers and hot roles the signer holds plus multisig membership, and `multisig proposals` lists recent proposals with status, approval counts, and timelock ETAs.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`8663104`](https://github.com/velocity-exchange/velocity-v1/commit/866310405ee3cc99ea37dde1a3e81a482d115c3b) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `fees set-schedule` command: rewrite the perp fee schedule in one instruction — taker fee in bps for the three live tiers (unused tiers mirror tier 2), with options to set the maker rebate, referrer/referee percentages, and the amm/if fee split in the same update. Fetches the current structure and patches only what is passed.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`eed4c36`](https://github.com/velocity-exchange/velocity-v1/commit/eed4c36c102d9c5e9643e038498c9c8fb4c9e92b) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `multisig close-accounts` command: reclaim rent by closing the VaultTransaction + Proposal accounts of settled proposals (Executed/Rejected/Cancelled, plus stale non-approved ones; approved-but-unexecuted proposals are never touched). Requires the multisig's rent collector to be configured — rent is paid to it.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`3996355`](https://github.com/velocity-exchange/velocity-v1/commit/39963555a36bf4076003d07f57bd5311f0f77b8e) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `multisig execute` command: execute an approved vault transaction as a member with an explicit compute-unit limit (default 1.4M) and optional priority fee. The Squads UI executes at the 200k CU default, which CPI-heavy inner transactions such as Jupiter swaps exceed.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`eed4c36`](https://github.com/velocity-exchange/velocity-v1/commit/eed4c36c102d9c5e9643e038498c9c8fb4c9e92b) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `multisig inspect` command: decode a vault transaction's inner instructions with account keys resolved through its lookup tables, and simulate its execution with a full compute budget, reporting the error or the compute units consumed. Before approval the simulation reports InvalidProposalStatus, which is the proposal gate rather than a broken transaction.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`8d55b02`](https://github.com/velocity-exchange/velocity-v1/commit/8d55b027cd327ad51e52bc764a7528a3ee957d28) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `multisig set-rent-collector` command: propose a Squads config transaction setting the multisig's rent collector, the prerequisite for `close-accounts` rent reclamation. Refuses multisigs governed by a config authority, and warns that executing a config transaction marks still-Active vault proposals stale.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`b18a559`](https://github.com/velocity-exchange/velocity-v1/commit/b18a559f9f6efb997a689c65ead6259364e9220e) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `show perp-markets` and `show spot-markets` read-only inspectors (per-market risk, quoting, and lending params, deposit-cap headroom, fee/pnl pool and insurance vault balances), and a `--to-token-account` flag on `wallet transfer` for sending directly to a program vault token account instead of a derived ATA.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`c982bc8`](https://github.com/velocity-exchange/velocity-v1/commit/c982bc8749ee54d0b8abc80aa927589a56f6836a) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `spot-market set-max-token-deposits` command: update a spot market's hard deposit cap (raw token base units, 0 = uncapped) as warm/cold admin, with the usual multisig proposal routing.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`3996355`](https://github.com/velocity-exchange/velocity-v1/commit/39963555a36bf4076003d07f57bd5311f0f77b8e) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `wallet balances` command: read-only view of an owner wallet (signer, `--authority`, or a Squads vault PDA at `--vault-index`) showing native SOL, SPL token balances (spot-market mints labeled by market name), Velocity spot deposits/borrows per sub-account, and insurance-fund stakes with share counts.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`1faec62`](https://github.com/velocity-exchange/velocity-v1/commit/1faec62b049ebf44a41d6e26db19a43357c71522) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `wallet swap` command: Jupiter swap from the signer wallet or a Squads vault PDA at any `--vault-index`, with `--slippage-bps`, `--only-direct-routes`, and `--dry-run`. Compute-budget instructions are stripped from the inner message (not CPI-able from the vault executor) and lookup tables are carried through proposal creation; direct sends with lookup tables go out as v0 transactions.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`8663104`](https://github.com/velocity-exchange/velocity-v1/commit/866310405ee3cc99ea37dde1a3e81a482d115c3b) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `wallet transfer` command: SPL token transfer from the signer wallet or a Squads vault PDA to a recipient's associated token account (created idempotently), using transferChecked against the mint decimals read on chain, with a source-balance preflight and the usual multisig proposal routing.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`1773e6c`](https://github.com/velocity-exchange/velocity-v1/commit/1773e6c7ed3ce7ec7324176d2544372bcf513517) Thanks [@0xahzam](https://github.com/0xahzam)! - Add `wallet wrap-sol` command: wraps native SOL from the signer wallet or a Squads vault PDA into its wSOL ATA (ATA created idempotently, syncNative in the same transaction), with the usual `--multisig` proposal routing, `--dry-run`, and a `--min-remaining` guard so the wallet keeps SOL for rent and fees.

### Patch Changes

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`810440c`](https://github.com/velocity-exchange/velocity-v1/commit/810440c3c08d7793c954012f183d8d5d4543b550) Thanks [@0xahzam](https://github.com/0xahzam)! - `--dry-run` now reports the size of the proposal transaction against the 1232-byte limit, and says how far over it is when a batch will not fit. Previously an oversized batch compiled and dry-ran cleanly, then failed only at propose time with `Transaction too large`. The estimate builds the same instructions and memo the real dispatch uses, since the memo is stored inline and affects the size.

- [#463](https://github.com/velocity-exchange/velocity-v1/pull/463) [`f105cdd`](https://github.com/velocity-exchange/velocity-v1/commit/f105cdd424cd3b2a6a7e1e7a324d4ef2c3cd52dc) Thanks [@0xahzam](https://github.com/0xahzam)! - Harden `wallet swap` against a compromised swap API: every instruction returned by the swap-instructions endpoint is validated against a program allowlist (Jupiter v6, SPL Token, Token-2022, Associated Token Program, System Program) and rejected if it requires a signature from any account other than the owner. The instruction program list is printed before signing or proposing so reviewers see what is actually being signed.

- [#452](https://github.com/velocity-exchange/velocity-v1/pull/452) [`1baadf0`](https://github.com/velocity-exchange/velocity-v1/commit/1baadf0b90f744098d758d88267455068e049f3f) Thanks [@0xahzam](https://github.com/0xahzam)! - Fix the account-extension migration path against a live cluster: `auth set-hot-admin` no longer subscribes the client (it only needs the state PDA and the signer, and must work while zero-copy accounts are pre-extension size), and `extend-account` looks up coder account names in camelCase, matching anchor's Program-converted IDL (`perpMarket`, not `PerpMarket`).

- [#455](https://github.com/velocity-exchange/velocity-v1/pull/455) [`48b8529`](https://github.com/velocity-exchange/velocity-v1/commit/48b85296c60250316ae30e3f980237af97591ec4) Thanks [@0xahzam](https://github.com/0xahzam)! - `getUpdateHotAdminIx` accepts an optional `admin` authority override, and `auth set-hot-admin` passes the Squads vault PDA through it when `--multisig` is set. Previously the instruction always listed the local wallet as the admin signer, so proposing the rotation through a multisig failed (the vault was not a required signer of any instruction).

- Updated dependencies [[`4b55e4e`](https://github.com/velocity-exchange/velocity-v1/commit/4b55e4e6c7ae161b42d86f12a61da9d2c1003141), [`560a198`](https://github.com/velocity-exchange/velocity-v1/commit/560a198fa8a0f22ba7f3dc7f926164f8ca91dff5), [`48b8529`](https://github.com/velocity-exchange/velocity-v1/commit/48b85296c60250316ae30e3f980237af97591ec4)]:
  - @velocity-exchange/sdk@0.18.0

## 0.12.1

### Patch Changes

- Updated dependencies [[`079d579`](https://github.com/velocity-exchange/velocity-v1/commit/079d579d4fac0f3402a4a9f8fb1aeccf21ac32ba)]:
  - @velocity-exchange/sdk@0.17.0

## 0.12.0

### Minor Changes

- [#447](https://github.com/velocity-exchange/velocity-v1/pull/447) [`ce01885`](https://github.com/velocity-exchange/velocity-v1/commit/ce0188563670520bfcddb689866e37c1fa19ed00) Thanks [@0xahzam](https://github.com/0xahzam)! - Slot-duration transition archive and permissionless sync. `StateAccount` gains `slotDurationTransitionSlots` (first slot of each IBRL regime); `activeSlotDurationFromState` consults the archive first, and new `elapsedMillis` / `elapsedMillisFromSlotDelta` integrate elapsed intervals per slot-duration regime, mirroring the program's `SlotClock`. Program mirrors that measure elapsed time (`getOracleValidity`, `isOracleValid`, `getSpotOracleValidity`, `blockOperation`, `getLiquidationFee`, `calculateMaxPctToLiquidate`, `User.canMakeIdle`) now take a trailing `SlotDurationState` (the decoded `State`) instead of a `SlotDurationMs`. Forward deadlines, MM-oracle gates, and vAMM spread smoothing also use the transition archive instead of one endpoint duration. The admin instruction `updateStateSlotDurationMs` was replaced by the permissionless `syncStateSlotDuration` (`AdminClient.syncStateSlotDuration` / `getSyncStateSlotDurationIx`; `IBRL_FEATURE_WARMUP_SLOTS` removed, the effective slot now derives onchain from the `EpochSchedule` sysvar). CLI: `exchange set-slot-duration-ms` is now `exchange sync-slot-duration`. Auction durations (`Order.auctionDuration`, `OrderParams.auctionDuration`) now mean wall-clock 400ms units instead of live slots (identical raw values at the 400ms baseline); auction mirrors (`isAuctionComplete`, `getAuctionPrice*`, `getLimitPrice`, `hasLimitPrice`, `hasAuctionPrice`, `isRestingLimitOrder`, `DLOBNode.getPrice`) take a trailing optional `SlotDurationState`, and `DLOB.slotDurationState` carries it for book math. When converting an auction duration from ms, divide by 400 (ceil), not by the live slot duration.

### Patch Changes

- Updated dependencies [[`ce01885`](https://github.com/velocity-exchange/velocity-v1/commit/ce0188563670520bfcddb689866e37c1fa19ed00), [`823724e`](https://github.com/velocity-exchange/velocity-v1/commit/823724e4a8ea0d34b5a79883512eec9cb40b6123)]:
  - @velocity-exchange/sdk@0.16.0

## 0.11.1

### Patch Changes

- Updated dependencies [[`d3824be`](https://github.com/velocity-exchange/velocity-v1/commit/d3824be0f2261e709477e8a1aceedcd11da842c5), [`fe0adbd`](https://github.com/velocity-exchange/velocity-v1/commit/fe0adbd72d77eefaada569292bca2f5baf1e1e58)]:
  - @velocity-exchange/sdk@0.15.0

## 0.11.0

### Minor Changes

- [#429](https://github.com/velocity-exchange/velocity-v1/pull/429) [`4124e93`](https://github.com/velocity-exchange/velocity-v1/commit/4124e9313dd70610a705817570bd9e428c8dea85) Thanks [@0xahzam](https://github.com/0xahzam)! - Referrer rewards split into a Standard and an Accelerated rate. Standard stays per-fee-tier
  (`FeeTier.referrerRewardNumerator`, whose fresh default drops from 15% to 10%); Accelerated is
  the fixed `ACCELERATED_REFERRER_REWARD_PERCENT` constant, independent of the tier. The referee
  discount keeps reading the fee tier. `UserStatsAccount.acceleratedReferralStatus` mirrors the
  new onchain field, with the `AcceleratedReferralStatus` flags, the
  `AcceleratedReferralStatusChange` action enum, and the
  `AcceleratedReferralStatusChangedRecord` event (subscribed by default). `AdminClient` gains
  `updateUserAcceleratedReferralStatus`, wrapped by the admin CLI as
  `user set-accelerated-referral` alongside `fees set-referral-rate`. Automatic enrollment is
  gated by a beta-scoped program constant rather than a state field, so there is no client
  surface to toggle it.

  Fill instruction builders now append the referred taker's referrer `UserStats` (readonly) after
  the taker's `RevenueShareEscrow`, which is what selects the Accelerated rate. The account is
  optional onchain, so a client that omits it still fills at the Standard rate.
  `getFillPerpOrderIx` takes a new trailing `takerReferrer` argument and `ReferrerMap` exposes
  `getReferrerAuthority`; passing the referrer keeps the fill path free of an extra `UserStats`
  fetch.

- [#388](https://github.com/velocity-exchange/velocity-v1/pull/388) [`77499bb`](https://github.com/velocity-exchange/velocity-v1/commit/77499bb3c0644730d5d48e6e3b331988cc5c2b02) Thanks [@0xahzam](https://github.com/0xahzam)! - Rework the perp fee schedule. Fee tiers cut from 6 to 3 (Regular / VIP 1 / VIP 2) with new 30d-volume thresholds ($5M / $80M) and new defaults (4/3/2bps taker, flat -0.25bp maker rebate); `getUserFeeTier` mirrors the new thresholds, projects the rolling-volume decay to now (demotion tracks the live trailing window), and applies the new promotional tier floor. New onchain knobs with SDK/CLI surface: per-market additive taker-fee surcharge (`PerpMarketAccount.takerFeeAddonTenthBps`, unsigned, applied by `getMarketFees` before `feeAdjustment`; `AdminClient.updatePerpMarketTakerFeeAddon`, `velocity-admin fees set-taker-addon`) and the promo fee-tier floor (`StateAccount.promoFeeTier`, effective tier = max(volume tier, promo tier), 0 = off; `AdminClient.updatePromoFeeTier`, `velocity-admin fees set-promo-tier`).

- [#392](https://github.com/velocity-exchange/velocity-v1/pull/392) [`4d0946b`](https://github.com/velocity-exchange/velocity-v1/commit/4d0946b70b336cf71cfcdca202a47dc9d8c81e05) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - Accrued builder/referrer revenue share can now be collected without the escrow owner's participation, and is paid out rather than written off when a market is delisted.

  `settleRevenueShare` / `getSettleRevenueShareIx` wrap the new permissionless `settle_revenue_share` instruction, which settles one escrow's rows for one perp market out of that market's pnl pool. Previously the only payer ran inside `settlePNL` and only when that settle actually moved PnL, so once an escrow owner flattened and stopped trading a market their beneficiaries' fees were stranded and the market's `pendingRevenueShare` kept reserving pnl-pool value against a claim nobody could settle.

  `forfeitRevenueShareOrder` / `getForfeitRevenueShareOrderIx` wrap `forfeit_revenue_share_order`, which writes off a row of a market in settlement or delisted that provably cannot be paid — the beneficiary has no payout account, the wound-down pool cannot cover it, or it names no reachable beneficiary. Anything still payable is rejected with `RevenueShareOrderNotForfeitable` (6373).

  Delisting a market now requires that revenue share to have been resolved: `settle_expired_market_pools_to_revenue_pool` rejects with `UnsettledRevenueShareOnDelist` (6372) while `pendingRevenueShare` is non-zero. There is no time-based escape, because between the two instructions above every row is terminally resolvable. A delisted market therefore always reports `pendingRevenueShare` as zero, and consumers must not treat a delisted market's counter as an outstanding liability.

  `RevenueShareEscrowMap.getEscrowsOwingRevenueShare(marketIndex)` returns the escrows still owed on a market — the work list to clear before delisting. `calculateRevenueShareSweepAvailable`, `calculateBankruptcyIfTrancheReservation` and `calculateBankruptcyIfFloor` (`math/market`) mirror the reservation the on-chain sweep applies, so a keeper can predict whether a call will pay before sending it.

  CLI: new `velocity-admin fees settle-revenue-share <market> [escrowAuthority]`, with `--all` to scan a market, settle every escrow still owed, and forfeit any stragglers that cannot be paid.

- [#425](https://github.com/velocity-exchange/velocity-v1/pull/425) [`193c357`](https://github.com/velocity-exchange/velocity-v1/commit/193c35720365eefac9bfe9fbf1b241cf809029ff) Thanks [@0xahzam](https://github.com/0xahzam)! - Slot-duration scaling for the Solana slot-time reduction (400 -> 350 -> 300 -> 250 -> 200ms feature gates). New `State` fields `slotDurationMs` (0 = unset = 400ms baseline), `pendingSlotDurationMs`, and `slotDurationEffectiveSlot`, plus the `updateStateSlotDurationMs` admin instruction. The instruction _stages_ the next value during the target IBRL gate's warmup: it accepts only the exact next value on the 400 -> 350 -> 300 -> 250 -> 200 schedule, reads the switch slot from the gate's feature account (passed as a remaining account; the account activation slot is exposed one epoch ahead), and records it as `pendingSlotDurationMs` + `slotDurationEffectiveSlot`. `State` then switches itself at that slot in lockstep with the chain, no second transaction. `updateStateSlotDurationMs`/`getUpdateStateSlotDurationMsIx` fill in the feature account automatically and take an optional explicit `admin` pubkey (default `warmAdmin` when set else `coldAdmin`). Resolve the live value with the new `activeSlotDurationFromState(state, currentSlot)` (the base-only `slotDurationFromState` still exists). Onchain duration arithmetic uses `Millis`; compact account fields use the new transparent `StoredSlotDuration<T, SLOT_MS>`, which preserves `T`'s wire width while recording the slot length assumed by that encoding (the IDL remains primitive-compatible). TypeScript exports branded `Millis`/`SlotDurationMs` with `slotDurationFromState`/`activeSlotDurationFromState`/`millisToSlots`/`millisToSlotsCeil`/`millisFromSlots`/`millisFromStoredUnits`/`divPeriods` (plus number-domain `msToSlotsNum`/`msToSlotsCeilNum`/`slotsToMsNum`), mirroring the onchain `math::time`. `getOracleValidity`, `isOracleValid`, `getSpotOracleValidity`, and `User.canMakeIdle` take an optional trailing `SlotDurationMs` (default the 400ms baseline); `getVammL2Generator` takes a required `slotDuration`; `calculateBidPrice`/`calculateAskPrice`/`calculateUpdatedAMMSpreadReserves`/`calculateTradeSlippage`/`calculateTradeAcquiredAmounts`/`calculateTargetPriceTrade`/`calculateBaseAssetValue` take an optional trailing `slotDuration` (default the 400ms baseline); `VelocityClient.getMMOracleDataForPerpMarket` takes an optional trailing `currentSlot` (pass a live slot for correct post-transition validity); `calculateMaxPctToLiquidate` takes its ramp length as `Millis` (decode the stored field with `millisFromStoredUnits`). New `getLiquidationFee` and `blockOperation` helpers mirror the program's duration-aware liquidation-fee and funding-block decisions. Force-close perp auctions now convert their legacy 32-second duration through the live slot length instead of hardcoding 80 slots. Renames: `IDLE_TIME_SLOTS` -> `IDLE_TIME` (Millis), `MM_ORACLE_MIN_SLOT_GAP` -> `MM_ORACLE_MIN_WRITE_GAP` (Millis), `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` -> `MM_ORACLE_MAX_SOURCE_AGE` (Millis). `SLOT_TIME_ESTIMATE_MS` is deprecated. Admin CLI gains `exchange set-slot-duration-ms` (validates an exact integer, prints current -> new, previews the target IBRL gate's activation/effective slots from the on-chain feature account, and dispatches under the correct authority for direct or `--multisig` use). New SDK exports `getIbrlFeatureGate(slotDurationMs)` and `IBRL_FEATURE_WARMUP_SLOTS` support that preview.

  VLP constituent initialization and updates now reject oracle-staleness thresholds above 1,000,000 historical 400ms units, matching the defensive margin-oracle ceiling.

- [#387](https://github.com/velocity-exchange/velocity-v1/pull/387) [`6e34ce3`](https://github.com/velocity-exchange/velocity-v1/commit/6e34ce3a14292eb4f6ceecfc67cde2e15590bd35) Thanks [@0xahzam](https://github.com/0xahzam)! - Add the vAMM maker rebate feature flag. New onchain `FeatureBitFlags::VammMakerRebate` (bit 8, off by default): when enabled, the vAMM earns the maker rebate on fills it makes against a taker, carved off the taker-fee remainder before the protocol/IF/AMM split and folded into the AMM's fee provision. The taker's fee is unchanged; only the distribution shifts. SDK: `FeatureBitFlags.VAMM_MAKER_REBATE`, `AdminClient.updateFeatureBitFlagsVammMakerRebate` / `getUpdateFeatureBitFlagsVammMakerRebateIx`. Admin CLI: `velocity-admin feature-flags vamm-maker-rebate <true|false>`.

### Patch Changes

- [#397](https://github.com/velocity-exchange/velocity-v1/pull/397) [`1004b31`](https://github.com/velocity-exchange/velocity-v1/commit/1004b31a45f0da9cf8faed18c5c82f2351730c75) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `PerpMarketAccount.pendingBankruptcyClaims` mirrors the new per-market counter of unresolved
  bankrupt quote debts. While it is above zero the program's fee sweep withholds the whole
  `feeLedger.pendingIfFee`, so a permissionless sweep cannot drain the bankruptcy first-loss tranche
  between the latch and the resolution. `PositionFlag.BankruptcyClaim` marks the position whose debt
  is counted. `AdminClient.settleExpiredMarketPoolsToRevenuePool` now fails while that counter is above
  zero, because the delist sweep bypasses the floor; resolve every bankruptcy in the market
  first.

  `bankruptcyIfFloorPct` changes meaning: `0` now selects `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT` (10 bps),
  which is what a market written before the field existed reads, and the new
  `BANKRUPTCY_IF_FLOOR_DISABLED` sentinel turns the standing floor off. Callers that passed `0` to
  `AdminClient.updatePerpMarketBankruptcyIfFloorPct` to disable the floor must pass the sentinel
  instead. The admin CLI accepts `perp-market set-bankruptcy-if-floor <market> disabled`.

- [#355](https://github.com/velocity-exchange/velocity-v1/pull/355) [`b7b5ae8`](https://github.com/velocity-exchange/velocity-v1/commit/b7b5ae80040b66651e6553d16354cbd075113cbb) Thanks [@0xahzam](https://github.com/0xahzam)! - Harden the equity breaker recovery path. Cure transfers: `transferDepositByDelegate` with a zero floor delta into a subaccount below its buffered floor now passes onchain while the breaker is tripped, so a breach can be topped up from internal surplus instead of requiring fresh deposits; `EquityFloorManager` gains `planCureTransfers()` and `cureBreaches()` (plus the pure `planCureMoves`) to plan and submit those transfers, deepest breach first, without drawing any donor below its own buffered floor. Self-verifying reset: `resetEquityFloorBreaker` now carries every live subaccount of the authority (count pinned by `UserStats.numberOfSubAccounts`) plus their markets and oracles, and reverts with the new `InvalidEquityBreakerReset` (6368) unless every floored subaccount clears its floor + buffer at execution time, so a stale approval fails instead of unfreezing a breached authority; `AdminClient.resetEquityFloorBreaker`/`getResetEquityFloorBreakerIx` build the account set automatically, with an optional `userAccounts` override for connections without `getProgramAccounts`. Neither path clears the flag automatically; the admin reset remains the only unfreeze.

- [#386](https://github.com/velocity-exchange/velocity-v1/pull/386) [`4e29bc0`](https://github.com/velocity-exchange/velocity-v1/commit/4e29bc0f131ad278450042e2554fd64bac4315ee) Thanks [@0xahzam](https://github.com/0xahzam)! - `user equity-floor-status` reads the fail-closed floor metric (`getFloorNetEquity`) and reports "invalid oracle: floor gates blocked" instead of describing the removed lower bound.

- [#380](https://github.com/velocity-exchange/velocity-v1/pull/380) [`ff3b884`](https://github.com/velocity-exchange/velocity-v1/commit/ff3b8841df91275b6bbace2bf5449b8457d38e95) Thanks [@ChewingGlass](https://github.com/ChewingGlass)! - `user equity-floor-status` now reports net equity, the quantity every onchain floor gate compares, instead of the initial-margin total collateral (which never subtracts spot borrows and applies asset weights). It reads the lower equity bound at the current slot and appends a stale-oracle note when any oracle is invalid, so the printed figure is not mistaken for exact.

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

- Updated dependencies [[`4124e93`](https://github.com/velocity-exchange/velocity-v1/commit/4124e9313dd70610a705817570bd9e428c8dea85), [`74786b4`](https://github.com/velocity-exchange/velocity-v1/commit/74786b44c1009369c98d920b10d8f322a2214e26), [`1004b31`](https://github.com/velocity-exchange/velocity-v1/commit/1004b31a45f0da9cf8faed18c5c82f2351730c75), [`48e9301`](https://github.com/velocity-exchange/velocity-v1/commit/48e930147f8110454ef83f13f27a8ce8b921791a), [`01a7131`](https://github.com/velocity-exchange/velocity-v1/commit/01a71316b0327acd32be6e90686edd296e592af6), [`7ee2feb`](https://github.com/velocity-exchange/velocity-v1/commit/7ee2febf9c4bfe9cb0e7361828a1aad087216df7), [`94bb6ce`](https://github.com/velocity-exchange/velocity-v1/commit/94bb6ce94aad1981e4ee7910a85ab9ffbfe1d7c3), [`b7b5ae8`](https://github.com/velocity-exchange/velocity-v1/commit/b7b5ae80040b66651e6553d16354cbd075113cbb), [`06fac9e`](https://github.com/velocity-exchange/velocity-v1/commit/06fac9ed1584d51a6599dfb673977c0a4626c943), [`4e29bc0`](https://github.com/velocity-exchange/velocity-v1/commit/4e29bc0f131ad278450042e2554fd64bac4315ee), [`02078e6`](https://github.com/velocity-exchange/velocity-v1/commit/02078e625eb89c3fd5798af8d07693a21268a30e), [`77499bb`](https://github.com/velocity-exchange/velocity-v1/commit/77499bb3c0644730d5d48e6e3b331988cc5c2b02), [`fccd4f6`](https://github.com/velocity-exchange/velocity-v1/commit/fccd4f63d7522eca86d79aa8ec93092af2b63b7f), [`a6bffcb`](https://github.com/velocity-exchange/velocity-v1/commit/a6bffcb20a909f98552ef8f3adee8b5665e4257a), [`4227e3e`](https://github.com/velocity-exchange/velocity-v1/commit/4227e3e6fe3805cd0986f81ca6cc4a7513c0c460), [`4872b4f`](https://github.com/velocity-exchange/velocity-v1/commit/4872b4f49942c0f2ef830d10214ac26f46464c38), [`aaec40f`](https://github.com/velocity-exchange/velocity-v1/commit/aaec40fe81268dcc5922f8bbdb1301ea635a6dfd), [`b808fbb`](https://github.com/velocity-exchange/velocity-v1/commit/b808fbb90c4fea6bc597929203b25b6b9cf415d5), [`a6bd667`](https://github.com/velocity-exchange/velocity-v1/commit/a6bd667c28c3216ac213556d160aaea8e459191f), [`d3ef5e5`](https://github.com/velocity-exchange/velocity-v1/commit/d3ef5e5ed17e0ac51e8b8eb2fd039c381e2cff30), [`15db231`](https://github.com/velocity-exchange/velocity-v1/commit/15db231101dd2ac6ed3a94d63d0b41e5acecceb3), [`ede187b`](https://github.com/velocity-exchange/velocity-v1/commit/ede187be1060f4790f03f459733e0485096aaf69), [`1b81121`](https://github.com/velocity-exchange/velocity-v1/commit/1b8112143db861aab3507df64028911285425827), [`1a6af18`](https://github.com/velocity-exchange/velocity-v1/commit/1a6af1819be7822e56009e444d82c7a2fa84aed9), [`ae71278`](https://github.com/velocity-exchange/velocity-v1/commit/ae7127876ef98465ab53d611ee3447db73b224a2), [`4d0946b`](https://github.com/velocity-exchange/velocity-v1/commit/4d0946b70b336cf71cfcdca202a47dc9d8c81e05), [`e8a894c`](https://github.com/velocity-exchange/velocity-v1/commit/e8a894c90edd03814330206b8f666591be72a774), [`193c357`](https://github.com/velocity-exchange/velocity-v1/commit/193c35720365eefac9bfe9fbf1b241cf809029ff), [`dbea9aa`](https://github.com/velocity-exchange/velocity-v1/commit/dbea9aae45f27f8800cc80480443974ce68c031d), [`64301e1`](https://github.com/velocity-exchange/velocity-v1/commit/64301e1f19257152bf3c51174f4549dfbbdc9009), [`6e34ce3`](https://github.com/velocity-exchange/velocity-v1/commit/6e34ce3a14292eb4f6ceecfc67cde2e15590bd35), [`fabc75c`](https://github.com/velocity-exchange/velocity-v1/commit/fabc75ce6daeb7faf75909ceacaac8ffac257bad), [`98e787d`](https://github.com/velocity-exchange/velocity-v1/commit/98e787decb6153bacf6ec7f25e867cdcf217b413)]:
  - @velocity-exchange/sdk@0.14.0

## 0.10.2

### Patch Changes

- Updated dependencies [[`872edd6`](https://github.com/velocity-exchange/velocity-v1/commit/872edd66c5d94a01d6c694a06b03c4ed20c2054c)]:
  - @velocity-exchange/sdk@0.13.0

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
