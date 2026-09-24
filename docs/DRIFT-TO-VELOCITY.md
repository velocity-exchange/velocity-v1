# Migrating from Drift Protocol v2 to Velocity

This repo is a fork of [`velocity-exchange/protocol-v2`](https://github.com/velocity-exchange/protocol-v2)
(fork point `0ae3e3b1d`, SDK `v2.163.0-beta.0`, April 2026). The original Drift program is
paused. Velocity is an entirely new program deployment with a new program ID, a reduced
feature set, and a renamed SDK.

This document tracks everything that changed between the two repos from an integrator's
point of view. It reflects the current state of `master`, and every PR referenced below is
merged.

A note on the numbers in this document. Account sizes are the full account-data length,
including the 8-byte Anchor discriminator. Field offsets are offsets into the Rust struct,
so add 8 to get the offset into account data.

---

## 1. At a glance

|                   | Drift (old)                                   | Velocity (new)                                                   |
| ----------------- | --------------------------------------------- | ---------------------------------------------------------------- |
| Program ID        | `dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH` | `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P` (devnet and mainnet) |
| npm package       | `@drift-labs/sdk` `2.163.0-beta.0`            | `@velocity-exchange/sdk` `0.2.x` (version reset)                 |
| Main client class | `DriftClient`                                 | `VelocityClient` (no back-compat aliases)                        |
| Anchor            | 0.29.0                                        | 1.0 (`@anchor-lang/core@1.0.1`), new IDL format                  |
| IDL               | `drift.json`                                  | `velocity.json`                                                  |
| Rust crate        | `drift` (`programs/drift/`)                   | `velocity` (`programs/velocity/`)                                |
| Package manager   | yarn                                          | bun                                                              |

Because the program ID is new, every PDA address changes. The seed strings are unchanged,
but the program ID that feeds derivation is different. No on-chain state carries over
either, so users, markets, and balances start fresh on Velocity.

Anchor derives account and instruction discriminators from names rather than from the
program ID. The discriminators for surviving accounts and instructions (`User`,
`PerpMarket`, `place_perp_order`, and so on) are therefore byte-identical to Drift's. The
account layouts behind them did change (see §5), so do not point an old decoder at a
Velocity account.

---

## 2. Feature removals

These Drift features do not exist on Velocity. Integrations touching them must be removed
or reworked.

| Feature                                                   | Removed in | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| --------------------------------------------------------- | ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Spot DLOB trading                                         | #6         | `place_spot_order`, `place_and_take_spot_order`, `place_and_make_spot_order` and `fill_spot_order` are deleted. New error `SpotDlobTradingDisabled` (6350). Spot markets still exist for collateral and borrow-lend, but cannot be traded on the order book. |
| External spot fulfillment (Serum, Phoenix, OpenBook v2)   | #36        | All `*_fulfillment_config` instructions and SDK subscribers (`serumSubscriber`, `phoenixSubscriber`, `openbookV2Subscriber`, fulfillment config maps) are deleted. |
| Fuel (points and incentives)                              | #36        | All `*_fuel` instructions, `User.last_fuel_bonus_update_ts`, `PerpMarket.fuel_boost_*`, SDK `math/fuel`, `FuelSeasonRecord` and `FuelSweepRecord` are deleted. |
| vAMM LP ("BAMM" LP shares)                                | #36        | `PerpPosition.lp_shares` and friends are removed, and the `LPRecord` / `LPAction` types are deleted. The VLP module replaces them (§3). |
| Protected maker mode                                      | #38        | Deleted: all `protected_maker_*` instructions (`update_user_protected_maker_orders`, `update_perp_market_protected_maker_params`, `initialize_protected_maker_mode_config`, `update_protected_maker_mode_config`), the `ProtectedMakerModeConfig` on-chain account (PDA seed `protected_maker_mode_config`), the `UserStatus::ProtectedMakerOrders` bit, and SDK `math/protectedMakerParams`, `math/userStatus`, `getProtectedMakerModeConfigPublicKey`, `AdminClient.initializeProtectedMakerModeConfig`, `updateProtectedMakerModeConfig` and `VelocityClient.updateUserProtectedMakerOrders`. Padding replaces the `PerpMarket.protected_maker_*` fields in place. `InvalidProtectedMakerModeConfig` survives as a `@deprecated` stub. |
| High leverage mode                                        | #2, #47    | Deleted: `enable_user_high_leverage_mode`, `disable_user_high_leverage_mode`, `initialize_high_leverage_mode_config`, `update_high_leverage_mode_config`, `update_perp_market_high_leverage_margin_ratio`. `padding_former_margin_mode: u8` replaces `User.margin_mode` (a `MarginMode` enum) in place, and the `MarginMode` enum is deleted. `padding_former_hlm: [u8; 4]` replaces `PerpMarket.high_leverage_margin_ratio_initial` and `_maintenance`. `InvalidHighLeverageModeConfig` and `CouldNotDeserializeHighLeverageModeConfig` became `Deprecated*` stubs with their numeric codes preserved. SDK `PollingHighLeverageModeConfigAccountSubscriber` and `WebSocketHighLeverageModeConfigAccountSubscriber` are deleted. PR #47 removed the residual `HIGH_LEVERAGE_MIN_MARGIN_RATIO` constant. |
| Prediction markets                                        | #13        | `initialize_prediction_market` is deleted. `ContractType::Prediction` became `ContractType::DeprecatedPrediction`, keeping its discriminant, which is not reused. `InvalidPredictionMarketOrder` became `DepreciatedPredictionMarketOrder` (code 6284). The SDK `ContractType` no longer exposes a `PREDICTION` static. |
| Pyth pull and push (legacy)                               | #7         | Program instructions `initialize_pyth_pull_oracle`, `update_pyth_pull_oracle`, `post_pyth_pull_oracle_update_atomic` and `post_multi_pyth_pull_oracle_updates_atomic` are deleted, so keepers posting pull oracle updates must stop calling them. SDK `pythPullClient` and `pythOracleUtils` are deleted. The `AdminClient.initializePerpMarket` / `initializeSpotMarket` default `oracleSource` changed from `OracleSource.PYTH` to `OracleSource.PYTH_LAZER`. Pyth Lazer is the supported Pyth path. The pull `OracleSource` variants (`PythPull`, `Pyth1KPull`, `Pyth1MPull`, `PythStableCoinPull`) keep their original names. They carry a `@deprecated` doc-comment and were not renamed to `Deprecated*`. |
| Switchboard oracles                                       | #14        | Both classic and on-demand are removed from the SDK (`oracles/switchboardClient`, `oracles/switchboardOnDemandClient`). The `OracleSource` discriminants survive as `Deprecated*`. |
| Legacy referrer-reward fee path                           | #67        | Removes the legacy epoch-capped referrer-reward path routed through `UserStats`. Deleted: `UserStats.fees.total_referrer_reward`, `UserStats.fees.current_epoch_referrer_reward`, `UserStats.next_epoch_ts`, the `FeatureBitFlags::BuilderReferral` bit, `State.builder_referral_enabled()`, and the `MAX_REFERRER_REWARD_EPOCH_UPPER_BOUND` constant. `FeeStructure.referrer_reward_epoch_upper_bound` became `padding`, preserving offset and size while changing the IDL field name. `RevenueShareEscrowAccount` lost four fields (`referrerBoostExpireTs`, `referrerRewardOffset`, `refereeFeeNumeratorOffset`, `referrerBoostNumerator`), and its `reservedFixed` grew from 17 to 24 bytes. In the SDK, `referrerInfo?: ReferrerInfo` is gone from `placeAndMakePerpOrder`, `placeAndMakeSignedMsgPerpOrders`, `fillPerpOrder` and related ix-builders. Referrer rewards now flow only through the escrow-based path (#73). |
| Gov-token (DRIFT) stake fee discount                      | #80        | Staking the governance token in the spot-market-15 insurance fund no longer grants a fee discount. 30-day volume alone determines perp fee tiers. Instructions `update_user_gov_token_insurance_stake` and `update_delegate_user_gov_token_insurance_stake` are deleted, and padding replaces `UserStats.if_staked_gov_token_amount`. Spot market 15 has no special treatment. The gov-specific IF revenue-settle APR cap is removed, and the general cap applies. |
| Protocol-owned insurance fund shares and IF rebalance     | #75        | The IF is 100% staker-owned. Deleted: `admin_withdraw_from_insurance_fund_vault`, `transfer_protocol_if_shares_to_revenue_pool`, `begin_insurance_fund_swap`, `end_insurance_fund_swap`, `initialize_if_rebalance_config`, `update_if_rebalance_config`, `initialize_protocol_if_shares_transfer_config`, `update_protocol_if_shares_transfer_config`, `deposit_into_insurance_fund_stake`, the `IfRebalanceConfig` and `ProtocolIfSharesTransferConfig` accounts, and `HotRole::IfRebalance` with its `State.hot_if_rebalance` key. A single `if_fee_factor` replaces `InsuranceFund.total_factor` and `user_factor`, and is the lending-yield carveout to stakers. Protocol revenue no longer flows through IF shares at all. |
| jit-proxy (JIT-auction helper program + client)            | feat/propamm | The jit-proxy program (`J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ`) and its `@velocity-exchange/jit-proxy` npm package are deleted, along with the velocity-rs `jit_client` module (`JitProxyClient`, `JitIxParams`, `JitTakerParams`, `JitSwiftParams`, `constants::JIT_PROXY_ID`, `SdkError::JitOrderNotFound`) and the keeper-bots-v2 `jitMaker` bot. The program id is retired. Nothing upgrades it and no client references it. The CLOB's `activation_slot` speed bump is the taker protection that replaced the JIT auction (§3), so a JIT maker's role is now a CLOB maker's: rest orders on the book and let the router match them. On-chain, `place_and_make_perp_order` (v0) is removed and `place_and_make_perp_order_v1` no longer names a taker: it builds a post-only maker limit order that rests straight on the market's CLOB (the `taker` / `taker_stats` accounts and the `taker_order_id` arg are gone; the order never occupies a `User.orders` slot). The `jit_maker_order_id` fill parameter and the `FillMode::PlaceAndMake` variant are deleted, and the `OrderFilledWithMatchJit` record label is retired (a match always records `OrderFilledWithMatch`). SDK: `placeAndMakePerpOrder` / `getPlaceAndMakePerpOrderIx` take `clobAccounts` in place of `takerInfo`. |
| AMM JIT (just-in-time auction participation)               | feat/propamm | The vAMM no longer front-runs inside a DLOB match. It is quoted as one level ladder among all sources and the router splits the take across them by priority tier, so a fill never has the AMM JIT-ing at a resting maker's price. Deleted: `AmmJitQuoter`, `vlp::amm::math::jit`, `amm_jit_intensity` gating on the last-look path, and the two legacy engines the JIT ran inside (`fill_amm_only`, `match_take`). `PerpMarket.amm_jit_intensity` stays in the layout but no longer gates anything. Off-chain fill models must drop JIT sizing (the #182 / #269(4) behaviours) entirely |
| Oracle-offset limit orders                                 | feat/propamm | `validate_limit_order` refuses any `OrderType::Limit` order whose `oracle_price_offset` is nonzero with `InvalidOrderOracleOffset` (6055, an existing error). An oracle-floating limit price cannot rest on a CLOB, so such an order could only strand in `User.orders` on the legacy DLOB. An oracle-relative maker quote is a PropAMM quoter's job. The `Order.oracle_price_offset` field, the `OrderParams` field and the IDL are unchanged. Only the placement semantics are refused. `OrderType::Oracle` orders keep the offset, which holds their worst-price bound, and trigger orders never accepted an offset. |
| Order auctions (the price ramp) | feat/propamm | An order no longer ramps its price from a start to an end over a duration. The ramp existed because an order rested in the DLOB and competing fillers watched it cross their price. An order routes to the book now, so the ramp only made the fill price depend on how long the sender took to land the transaction. `Order.auction_start_price` / `auction_end_price` are renamed `clob_node_index` / `clob_order_id` (the placed-trigger shadow's CLOB handle, which already used them) and `auction_duration` becomes `unused_auction_duration`. `Order` stays 104 bytes and `User` is unchanged. `Order.price` is the worst price the order accepts, for every order type. A market order's `price` is its cap, and the sender chooses how far from the oracle it sits. A market order that names no price takes `DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION` (oracle / 200, which is 0.5 percent) as its cap. An oracle-relative order holds the bound in `oracle_price_offset`. `OrderParams` and `ModifyOrderParams` lose the three auction fields. `OrderParams` gains `activation_delay_slots: Option<u32>`, which sets how long a rested remainder waits before the book will take it. That is the duration knob the ramp used to serve. A taker-origin remainder is crossed at the counterparty's price, so a longer wait can only improve the taker's fill. It is the one delay knob: `place_and_take_perp_order_v1`, `place_signed_msg_taker_order` and `place_and_make_perp_order_v1` all read it, and `PlaceAndMakePerpOrderV1Args` no longer carries a separate `activation_delay_slots`. A value below the book's default needs the flow-authority attestation, on the take path as on the make path. A trigger order stores no delay, so `place_trigger_orders_v1` refuses one that names it (`InvalidOrder`). `max_ts` keeps its meaning: it ends the order and has nothing to do with the delay. A market or oracle order that names no `max_ts` lives `DEFAULT_MARKET_ORDER_LIFETIME_SECONDS` (30 seconds). SDK: `placeAndMakePerpOrder`, `getPlaceAndMakePerpOrderIx` and `buildPlaceAndMakePerpOrderInstruction` drop their `activationDelaySlots` argument in favor of `orderParams.activationDelaySlots`, and `modifyOrder` / `modifyOrderByUserOrderId` drop theirs. dlob-server's `/auctionParams` is replaced by `/marketOrderParams`, which quotes a market order's worst price as `price` (or `oraclePriceOffset`) plus an optional `activationDelaySlots`. `docs/clob-client-integration.md` maps the old fields. `User.has_open_auction` is never set, so the SDK drops `getUserWithAuctionFilter` and velocity-rs drops `AuctionSubscriber` and `get_user_with_auction_filter`. swift's worst-price guard is renamed end to end: the env vars `AUCTION_ORACLE_BAND_BPS` and `AUCTION_ORACLE_MAX_STALENESS_SLOTS` become `WORST_PRICE_ORACLE_BAND_BPS` and `ORACLE_BAND_MAX_STALENESS_SLOTS`, the metrics `swift_auction_band_guard_count` and `swift_auction_oracle_staleness_slots` become `swift_oracle_band_guard_count` and `swift_oracle_band_staleness_slots`, and the rejection message is now `Worst price outside oracle band`. keep-rs drops the `auction_fill` transaction intent, which nothing built. Deleted: `math::auction` (`calculate_auction_price*`, `is_auction_complete`, `auction_progress*`, `calculate_auction_prices`, `calculate_auction_params_for_trigger_order`), `OrderParams::update_perp_auction_params*`, `derive_*_auction_params`, `has_valid_auction_params`, `get_perp_baseline_*`, and the `validate_auction_params` / `validate_oracle_auction_params` / `validate_limit_order_auction_params` checks. `FillMode::PlaceAndTake(bool, u8)` becomes the unit variant `FillMode::PlaceAndTake`. `PlaceAndTakePerpOrderV1Args.success_condition` changes from a packed `Option<u32>`, which carried the condition in its low byte and an auction fraction in the next byte, to `Option<PlaceAndTakeOrderSuccessCondition>`. The enum is borsh-encoded, so `PartialFill` is `0` and `FullFill` is `1`. `parse_optional_params` is removed. The SDK's `PlaceAndTakeOrderSuccessCondition` becomes a variant class (`PARTIAL_FILL`, `FULL_FILL`). `placeAndTakePerpOrder`, `getPlaceAndTakePerpOrderIx` and `preparePlaceAndTakePerpOrderWithAdditionalOrders` drop their `auctionDurationPercentage` argument, so every later positional argument moves one place left. `buildPlaceAndTakePerpOrderInstruction` takes `successCondition` in place of `optionalParams`. `Order::get_limit_price` and `force_get_limit_price` drop their `slot` and `slot_clock` arguments. `User.open_auctions` and `has_open_auction` stay in the layout and are always zero. A trigger order is stamped with its worst price when it fires, not when it is armed. A signed message's `max_slot` is its landing deadline, `signed_msg_max_slot`: a resting limit's message slot itself, and any other order's message slot plus `SIGNED_MSG_FILL_WINDOW` (30 seconds), where the auction length used to be. SDK: `math/auction` is deleted and replaced by `math/worstPrice` (`deriveWorstPrice`). `isFallbackAvailableLiquiditySource` moves to `math/orders`. `getLimitPrice`, `hasLimitPrice`, `isRestingLimitOrder` and `isRestingSignedMsgLimitOrder` lose their auction and slot arguments, and `signedMsgOrderMaxSlot` trades its auction duration for `isRestingLimit`. `hasAuctionPrice` is removed. `SIGNED_MSG_FILL_WINDOW_MS` and `DEFAULT_MARKET_ORDER_SLIPPAGE_FRACTION` are added. velocity-rs loses `math::auction`, `math::order` and its `dlob` module. Nothing re-prices a signed order at placement, so the swift feed drops `will_sanitize` from every order message, `SwiftOrderSubscriber.subscribe` drops its `acceptSanitized` argument, `VelocityClient::subscribe_swift_orders` drops `accept_sanitized`, and the `swift_order_types_count` metric drops its `sanitized` label. |

## 3. Feature additions

| Feature                                          | Added in       | Integrator impact                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ------------------------------------------------ | -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| VLP module (`programs/velocity/src/vlp/`)        | #65, #66       | New AMM and hedge architecture. `PerpMarket` gains `hedge_config: HedgeConfig` and `market_stats: MarketStats`. A single `hedgeConfig` sub-object (`{ poolId, status, pausedOperations, exchangeFeeExclusionScalar, feeTransferScalar }`) replaces the five flat LP-pool config fields on `PerpMarketAccount` (`lpPoolId`, `lpStatus`, `lpPausedOperations`, `lpFeeTransferScalar`, `lpExchangeFeeExcluscionScalar`). |
| Tiered admin keys                                | #36, #63       | A cold, warm and hot key model (`cold_admin`, `warm_admin`, `hot_*` keys) replaces `State.admin`. Anyone reading `State.admin` directly must update. PR #76 promoted `update_spot_market_oracle` and `update_spot_market_expiry` from warm-admin to cold-admin only, because swapping an oracle re-prices the withdraw-guard notional cap. |
| Native fast-path entrypoint                      | fork; #63      | Keeper instructions with discriminator `[0xFF, 0xFF, 0xFF, 0xFF, opcode]` bypass Anchor dispatch. Opcode 0 is the MM oracle update, opcode 1 the AMM spread adjustment, opcode 2 the batched MM oracle update. These do not appear in the IDL. The entrypoint is inherited from the Drift fork, and #63 refactored the native admin handlers onto a zero-copy struct cast. The handlers re-establish Anchor's account guarantees before trusting any byte. They load `State` and `PerpMarket` through `AccountLoader`, which checks program ownership and the discriminator, and they take the slot from the `Clock` sysvar. See the `native-path` change-log row. |
| `transfer_deposit_by_delegate` and `update_user_allow_delegate_transfer` | #45 | Delegates can transfer spot deposits between subaccounts once the authority opts in. Before a delegate can call `transfer_deposit_by_delegate`, the owner must call `update_user_allow_delegate_transfer(true)` to set the `AllowDelegateTransfer` bit in `UserStats.delegate_permissions`. SDK: `VelocityClient.updateUserAllowDelegateTransfer(...)` and `transferDepositByDelegate(...)`. `UserStatsAccount` gains a `delegatePermissions: number` field, one byte carved from trailing padding, with the size and all other offsets unchanged at 240 bytes. |
| `transfer_fee_and_pnl_pool`                      | #1             | Admin instruction that rebalances tokens between a perp market's AMM fee pool and PnL pool, in the same market or across two. Requires the warm admin key. SDK: `AdminClient.transferFeeAndPnlPool(perpMarketIndexWithFeePool, perpMarketIndexWithPnlPool, amount, direction)` and `getTransferFeeAndPnlPoolIx(...)`. Direction comes from the new `TransferFeeAndPnlPoolDirection` export (`.FEE_TO_PNL_POOL` or `.PNL_TO_FEE_POOL`). Emits a `TransferFeeAndPnlPoolRecord` event with `ts`, `slot`, `perp_market_index_with_fee_pool`, `perp_market_index_with_pnl_pool`, `direction` and `amount`. |
| Funding rate clamp and floor increase            | #12            | Funding floor raised from 7.3% to 10.95% annualized (`FUNDING_RATE_OFFSET_DENOMINATOR` from 5000 to 3333). Adds a dead-zone clamp. When `\|mark_twap − oracle_twap\|` is at most 0.05% of the oracle price (`FUNDING_RATE_CLAMP_DENOMINATOR = 2000`), the funding premium is suppressed to the offset-only floor value. Low-divergence markets therefore behave differently from Drift. Superseded by the per-market continuous dead zone in #94. |
| Continuous funding dead zone (per-market)        | #94            | Replaces #12's global hard cutoff with a per-market continuous ramp. Two new `AMM` fields occupy the 8 bytes that were `_padding_funding_twap`. They are `funding_clamp_threshold: u32` (noise band, BPS_PRECISION, default 5 bps) and `funding_ramp_slope: u32` (PERCENTAGE_PRECISION, default 1.0x). New admin instruction `update_perp_market_funding_dead_zone(funding_clamp_threshold, funding_ramp_slope)`, with SDK `AdminClient.updatePerpMarketFundingDeadZone(...)` and `getUpdatePerpMarketFundingDeadZoneIx(...)`. `PerpMarketAccount` gains `fundingClampThreshold` and `fundingRampSlope` in place of `paddingFundingTwap`. `PerpMarket` size is unchanged by this PR. |
| MM oracle validation                             | #60            | Adds slot-monotonicity, a minimum 2-slot gap, and a 1% per-write step cap to the existing MM oracle native handler. |
| Special user status                              | #17            | New `User.special_user_status: u8` bitmask field, replacing one byte of padding, with the account size unchanged. New `SpecialUserStatus` SDK enum (`VammHedger = 1`). Two new instructions. `update_special_user_status(status)` is admin and hot-wallet callable (`AdminClient.updateSpecialUserStatus`, `getUpdateSpecialUserStatusIx`). `special_transfer_perp_position_to_vamm(market_index, amount)` is user-callable, the authority signs, and it works only while `special_user_status == VammHedger` (`VelocityClient.specialTransferPerpPositionToVamm`, `getSpecialTransferPerpPositionToVammIx`). New error `InvalidTransferPerpPosition` (6312). |
| Builder codes                                    | #68            | Optional `builder_idx` and `builder_fee_tenth_bps` on `OrderParams`, plus a new `change_approved_builder` instruction and a `RevenueShareEscrow` account. Existing order placements are unaffected, since the fields are optional. |
| Revenue-share fill enforcement                   | #68, #73       | Perp fills fail with `UnableToLoadRevenueShareAccount` (6324 / `0x18b4`) unless the taker's `RevenueShareEscrow` is passed in remaining accounts, in two cases. Either the taker order carries a builder code, or the taker's `UserStats.referrer_status` has the `BuilderReferral` bit, meaning an escrow exists with a referrer. Liquidation fills and the feature-flag-off state are exempt. Fillers must attach the escrow for any taker that has one with a referrer (§4.4). Referral rewards no longer accrue, and referral slots are no longer created, for escrows without a referrer. |
| Funding bias spread widening                     | #77            | New `AMM.funding_bias_sensitivity` field widens the vAMM's paying-side spread while it pays funding, up to `1 + sensitivity/100` at the funding offset floor. New admin instruction `update_perp_market_funding_bias_sensitivity`, with SDK `AdminClient.updatePerpMarketFundingBiasSensitivity`. The default 0 is off, so quotes do not change until it is enabled. The same PR moved `last_funding_oracle_twap` from `PerpMarket` to `MarketStats`, carving it out of `MarketStats.padding`. The old `PerpMarket` slot became `_padding_funding_twap`, which #94 later repurposed as the live `funding_clamp_threshold` and `funding_ramp_slope` fields. All offsets and sizes are unchanged and existing accounts need no migration. SDK: `PerpMarketAccount.lastFundingOracleTwap` is now `marketStats.lastFundingOracleTwap`. |
| Withdraw guard notional cap                      | #76            | `update_withdraw_guard_threshold` now requires the spot market's `oracle` account and rejects any threshold worth more than $10k notional (`MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL`, priced at the max of the live price and the 5-min TWAP). New error `WithdrawGuardThresholdNotionalTooLarge` (6352 / `0x18D0`). SDK `AdminClient.updateWithdrawGuardThreshold(spotMarketIndex, withdrawGuardThreshold, oracle?)` and `getUpdateWithdrawGuardThresholdIx(...)` gain an optional trailing `oracle?`, resolved from the subscription cache or on-chain when omitted. Manual instruction construction must include it. |
| `VelocityCore` SDK module                        | #21            | Subscription-free instruction-building API (`packages/sdk/src/core/`, exported via `export * from './core'`). It provides static helpers for PDAs, account decoding, remaining-accounts construction and signed-msg handling, plus pure instruction builders for deposit, withdraw, orders, fill, trigger, settlement, perp liquidation, place-cancel-modify and funding-rate updates, none of which need a subscribed `VelocityClient`. See §4.5. |
| Vaults program and SDK                           | #83, #85       | The drift-vaults program and its TS client are first-party in this repo. They are the `vaults` program (`programs/vaults/`, ID `vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR`) and the `@velocity-exchange/vaults-sdk` package (`packages/vaults-sdk/`). Renames against upstream drift-vaults: the Rust crate and directory `drift_vaults` became `vaults`; the CPI dep on the core program resolves by its real crate name `velocity` rather than the `drift` alias; SDK type `DriftVaults` became `Vaults`; IDL `drift_vaults.json` became `vaults.json`, generated from the program with `bun run program:idl:vaults` and never hand-edited. The program ID is unchanged. |
| Fee redesign and AMM isolation                   | #75            | Explicit per-fill three-way fee split (`FeeStructure.amm_fee_numerator`, `if_fee_numerator`, with the protocol taking the residual). Per-market `PerpMarket.fee_ledger: FeeLedger` tracks gross fees and pending carveouts. Protocol fees accrue to a withdrawable `protocol_fee_pool` on perp and spot, and leave through `withdraw_protocol_fees_perp` / `withdraw_protocol_fees_spot` under the new `HotRole::FeeWithdraw` key. Those pay the ATA of `State.protocol_fee_recipient_perp` or `_spot`, two separately configurable treasuries, created on demand. The permissionless streaming sweep `sweep_perp_market_fees` materializes carveouts out of the pnl pool and emits `PerpMarketFeeSweepRecord`. The AMM holds only its own accounted equity, and its configurable fee provision is clawed back in bankruptcy as the last backstop. Liquidations gain a `protocol_liquidation_fee` cut, with a new `protocol_fee` field on liquidation records. New errors `InvalidProtocolFeeRecipient` (6353) and `InsufficientProtocolFees` (6354). Full design doc: [`FEES.md`](./FEES.md). |
| Build-gated features: isolated positions and VLP hedge | #201 | Two opt-in cargo features, `isolated-position` and `vlp-hedge`, gate two sets of instructions. `isolated-position` covers `deposit_into_isolated_perp_position`, `transfer_isolated_perp_position_deposit`, `withdraw_from_isolated_perp_position`, and the signed-msg `isolated_position_deposit` field, which is rejected with `IsolatedPositionDisabled` (6357) when gated. `vlp-hedge` covers the VLP hedge and LP-pool component, meaning pool and constituent init and config, swap, add and remove liquidity, the program vault, the AUM and target-base cranks, and `settle_perp_to_lp_pool`. Mainnet builds compile both out until audit, and devnet and test builds keep them, since `anchor-test` implies both. Five admin config instructions stay in every build because they share `#[derive(Accounts)]` structs with ungated admin instructions, and anchor's cpi codegen cannot mix gated and ungated users of one struct. They are `update_perp_market_lp_pool_id`, `update_perp_market_lp_pool_paused_operations`, and the three `update_feature_bit_flags_*_lp_pool` toggles. All five are inert config writes whose readers are compiled out, and the `hedge_config.status` activator is itself gated. Account layouts and the IDL are identical across builds, so `PerpPosition.isolated_position_scaled_balance`, `PerpMarket.hedge_config` and every LP-pool account type remain. Calling a gated instruction on a mainnet deployment hits Anchor's fallback and fails with instruction-not-found. Enabling later means adding the features to the mainnet build invocation and upgrading in place. |
| Per-user equity floor                            | equity-floor, equity-floor-buffer | A warm-admin per-subaccount minimum net equity, with an authority-wide breaker on top. New fields `User.equity_floor: u64` and `User.equity_floor_buffer: u64`, new error codes 6358, 6359 and 6368, and a substantial SDK surface. See [§3.1](#31-per-user-equity-floor) for the full description. |
| Bulk order margin enforcement (per risk-scope)   | #135           | `place_orders` and `place_scale_orders` run the initial-margin check once per risk scope touched by the batch, meaning cross-margin plus each isolated market carrying a risk-increasing order, rather than once at the end of the batch. Before this, an early risk-increasing order's exposure was not accumulated into that check, and the check could be skipped altogether when the final order in the batch was a no-op, so a batch could slip a risk-increasing order past a weaker or absent margin gate. Batches that previously succeeded may now be rejected with `InsufficientCollateral`. There is no SDK-side logic mirror, since this is a program-only enforcement tightening. |
| Account extension (`extend_account`)             | #326 account-extension | New instruction `extend_account` grows a zero-copy account to the size the deployed program compiles in for its discriminator's type. It covers `User`, `UserStats`, `ReferrerName`, `PerpMarket`, `SpotMarket`, `State`, `InsuranceFundStake`, `PrelaunchOracle`, `PythLazerOracle`, `RevenueShare`, `LPPool` and `Constituent`, and is the migration crank for a future upgrade that appends fields to an account struct. Authorization uses the new `HotRole::AccountExtension` variant, appended to the enum, and accepts the cold, warm or configured hot key. That key lives in a new `State.hot_account_extension: Pubkey` carved from tail padding, with `State` unchanged at 1752 bytes. The instruction is grow-only, the target size is compiled in rather than caller-chosen, the payer covers the rent-exempt shortfall, the tail is zero-filled, and it is a no-op at size. Borsh accounts and unknown discriminators are rejected with the new error `InvalidAccountExtension` (6367 / `0x18DF`). A devnet and test-only `extend_account_devnet(new_len)` grows to an arbitrary larger size for exercising the flow, under the same role gate. It is compiled out of production mainnet builds and kept by `anchor-test`. SDK: `VelocityClient.extendAccount`, `getExtendAccountIx`, `extendAccountDevnet`, `getExtendAccountDevnetIx`, `HotRole.AccountExtension`, `StateAccount.hotAccountExtension`. Admin CLI: `velocity-admin extend-account <account>` plus a `--type <t>` batch crank, and `auth set-hot-admin accountExtension` assigns the role. Integrators must not assume fixed account data lengths, so no `dataSize` filters and no exact-size decode. See [`ACCOUNT-EXTENSION.md`](./ACCOUNT-EXTENSION.md). |
| `settle_revenue_share`                           | revshare-settle-liveness | New permissionless instruction that settles one `RevenueShareEscrow`'s accrued builder and referrer rows for one perp market out of that market's pnl pool, without requiring the escrow owner to settle pnl. Accounts: `state`, `escrow_authority`, `revenue_share_escrow` (PDA-seeded), `spot_market_vault`. Args: `market_index: u16`, `num_owner_sub_accounts: u8`. The `remaining_accounts` order is the usual oracle and market set, then `num_owner_sub_accounts` read-only `User` accounts owned by the escrow authority, then the beneficiary `User` and `RevenueShare` accounts, writable. The read-only accounts let the program mark rows whose orders have closed as `Completed`, which is a precondition for paying a builder row. The read-only and writable split delimits the two regions, so a miscount errors rather than misassigning an account. A `Delisted` market is rejected with `MarketDelisted`, since delisting already requires the liability to be zero and drains the pnl pool. SDK: `settleRevenueShare`, `getSettleRevenueShareIx`, `fetchRevenueShareEscrowAccount`. CLI: `velocity-admin fees settle-revenue-share <market> <escrowAuthority>`. |
| `forfeit_revenue_share_order`                    | revshare-settle-liveness | New permissionless instruction that writes off one revenue-share row of a market in `Settlement` or `Delisted` that provably cannot be paid, so the liability counter can always reach zero and delisting is never blocked by an uncollectable claim. Accounts: `state`, `perp_market`, `spot_market`, `escrow_authority`, `revenue_share_escrow` (both PDA-seeded), `beneficiary_user`. Args: `market_index: u16`, `order_index: u32`. It turns on a proof rather than on an omitted account. The three accepted proofs are that the beneficiary has no payout `User`, that the wound-down pool cannot cover the row, or that the row names no reachable beneficiary. For the first, the handler derives the address that account must have from the row's own beneficiary and rejects any other, so the account cannot be substituted or left out. That reason is accepted only after `expiry_ts + escrow_period_before_transfer`, because a beneficiary can create the account at any time. The other two reasons cannot be undone and need no wait. Anything still payable is rejected with `RevenueShareOrderNotForfeitable` and must go through `settle_revenue_share`. It moves no tokens. SDK: `forfeitRevenueShareOrder`, `getForfeitRevenueShareOrderIx`. |
| Auction-duration floor on requested spread       | #282 auction-floor-client-spread | Order sanitization's duration floor, `max(client duration, spread% x tier slots-per-pct)`, measures the narrower of the client-requested and post-sanitize price ranges. Start-price improvements toward baseline no longer inflate auction durations. The effect is largest on tail-tier markets (C and below) with wide baseline spreads, where fully-specified signed-msg orders were floored to the baseline spread regardless of the requested duration. Expect materially shorter auctions for tight-spread orders on those markets. Genuinely wide requested spreads are floored as before. Program-only behavior change with no layout or IDL change (§6). |
| Fee schedule rework (3 tiers, add-on, promo)     | #388 fee-schedule | Perp fee tiers cut from 6 to 3 (Regular, VIP 1, VIP 2) with new hardcoded 30d-volume thresholds of $5M and $80M, replacing $2M, $10M, $20M, $80M and $200M, and new default tier values of 4, 3 and 2 bps taker with a flat -0.25 bp maker rebate, replacing 10 down to 3.5 bps taker with a 2 bp rebate. Tier determination projects the rolling volume decay to the current timestamp at read time through `UserStats::get_total_30d_volume_at`, so demotion tracks the live trailing-30d window at every fill. Promotion was already instant. Two new admin knobs. `PerpMarket.taker_fee_addon_tenth_bps` (u16, carved from `_padding_buffer`) is an unsigned additive taker-fee surcharge applied before `fee_adjustment` scales the sum, on the taker leg only. It is a surcharge only, because a discount could push the taker fee below the maker rebate it funds. It is set by the new warm-admin instruction `update_perp_market_taker_fee_addon`, capped at 100 tenth-bps. `State.promo_fee_tier` (u8, carved from padding) is a promotional tier floor for every account, so the effective tier is `max(volume tier, promo tier)` and 0 disables it. It is set by the new warm-admin instruction `update_promo_fee_tier`. SDK: `PerpMarketAccount.takerFeeAddonTenthBps`, `StateAccount.promoFeeTier`, `getUserFeeTier` mirrors the new thresholds, projection and promo, `getMarketFees` applies the add-on, `AdminClient.updatePerpMarketTakerFeeAddon` and `updatePromoFeeTier`. Admin CLI: `fees set-taker-addon`, `fees set-promo-tier`. Account sizes are unchanged, since both fields occupy former padding. Integrators pinning fee-tier thresholds or the 6-tier layout must update. |
| Fee tier VIP 3                                   | fee-tier-vip3 | A fourth perp fee tier at $200M trailing-30d volume, so the tiers are Regular, VIP 1, VIP 2 and VIP 3 at indices 0 through 3 and `PERP_FEE_TIER_MAX_INDEX` goes from 2 to 3. `fee_tiers[3]` was already a populated-by-default spare slot, since `set-schedule` mirrored tier 2 into it, so there is no layout or IDL change. `update_promo_fee_tier` now accepts 3, and a promo at 3 puts every account on the top tier. The default seed adds a 1.5 bps tier 3, and live rates stay admin params. SDK: `VIP_FEE_TIER_THREE_VOLUME_QUOTE`, plus a third entry in `PERP_FEE_TIER_VOLUME_THRESHOLDS`. Admin CLI: `fees set-schedule` takes four tier fees, and `show fees` prints the VIP 3 row. |
| vAMM maker rebate (feature-flagged)              | #387 vamm-maker-rebate | New `FeatureBitFlags::VammMakerRebate` bit (8) on `State.feature_bit_flags`, off by default. When enabled, the vAMM earns the maker rebate on fills it makes against a taker. The rebate uses the same `maker_rebate_numerator` schedule and `fee_adjustment` scaling as a user maker's, clamped to the remainder. It is carved off the taker-fee remainder before the protocol, IF and AMM split and folded into the AMM's fee provision (`amm_fee`), so it travels the existing fee-ledger and `pending_amm_provision` sweep path. The taker's fee is unchanged, and only the distribution shifts, since the protocol and IF cuts shrink by the rebate share. `OrderActionRecord.makerRebate` stays 0 on these fills, because the rebate is the AMM's rather than a user maker's. New admin instruction `update_feature_bit_flags_vamm_maker_rebate`, cold admin to enable. SDK: `FeatureBitFlags.VAMM_MAKER_REBATE`, `AdminClient.updateFeatureBitFlagsVammMakerRebate`, `getUpdateFeatureBitFlagsVammMakerRebateIx`. Admin CLI: `velocity-admin feature-flags vamm-maker-rebate <true\|false>`. No account-layout or error-code change. |
| Quoter registry (PropAMM order flow)         | feat/propamm   | Two-account registry: a zero-copy `QuoterV0` staging entry per (perp market, quoter program, quoted user), and one `QuoterSlabV0` per market holding every approved config. `initialize_quoter` creates the staging entry (for Custom-type entries the quoted user's authority must be the creating authority, because creation is consent); `update_quoter_accounts` sets one unified CPI account list (max 12) plus per-leg index lists into it in a single call; `update_quoter_config` / `update_quoter_watch` edit staging. Nothing fills from staging: `update_quoter_approved` (warm/cold-admin) copies the staged config into a slab slot, and fills read only that copy. A later staging edit stays inert until the admin copies again, while the vetted copy keeps serving. Slot 0 is reserved for the market's book; Custom quoters occupy slots 1+; response accounts are unique per slab (a route names the quoters it consults by carrying their response accounts). Revoking a Custom slot clears it; revoking the book suspends it so removal paths keep working. Three fields write through to the live slot without re-vetting: `update_quoter_active` (the maker's kill switch), `update_quoter_priority` (admin; at a price, lower tiers fill first, pro rata within; defaults vAMM 0, CLOB 10, Custom 20), and `update_quoter_max_oracle_deviation` (tighten-only band). `initialize_quoter_slab(args)` is permissionless and creates a one-slot slab; approval right-sizes the account from then on (growth paid by the admin, trailing vacancy refunded on revocation), so there is no separate extend instruction. New errors `InvalidQuoterConfig` (6375) / `InvalidQuoterAuthority` (6376) / `QuoterSlabFull` (6405) / `QuoterNotOnSlab` (6406). SDK: `QuoterConfigV0`, `QuoterV0Account`, `QuoterSlotV0`, `QuoterSlabV0Account` type mirrors, `getQuoterSlabPublicKey`, `decodeQuoterSlab`, `VelocityClient.getQuoterSlabAccount`. Admin CLI: `velocity-admin quoter …` command group. |
| Cancel-all (CLOB + midpoint)                 | feat/cancel-all | A maker can withdraw a whole side (or both) in one instruction on either quoter type. Velocity: `cancel_orders_v1` (`CancelOrdersV1Params { market_index: u16, sides: ClobCancelSides }`, accounts `state`, `user` (w), `authority`, `quoter`, `clob_market` (w), `clob_program`, `quoter_signer`, `crank_conditions` (w, optional)) CPIs the CLOB's new `cancel_all_v0` and unwinds `open_bids`/`open_asks` from per-side base totals plus one order count, so sweeping twenty orders costs the same bookkeeping as sweeping one. Placed-trigger shadows are freed by re-checking each shadow's node against the post-sweep book rather than by matching returned ids, so a shadow is released exactly when its order is gone by any route. Not gated on the quoter entry's active/approved flags or on `exchange_not_paused`: a maker must always be able to pull quotes off a killed, de-listed or halted book. The CLOB caps one sweep at `CANCEL_ALL_ORDERS_CEILING` (128) removals and reports `exhaustive`; velocity unwinds what was actually removed, logs when the cap stopped it early, and the call is safe to repeat. New CLOB event `OrdersCancelRecordV0` carries the removed order ids (reference-by-id, like `ExecuteRecordV0`) so an indexer can reconcile the book from one record. Midpoint's `cancel_all_v0` (`CancelAllArgsV0 { sides: CancelSidesV0, clear_mid: bool }`) zeros the live rungs of the named sides and optionally the mid, signed by either maker key (hot or config). The spline has no orders, so this is the equivalent operation, and it reserves nothing on velocity's side. |
| Slot-duration scaling                            | slot-duration-scaling | Solana slot time is dropping from 400ms to 350, 300, 250 and 200ms via feature gates. `State.slot_duration_transition_slots: [u64; 4]` records the first slot of each post-baseline regime, carved out of padding with no layout break. The legacy `slot_duration_ms`, `pending_slot_duration_ms` and `slot_duration_effective_slot` staging trio remains as fallback, where 0 means unset and resolves to the 400ms baseline. The permissionless `sync_state_slot_duration` records each transition from its IBRL feature-gate account, deriving the effective slot from the `EpochSchedule` sysvar as the first slot of the epoch after activation, mirroring Agave. It replaced the warm-admin `update_state_slot_duration_ms`. Wall-clock durations are typed (`math::time::Millis`), and measured intervals integrate per regime through `math::time::SlotClock`. Oracle staleness windows, liquidation ramps, auction durations, idle windows, MM-oracle write gates and per-period rates therefore keep constant wall-clock behavior across gates, including intervals that span a transition. Legacy stored fields keep their compact 400ms-unit encoding behind typed getters. SDK mirror in `math/time.ts`, and `SLOT_TIME_ESTIMATE_MS` is deprecated. |
| Accelerated referrals                            | #429 accelerated-referrals | Referrer rewards split into two rates. The Standard rate stays per-fee-tier (`FeeTier.referrer_reward_numerator`, with the fresh default cut from 15% to 10%). Accelerated is a fixed constant (`ACCELERATED_REFERRER_REWARD_NUMERATOR`, 20%), independent of the fee tier. The referee discount keeps using the fee tier. New `UserStats.accelerated_referral_status`, carved out of padding with no layout break, plus `AcceleratedReferralStatus` flags. While the beta-scoped `ACCELERATED_REFERRAL_ENROLLMENT_ENABLED` constant is true, account initialization and eligible interactions grant the status permanently. Those interactions are perp fills for both taker and maker, and completed swaps. Liquidation fills do not enroll the liquidatee. Ending enrollment is a program upgrade rather than a config change. New warm and cold admin instruction `update_user_accelerated_referral_status`, where a revoke also blocks automatic reenrollment until a later grant clears it. New `AcceleratedReferralStatusChangedRecord` event. Fill paths that carry a referred taker's `RevenueShareEscrow` may append the referrer's readonly `UserStats` after the escrow. It is optional, and omitting it applies the Standard rate. |

### 3.1 Per-user equity floor

Labels: `equity-floor`, `equity-floor-gaps`, `equity-floor-buffer`, `equity-floor-net-equity`
(#328), `equity-breaker-lazy-trip`, `equity-breaker-hardening`, `equity-floor-oracle-validity`,
`equity-floor-fail-closed`, `equity-floor-exemption-hardening`, `breaker-liquidation-followups`,
`equity-breaker-trip-dust-tolerance`, `equity-breaker-trip-remove-twap-concession`.

#### The metric

`User.equity_floor: u64` (QUOTE_PRECISION) sets a minimum net equity for a subaccount. Net
equity is unweighted asset value plus funding-inclusive perp PnL minus unweighted spot
liability value, at live oracle prices. It is what `calculate_user_equity` returns and what
`User.getNetUsdValue()` mirrors in the SDK. The field is carved from tail padding at struct
offset 4472, and `User` stays 4496 bytes.

`User.equity_floor_buffer: u64` (struct offset 4480, consuming the last 8 padding bytes)
adds required headroom on top of the floor. Every risk-increasing gate checks
`floor + buffer`, while the permissionless breaker trips at the raw floor. No permitted
action can therefore leave a subaccount trippable, and a passive drawdown must burn through
the whole buffer before the breaker can fire.

A floor of `0` disables both checks, which is the default for every existing account.

#### What the floor blocks

While net equity is below `floor + buffer`, the program rejects the following with
`EquityBelowFloor` (6358 / `0x18D6`):

- risk-increasing order placement and fills, on both the taker and the maker side;
- withdrawals;
- generic spot swaps (`end_swap`);
- deposit transfers out (`transfer_deposit`, `transfer_deposit_by_delegate`);
- `transfer_perp_position`, checked on the sender's and the recipient's post-transfer
  position alike;
- trigger-order activation.

Reduce-only activity stays allowed, and keepers may force-cancel risk-increasing resting
orders below the floor.

One swap is exempt. A strictly reducing swap, meaning one whose in leg consumes an existing
deposit and whose out leg repays an existing borrow with no new liability or deposit
exposure, stays allowed. It requires margin-valid oracles on both legs and rejects with
`InvalidOracle` otherwise. Its execution value is bounded to at most 1% loss
(`EQUITY_FLOOR_SWAP_MAX_VALUE_LOSS_BPS`), with the in leg valued at the strict max and the
out leg at the strict min of the live oracle price and the 5-min TWAP, rejecting with
`InvalidSwap` beyond that.

#### Setting the floor

Only the warm-admin instruction `update_user_equity_floor(equity_floor, equity_floor_buffer)`
sets these fields. The account's authority and delegate cannot change them. SDK:
`AdminClient.updateUserEquityFloor` and `getUpdateUserEquityFloorIx`. Admin CLI:
`velocity-admin user set-equity-floor <user> <floor> <buffer>`. Both the instruction and the
CLI command took a single floor argument in the first version, so this is a signature change.

#### Moving floor between subaccounts

`transfer_deposit_by_delegate` gained an `equity_floor_delta: u64` argument, which is a
signature change against the #45 form. It atomically moves floor along with funds between
same-authority subaccounts, under these rules:

- the debited side must not already be below the floor being reduced, so a below-floor
  subaccount cannot reduce its own floor to avoid a trip;
- the debited side must stay at or above its reduced floor;
- the credited side's post-transfer equity must back its increased floor, otherwise the
  instruction fails with `InvalidEquityFloorTransfer` (6359 / `0x18D7`);
- the sum of floors is preserved.

A delegate can therefore rebalance capital freely, while total equity across floored
subaccounts can never drop below the sum of floors. The anti-defuse pre-check stays at the
raw floor, so a subaccount inside its buffer band may still move floor away.

A floor transfer also carries a proportional share of the debited side's buffer, rounded up
on the debited side. The sums of floors and of buffers are both conserved, and shedding the
whole floor sheds the whole buffer, so a delegate cannot leave a buffer on a subaccount
whose checks are disabled.

#### The authority-wide breaker

The breaker escalates the per-subaccount freeze to every subaccount of one authority. The
permissionless `trip_equity_floor_breaker` proves that one subaccount's net equity is below
its raw floor and sets `UserStats.equity_breaker_tripped`, one byte carved from trailing
padding with `UserStats` unchanged at 240 bytes.

The trip is a provable upper bound on equity. Positions with valid oracles count exactly.
Invalid-oracle liabilities and short base legs take their sound zero upper bound, while
stored quote and funding legs count exactly. Any invalid-oracle asset or long, or a required
quote conversion the program cannot make, leaves the breach unprovable and the instruction
rejects with `InvalidOracle`. No stored TWAP bounds an invalid source, so a freeze cannot
arm from a correlated stale price. See `equity-breaker-trip-remove-twap-concession` in §6.

While the flag is set, every subaccount of the authority:

- rejects risk-increasing fills, withdrawals, spot swaps and transfers out;
- is barred from acting as liquidator in position-acquiring liquidations (`liquidate_perp`,
  `liquidate_spot`, `liquidate_borrow_for_perp_pnl`, `liquidate_perp_pnl_for_deposit`, and
  the swap-backed `liquidate_spot_with_swap_begin` / `_end`), which acquire the liquidatee's
  risk and earn a fee. Pnl-settlement liquidations stay allowed as protocol-protective;
- has `trigger_order` cancel rather than activate a risk-increasing trigger order, paying no
  keeper reward on that cancel, because the floor, breaker and margin evaluation runs before
  the reward payment.

Two exemptions survive a tripped breaker. The strictly reducing swap described above still
works, so a frozen account can deleverage. And a funds-only `transfer_deposit_by_delegate`
with a zero floor delta into a same-authority subaccount that sits below its buffered floor
is allowed as a cure transfer, so internal surplus can top up a breach. A cure never clears
the flag, partial cures compose across several donor subaccounts, and the debited side stays
gated at its own `floor + buffer` by the withdraw margin check inside the transfer.

Independently of the breaker, a floored liquidator subaccount must clear its own
`floor + buffer` after the liquidation in all four balance-acquiring paths, matching the
risk-increasing fill gate.

#### Arming and clearing

The breaker arms two ways. The permissionless `trip_equity_floor_breaker` transaction is one.
The other is a lazy inline trip. A reducing perp fill on either side, a strictly reducing
swap, or a trigger cancel that observes a floored subaccount provably below its raw floor
sets `equity_breaker_tripped` as a side effect of the succeeding instruction, using the same
upper-bound verdict as the permissionless trip. `trigger_order`'s `user_stats` account is
writable for this. The permissionless trip transaction is therefore needed only when the
authority is not transacting at all.

Only the warm-admin `reset_equity_floor_breaker` clears the flag, and the clear is
self-verifying. It carries every live subaccount of the authority in `remaining_accounts`,
with the count pinned by `UserStats.number_of_sub_accounts`, followed by the markets and
oracles their positions reference. It reverts with `InvalidEquityBreakerReset` (6368) unless
every floored subaccount clears its `floor + buffer` at execution time, and with
`InvalidOracle` on a bad price. SDK: `VelocityClient.tripEquityFloorBreaker` and
`AdminClient.resetEquityFloorBreaker`. Admin CLI: `user reset-equity-breaker`.

#### Oracle handling

Every strict authorization gate fails closed on an invalid oracle. The floor metric is exact
net equity plus a validity verdict (`FloorNetEquity`). A gate that authorizes an action
rejects with `InvalidOracle` while any oracle the subaccount depends on is invalid, and with
`EquityBelowFloor` when a fully valid value sits below `floor + buffer`. `force_cancel_orders`
runs in the opposite direction, so it treats "below floor" as grounds only when all oracles
are valid and equity is below the raw floor. The breaker uses the separate upper-bound rule
described above. See `equity-floor-oracle-validity` and `equity-floor-fail-closed` in §6.

#### Instruction account changes

`trigger_order` gained a required `user_stats` account, writable. `liquidate_spot` gained a
required `liquidator_stats` account. `liquidate_spot_with_swap_begin` and `_end` each gained
a required `liquidator_stats` account. See §5 for positions and seeds.

#### SDK and CLI surface

- `UserAccount.equityFloor: BN`, `UserAccount.equityFloorBuffer: BN`,
  `UserStatsAccount.equityBreakerTripped: number`.
- `User.isBelowEquityFloor()`, `getEquityAboveFloor(slot)`, `isBelowBufferedEquityFloor(slot)`,
  `getBufferedEquityFloor()`, `getEquityAboveBufferedFloor(slot)`,
  `getFloorNetEquity(slot)` (`{ value, allOraclesValid }`), `getTripNetEquity(slot?)` and
  `provesEquityFloorBreach(slot?)`.
- `getWithdrawalLimit` caps by equity above the floor.
- `transferDepositByDelegate(..., equityFloorDelta?)`, which accepts `'auto'` on the quote
  market to move the minimal floor the debited side needs. `'auto'` targets `floor + buffer`.
- Pure helpers `calculateEquityFloorAutoDelta` and `getEquityFloorLevel`
  (breached, critical, warning, healthy), in `math/margin`.
- `EquityFloorManager`, giving aggregate status and levels, haircut-padded `planQuoteTransfer`
  and `transferQuote`, `getMaxWithdrawable` and `getMaxQuoteTransferable`,
  proportional-to-equity `planFloorRebalance` and `rebalanceFloors` via zero-amount floor
  moves, and breach-curing `planCureTransfers` and `cureBreaches`.
- Admin CLI read-only `user equity-floor-status <authority>`, and `user close-positions`,
  a cancel-all plus reduce-only close sweep for a tripped authority, signed by the account
  authority.
- velocity-rs `math::equity_floor` (`calculate_equity_floor_auto_delta`, `equity_floor_level`,
  `EquityFloorLevel`) mirrors the TS helpers.

---

## 4. SDK surface changes

### 4.1 Package and tooling

```bash
# old
npm install @drift-labs/sdk
# new
npm install @velocity-exchange/sdk
```

- Versioning reset from `2.163.0-beta.0` to `0.x`. Changesets manage releases from this repo,
  and release-please was removed.
- Anchor dependency moved from `@coral-xyz/anchor@0.29.0` to `@anchor-lang/core@1.0.1`,
  aliased as `@coral-xyz/anchor`. The IDL is Anchor-1.0 format and will not load in 0.29
  clients.
- IDL account name casing. Anchor 1.0 emits camelCase account names in the IDL. Any code that
  passes an account name as a string to a coder method, for example
  `program.coder.accounts.decodeUnchecked('PerpMarket', ...)`, must switch from PascalCase to
  camelCase. `'PerpMarket'` becomes `'perpMarket'`, `'SpotMarket'` becomes `'spotMarket'`,
  `'User'` becomes `'user'`, and so on.
- Repo tooling moved from yarn to bun, which only matters if you build from source.
- jit-proxy is gone. There is no `@velocity-exchange/jit-proxy` package and no jit-proxy
  program (§2). Drift's `@drift-labs/jit-proxy` has no Velocity equivalent. Port jitters and JIT
  makers onto the CLOB's activation-slot auction.
- Anchor imports inside the SDK go through an isomorphic layer (`sdk/src/isomorphic/anchor`)
  with separate node and browser builds.

### 4.2 Renames (no deprecated aliases, find and replace required)

PR #37 originally shipped `@deprecated` Drift aliases. They have since been removed, and the
old names no longer exist.

| Old                                         | New                                                                 |
| ------------------------------------------- | ------------------------------------------------------------------- |
| `DriftClient`                               | `VelocityClient`                                                    |
| `DriftClientConfig`                         | `VelocityClientConfig`                                              |
| `DriftClientSubscriptionConfig`             | `VelocityClientSubscriptionConfig`                                  |
| `DriftEnv`                                  | `VelocityEnv`                                                       |
| `DRIFT_PROGRAM_ID`                          | `VELOCITY_PROGRAM_ID`                                               |
| `DRIFT_ORACLE_RECEIVER_ID`                  | `VELOCITY_ORACLE_RECEIVER_ID` (same pubkey)                         |
| `USDC_MINT_ADDRESS`                         | `QUOTE_MINT_ADDRESS` (#18; the devnet value changed to the dUSDT placeholder `GqmEqYsy8EyvofDpmtFxK8zhYrgWgNokAtYoduQdL7v6`, and mainnet USDC is unchanged) |
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
`PYTH_PULL_ORACLE_LOOKUP_TABLE` and `JIT_PROXY_PROGRAM_ID` were dropped from the env config
object.

Mainnet `MARKET_LOOKUP_TABLE` and `MARKET_LOOKUP_TABLES` now point at the relaunch lookup
table `4E971nER9Jn4JjT8mKEX1nvkfg8Qycp7zNEcCq2nT8ZY`, which covers state, signer, spot
markets 0 and 1 with their oracles, mints, vaults and IF vaults, perp markets 0 through 3
with their oracles, and the token, ATA and system programs. Drift's original tables, whose
addresses begin `Fpys8` and `EiWSs`, reference pre-relaunch accounts and must not be used against the
Velocity program.

Gov-token stake fee discount removal (#80) removed
`VelocityClient.updateUserGovTokenInsuranceStake`,
`getUpdateUserGovTokenInsuranceStakeIx`,
`AdminClient.updateDelegateUserGovTokenInsuranceStake`,
`getUpdateDelegateUserGovTokenInsuranceStakeIx`, and the constants `GOV_SPOT_MARKET_INDEX`
and `MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT_GOV` (the `constants/insuranceFund`
module).

Dead-export cleanup (#82) removed the following previously-exported symbols, which had no
consumer inside the SDK, its tests, or any velocity-exchange org repository. The module
`tx/forwardOnlyTxSender` was deleted along with its `ForwardOnlyTxSender` class, then
restored in #89 (see §4.6).
Removed `math` functions: `builderCodesEnabled`, `builderReferralEnabled`,
`calculateAvailablePerpLiquidity`, `calculateBudgetedK` (the non-`BN` variant;
`calculateBudgetedKBN` is unaffected), `calculateCollateralValueOfDeposit`,
`calculateLiquidationPrice` (`calculateLiquidationPriceAfterPerpTrade` is unaffected),
`calculateMaxSpread`, `calculateNewMarketAfterTrade`,
`calculateOraclePriceForPerpMargin`, `calculateOracleReserveSpread`,
`calculatePerpMarketBaseLiquidatorFee`, `calculatePositionFundingPNL`,
`calculateUserMaxPerpOrderSize`, `fetchMSolMetrics`, `isOrderReduceOnly`,
`isOrderRiskIncreasing`, `isOrderRiskIncreasingInSameDirection`, `isTakingOrder`,
`trimVaaSignatures`. Also removed: the `memcmp` helper `getUserThatHasBeenLP`, the constants
`MAX_I64` and `TEN_MILLION`, the type `MSOL_METRICS_ENDPOINT_RESPONSE`, and the
deep-import-only `PYTH_SOLANA_RECEIVER_IDL` (`pyth/types`). The misspelled constant
`PTYH_LAZER_PROGRAM_ID` was renamed to the correctly spelled `PYTH_LAZER_PROGRAM_ID`.

`calculateMaxRemainingDeposit` was in this removal batch but was restored in #89 (see §4.6).

Legacy referrer migration removal (#149) removed `VelocityClient.migrateReferrer` and
`getMigrateReferrerIx`. These wrapped the `migrate_referrer` program instruction, which
backfilled `RevenueShareEscrow.referrer` from `UserStats.referrer` for escrows created
before that copy was folded into escrow initialization. The instruction's entrypoint had
already been removed with the legacy referral model, so it was absent from the IDL and the
SDK methods threw at runtime. Escrow initialization now copies the referrer unconditionally,
which makes the migration redundant.

Swap provider unification (#331) removed `TitanClient.getSwap`,
`TitanClient.getTitanInstructions`, `UnifiedSwapClient.getSwap` and the types
`SwapTransactionParams` and `SwapTransactionResult`. Both swap clients now implement the new
`SwapProvider` interface. Call `getQuote`, then either `getRouteInstructions({ quote,
userPublicKey })` for a swap running inside velocity's `beginSwap` and `endSwap` bracket, or
`getSwapTransaction({ quote, userPublicKey })` for a standalone transaction the caller signs
and sends itself. `getQuote` returns a `SwapQuote` carrying the route it quoted, so building
a swap no longer depends on client state or on call ordering. `getRouteInstructions` takes no
`slippageBps`, because the quote's own `slippageBps` is authoritative.
`JupiterClient.getJupiterInstructions` still works but is deprecated in favour of the shared
`filterRouteInstructions`. Quote parameters typed `QuoteResponse` or `UnifiedQuoteResponse`
(`swap`, the `AdminClient` swap helper, and the `superStake` `jupiterQuote` arguments) now
take `JupiterSwapQuote` or `SwapQuote`. A quote must come from the client that will execute
it, be for the pair the swap brackets, be for the amount being swapped, and on Titan be for
the executing wallet. Those checks apply to a quote `getProviderSwapIx` fetches itself as
well as to one passed in, and the quote's own `swapMode` is the effective mode either way. A
quote is also checked against the route it carries, so a modified copy of a returned quote is
rejected rather than executed as the swap it was originally quoted for. The three
per-provider builders `VelocityClient.getSwapIxV2`, `getJupiterSwapIxV6` and `getTitanSwapIx`
were removed in favour of a single `getProviderSwapIx({ swapProvider, ... })` that accepts
any `SwapProvider`. `swap`'s `swapClient` parameter is typed `SwapProvider` and no longer
branches on the concrete client. `getSwapIx`, the `beginSwap` and `endSwap` pair, is
unchanged.

Jupiter Swap API v2 opt-in (jupiter-swap-api-v2) removed the former v1-only
`JupiterClient.getSwap` (`POST /swap`). Callers use `getSwapTransaction`, which still posts
to `/swap` under `apiVersion: 'v1'`, or `getRouteInstructions`.

DLOB removal (feat/propamm) removed the off-chain order book and its subscribers with the venue
they served. Gone: `DLOB`, `DLOBNode`, `NodeList`, `DLOBSubscriber`, the `dlob/types` module
(`DLOBSource`, `DLOBSubscriptionConfig`, `DLOBSubscriberEvents`), `OrderSubscriber` and its
subscription modules, `AuctionSubscriber`, and `UserMap.getDLOB`. `SlotSource` survives and
moves to `slot/SlotSubscriber`. The vAMM ladder generators that fed the book are gone too:
`getVammL2Generator`, `createL2Levels`, `mergeL2LevelGenerators`, `getL2GeneratorFromDLOBNodes`,
`L2OrderBookGenerator`, `DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS` and
`MAJORS_TOP_OF_BOOK_QUOTE_AMOUNTS`. A client that priced the vAMM through them uses
`math/vammLadder`'s `vammQuoteLevels`, which mirrors the program's own ladder.

The book shapes stay. `L2Level`, `L2OrderBook`, `L3Level`, `L3OrderBook`, `groupL2` and
`uncrossL2` move from `dlob/orderBookLevels` to `orderBookLevels`. The package barrel exports
them under the same names, so only a deep import moves. `L2Level['sources']` now reports
`'vamm'`, `'clob'` and `'propamm'`. `'dlob'` and `'indicative'` are removed, because the book
publisher labels a level by the router source that quoted it.

### 4.4 Type-level breaking changes

- `oraclePriceOffset` is now `BN` rather than `number` on `Order` and `OrderParams`, having
  been widened to i64 on-chain in #51. Code passing raw numbers must wrap them in
  `new BN(...)`.
- `Order.quoteAssetAmount` is removed. This field never existed on the on-chain `Order`
  struct, which only has `quoteAssetAmountFilled`. It was a vestigial SDK-type member that
  the decoder always populated with `0`. The TS `Order` type now matches the IDL. Read filled
  quote from `quoteAssetAmountFilled` instead.
- `PerpMarketAccount`. The oracle fields (`oracle`, `oracleSource`, and siblings) moved from
  `amm.*` to the top level. Aggregate position and funding stats moved into the market. There
  are new `marketStats` and `hedgeConfig` sub-structs, the latter replacing the flat `lp*`
  fields (#66). The fuel, PMM, HLM and LP fields are removed. `lastFundingOracleTwap` now
  lives at `marketStats.lastFundingOracleTwap` (#77), and `fundingClampThreshold` /
  `fundingRampSlope` replace `paddingFundingTwap` (#94).
- `PerpPosition`. `lpShares`, `lastQuoteAssetAmountPerLp` and `perLpBase` are removed.
- `StateAccount`. The single `admin` is replaced by the cold, warm and hot key set.
- `UserStatsAccount`. `ifStakedGovTokenAmount` is removed with the gov-stake fee discount
  (#80), and `getUserFeeTier` no longer applies a stake-based discount. There is a new
  `delegatePermissions: number` field (#45), set and cleared by
  `update_user_allow_delegate_transfer`, which gates whether a delegate may call
  `transfer_deposit_by_delegate`.
- The SDK sizes the compute limit by simulating, and caps loaded-accounts data size.
  `VelocityClient`'s default `txParams` sets `useSimulatedComputeUnits: true`, so every
  transaction it builds asks for what the simulation burned rather than a flat 600,000.
  `txParams.computeUnits` (still 600,000) is the ceiling that clamps the simulated figure and
  the fallback when simulation fails. This costs one extra RPC round trip per transaction.
  Pass `useSimulatedComputeUnits: false` to opt out. `BaseTxParams` also gained
  `loadedAccountsDataSize`, defaulted to `LOADED_ACCOUNTS_DATA_SIZE_DEFAULT` (12 MiB) and
  emitted as a `SetLoadedAccountsDataSizeLimit` instruction. `0` omits it and takes the
  network's 64 MiB default. Both limits are billed on the figure a transaction requests, so a
  transaction that genuinely loads more than the default must raise it. The velocity program
  and its program data count toward the limit. `velocity-rs` applies the same default:
  `TransactionBuilder::build` adds the limit when the caller set none
  (`constants::LOADED_ACCOUNTS_DATA_SIZE_DEFAULT`, override with
  `with_loaded_accounts_data_size`). The instruction is appended, not prepended, and a client
  that adds its own must do the same. The signed-message flows encode absolute instruction
  indices, because an ed25519 verify instruction points at the instruction that holds the
  message it verifies (`createMinimalEd25519VerifyIx` / `new_ed25519_ix_ptr`). An instruction
  added at the front shifts every index behind it.
- `SwapInfo.feeAmount` and `SwapInfo.feeMint` are now optional (`jupiter/jupiterClient`).
  Jupiter's v2 API omits per-hop fees from `routePlan[].swapInfo`. The v1 path and
  `TitanClient` still populate both, so this only widens what a consumer must handle when
  reading a route plan.
- The `CurveRecord` event became `AmmCurveChanged`, and its fields changed too.
- Revenue-share escrow on fills (#68, #73). The `ReferrerStatus` enum gains
  `BuilderReferral = 4`, and `math/builder` gains `isBuilderReferral(userStats)`,
  `escrowHasReferrer(escrow)` and `hasBuilderParams(orderParams)`.
  `fillPerpOrder` / `getFillPerpOrderIx`, `placeAndTakePerpOrder` /
  `getPlaceAndTakePerpOrderIx`, `placeAndMakePerpOrder` / `getPlaceAndMakePerpOrderIx`, and
  `getPlaceAndMakeSignedMsgPerpOrderIxs` accept an optional trailing `takerEscrow`, the
  taker's decoded `RevenueShareEscrowAccount`, for example from a `RevenueShareEscrowMap`.
  That attaches the taker's escrow when the taker is referred, which the program's fill-time
  enforcement requires (§3). The builders validate `takerEscrow.authority` against the
  taker's authority. Note that #68 originally took a
  `revenueShareEscrowMap?: RevenueShareEscrowMap` on `placeAndTakePerpOrder` and
  `getPlaceAndTakePerpOrderIx`, and #73 replaced that with the decoded `takerEscrow?`.
  Callers passing a map must switch to the decoded escrow account. The settle-PnL builders
  keep their map-based `revenueShareEscrowMap` param.
- `PerpOperation` bit values. There are 8 flags total, with `AMM_IMMEDIATE_FILL = 64` and
  `SETTLE_REV_POOL = 128`. Code hardcoding these numeric values instead of referencing the
  `PerpOperation` enum must update.
- `OrderBitFlag` gains `HasBuilder = 16` and `IsIsolatedPosition = 32`, matching the full
  6-bit on-chain flag set. Code reading `OrderRecord.order.bitFlags` or
  `OrderActionRecord.bitFlags` can detect builder-fee and isolated-margin orders through the
  shared enum instead of redefining local flag constants.
- Fee redesign (#75):
  - `PerpMarketAccount`. `totalExchangeFee` and `totalLiquidationFee` moved into a new nested
    `feeLedger: FeeLedger`, which also carries `pendingProtocolFee`, `pendingIfFee`,
    `ammProtocolFeesReceived` and `pendingAmmProvision`. New `protocolFeePool`,
    `protocolLiquidationFee` and `feePoolBufferTarget` fields.
  - `SpotMarketAccount`. New `protocolFeePool`, `protocolLiquidationFee` and
    `protocolFeeFactor`. `insuranceFund.totalFactor` and `userFactor` became `ifFeeFactor`.
  - `StateAccount`. New `protocolFeeRecipientPerp` and `protocolFeeRecipientSpot`, two
    separately configurable treasury keys, one for perp and one for spot, plus
    `hotFeeWithdraw`. `FeeStructure` gains `ammFeeNumerator` and `ifFeeNumerator`, carved
    from reserved padding.
  - `calculateUpdatedAMM`, `calculateBidAskPrice`, `calculateUpdatedAMMSpreadReserves`,
    `calculateOptimalPegAndBudget` and `calculateNewAmm` dropped their `totalExchangeFee`
    parameter, because the AMM no longer has a fee floor.
  - `updatePerpMarketAmmSummaryStats` dropped `excludeTotalLiqFee`.
- Strict null-checking exposed some accessors (#74, #78, when the SDK turned on
  `"strict": true`). A few public signatures were widened to expose the `undefined` the
  runtime already returned:
  - `DLOBNode.getPrice(...)` now returns `BN | undefined` rather than `BN`. It always could
    return `undefined` for orders without a resolvable limit price, for example post-auction
    market orders, and the type now admits it. A new `getPriceOrThrow(...)` serves call sites
    that structurally require a defined price.
  - `BlockhashSubscriber.getLatestBlockHeight()` now returns `number | undefined` rather than
    `number`, returning `undefined` before any blockhash has been fetched, as the runtime
    already did.
  - `nextRevenuePoolSettleApr(spotMarket, vaultBalance, amount)`'s third positional
    `amount: BN` is now required rather than `amount?: BN`. The function always dereferenced
    it, so omitting it already produced `NaN` or threw at runtime.
  - `BasicUserAccountSubscriber.getUserAccountAndSlot()` and
    `BasicUserStatsAccountSubscriber.getUserStatsAccountAndSlot()` now return
    `DataAndSlot<T> | undefined` rather than the non-optional `DataAndSlot<T>`, matching the
    `UserAccountSubscriber` and `UserStatsAccountSubscriber` interfaces. They return
    `undefined` until an account is loaded, as the runtime already did. Relatedly, the
    `{ data, slot }` pair these and the polling subscribers store is now atomic. A loaded
    account always carries a real `slot`, a `number` and never `undefined`, with seeded
    accounts using `0` as an oldest-possible sentinel, so `DataAndSlot.slot` can be relied on
    as defined. `doesAccountExist()` on these subscribers is now a type predicate.
  - `User.getUserAccountAndSlot()` and `VelocityClient.getUserAccountAndSlot()` keep their
    `DataAndSlot<UserAccount> | undefined` return, which is `undefined` until the account
    loads, as the runtime already did. A new `User.getUserAccountAndSlotOrThrow()` serves
    call sites that structurally require a loaded account.
  - The `UserAccountSubscriber` "not subscribed" contract is now uniform. Every
    implementation's `getUserAccountAndSlot()` throws `NotSubscribedError` when called before
    `subscribe()`. The WebSocket and polling subscribers already did, and the gRPC-multi and
    WebSocket-program subscribers now match. `User.getUserAccount()` therefore throws when
    not subscribed, and returns `undefined` only when subscribed but the account was not
    found on chain. Because `subscribe()` awaits the initial fetch, `undefined` means "not
    found" rather than "still loading". The `getUserAccountOrThrow()` and
    `getUserAccountAndSlotOrThrow()` error message changed from `User account not loaded:
    <pubkey>` to `User account not found: <pubkey>`, and both still propagate
    `NotSubscribedError` when called before subscribing. Consumers that matched on the old
    message string should update.

### 4.5 New: `VelocityCore` (#21)

A subscription-free instruction-building module (`export * from './core'`) for integrators
who only need to construct instructions. It covers PDAs, remaining accounts, and deposit,
withdraw, order, fill and liquidation builders, without running a full subscribed client.

### 4.6 New exports

These public exports were added, or restored, relative to the fork point:

- `TransferFeeAndPnlPoolDirection` enum-class (`FEE_TO_PNL_POOL`, `PNL_TO_FEE_POOL`), plus
  `AdminClient.transferFeeAndPnlPool` and `getTransferFeeAndPnlPoolIx` (#1).
- `SpecialUserStatus` enum (`VammHedger = 1`), `AdminClient.updateSpecialUserStatus`,
  `getUpdateSpecialUserStatusIx`, `VelocityClient.specialTransferPerpPositionToVamm` and
  `getSpecialTransferPerpPositionToVammIx` (#17).
- `VelocityClient.updateUserAllowDelegateTransfer` and `transferDepositByDelegate` (#45).
- CLOB order surface (feat/propamm): `VelocityClient.cancelOrderV1`, `modifyOrderV1` and
  `cancelOrdersV1` and their `get*Ix` builders, with `CancelOrderV1Params`,
  `ModifyOrderV1Params`, `CancelOrdersV1Params`, `ClobOrderRefV0` and `CancelSidesV0`.
  Placement is `placeAndMakePerpOrder` (§2, jit-proxy row). A caller names a market and
  nothing else. The book's program and account resolve from the market's quoter slab (slot 0),
  and the rest are PDAs.
- `UserClobOrdersClient` and `UserClobOrder` (feat/propamm) read a user's resting book orders
  over the dlob-server's `GET /userOrders` and its `user_orders` websocket channel. This
  replaces `user.getOpenOrders()` for orders that rest on a book, which have no `User.orders`
  slot. Every row carries the handle a cancel or a modify takes.
- `liquiditySource`, and so `L2Level['sources']`, gains `'clob'` and `'propamm'`
  (feat/propamm).
- `EquityFloorManager` and its plan types (`equityFloorManager`),
  `calculateEquityFloorAutoDelta`, `getEquityFloorLevel`, `EquityFloorLevel` (`math/margin`),
  and `User.isBelowBufferedEquityFloor`, `getBufferedEquityFloor`,
  `getEquityAboveBufferedFloor` (equity-floor-buffer).
- `AdminClient.updatePerpMarketFundingDeadZone` and `getUpdatePerpMarketFundingDeadZoneIx`
  (#94).
- `areQuotedLevelsValid`, `quotedPrefix`, `isExecutedNotionalInQuote`,
  `isChangeNotionalInQuote` and `RouterQuotedPrefix` (`math/router`, feat/propamm), the
  TypeScript mirror of the bounds velocity holds an external quoter's `execute_v0` response
  to, so a client can predict whether a router fill lands.
- `VelocityClient.updateMmOracleBatchNative` and `getUpdateMmOracleBatchNativeIx`, plus the
  `MmOracleBatchUpdate` entry type and the `MM_ORACLE_BATCH_MAX_MARKETS`,
  `MM_ORACLE_MIN_WRITE_GAP` and `MM_ORACLE_MAX_SOURCE_AGE` constants, the last two as `Millis`
  durations rather than slot counts (mm-oracle-batch-native, mm-oracle-freshness-fixes). The batch writes the MM oracle for
  many perp markets in one native instruction, so a cranker pays one signature fee for the
  whole set, and a per-market rate-limit rejection skips only that market. Each entry carries
  an `oracleSourceSlot`, the slot the price was observed at, and `updateMmOracleNative` /
  `getUpdateMmOracleNativeIx` take the same value as a new required parameter, which is
  breaking. The builders reject an empty list, more than `MM_ORACLE_BATCH_MAX_MARKETS`
  markets, duplicate market indexes, non-positive prices (because `BN` serialization drops
  the sign, so a negative price would otherwise be written as its magnitude), and values that
  do not fit their program-side width (`i64` price, `u64` sequence id and source slot).
- `AdminClient.updatePerpMarketFundingBiasSensitivity` (#77).
- `AdminClient.updateWithdrawGuardThreshold` and `getUpdateWithdrawGuardThresholdIx` gained
  an optional trailing `oracle?` arg (#76).
- `VelocityCore` module (#21, see §4.5).
- Restored in #89, having been removed in #82: `ForwardOnlyTxSender`
  (`tx/forwardOnlyTxSender`) and `calculateMaxRemainingDeposit` (`math/spotMarket`).
- `PriceUpdateAccount` is re-exported from the package root (#97). It was previously
  reachable only via a subpath import.
- `AdminClient.updatePauseAdmin` and `getUpdatePauseAdminIx`, the cold-admin rotation of the
  emergency `pause_admin` key (`StateAccount.pauseAdmin`), sitting alongside
  `updateWarmAdmin` and `updateHotAdmin`.
- `isIsolatedPositionBankrupt(user, marketIndex)` and `hasIsolatedMarginBankrupt(user)`
  (`math/bankruptcy`), mirroring the isolated half of the program's bankruptcy routing
  (`is_isolated_margin_bankrupt` and `has_isolated_margin_bankrupt`). They are needed because
  `User.isBankrupt()` reads only the account-level `UserStatus.BANKRUPT` bit, which is never
  set for isolated-only bankruptcies, and `isUserBankrupt` (cross) skips isolated positions
  by design. `isIsolatedPositionBankrupt` throws `InvalidPerpPosition` on a non-isolated
  index.
- `calculatePerpIfFee` and `calculateSpotIfFee` (`math/liquidation`), porting the
  margin-shortage-aware insurance-fund fee caps. Feed their output, rather than the raw
  `if + protocol` sum, into the covering-amount helpers. `calculateMaxPctToLiquidate` gained
  an `isIsolatedPosition` param, which returns 100% in one shot for isolated positions, per
  `IsolatedMarginLiquidatePerpMode`.
- `User.calculateFeeForQuoteAmount` was renamed to `User.calculatePerpTakerFee`. The old name
  is gone, so update call sites. It also gained an optional trailing `builderInfo`
  (`Pick<OrderParams, 'builderIdx' | 'builderFeeTenthBps'>`) arg. When present, the builder
  fee (`quoteAmount * builderFeeTenthBps / 100_000`) is added on top of the tiered fee.
  `VelocityClient.getMarketFees` also applies the referee discount to the taker fee, which
  this path previously omitted, so referred users get a lower predicted fee.
- `User.isBuilderFeeCharged()` is new. It reports whether the program charges a builder fee
  on this user's perp fills, which it does only while the user meets initial margin. Both
  `User.calculatePerpTakerFee` and `VelocityClient.getMarketFees` consult it, so a predicted
  fee for a user below initial margin no longer includes the builder fee. The method is an
  estimate. It applies the strict, TWAP-bounded prices the program's gate applies, but it
  does not model oracle validity, so it can report `true` where the program waives the fee on
  an invalid oracle.
- `MMOraclePriceData` gained optional `isMMOracleEnabled`, `isMMOracleAsRecent` and
  `isMMExchangeDiffBpsHigh` fields, populated by `getMMOracleDataForPerpMarket`.
  `isFallbackAvailableLiquiditySource` now mirrors `amm_fill_gates_ok` fully. It also
  suppresses AMM fallback on market drawdown and on MM-versus-exchange oracle volatility,
  meaning a diff above 1% while the MM oracle is enabled and as-recent.
- `TRIGGER_PRICE_LAST_FILL_MAX_AGE` (`constants/numericConstants`), the max age of the last
  fill before `getTriggerPrice` treats the last-fill leg as absent and substitutes the oracle
  price.
- `calculateUserProtectiveAssetPrice` and `calculateUserProtectiveLiabilityPrice`
  (`math/liquidation`), mirroring the program's user-protective conversion pricing used by
  spot and pnl-versus-spot liquidations when the deposit or borrow oracle is margin-invalid.
  The asset leg is `max(oracle, 5min twap, oracle+conf)` and the liability leg is
  `min(oracle, 5min twap, oracle−conf)`, floored at 1. Feed the result as `assetPrice` and
  `liabilityPrice` to `calculateAssetTransferForLiabilityTransfer` to predict on-chain
  transfer amounts in that case.
- `SwapProvider`, `SwapQuote`, `SwapProviderRoute`, `SwapRouteFields`,
  `GetRouteInstructionsParams`, `SwapRouteInstructions`, `buildSwapQuote`,
  `expectProviderRoute`, `DEFAULT_SWAP_MAX_ACCOUNTS` (`swap/types`) and
  `filterRouteInstructions` (`swap/routeInstructions`) (#331), which together are the shared
  swap-provider contract. `JupiterClient` and `TitanClient` both implement `SwapProvider`,
  and `UnifiedSwapClient` forwards to whichever is configured. Implement `SwapProvider` to
  add a provider. The three methods (`getQuote`, `getRouteInstructions`,
  `getSwapTransaction`) plus the route-on-the-quote convention are the whole contract. Return
  `getQuote`'s result through `buildSwapQuote(normalizedQuote, providerRoute)` rather than
  assembling the object by hand, since that records the `SwapRouteFields` that let
  `expectProviderRoute` reject a quote whose pair, size, mode or slippage no longer matches
  the route it carries. Treat a returned `SwapQuote` as immutable and re-quote instead of
  editing one. `JupiterSwapQuote` (`jupiter/jupiterClient`) is Jupiter's `QuoteResponse`
  widened to a `SwapQuote`. See §4.3 for what this replaced.
- `JupiterApiVersion` (`'v1' | 'v2'`), `JUPITER_API_V2_VERSION` (`'/v2'`),
  `JupiterBuildResponse`, `JupiterApiInstruction` (`jupiter/jupiterClient`), plus a new
  `apiVersion` option on the `JupiterClient` constructor and a matching `jupiterApiVersion`
  option on `UnifiedSwapClient`. Together these are opt-in support for Jupiter Swap API v2
  (`GET /swap/v2/build`), which returns the quote and its raw instructions in one response.
  Under `apiVersion: 'v2'`, `getQuote` requires `userPublicKey`, because v2 builds for a
  named `taker`, so the quote is wallet-bound and `getRouteInstructions` and
  `getSwapTransaction` reject it for any other wallet. `getRouteInstructions` and
  `getSwapTransaction` issue no further HTTP request, since there is no v2 `/swap` endpoint
  and both build locally from the quote's carried instructions. `autoSlippage` throws,
  because v2 has no auto-slippage equivalent and ignores the params without reporting an
  error, returning zero slippage tolerance. `swapMode: 'ExactOut'` throws for the same
  reason. `/build` is ExactIn-only, and sent ExactOut it answers `200` with
  `swapMode: 'ExactIn'`, spending the requested amount as the input instead of receiving it
  as the output. A v2 `getSwapTransaction` also takes an optional `computeUnitLimit`, since
  v1's `/swap` sized the transaction whereas `/build` returns a compute unit price but no
  limit, so without one the transaction runs on the runtime default. An unresolvable address
  lookup table now throws by name rather than being dropped. The default is still `'v1'`, and
  `RECOMMENDED_JUPITER_API_VERSION` (`'/v1'`) is unchanged. There is no program change, since
  every v2 route executes through the already-whitelisted Jupiter v6 program.
- `settleRevenueShare`, `getSettleRevenueShareIx` and `fetchRevenueShareEscrowAccount`
  (`velocityClient`), wrapping the new permissionless `settle_revenue_share` instruction (§3)
  and assembling its two-region remaining-account list from a decoded escrow. Also
  `forfeitRevenueShareOrder` and `getForfeitRevenueShareOrderIx`, wrapping
  `forfeit_revenue_share_order` and resolving the row's beneficiary the same way the program
  does.
- `RevenueShareEscrowMap.getEscrowsOwingRevenueShare(marketIndex)`, the escrows still owing
  on a market, which is the work list a keeper must clear before that market can be delisted.
- `calculateRevenueShareSweepAvailable`, `calculateBankruptcyIfTrancheReservation` and
  `calculateBankruptcyIfFloor` (`math/market`), mirroring the reservation
  `sweep_completed_revenue_share_for_market` applies, which is
  `pnlPoolTokens − max(netUserPnl, 0) − min(pendingIfFee, bankruptcyIfFloor)`, valued at
  `expiryPrice` while the market is in settlement. A keeper can use these to predict whether
  a `settleRevenueShare` call will pay.
- `MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN` and `MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN`
  (`constants/numericConstants`), `maxSpotInterestStalenessForMargin` (`math/spotBalance`),
  `VelocityClient.getStaleSpotInterestMarketIndexes` and
  `VelocityClient.getStaleSpotInterestCrankIxs` (fill-stale-margin-bad-debt). They name the
  spot markets whose interest must be accrued before a set of accounts can be used on a
  value-releasing path, and they build the permissionless cranks for them. Prepend the cranks
  to a fill, withdraw, transfer, or swap. Each market gets its own window from its rate
  ceiling, so a market that may charge more interest must be cranked more often. The program
  exempts a borrow whose un-booked interest is still under one token unit, which these
  helpers do not model, so they name a superset. The Rust SDK gains the equivalent pair,
  `VelocityClient::stale_spot_interest_markets` and
  `TransactionBuilder::update_spot_market_cumulative_interest`.
- The market's quoter slab is the external-CPI signer (`getQuoterSlabPublicKey(programId,
  marketIndex)`). It is the PDA velocity signs every CPI into an external quoter program
  with, and a CLOB book's `place_authority` and a midpoint instance's `execute_authority` must
  be set to it. A quoter's registered CPI account list names it where the signer goes. It is
  not `getVelocitySignerPublicKey` / `getSignerPublicKey`, which is the token authority on the
  spot and insurance-fund vaults. `getClobAuthorityPublicKey` and `getQuoterSignerPublicKey`
  (and the `VelocityClient` accessors) are removed. See §6 (feat/propamm).
- `AdminClient.updateTransactionFeeRails(rails)` and `getUpdateTransactionFeeRailsIx(rails)`,
  and the `TransactionFeeRails` type (§5). Admin CLI:
  `velocity-admin fees set-transaction-rails <inclusionLamports> <signatureLamports>
  <resourceFeeNum> <resourceFeeDenom>`.
- `CrankPaymentsV0` and `CrankCostUnitsV0` types, and `math/crankFee`:
  `requestedCostUnits`, `transactionCost`, `deriveCrankPayments`, plus the runtime's
  cost-model constants (`SIGNATURE_COST_UNITS`, `WRITE_LOCK_COST_UNITS`,
  `INSTRUCTION_DATA_BYTES_PER_COST_UNIT`, `LOADED_ACCOUNTS_PAGE_BYTES`,
  `LOADED_ACCOUNTS_PAGE_COST_UNITS`). It mirrors `CrankPaymentsV0::derive`, so it predicts
  the payments an attach will write.
- `LOADED_ACCOUNTS_DATA_SIZE_DEFAULT` and `setLoadedAccountsDataSizeLimitIx(bytes)` /
  `isSetLoadedAccountsDataSizeIx(ix)` (`util/computeUnits`). See §4.4.
- `getRouteDigest(route)` (`math/orders`) returns the eight bytes a signed-message order's
  record holds. It mirrors the program's `state::order_params::route_digest`: the route
  sorted, deduped and hashed, with an empty route digesting to zero and a real one never
  doing so. A filler needs it, because a fill claims a route and the program rejects it
  unless the claim digests to what the order carries. The digest moved off `Order` onto
  `SignedMsgUserOrders` and widened from five bytes to eight. `Order.route_digest` is retired
  to padding, so `Order` stays 104 bytes and the move is ABI-safe for anything decoding an
  order.
- The `AcceleratedReferralStatus` enum, the `AcceleratedReferralStatusChange` enum-class and
  the `AcceleratedReferralStatusChangedRecordV0` event type;
  `UserStatsAccount.acceleratedReferralStatus`;
  `AdminClient.updateUserAcceleratedReferralStatus` and
  `getUpdateUserAcceleratedReferralStatusIx`; the `ACCELERATED_REFERRER_REWARD_PERCENT`
  constant; `ReferrerMap.getReferrerAuthority`; and a `takerReferrer` parameter on
  `VelocityClient.getFillPerpOrderIx` (#429 accelerated-referrals).
- `currentSlotClock` and `currentSlotDuration`, plus the `SlotClock`, `SlotDurationSource`
  and `SlotDurationState` types (`math/time`, off-chain-slot-clock). They resolve the slot
  length an off-chain client should convert with, which is the live duration from a
  subscribed `State` at the current slot, or the hardcoded 400ms `SLOT_DURATION_BASELINE`
  when state or slot is unavailable. `source` is duck-typed on `{ getStateAccount() }`, so
  any client satisfies it structurally. A missing or `0` `currentSlot` resolves to the
  baseline rather than being read as slot zero, since a failed slot subscription reports `0`
  and slot zero precedes every effective slot. Integrators converting between slots and
  wall-clock should use these rather than `SLOT_TIME_ESTIMATE_MS`, which stays exported and
  deprecated.
- `SLOT_DURATION_SCHEDULE_MS` and `SLOT_DURATION_FLOOR` (`math/time`, off-chain-slot-clock),
  the mirror of the program's schedule. `SLOT_DURATION_SCHEDULE_MS` is `[400, 350, 300, 250,
  200]`, the 400ms baseline followed by the program's `SLOT_DURATION_TRANSITION_MS`.
  `SLOT_DURATION_FLOOR` is its last entry. `SLOT_DURATION_FLOOR` is what a user-protection window (a countdown, a
  signing budget, a cache TTL) substitutes when `currentSlotClock` reports `isLive: false`,
  since the 400ms baseline would promise up to twice the wall clock actually available.
- `signedMsgOrderMaxSlot` and `signedMsgOrderSlotReached` (`math/orders`, #470
  swift-slot-gate plus follow-up; the "placed yet" half is superseded by
  `signedMsgOrderPlaceable`, next bullet). These are the off-chain mirrors of
  `place_signed_msg_taker_order`'s placement window.
  `signedMsgOrderMaxSlot(state, orderSlot, isRestingLimit)` is the last slot the order may
  still be placed. For a resting limit that is the message slot itself. For any other order
  it is the message slot plus `SIGNED_MSG_FILL_WINDOW_MS`, integrated across slot duration
  transitions. `signedMsgOrderSlotReached(orderSlot, currentSlot)` is whether
  it may be placed yet, since the program rejects `order_slot > clock.slot` with
  `InvalidSignedMsgOrderParam` and takers stamp the message a few slots ahead as a signing
  buffer.
- `signedMsgOrderPlaceable`, `isRestingSignedMsgLimitOrder` and
  `SIGNED_MSG_RESTING_LIMIT_MAX_LEAD_MS` (`math/orders`, swift-resting-limit-placement).
  `signedMsgOrderPlaceable(state, order, currentSlot)` supersedes
  `signedMsgOrderSlotReached` as the "may be placed yet" predicate. Any other order still
  waits for its message slot, but a resting limit, as identified by
  `isRestingSignedMsgLimitOrder`, may be placed ahead of it as long as the
  stamp is within `SIGNED_MSG_RESTING_LIMIT_MAX_LEAD_MS` (30s) of the current slot.
- `getPerpFeeTierIndex`, `PERP_FEE_TIER_VOLUME_THRESHOLDS` and `PERP_FEE_TIER_MAX_INDEX`
  (`math/fees`, promo-fee-tier), the mirror of the program's `determine_perp_fee_tier`,
  factored out so the tier rule has one definition. It applies the trailing-30d volume
  breakpoints, then the `State.promoFeeTier` floor that lifts every account to at least that
  tier while it is set. `User.getUserFeeTier` and `VelocityClient.getMarketFees` both select
  through it. Anything that ranks the tier itself, such as highlighting the active row of a
  fee schedule or showing progress to the next tier, should call it, or
  `User.getUserPerpFeeTierIndex`, instead of re-deriving the ladder.
- `User.getUserPerpFeeTierIndex(now?)`, the index `getUserFeeTier` reads its rates from.
- `getMarketFeesForFeeTier` (`math/fees`, market-fees-for-tier), the per-market and
  per-account modifiers the program applies on top of a tier's own rates, in program order.
  Those are the market's `takerFeeAddonTenthBps` surcharge (taker leg, perp only), its
  `feeAdjustment` percentage (both legs), the referee discount (taker leg), then the builder
  fee (taker leg, unscaled). `VelocityClient.getMarketFees` is this function with the tier
  and the account-derived inputs resolved, and it takes a fifth argument `feeTierOverride`
  that prices a given tier instead of the account's, with every other modifier unchanged. Use
  it to quote a tier an account is not on, for example the saving a promo makes against a
  volume tier, so both figures come from one pipeline. Existing calls are unaffected.
- Several types were added by the `types.ts` and IDL reconciliation (see §4.7).

### 4.7 SDK type reconciliation (`types.ts` against the IDL)

The hand-maintained TypeScript mirrors in `sdk/src/types.ts` are not generated from the IDL,
since the SDK does not use Anchor's `IdlAccounts`, `IdlTypes` or `IdlEvents` helpers, and
they had drifted from the generated `idl/velocity.json`. This batch realigns them.
Integrators who decoded accounts or events with the previous TS shapes should note the
following.

- Added fields, present in the IDL and emitted on-chain all along, missing from the TS type:
  - Account structs: `StateAccount.pauseAdmin` and `lpPoolFeatureBitFlags`;
    `PerpMarketAccount.poolId`; `SpotMarketAccount.expiryTs`;
    `UserStatsAccount.disableUpdatePerpBidAskTwap` and `pausedOperations`;
    `InsuranceFundStake.lastValidTs`; `AmmCache.bump`;
    `LPPoolAccount.targetOracleDelayFeeBpsPer10Slots` and
    `targetPositionDelayFeeBpsPer10Slots`.
  - `AMM`: the bid and ask reserve set (`askBaseAssetReserve`, `askQuoteAssetReserve`,
    `bidBaseAssetReserve`, `bidQuoteAssetReserve`), `lastOracleReservePriceSpreadPct`,
    `lastSpreadUpdateSlot`, `longSpread`, `shortSpread`, `referencePriceOffset`.
  - Event and record types: `DepositRecord` (`signer?`, `userTokenAmountAfter`);
    `OrderActionRecord` (`triggerPrice`, `builderIdx`, `builderFee`); `LiquidationRecord`
    (`bitFlags`); `LiquidatePerpRecord` and `LiquidateSpotRecord` (`protocolFee`).
- Corrected field types, with no on-chain change, where the TS type was wrong:
  - `LiquidationRecord.canceledOrderIds`, from `BN[]` to `number[]`.
  - `LiquidatePerpRecord.userOrderId` and `liquidatorOrderId`, from `BN` to `number`.
  - `OrderFillerRewardStructure.rewardNumerator` and `rewardDenominator`, from `BN` to
    `number`.
  - `RevenueShareSettleRecord.ts`, from `number` to `BN`.
- Removed phantom fields that never existed on-chain: `LPSwapRecord.outMint`,
  `LPSwapRecord.inMint`, `LPMintRedeemRecord.lpMint`.
- New exported types: `PrelaunchOracleParams`, `PythLazerOracle`,
  `UpdatePerpMarketSummaryStatsParams`, `SignedMsgWsDelegatesAccount`,
  `PerpMarketFeeSweepRecord`, `ProtocolFeeWithdrawRecord`, `TransferFeeAndPnlPoolRecord`.
- Events wired into `EventSubscriber`: `PerpMarketFeeSweepRecord`,
  `ProtocolFeeWithdrawRecord`, `RevenueShareSettleRecord`, `TransferFeeAndPnlPoolRecord` and
  `LPBorrowLendDepositRecord` are now registered in `EventMap`, `VelocityEvent` and the
  default `eventTypes` list (`events/types.ts`). All five have long been emitted on-chain
  with IDL entries and TS types, but `parseEventsFromLogs` dropped any event not in that
  registration list without reporting an error. They are now subscribable like any other
  record type.

---

## 5. On-chain layout and ABI notes

### 5.1 Current account sizes

These are the sizes the deployed program compiles in today, in account-data bytes including
the 8-byte Anchor discriminator. Every field offset quoted elsewhere in this section is a
Rust struct offset, so add 8 for the offset into account data.

| Account      | Size | Note |
| ------------ | ---- | ---- |
| `User`       | 4496 | `size_of::<User>()` is 4488. |
| `UserStats`  | 240  | |
| `PerpMarket` | 1560 | `size_of::<PerpMarket>()` is 1552. `clob_market` sits at struct offset 1296 and `quoter_slab` at 1328, and the trailing `_padding_future: [u8; 192]` starts at 1360. |
| `SpotMarket` | 1064 | `size_of::<SpotMarket>()` is 1056, of which the trailing `_padding_future: [u8; 256]` starts at struct offset 800. |
| `State`      | 1752 | `size_of::<State>()` is 1744. |
| `QuoterV0`   | 792  | The staging entry of the quoter registry. |
| `QuoterSlabV0` | 168 + 776 per slot | Capacity is the account's size, not a layout constant. |
| `ClobCrankConditionsV0` | 808 | |
| `UserConditionsV0` | 6040 | |

`PerpMarket` and `SpotMarket` each grew by 256 reserved bytes in `market-account-padding`,
from 1304 and 808 respectively. Accounts created before that change must be grown with the
`extend_account` crank (§3). Several change-log rows below were written when the sizes were
1304 and 808, and they quote those figures as historical context; the sizes in this table are
the current ones.

### 5.2 Discriminators and PDA seeds

- Account discriminators are unchanged for surviving accounts (`User`, `UserStats`, `State`,
  `PerpMarket`, `SpotMarket`, and the rest), because Anchor derives them from the account
  name. The same holds for surviving instruction discriminators.
- PDA seed strings are unchanged (`drift_state`, `user`, `spot_market_vault`, and the rest).
  Only the program ID changed, so every derived address differs from Drift's.

### 5.3 Account layout changes

- `User` gained `equity_floor: u64` at struct offset 4472 and `equity_floor_buffer: u64` at
  struct offset 4480, carved from the tail padding after `special_user_status`, which keeps 3
  padding bytes ahead of them. The buffer consumed the last 8 padding bytes. No other offset
  moved, so existing accounts stay valid and read both fields as 0, meaning disabled. Custom
  decoders must add the fields. `User` grew from Drift's 4376 bytes to 4496.
- `UserStats` layout is preserved across the gov-stake fee discount removal (#80) and the
  delegate-permissions addition (#45). Padding replaced `if_staked_gov_token_amount` in
  place, and `delegate_permissions: u8` (#45), `equity_breaker_tripped: u8` (equity-floor
  breaker) and `accelerated_referral_status: u8` (#429) were each carved from the trailing
  padding. The account size and every other field offset are unchanged, so existing accounts
  stay valid, but custom decoders must account for the three new bytes. Pre-upgrade accounts
  read them as 0. `update_user_gov_token_insurance_stake` and
  `update_delegate_user_gov_token_insurance_stake` no longer exist. `State` is untouched by
  the referral field, because the beta-scoped `ACCELERATED_REFERRAL_ENROLLMENT_ENABLED`
  constant gates automatic enrollment rather than a state field, so ending the beta is a
  program upgrade.
- `PerpMarket` grew across several PRs. #16 took it from 1216 to 1240 for Anchor-1.0's
  16-byte `PoolBalance` alignment. The AMM decoupling (#65) and the `HedgeConfig` addition
  (#66) reorganized it down to 1224. #75 took it from 1224 to 1304 with the embedded
  `FeeLedger` and the protocol fee fields. `market-account-padding` appended 256 reserved
  bytes, giving today's 1560. u128 and i128 fields are front-loaded for alignment. Any custom
  decoder that does not read the IDL must be rebuilt against `sdk/src/idl/velocity.json`.
- `PerpMarket.pending_revenue_share: u64` (audit #73) was carved in place from the 8-byte
  alignment padding that precedes `amm`, formerly `_padding_align_amm`, as a u64 at the same
  8-aligned offset. No other field offset moved, and the account size did not change for this
  field. Existing accounts read it as 0, meaning nothing owed, but custom decoders must
  replace the padding with the field. It is a per-market aggregate of accrued builder and
  referrer revenue share owed out of the pnl pool, maintained by the program and reserved by
  the fee sweep. `PerpMarketAccount.pendingRevenueShare` was added. As of
  `revshare-settle-liveness`, `settle_expired_market_pools_to_revenue_pool` refuses to delist
  a market whose counter is non-zero, rejecting with `UnsettledRevenueShareOnDelist`. A
  `Delisted` market therefore always reads 0, and indexers must not treat a delisted market's
  `pendingRevenueShare` as an outstanding liability. There is no time-based escape, because
  every row is terminally resolvable by the permissionless `settle_revenue_share`, which
  pays, or `forfeit_revenue_share_order`, which writes off on proof that it cannot be paid.
- `PerpMarket.bankruptcy_if_floor_pct: u32` replaces the 4-byte trailing padding immediately
  before `market_stats`. No other field offset moved, and the account size did not change for
  this field, so existing accounts stay valid, but custom decoders must add it. The fee
  sweep's IF drain leaves this fraction of open-interest notional, valued at the market's
  oracle TWAP, behind in `fee_ledger.pending_if_fee`. That keeps a standing first-loss tranche
  available to `resolve_perp_bankruptcy` that a permissionless sweep cannot clear ahead of a
  resolution. A value of `0` means `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT` (10 bps), so an account
  written before the field existed carries the tranche with no admin call. The sentinel
  `BANKRUPTCY_IF_FLOOR_DISABLED` (`u32::MAX`) turns the floor off. It is set by the new
  warm-admin instruction `update_perp_market_bankruptcy_if_floor_pct`.
- `PerpMarket.pending_bankruptcy_claims: u16` replaces 2 of the 6 alignment-padding bytes
  before `last_fill_price`, and the remaining 4 stay padding. No other field offset moved,
  and the account size did not change for this field. Existing accounts read 0, meaning no
  pending claim, but custom decoders must add it. It counts unresolved bankrupt quote debts
  booked against the market. A liquidation that latches a user bankrupt increments it, and
  the count is released when the debt is discharged. While it is above zero, the fee sweep
  withholds the whole `fee_ledger.pending_if_fee` rather than only the floor described above.
  A bankrupt estate's positions are closed before its debt resolves, so a floor sized on open
  interest can be zero at the moment the tranche is needed.
- `PositionFlag::BankruptcyClaim` (`0b1000`) is a new bit in `PerpPosition.position_flag`. It
  marks the position whose debt is counted in its market's `pending_bankruptcy_claims`, so a
  repeated latch cannot count the same debt twice. Decoders that match `position_flag`
  exactly, rather than masking, must add it.
- `SpotMarket` per-market withdraw and deposit limit fields (deposit-caps). Three fields were
  carved from the 13-byte alignment gap before `protocol_fee_pool`. They are
  `withdraw_circuit_breaker_bps: u16` at struct offset 740 and `max_deposit_bps_per_day: u16`
  at struct offset 742, both in basis points where 10000 is 100%, and
  `deposit_guard_threshold: u64` at struct offset 744, a token amount. No other field offset
  moved and the account size did not change for these fields, so `protocol_fee_pool` remains
  at struct offset 752. Existing accounts stay valid and read the new fields as 0, which
  means a default 25% breaker and a disabled deposit cap. SDK `SpotMarketAccount` gains
  `withdrawCircuitBreakerBps` and `maxDepositBpsPerDay` (`number`, basis points) and
  `depositGuardThreshold` (`BN`).
- `SpotMarket.if_last_settle_vault_amount: u64` (struct offset 792) replaced the 8-byte
  trailing padding. No other field offset moved and the account size did not change for this
  field. Existing accounts stay valid and read 0 until the market settles revenue once, and
  new markets initialize to 0, but custom decoders must add it. It holds the lowest IF vault
  balance since the end of the last revenue settle. `settle_revenue_to_insurance_fund` starts
  each period by writing the live vault balance plus the amount that settle transfers in, and
  `record_insurance_fund_outflow` lowers it on every path that moves tokens out of the vault,
  namely `remove_insurance_fund_stake`, `resolve_perp_pnl_deficit`, `resolve_perp_bankruptcy`
  and `resolve_spot_bankruptcy`. A transfer into the vault never raises it, so it lags the
  live vault by up to one `revenue_settle_period`. It is the base for the per-period
  revenue-settle APR cap in `settle_revenue_to_insurance_fund`, sized off
  `min(live_if_vault, this)`, so the cap counts only capital the fund held for the whole
  period. A donation transferred in right before a settle is absent from the field and cannot
  inflate the cap, and the running minimum closes the same route after a dip, since a loss
  draw or an unstake takes the vault down and a donation that restores the live balance does
  not restore the cap. A donation that survives a full period does count, because by then it
  belongs to the stakers pro rata and the fund really is that large. A `0` snapshot, meaning
  never settled or settled on an empty vault, gives a cap base of 0 for one period. That
  settle moves nothing but still records the balance it leaves behind, so the next period is
  normal. Integrators who read this field to predict a settle amount must apply the same
  `min`, not the live balance alone. This is the field's only consumer. The unstake-cancel
  share forfeiture is protected against donations independently (see below) and does not read
  it. There is no new instruction. `settle_revenue_to_insurance_fund` writes the snapshot, and
  the unstake, bankruptcy and deficit paths lower it.
- `PoolBalance` gained two fields from its own padding. `pending_interest_split_dust: u32` is
  at struct offset 20 and `pending_interest_dust: u64` at struct offset 24, taken from the
  14-byte `padding`, which shrank to 2 bytes and kept its name. The struct size (32) and
  every other field offset are unchanged, so the `PerpMarket` and `SpotMarket` layouts are
  untouched. Existing accounts read 0, which is the correct starting value. Only a spot
  market's `revenue_pool` and `protocol_fee_pool` use the new fields. `PoolBalance` in
  `sdk/src/types.ts` mirrors them.
- `State` slot-duration fields (slot-duration-scaling) were carved in place from the padding
  after `promo_fee_tier`. They are `slot_duration_ms: u16` at struct offset 1498,
  `pending_slot_duration_ms: u16` at 1500, `slot_duration_pad: [u8; 2]` at 1502,
  `slot_duration_effective_slot: u64` at 1504, and
  `slot_duration_transition_slots: [u64; 4]` at 1512, all formerly zeroed padding. `State`
  stays 1752 bytes and no other field offset moved. Pre-upgrade accounts read `0`, which
  means unset and resolves to the 400ms baseline. Custom decoders must replace the padding
  with the fields.

  On semantics, every wall-clock duration in the program is a typed `math::time::Millis`.
  Code constants are defined in milliseconds. Legacy admin-set fields keep their compact
  on-chain encoding in 400ms units and decode through typed getters, which covers oracle
  guard-rail staleness windows, per-market slot-delay overrides, `liquidation_duration` and
  auction durations. Measured intervals integrate per slot-duration regime through
  `math::time::SlotClock`, built from the transition archive, and forward window conversions
  use the `SlotDuration` at the current slot. Once any archive entry is set, the archive is
  authoritative over the legacy staging trio, which remains as fallback, where a staged
  `pending_slot_duration_ms` applies once the chain slot reaches
  `slot_duration_effective_slot`. Integrators converting slots to wall-clock must read the
  live value rather than assuming 400ms, using SDK `activeSlotDurationFromState(state,
  currentSlot)` and `elapsedMillis` for elapsed intervals. `SLOT_TIME_ESTIMATE_MS` is
  deprecated. The writer is the permissionless `sync_state_slot_duration`, which replaced the
  warm-admin `update_state_slot_duration_ms`. It takes one of the four IBRL feature-gate
  accounts, with the key and owner constrained on the accounts struct, verifies activation,
  and derives the effective slot from the `EpochSchedule` sysvar as the first slot of the
  epoch after activation, mirroring Agave. Transitions sync in order, re-syncs are
  idempotent, and there is no terminal bookkeeping transaction. `StateAccount.slotDurationMs`,
  `pendingSlotDurationMs`, `slotDurationEffectiveSlot` and `slotDurationTransitionSlots` were
  added.
- `MarketStatus` discriminants shifted (#5). The deprecated `FundingPaused`, `AmmPaused`,
  `FillPaused` and `WithdrawPaused` variants were removed, so the surviving variants are
  `Initialized` (0), `Active` (1), `ReduceOnly` (2), `Settlement` (3) and `Delisted` (4),
  against Drift's `ReduceOnly` (6), `Settlement` (7) and `Delisted` (8). `MarketStatus` is
  stored directly in `PerpMarket.status` and `SpotMarket.status`, so any custom decoder built
  against the old Drift discriminants will misread these states without reporting an error.
- `SignedMsgUserOrders` layout (feat/propamm). `SignedMsgOrderId` grows from 24 to 40 bytes. It
  gains `clob_order_id: u64` and an 8-byte `route_digest`, because a signed-message order now
  rests on the book rather than in `User.orders` and its record is what names the resting order
  and the route it signed for. The account's size derives from `size_of` rather than a
  hardcoded constant, so `space()` and any client allocation must be recomputed. The previous
  816-byte figure is wrong. Entry lifetime changes with it: an entry holding a live
  `clob_order_id` survives past the `max_slot` prune until that order leaves the book, and is
  cleared on fill, cancel, eviction and expiry. A missed clear leaks an entry against the
  128-entry cap.
- `QuoterV0` (new account, feat/propamm). Zero-copy, 792 bytes including the 8-byte
  discriminator; PDA seeds `["quoter", market_index as u16 LE, quoter_program, user]`. The
  staging half of the registry: `{ config: QuoterConfigV0 (736 bytes), padding: [u8; 48] }`.
  `QuoterConfigV0` holds one unified 12-slot CPI account list plus per-leg index lists
  (`quote_account_indexes` / `execute_account_indexes`), the three raw CPI discriminators
  (`quote_v0` / `execute_v0` / the optional `quote_l3_v0`, all-zero when unimplemented), the
  response account, the watch declaration, the oracle band, and the book-rule mirrors. There is
  no `is_approved` flag. Approval is presence in the market's slab. Two new error variants
  appended at the end of the enum: `InvalidQuoterConfig` (6375 / `0x18E7`) and
  `InvalidQuoterAuthority` (6376 / `0x18E8`).
- `QuoterSlabV0` (new account, feat/propamm). PDA seeds `["quoter_slab", market_index as u16
  LE]`, one per perp market. A 160-byte fixed header (`market: u16`, `capacity: u16`, `bump:
  u8`, `clob_market: Pubkey` at account offset 16, reserved padding) after the 8-byte
  discriminator, then `capacity` back-to-back 776-byte `QuoterSlotV0 { entry: Pubkey,
  suspended: bool, padding: [u8; 7], config: QuoterConfigV0 }` starting at byte 168. Capacity
  is the account's size, not a layout constant, and the approval flow keeps it right-sized: a
  slab is born with one slot, `update_quoter_approved` grows the account by exactly the slot it
  needs (the admin pays the rent) and gives trailing vacancy back on revocation (ceiling 128
  slots); occupied slots never move. The stored `bump` is the slab's own PDA bump, because the
  slab is the identity velocity signs every external quoter CPI as. Slot 0 is the market's
  book; a vacant slot's `entry` is the default pubkey. Router fills, `quote_router` and every
  CLOB order instruction carry this account where they carried `QuoterV0` entries; a slot is
  consulted when its `response_account` rides the transaction. New error variants appended at
  the end of the enum: `QuoterSlabFull` (6405 / `0x1905`) and `QuoterNotOnSlab` (6406 /
  `0x1906`).
- Transaction fee rails (feat/propamm). `State` gained `transaction_fee_rails:
  TransactionFeeRails`, which is `{ inclusion_lamports: u32, signature_lamports: u32,
  resource_fee_numerator: u32, resource_fee_denominator: u32,
  max_priority_micro_lamports_per_cu: u32 }`, 20 bytes, alongside `hot_flow_authority: Pubkey`
  (32), `liquidation_crank_reimbursement_bps: u16`, `sol_spot_market_index: u16` and 2 bytes of
  trailing filler. All of it comes out of tail `padding`, which shrinks to 110 bytes; `State`
  stays 1752 bytes and no existing field moved. `initialize` writes a flat 5,000 lamports per
  signature and nothing else, which is what the network charges today; a zero denominator
  prices cost units at nothing. New warm/cold-admin instruction
  `update_transaction_fee_rails(rails)` over `AdminUpdateState`. Every relay crank payment is
  derived from it, so a change to the network's fee model is one write here rather than a
  re-price of every market.
- Per-crank keeper payments (feat/propamm). `ClobCrankConditionsV0.keeper_payment_lamports`
  (`u64`) became `crank_payments: CrankPaymentsV0`, seven `u32` lamport figures, one per crank
  (`removal`, `cross`, `taker_origin_cross`, `trigger`, `liquidation`, `force_cancel`,
  `refill`), plus a `u32` of padding. The account stays 808 bytes, so no conditions PDA needs a
  resize. A book removal and a two-legged cross differ by an order of magnitude in what they
  request and the network charges a transaction for what it requests, so one figure for the
  market either underpays the cross or overpays every removal. The figures are derived at
  attach time from the rails above and the cost units an admin measured; they are stored rather
  than recomputed so a crank costs no compute to price itself and a staged executor cannot
  re-price its own work. `update_perp_market_clob_quoter`'s first argument changed from
  `keeper_payment_lamports: u64` to `crank_cost_units: CrankCostUnitsV0` (the same seven
  fields). `SyncLiqConditionsArgs.sync_payment_lamports: u64` became `sync_cost_units: u32` for
  the same reason, and `sync_liq_conditions` / `sync_user_conditions` gained a read-only
  `state` account (position 2, after `payer`) to price it. `UserConditionsV0` is unchanged. It
  still stores the derived lamport figure, which is what keeps a staged resync from re-pricing
  itself.

### 5.4 Instruction account-list changes

- `request_remove_insurance_fund_stake` (if-request-remove-settle). The instruction settles
  already-due revenue before freezing the exit value, so its `#[derive(Accounts)]` gained
  `state`, `spot_market_vault`, `velocity_signer` and `token_program`. The order is `state`,
  `spot_market`, `insurance_fund_stake`, `user_stats`, `authority`, `spot_market_vault`,
  `insurance_fund_vault`, `velocity_signer`, `token_program`, plus optional transfer-hook
  `remaining_accounts` and token-mint like the add path. The discriminator is unchanged, but
  a manual builder that does not use an SDK must now supply these accounts.
  `cancel_request_remove_insurance_fund_stake` was split onto its own
  `CancelRequestRemoveInsuranceFundStake` struct with the same 5 accounts it always had
  (`spot_market`, `insurance_fund_stake`, `user_stats`, `authority`,
  `insurance_fund_vault`), so cancel callers saw no change at that point. There is no
  on-chain account layout change.
- `cancel_request_remove_insurance_fund_stake` (if-cancel-settle-first / OtterSec #141). The
  instruction now settles already-due revenue before pricing the forfeiture, so it carries
  the same accounts as `request_remove_insurance_fund_stake`, with `state` prepended plus
  `spot_market_vault`, `velocity_signer` and `token_program`. This is a reorder rather than
  an append, so manual ix builders must rebuild the account list instead of adding to it.
  Both SDKs pass them automatically. The `vaults` program's CPI wrapper
  (`CancelRequestRemoveInsuranceFundStake`) gained `velocity_spot_market_vault`,
  `velocity_state`, `velocity_signer` and `token_program`, which is itself an ABI change to
  the vaults instruction.
- `delete_user` and `force_delete_user` each gained a required account
  (delete-user-orphan-builder-rows / OtterSec #128). Both now take the authority's
  `RevenueShareEscrow` PDA (`["REV_ESCROW", authority]`). `DeleteUser` takes it as a fifth
  account appended after `authority`, and `ForceDeleteUser` as a seventh account appended
  after `velocity_signer`, so it still sits ahead of `remaining_accounts`. Anyone
  constructing either instruction manually must add it. It is required even when the
  authority never created an escrow, because the address is pinned by `seeds`, so an
  uninitialized account proves absence rather than signalling an omitted check. That is what
  stops a caller skipping the settlement by leaving it out. The TS SDK derives and passes it
  automatically in `getUserDeletionIx` and `getForceDeleteUserIx`, so SDK callers need no
  change. `getForceDeleteUserIx` also no longer appends the escrow to `remaining_accounts`
  for an account holding builder orders. The named account replaces it, and the program never
  read the trailing copy. velocity-rs picks both up from the regenerated IDL.
- Three instructions gained a required `State` account (slot-duration-scaling), because they
  load market and oracle maps and so need the live slot duration. In velocity they are
  `update_user_margin_trading_enabled` and `update_user_pool_id`, now on a separate
  `UpdateUserWithMarkets` accounts struct, with the other `update_user_*` handlers unchanged.
  In jit-proxy it is `check_order_constraints`. In the vaults program,
  `manager_update_borrow`, `update_margin_trading_enabled` and `update_pool_id` gained
  `velocity_state` for the same reason. Each new account is appended after the existing ones
  and is read-only. Hand-built transactions must add it, and the SDKs and CLI fill it in.
  Without it, those paths sized their oracle staleness windows off the 400ms baseline, which
  on a faster chain rejects oracles the rest of the protocol accepts.
- `trigger_order` gained a required `user_stats` account (equity-floor-gaps). It is the order
  owner's `UserStats`, derived `["user_stats", authority]`, appended to the `triggerOrder`
  account list so the handler can read the authority-wide `equity_breaker_tripped` flag.
  Existing callers must add this account or the instruction fails account resolution. The
  workspace SDK (`getTriggerOrderIx`, `buildTriggerOrderInstruction`) and the velocity-rs
  builder already do. As of equity-breaker-lazy-trip the account is writable, because the
  cancel path may arm the breaker inline, so clients built from an older IDL that pass it
  read-only fail the `mut` constraint. No other instruction's account list changed and no
  account layout moved.
- `liquidate_spot` gained a required `liquidator_stats` account (equity-floor-gaps). It is
  the liquidator's `UserStats`, writable, derived `["user_stats", liquidator authority]` and
  constrained `is_stats_for_user`, so the handler can read the authority-wide
  `equity_breaker_tripped` flag and bar a tripped authority from position-acquiring
  liquidations. `liquidate_perp` already had `liquidator_stats`, and only `liquidate_spot`'s
  account list changed. Existing callers must add this account. The workspace SDK
  (`getLiquidateSpotIx`) and the velocity-rs `liquidate_spot` builder already do. No account
  layout moved.
- `liquidate_spot_with_swap_begin` and `_end` each gained a required `liquidator_stats`
  account (breaker-liquidation-followups). It is the liquidator's `UserStats`, read-only,
  derived `["user_stats", liquidator authority]` and constrained `is_stats_for_user`, and it
  is appended to both instructions' account lists. It is therefore the last fixed account at
  index 11, and every pre-existing account keeps its index. Position matters on this pair
  beyond the usual reason. The begin handler introspects the matching end instruction and
  binds accounts by index, and the swap accounts in `remaining_accounts` start immediately
  after the fixed block, which is now 12 accounts long rather than 11. `begin` reads the
  account to bar a tripped authority from swap-backed liquidations, matching the four direct
  routes. Existing callers must add this account. The workspace SDK
  (`getLiquidateSpotWithSwapIx`, and `getJupiterLiquidateSpotWithSwapIxV6` through it) and
  the velocity-rs builders already do. No account layout moved.
- Optional referrer `UserStats` in the fill paths (#429 accelerated-referrals).
  `fill_perp_order`, `place_and_take_perp_order`, `place_and_make_perp_order` and
  `place_and_make_signed_msg_perp_order` read one more `remaining_account` after a referred
  taker's `RevenueShareEscrow`, which is the referrer's authority-matching `UserStats`,
  read-only so a popular referrer is not a write-lock hotspot. It selects the Accelerated
  rather than the Standard referrer reward. The account is peeked rather than required.
  Anything that is not an initialized `UserStats` belonging to the escrow's referrer is left
  for the group that owns it and the fill proceeds at the Standard rate. Clients built
  against the older layout keep working and simply forgo the Accelerated rate.
- `resolve_spot_bankruptcy` now requires the quote spot market to be writable
  (bankruptcy-recover-then-forfeit). It was already a required read-only account, because the
  claim passes read it, but a recovered claim now lands in the estate's quote deposit. SDK
  `getResolveSpotBankruptcyIx` passes `QUOTE_SPOT_MARKET_INDEX` in
  `writableSpotMarketIndexes`. Previously it never added index 0 at all, so the account
  arrived only when the user or liquidator happened to hold a live quote row.
- `resize_signed_msg_user_orders` lost its now-redundant `user` account from the
  `ResizeSignedMsgUserOrders` accounts struct (#271 signed-msg-hardening), which is an IDL
  and ABI change. SDK `resizeSignedMsgUserOrders` and
  `getResizeSignedMsgUserOrdersInstruction` drop the trailing `userSubaccountId` parameter
  accordingly.
- The `vaults` program changed 20 instruction account lists
  (vault-nav-spot-market-refresh, following vault-nav-interest-refresh). See
  [§6.2 vault-nav-spot-market-refresh](#vault-nav-spot-market-refresh) and
  [§6.2 vault-nav-interest-refresh](#vault-nav-interest-refresh) for the mechanism. The
  affected instructions are `deposit`, `manager_deposit`, `withdraw`, `manager_withdraw`,
  `protocol_withdraw`, `force_withdraw`, `request_withdraw`, `manager_request_withdraw`,
  `protocol_request_withdraw`, `cancel_withdraw_request`, `manager_cancel_withdraw_request`,
  `protocol_cancel_withdraw_request`, `apply_rebase`, `apply_rebase_tokenized_depositor`,
  `apply_profit_share`, `tokenize_shares`, `redeem_tokens`,
  `transfer_vault_depositor_shares`, `liquidate` and `manager_update_fees`. Each carries
  `velocity_state` and `velocity_program` for the CPI that books lending interest before
  pricing shares. The markets themselves are not named accounts. They travel as writable spot
  markets inside the instruction's `remaining_accounts`, which the vaults program already
  passes to `load_maps`. `manager_borrow`, `manager_repay` and `manager_update_borrow` are
  unchanged by design, because their equity snapshot only feeds event fields.

  An earlier revision of this fix named the denomination market as three extra accounts on
  those instructions. Those are removed: `velocity_spot_market` and `velocity_oracle` from
  all of them, and `velocity_spot_market_vault` from the thirteen that do not need it for a
  deposit or withdraw CPI of their own. `VaultClient` builds every affected instruction, so
  SDK callers need no change. Manual builders must drop the removed accounts and must mark
  every spot market in `remaining_accounts` writable, because velocity fails the load with
  `SpotMarketWrongMutability` when it is asked to refresh a market it was handed read-only.
  There is no on-chain account layout change in either program.

  `manager_update_fees` additionally gained a `velocity_user` account plus the spot market
  and its oracle in `remaining_accounts` (vaults-fee-policy-grandfathering). SDK
  `getManagerUpdateFeesIx` passes them, and protocol vaults still append `VaultProtocol`.
- `place_and_make_signed_msg_perp_order` is removed (feat/propamm). It existed only to match a
  signed-message order already resting in `User.orders`, and no signed-message order rests
  there any more. There is no v0/v1 pair and no frozen second path; velocity controls the
  fillers. The taker-facing signed message and its broadcast to swift are unchanged.

### 5.5 New instructions with layout or account implications

- `update_perp_market_clob_book_config` and `resize_perp_market_clob_book` (velocity,
  feat/propamm, warm/cold admin) are the only paths that change an attached book's rules or
  grow its arena, because the market's quoter slab is the book's config authority. Accounts
  for the config path: `admin`, `state`, `perp_market`, `quoter` (writable), `quoter_slab`
  (writable), `clob_market` (writable), `clob_program`. The resize path drops `quoter`, takes
  `quoter_slab` read-only and adds `system_program`, and `admin` is writable because it pays
  the rent.

- `refresh_spot_market_interest` (velocity) books the lending interest of up to sixteen spot
  markets in one call. Accounts: `state`, plus writable spot markets in `remaining_accounts`.
  Argument: `market_indexes: Vec<u16>`. It is permissionless, like the single-market
  `update_spot_market_cumulative_interest` crank it sits beside, and it keeps that crank's
  `exchange_not_paused` guard. It differs from that crank in two deliberate ways. It passes
  no oracle, so it leaves every oracle TWAP untouched, and it has no `spot_market_valid`
  access control and makes no spot-vault assertion.
  `update_spot_market_cumulative_interest` is unchanged and remains the crank that keeps a
  spot market's oracle EMA fresh. SDK: `VelocityClient.refreshSpotMarketInterest` and
  `refreshSpotMarketInterestIx`.

  Operator note. Because the vaults program CPIs this instruction, a fully paused exchange,
  meaning every `ExchangeStatus` bit set, blocks the vault instructions that snapshot NAV,
  including `request_withdraw` and `cancel_withdraw_request`. A partial pause does not, since
  `exchange_not_paused` trips only on `is_all()`. Deposits, withdrawals, fills and
  liquidations are already blocked in a full halt, so the added coupling is share accounting
  only, and it lifts when the halt lifts.
- Mainnet `initialize` requires a fixed signer (#158). The one-time global `State` creation
  locks the `admin` account to `state_init_authority`
  (`prpHJmuXnqdaz92tBVdwsqmqyhqPLuq5Km35a5QWco3`) on real mainnet builds only, to prevent
  front-running of genesis. Devnet, localnet and the integration-test build are unaffected.

### 5.6 Error codes

Error codes are ABI-stable. Removed variants were renamed to `Deprecated*` stubs in place,
preserving their numeric codes, and new variants are appended at the end. The tail of the
enum is:

| Code | Variant |
| ---- | ------- |
| 6350 | `SpotDlobTradingDisabled` |
| 6351 | `InvalidAdminTier` |
| 6352 | `WithdrawGuardThresholdNotionalTooLarge` |
| 6353 | `InvalidProtocolFeeRecipient` |
| 6354 | `InsufficientProtocolFees` |
| 6355 | `InvalidNativeStateAccount` |
| 6356 | `InvalidNativePerpMarketAccount` |
| 6357 | `IsolatedPositionDisabled` |
| 6358 | `EquityBelowFloor` |
| 6359 | `InvalidEquityFloorTransfer` |
| 6360 | `IFDepositMintsZeroShares` |
| 6361 | `LiquidationWorsensAccountHealth` |
| 6362 | `PerpBankruptcyMustPrecedeSpot` |
| 6363 | `InvalidRevenueShareRecipient` |
| 6364 | `DailyDepositLimit` |
| 6365 | `ReservedSpotMarketName` |
| 6366 | `CannotModifyBuilderOrder` |
| 6367 | `InvalidAccountExtension` |
| 6368 | `InvalidEquityBreakerReset` |
| 6369 | `InvalidNativeInstructionData` |
| 6370 | `MmOracleUpdateDisabled` |
| 6371 | `SpotMarketInterestStaleForMargin` |
| 6372 | `UnsettledRevenueShareOnDelist` |
| 6373 | `RevenueShareOrderNotForfeitable` |
| 6374 | `VammQuoteManagementValueOutOfBounds` |
| 6375 | `InvalidQuoterConfig` |
| 6376 | `InvalidQuoterAuthority` |
| 6377 | `InsufficientCrankReservoir` |
| 6378 | `OrderPlacedOnClob` |
| 6379 | `OrderAwaitingTriggerRecross` |
| 6380 | `CrossMatchImbalanced` |
| 6381 | `CrossMatchUnprofitable` |
| 6382 | `UnattestedFastActivation` |
| 6383 | `InvalidQuoterResponse` |
| 6384 | `QuoterOverfilled` |
| 6385 | `QuoterFillOffQuote` |
| 6386 | `QuoterSubjectNotPermitted` |
| 6387 | `TooManyQuoterWireUsers` |
| 6388 | `SignedRouteMismatch` |
| 6389 | `SignedRouteEntryMissing` |
| 6390 | `CrossedTakerRemainderPending` (deprecated, no longer emitted) |
| 6391 | `NoTakerOriginCross` |
| 6392 | `TakerOriginCrossWorseForTaker` |
| 6393 | `InsufficientCrankTreasury` |
| 6394 | `CrankReservoirNotLow` |
| 6395 | `InvalidUserConditionsSync` |
| 6396 | `FillerOmittedReachableMaker` |
| 6397 | `FillerPaddedTheUserSet` |
| 6398 | `FillerObligationUncountable` |
| 6399 | `QuoterFilledShort` |
| 6400 | `FillerCarriedUnroutedQuoter` |
| 6401 | `ReduceOnlyOrderCannotRestOnClob` |
| 6402 | `LiquidationConflictsWithClobOrders` |
| 6403 | `QuoterReportExceedsReservation` |
| 6404 | `UnattestedSynchronousTake` |
| 6405 | `QuoterSlabFull` |
| 6406 | `QuoterNotOnSlab` |
| 6407 | `CrossMatchLegsDoNotCross` |
| 6408 | `TakerExposureNotProtocolOwned` |
| 6409 | `ClobRestUnavailable` |
| 6410 | `OrderTypeNotConditional` |

Codes 6411 to 6458 replace `DefaultError` at the sites feat/propamm added, so a router, CLOB,
quoter or crank failure decodes to a name that states the cause. They cover quoter CPI
encoding and account wiring, relay condition blocks, crank conditions, cross participants,
router quote buffers, feature gates and slot duration sync. `DefaultError` keeps every site
Drift already used it for, so its meaning is unchanged on inherited instructions.

Decode errors by code as before, but expect `Deprecated*` names for retired features.

### 5.7 Behavior changes visible through the ABI

- `add_insurance_fund_stake`'s `amount` is an upper bound rather than the staked amount
  (if-add-exact-share-pricing). IF shares are indivisible, so the program transfers only the
  portion of `amount` that prices to whole shares and leaves the remainder, always less than
  one share price, in the depositor's token account. A request below the price of a single
  share is rejected with `IFDepositMintsZeroShares` (6360). Read the staked amount from
  `InsuranceFundStakeRecord.amount`. Do not assume the token account was debited in full, and
  do not derive minted shares from the requested amount.
- Unstake-cancel share forfeiture (`cancel_request_remove_insurance_fund_stake` and
  `calculate_if_shares_lost`) is framed and documented as withdraw-and-restake at the current
  active share price. A cancel is modeled as completing the withdrawal of the requested
  shares, paying out the value frozen at request time, and immediately re-staking the
  resulting tokens at the live price. Escrow-window appreciation is therefore forfeited, which
  is the anti-free-option rule, while a cancel with no appreciation is a no-op. It is immune
  to donations without consulting any accounted balance, because the withdraw leg is bounded
  by the request-time snapshot `last_withdraw_request_value`, so a raw SPL donation, spread
  pro-rata across all shareholders, cannot manufacture forfeiture an attacker could
  profitably capture. There is no ABI, account or token-flow change to the cancel instruction,
  which moves no tokens, and the vaults-program CPI wrapper is unaffected.
- `LiquidationRecord.bankrupt` is state-derived rather than a constant (#174). The top-level
  `bankrupt` flag on `LiquidationRecord`, a sibling of the nested `perpBankruptcy` and
  `spotBankruptcy` sub-records, which have no `bankrupt` field of their own, now reflects
  whether the user still holds a bankrupting liability after the resolve call completes,
  rather than always being `true`. Read it as `record.bankrupt`, not
  `record.perpBankruptcy.bankrupt`. The wire type is unchanged, still a `bool`, so this is
  invisible to type-checkers. Indexers and downstream consumers that assumed `bankrupt ==
  true` on every emitted record must re-check the field's value rather than treating its
  presence as the signal.
- Oracle support is Pyth (push), Pyth Lazer, Prelaunch and QuoteAsset. Switchboard is a
  `Deprecated*` enum stub. The legacy Pyth pull variants (`PythPull`, `Pyth1KPull`,
  `Pyth1MPull`, `PythStableCoinPull`) keep their original names rather than `Deprecated*`.
  All deprecated and removed sources return `InvalidOracle` if used.
- `OracleSource` Switchboard variants were renamed to their deprecated keys. `Switchboard`
  became `DeprecatedSwitchboard` and `SwitchboardOnDemand` became
  `DeprecatedSwitchboardOnDemand`, with discriminants preserved. The SDK's `OracleSource`
  class mirrors this with `DEPRECATED_SWITCHBOARD` and `DEPRECATED_SWITCHBOARD_ON_DEMAND`,
  whose Borsh keys are `deprecatedSwitchboard` and `deprecatedSwitchboardOnDemand`. Any code
  still matching on the old `switchboard` or `switchboardOnDemand` keys will fail to decode
  these oracle sources.

### 5.8 PropAMM quoter interface

These notes cover the wire and signer contract between velocity and an external quoter
program (feat/propamm). The registry accounts are in §5.3.

- Quoter CPI signer (feat/propamm). The market's `QuoterSlabV0` PDA (seeds `["quoter_slab",
  market_index as u16 LE]`) is the one identity velocity signs every external quoter CPI as,
  across the book's place, cancel, and crank surface and the registry `quote_v0`,
  `quote_l3_v0`, and `execute_v0` legs alike. A CLOB book's `place_authority` and a midpoint
  instance's `execute_authority` are set to it, and a quoter's registered account list names it
  where the signer goes. Sharing one per-market key is safe because approval refuses a
  registered list that names any other approved quoter's response account, and every
  authority-trusting instruction on a callee requires its response account, so a forwarded
  signature has no instruction it can complete. Per-market seeds keep it inert on every other
  market. Distinct from `State.signer` (seeds `["velocity_signer"]`), which stays the SPL token
  authority on every `spot_market_vault` / `insurance_fund_vault` and the protocol account's
  `User` authority.
- `QuoterReportExceedsReservation` (6403 / `0x1903`). Appended at the end of the `Error` enum.
  It is thrown when a quoter's response claims more filled or removed base, or more retired
  open-order slots, than velocity reserved for the user it names. The reservation is written at
  placement under the owner's signature, so this is the bound that keeps a book to orders its
  users really placed.
- Quoter interface, optional third leg (feat/propamm). `quote_l3_v0` answers *who* a ladder
  stands on. `L3ArgsV0 { direction, size, max_rows }` in, an `L3ResponseV0` of `L3RowV0 {
  price, size, order_id, node_index, user, flags, placed_slot }` (72 bytes each) out, written
  into the quoter's response account like every other response and located by the same
  `ResponsePointerV0`. A quoter implements it when its ladder stands on other people's orders.
  Of the quoter types only a book does. Everyone else leaves
  `QuoterV0::quote_l3_v0_discriminator` zero, and a reader attributes the whole ladder to the
  one `user` the entry names. The CLOB's implementation is capped at `L3_ROWS_CEILING` rows,
  which is what its existing response region holds and is 114 at the current region size. The
  market account rides every CPI and the runtime charges compute per byte of it, so describing
  a deeper book never widens it. `L3_ROW_FLAG_TAKER_ORIGIN` marks a migrated taker remainder,
  which is depth a cross cannot count on.
- Quoter CPI wire (feat/propamm). `QuoteArgsV0` / `ExecuteArgsV0` are borrowed
  (`QuoteArgsV0<'_>`), and `users` is a sequence a quoter reads in place: a `u32` count
  followed by that many 34-byte `ClobUserRefV0`s, at most `USER_SET_CAPACITY` (48) of them, and
  an empty set means unrestricted. The encoded argument is 4 bytes when no user is named and at
  most 1636 when 48 are. It was a fixed-width `QuoterUserSetV0` (a `u8` count plus
  `[ClobUserRefV0; 48]`, 1633 bytes on every call, copied into the quoter's stack frame), and
  before that an `Option<Vec<ClobUserRefV0>>`. The set leads the struct so its refs land on an
  even offset, which is the alignment a `ClobUserRefV0` reference needs; a field added ahead of
  it must keep that true. Velocity writes the bytes with wincode (`quoter_spec::write_args`,
  framed by `quoter_spec::ARGS_CONFIG`, which is `anchor_lang_v2::BORSH_CONFIG`), so one
  declaration defines both halves. Both legs also take the perp market index and reject an
  entry registered for a different market. A quoter program that implements this interface must
  update its arg decoding, and must refuse a set longer than 48: the caps address a user by its
  index in the set, and the exclusion bitmap holds one bit per slot up to that capacity, so a
  longer set would present an excluded user as settleable.
  `quoter_spec::user_set_within_capacity` is that check; the CLOB
  (`ClobError::OversizedUserSet`) and the midpoint (`MidpointError::OversizedUserSet`) apply it
  on both legs. Five error variants appended at the end of the enum: `InvalidQuoterResponse`
  (6383 / `0x18EF`), `QuoterOverfilled` (6384 / `0x18F0`), `QuoterFillOffQuote` (6385 /
  `0x18F1`), `QuoterSubjectNotPermitted` (6386 / `0x18F2`) and `TooManyQuoterWireUsers` (6387 /
  `0x18F3`). No account layout change. `reference_price` is an `Option<u64>`. `None` means a
  read that settles nothing, which is what `quote_router` and the cross-match resolvers send.
  The CLOB refuses a call whose caps carry a quote budget but no reference price
  (`ClobError::MissingReferencePrice`, 6031). The midpoint skips its band on a quote with no
  reference price, refuses an execute with none (`MidpointError::MissingReferencePrice`, 6012),
  and fails its band on a zero price.
- Quote price bound (feat/propamm). `QuoteArgsV0` gained a trailing `limit_price: u64` which is
  the worst price the caller will fill at, in `PRICE_PRECISION`, with zero meaning no bound.
  The widest quote argument is therefore 8 bytes longer than the widest execute argument; a
  quoter that decodes `QuoteArgsV0` positionally must read the field or it will read the next
  call's bytes at the wrong offset. `ExecuteArgsV0` is unchanged: execute is handed a size
  already cut off the ladder, so its walk visits no level a bound would remove. Honouring the
  bound is advisory, like the per-user caps: a ladder is walked best price first, so a quoter
  may stop as soon as a level is worse than the limit, and the caller discards those levels
  either way. What it saves is compute. A transaction is billed for the limit it requests, so a
  level the fill would never take is paid for twice. Velocity passes the taker's own limit
  price, resolved without the oracle: an order priced by `oracle_price_offset`
  passes zero rather than a guess, so the bound is either exactly the
  fill's own or absent. Discovery callers (`quote_router`, the cross-match resolvers) pass
  zero. No account layout change.
- Per-quoter base room (feat/propamm). `QuoteArgsV0` and `ExecuteArgsV0` both gained a trailing
  `self_base_room: u64`, the most base the quoter's own settlement user may take on, bounded by
  that user's own margin, with `u64::MAX` meaning unbounded. Both argument types are therefore
  8 bytes longer, and a quoter that decodes them positionally must read the field. A book
  ignores it: its makers rest depth that was margin-reserved at placement, and each is sized
  separately in `UserCapsV0`. It exists because `UserCapV0::budget` cannot express this bound.
  A budget prices the gap between a reserved order's limit and the mark, and it is unbounded at
  a price in the owner's favour, while a quoter filling from one account reserves nothing and
  pays initial margin on the base it takes. Honouring it is advisory, like the caps: velocity
  trims the returned ladder to the same number, so a quoter that ignores it publishes depth the
  caller then cuts. A quoter quoting for the taker itself is sent zero. Discovery callers
  (`quote_router`, the cross-match resolvers) send `u64::MAX`. The execute leg must carry the
  value its quote carried. No account layout change.
- Who a quoter may settle for (feat/propamm). A `Custom` entry is still held to the one account
  its registration consented for. A `Clob` entry may now name any user the transaction already
  carries, except the taker. Velocity used to read the book's arena and hold the response to
  the makers resting there; that check was the program against itself, since the arena is the
  book program's own state, so a book that wanted to name a stranger would write the stranger
  into a node first. What bounds a book instead: the type is a warm-admin decision at
  registration (a maker registering its own entry can only type it `Custom`). A market's
  `clob_market` is set once and never changed: `initialize_quoter` designates the book account,
  approval refuses a `Clob` entry naming any other, and `has_one` holds the attach to it. The
  response may only name loaded users, every balance change is held to the quoted prices, and
  every user touched is margin-checked after the fill. Approving a *third party's* book program
  is what would turn this into an exposure, the same way it already would for the removal
  reports velocity takes on faith. `initialize_quoter` gained a `state` account for the admin
  check.
- Protected flow and the cross crank (feat/propamm). `QuoteArgsV0.taker_served_window` says the
  taker's flow served a protection window. Two things read it: a `Clob` entry with a non-zero
  `book_default_activation_delay_slots` quotes an unprotected taker no depth, and a quoter may
  gate on it itself (the midpoint's `require_attested_flow`). Swift attestation sets it, and so
  does a crank that measured how long the book orders it sweeps have rested
  (`SERVED_WINDOW_MIN_SLOTS`, two slots). `crank_cross_match` reports `false` whenever the
  route consults a `Custom` entry that quotes. A `Custom` quoter prices during the call and
  keeps no resting order, so the crank has no rest to measure and the crossing side may be a
  quoter that repriced in the same slot. The consequence for an integrator running a `Custom`
  quoter: a cross between that quoter and the market's book settles through this crank only
  when the book runs no speed bump and the quoter does not require attested flow. Otherwise the
  cross rests until ordinary flow clears it, and reaching a protected source still means going
  through swift. `crank_taker_origin_cross` is unaffected, because it vouches for one named
  order whose rest it measured, and so is a liquidation.

---

## 6. Change log vs upstream (merged PRs)

Each row says what changed and what an integrator must do about it. Rows whose detail runs
long carry a one-line summary here and a link into §6.2.

### 6.1 Change log

| PR        | Change                                                                                                                                                                                                                                                                                                                                                                       |
| --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| #1        | `transfer_fee_and_pnl_pool` instruction (warm-admin) rebalances the AMM fee pool against the PnL pool. SDK `AdminClient.transferFeeAndPnlPool` / `getTransferFeeAndPnlPoolIx`, new `TransferFeeAndPnlPoolDirection` export, emits a `TransferFeeAndPnlPoolRecord` event |
| #2, #47   | Remove high leverage mode: the instructions, `User.margin_mode` and `MarginMode`, the `PerpMarket` HLM fields, the HLM config subscribers, and `HIGH_LEVERAGE_MIN_MARGIN_RATIO` (#47). The error variants became `Deprecated*` stubs |
| #5        | `MarketStatus` refactor: extracted into its own module, the deprecated `FundingPaused` / `AmmPaused` / `FillPaused` / `WithdrawPaused` removed, and the discriminants for `ReduceOnly` / `Settlement` / `Delisted` shifted from 6, 7, 8 to 2, 3, 4 |
| #6        | Disable spot DLOB trading. New error `SpotDlobTradingDisabled` (6350) |
| #7        | Remove legacy Pyth pull and push (program instructions plus SDK clients). The default `oracleSource` becomes `PYTH_LAZER` |
| #12       | Funding floor raised from 7.3% to 10.95% annualized (`FUNDING_RATE_OFFSET_DENOMINATOR` from 5000 to 3333), plus a 0.05% dead-zone clamp (`FUNDING_RATE_CLAMP_DENOMINATOR` = 2000) |
| #13       | Remove prediction markets. `ContractType::Prediction` becomes `DeprecatedPrediction`, and `InvalidPredictionMarketOrder` becomes `DepreciatedPredictionMarketOrder` (6284) |
| #14       | Remove Switchboard oracle support, classic and on-demand |
| #16       | Anchor 0.29 to 1.0. `PerpMarket::SIZE` 1216 to 1240 for 16-byte `PoolBalance` alignment. IDL account names are camelCase, which affects string-keyed coder calls |
| #17       | Special user account status: `User.special_user_status` bitmask plus the `SpecialUserStatus` enum, new `update_special_user_status` (admin) and `special_transfer_perp_position_to_vamm` (user), new error `InvalidTransferPerpPosition` (6312) |
| #18       | SDK quote-mint cleanup: `USDC_MINT_ADDRESS` becomes `QUOTE_MINT_ADDRESS`, the devnet value becomes the dUSDT placeholder, and mainnet is unchanged |
| #21       | SDK core (`VelocityCore`) expansion, isomorphic Anchor build, perp instruction delegation |
| #26       | New program ID plus devnet deployment |
| #36       | Remove fuel, vAMM LP, and the Serum and Phoenix orderbooks. Add admin commands |
| #37       | SDK rename from Drift to Velocity. The aliases have since been removed |
| #38       | Remove protected maker mode: instructions, the `ProtectedMakerModeConfig` account and PDA helper, SDK math/admin/client methods, and the `PerpMarket` fields |
| #39       | Yarn to Bun |
| #45       | `transfer_deposit_by_delegate` and `update_user_allow_delegate_transfer`, plus the `UserStats.delegate_permissions` field carved from padding with the size unchanged |
| #51       | `oracle_price_offset` widened to i64 |
| #52 to #59 | release-please publishing for the SDK (`0.0.x`), later replaced by changesets |
| #60       | MM oracle validation: strict slot-monotonicity, a minimum 2-slot gap, and a 1% per-write step cap on the existing MM oracle native handler |
| #63       | Zero-copy native admin handlers, a refactor of the inherited native fast-path entrypoint |
| #65       | Decouple the AMM from the rest of the codebase |
| #66       | VLP module (vAMM plus hedge). The flat `lp*` `PerpMarketAccount` fields are restructured into `hedgeConfig`, and `PerpMarket::SIZE` becomes 1224 |
| #67       | Remove the legacy referrer-reward fee path: the `UserStats` epoch fields, `FeeStructure.referrer_reward_epoch_upper_bound`, `FeatureBitFlags::BuilderReferral`, four `RevenueShareEscrowAccount` fields, and the SDK `referrerInfo?` params |
| #68       | Builder codes on non-swift orders, plus fill-time enforcement of builder and referral revenue share. The escrow is required when the taker has a builder order or a referred escrow |
| #70       | Rebrand the program crate from drift to velocity |
| #71       | This migration guide |
| #73       | Enforce referral revenue share at fill time. `fill_perp_order` rejects with `UnableToLoadRevenueShareAccount` when the taker has `BuilderReferral` but no escrow is supplied. The SDK `placeAndTakePerpOrder` param `revenueShareEscrowMap` becomes `takerEscrow` |
| #74, #78  | Enable TypeScript `strict` mode in the SDK. There is no runtime behavior change. A few public accessor signatures widened to expose an already-possible `undefined` (`DLOBNode.getPrice`, `BlockhashSubscriber.getLatestBlockHeight`, the basic and polling user and user-stats subscribers' `getUserAccountAndSlot()` and `getUserStatsAccountAndSlot()`), and `nextRevenuePoolSettleApr`'s `amount` became required. The user and user-stats subscribers' stored `{ data, slot }` pair is now atomic (§4.4) |
| #75       | Fee redesign (explicit per-fill carveouts, withdrawable protocol fees via `protocolFeeRecipientPerp` and `protocolFeeRecipientSpot`, a 100% staker-owned IF) plus AMM isolation. `PerpMarket::SIZE` 1224 to 1304. New errors 6353 and 6354 |
| #76       | Withdraw guard threshold notional cap. `update_withdraw_guard_threshold` requires an `oracle` account and rejects anything over $10k notional. New error `WithdrawGuardThresholdNotionalTooLarge` (6352). `update_spot_market_oracle` and `_expiry` promoted to cold admin. SDK `updateWithdrawGuardThreshold` gains an optional `oracle?` |
| #77       | Funding bias spread widening: `AMM.funding_bias_sensitivity` plus the `update_perp_market_funding_bias_sensitivity` admin ix. `last_funding_oracle_twap` moved from `PerpMarket` to `MarketStats`, preserving offsets |
| #80       | Remove the gov-token (DRIFT) stake fee discount: the gov stake-sync instructions, `UserStats.if_staked_gov_token_amount` (now padding), the gov IF revenue-settle APR cap, and `GOV_SPOT_MARKET_INDEX` |
| #82       | Remove 27 unused SDK exports (§4.3 dead-export cleanup). Rename the misspelled `PTYH_LAZER_PROGRAM_ID` to `PYTH_LAZER_PROGRAM_ID` |
| #83, #85  | Vendor drift-vaults into the monorepo as the `vaults` program plus `@velocity-exchange/vaults-sdk`, renaming `drift_vaults` to `vaults` and `DriftVaults` to `Vaults`, with the CPI dep resolving as `velocity` |
| #89       | Restore `ForwardOnlyTxSender` (`tx/forwardOnlyTxSender`) and `calculateMaxRemainingDeposit` (`math/spotMarket`) to the SDK public API. Both were removed in #82 |
| #94       | Continuous funding dead zone. Per-market `funding_clamp_threshold` and `funding_ramp_slope`, recycling `_padding_funding_twap`, replace #12's global hard cutoff. New `update_perp_market_funding_dead_zone` ix, `AdminClient.updatePerpMarketFundingDeadZone`, and `PerpMarketAccount.fundingClampThreshold` / `fundingRampSlope` in place of `paddingFundingTwap` |
| #97       | Re-export `PriceUpdateAccount` from the `@velocity-exchange/sdk` package root. Migrate dlob-server and keeper-bots-v2 to the workspace SDK |
| #127      | Reconcile the hand-written `sdk/src/types.ts` mirrors with the generated IDL: add previously-missing account and event fields, correct `BN` and `number` field types, drop phantom never-on-chain `*Mint` record fields, and export new param and record types (§4.7). No on-chain layout change |
| #134      | Fix `liquidate_spot_with_swap_begin` and `_end`. A stale fixed account-index and count guard (13 against the actual 11) made every real call fail with `InvalidLiquidateSpotWithSwap`. The instruction was non-functional before this fix and is now operational, so keepers that shelved it should re-verify their integration. The SDK builder (`velocityClient.ts`) was already correct, so there is no SDK change |
| #135      | Bulk `place_orders` and `place_scale_orders` enforce the initial-margin check once per risk scope touched by the batch, meaning cross-margin plus each isolated market with a risk-increasing order, rather than once after the last order. That closes a gap where an early risk-increasing order's exposure was not accumulated into the check, and where the check could be skipped entirely if the final order in the batch was a no-op. Batches that previously succeeded may now be rejected with `InsufficientCollateral` (§3) |
| #137      | Direct `deposit()` respects the per-market `SpotOperation::Deposit` pause bit (`MarketActionPaused`), independent of the pre-existing global deposit-pause and aggregate `max_token_deposits` cap checks. Deposits into a market with only the per-market deposit bit paused now revert |
| #139      | `transfer_deposit` and `transfer_deposit_by_delegate` enforce the same admission checks as direct deposit and withdraw. The recipient credit requires active spot-market status for a positive deposit balance (`MarketActionPaused`) and respects `max_token_deposits` (`MaxDeposit`), and the source debit honors direct-withdraw's reduce-only cap (`ReduceOnlyWithdrawIncreasedRisk`). Transfers that previously succeeded into a capped, non-active or reduce-only market now revert. No ABI or layout change |
| #141 native-path | Harden the native fast-path admin handlers (`update_mm_oracle_native`, `update_amm_spread_adjustment_native`). They authenticate against the program-owned `State` account loaded via `AccountLoader`, checking owner and discriminator, require the market slot to hold a program-owned `PerpMarket`, replacing an unchecked `bytemuck` cast, and read the slot from the `Clock` sysvar instead of a caller-supplied account. New errors `InvalidNativeStateAccount` (6355) and `InvalidNativePerpMarketAccount` (6356). `update_amm_spread_adjustment_native` requires the `State` account at index 2, and SDK `getUpdateAmmSpreadAdjustmentNativeIx` is now async and adds it |
| #149      | Remove the dead `migrate_referrer` program instruction, handler and accounts struct. The entrypoint was already removed with the legacy referral model, so there is no IDL or ABI change. Its non-functional SDK wrappers `VelocityClient.migrateReferrer` and `getMigrateReferrerIx` are removed too (§4.3) |
| #155      | Uniform `UserAccountSubscriber` "not subscribed" contract. The gRPC-multi and WebSocket-program subscribers' `getUserAccountAndSlot()` throw `NotSubscribedError` before `subscribe()`, matching WebSocket and polling, so `User.getUserAccount()` throws when not subscribed and returns `undefined` only when not found. The `getUserAccount(AndSlot)OrThrow` message changed from `User account not loaded` to `User account not found` (§4.4) |
| #158      | Mainnet `initialize`, the one-time global `State` creation, requires a fixed admin signer (`state_init_authority` = `prpHJmuXnqdaz92tBVdwsqmqyhqPLuq5Km35a5QWco3`) to prevent front-running of genesis. Devnet, localnet and the integration-test build are unaffected (§5) |
| #172      | Decouple solvency-repair from withdrawals. New `State.solvency_status` (1 byte carved from padding, size unchanged) plus a `SolvencyStatus` bitflag. `resolve_perp_pnl_deficit`, `resolve_perp_bankruptcy` and `resolve_spot_bankruptcy` are gated by `solvency_repair_not_paused` instead of `WithdrawPaused`. New cold-admin-only `update_solvency_status` instruction. SDK `SolvencyStatus` enum, `StateAccount.solvencyStatus`, `solvencyRepairPaused()` helper, `AdminClient.updateSolvencyStatus` |
| #174      | `LiquidationRecord.bankrupt`, the top-level flag rather than a field of the nested `perpBankruptcy` or `spotBankruptcy` sub-records, reflects whether the user still holds a bankrupting liability after the resolve call, instead of always being `true`. The wire type is unchanged, so consumers assuming `bankrupt == true` must update (§5) |
| #176 jit-proxy | Vendor the jit-proxy client into the monorepo as `@velocity-exchange/jit-proxy`, replacing `@drift-labs/jit-proxy`, ported to Anchor 1.0 and built against `@velocity-exchange/sdk`. Fixes a JIT-maker crash where the jitter read `perpMarketAccount.amm.minOrderSize`, removed on Velocity perps. The perp dust guard and the synthetic `Order`, which has no `quoteAssetAmount`, now match Velocity's layout. The `JitProxyClient` / `JitterSniper` / `JitterShotgun` API is unchanged, and the constructor `driftClient` fields take a `VelocityClient` (§4.1) |
| #182      | AMM JIT no longer participates in a DLOB match fill while a hard AMM-fill gate is active, meaning pause, drawdown, MM-oracle volatility or oracle invalidity. Match fills can therefore be smaller, or route entirely to the resting DLOB maker, under those conditions |
| #185 deposit-caps | Per-market configurable withdraw circuit breaker plus a daily deposit rate cap. The hardcoded 25% daily withdraw breaker becomes `SpotMarket.withdraw_circuit_breaker_bps` in basis points, where `0` means the default 2500 bps, so pre-existing markets keep prior behavior. A new daily deposit cap mirrors the withdraw side. `deposit_guard_threshold` (u64 token amount, no cap below it) and `max_deposit_bps_per_day` (basis points, `0` disables) bound how far resulting deposits may exceed the 24h deposit TWAP. It is enforced on the direct `deposit` and on the shared spot-credit path (`transfer_pools`, `end_swap` credits), reverting with the new `DailyDepositLimit` (6364). New warm and cold admin ixs `update_spot_market_withdraw_circuit_breaker`, where warm may only tighten toward the 25% default and loosening past it needs cold admin, and `update_spot_market_deposit_cap`. All three fields are carved from the existing 13-byte alignment gap before `protocol_fee_pool`, so every other offset is unchanged and no migration is needed, since existing accounts read 0. SDK `SpotMarketAccount.withdrawCircuitBreakerBps` / `maxDepositBpsPerDay` (basis points) / `depositGuardThreshold`, `AdminClient.updateSpotMarketWithdrawCircuitBreaker` / `updateSpotMarketDepositCap` plus ix builders, math `calculateMaxDepositTokenAmount` / `checkDepositLimits` and a configurable breaker in `calculateWithdrawLimit`, admin CLI `spot-market set-withdraw-breaker` / `set-deposit-cap` (§5) |
| #201      | Compile-time gate isolated positions (`isolated-position` feature) and the VLP hedge component (`vlp-hedge` feature) out of mainnet builds pending audit. Devnet and test builds keep both, since `anchor-test` implies them. New error `IsolatedPositionDisabled` (6357) rejects signed-msg orders carrying `isolated_position_deposit` on gated builds. Five lp-pool admin config ixs that share accounts structs with ungated admin ixs stay in all builds as inert config writes (§3). Layouts and the IDL are unchanged. The `update_initial_amm_cache_info` and `override_amm_cache_info` handlers moved from `vlp/hedge/admin.rs` to `vlp/amm/admin.rs` as shared amm-cache maintenance and stay on mainnet, and the dead `ResetAmmCache` accounts struct was removed |
| #213 bid-ask-twap-hardening | Harden `update_perp_bid_ask_twap`. (1) `update_funding_rate` is no longer called from the crank, because refreshing the caller-curated DLOB mark TWAP and applying funding in one instruction let the just-written TWAP feed funding at zero elapsed time. Funding now runs only via its own `update_funding_rate` crank and on fills, so integrators and keepers relying on the funding side-effect must call `update_funding_rate` separately. The keeper-bots `fundingRateUpdater` already does. (2) The oracle-divergence filter is symmetric, keeping DLOB levels only within `oracle ± BID_ASK_TWAP_MAX_ORACLE_DIVERGENCE_PERCENT` (15%) on both sides, so caller-supplied depth can no longer push the mark TWAP past the band via high bids or low asks. (3) `keeper_stats` is bound to the signer (`has_one = authority`), so a caller can no longer point at a third party's staked `UserStats` to pass the IF-stake gate. No layout or IDL change |
| #216 jit-proxy-id | Deploy the vendored jit-proxy program under Velocity's own id `J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ` on devnet and mainnet, a create-with-seed vanity address (see `deploy-scripts/deploy-jit-proxy.sh`), replacing Drift's `J1TnP8zvVxbtF5KFp5xRmWuvG9McnhzmBd9XGfCyuxFP` in `declare_id!`, the generated IDL, and the SDK config presets' `JIT_PROXY_PROGRAM_ID` (§4.1). Integrators must point jitters and JIT makers at the new program id |
| #220 equity-floor | The per-user equity floor and the authority-wide breaker. New warm-admin `update_user_equity_floor`, new `User.equity_floor: u64`, new `UserStats.equity_breaker_tripped`, new errors `EquityBelowFloor` (6358) and `InvalidEquityFloorTransfer` (6359), a new `equity_floor_delta` arg on `transfer_deposit_by_delegate` (signature change), the permissionless `trip_equity_floor_breaker`, and the warm-admin `reset_equity_floor_breaker`. See [§3.1](#31-per-user-equity-floor) for the current behavior, which later PRs changed substantially |
| #238 spot-bankruptcy-revenue-pool | `resolve_spot_bankruptcy` consumes the spot market's `revenue_pool` as a first-loss tranche before the staker-owned IF vault, and before socializing any remainder to depositors. This replaces upstream's unimplemented `todo` and mirrors the perp-bankruptcy tranche order, where in-transit IF revenue pays before the vault. The draw is counter-only with no token movement, bypasses the periodic revenue-settle timer and the staker APR cap, and shrinks `revenue_pool.scaled_balance` and `deposit_balance` in place. `SpotBankruptcyRecord` is unchanged, since `if_payment` still records only the IF-vault draw. The revenue-pool tranche appears in program logs only, and `cumulative_deposit_interest_delta` and `total_social_loss` now reflect the smaller post-tranche socialized loss. No layout or IDL change |
| #243      | Spot liquidations price both legs of the transfer exchange rate protectively when the corresponding oracle is margin-invalid, meaning `StaleForMargin` or `TooUncertain` but still `Liquidate`-acceptable. The seized collateral (deposit) leg is priced at `max(oracle, 5min twap, oracle + confidence)` and the repaid borrow (liability) leg at `min(oracle, 5min twap, oracle - confidence)`, instead of the raw oracle price. This applies in `liquidate_spot`, `liquidate_spot_with_swap_begin` and `_end` including the swap worst-case price, and the spot leg of `liquidate_perp_pnl_for_deposit` and `liquidate_borrow_for_perp_pnl`. Liquidatability and the shortage-to-token valuation are unchanged, so a stale price can still flag the account, but a depressed collateral price or an inflated debt price can no longer cheapen what the liquidator receives per unit repaid. SDK adds `calculateUserProtectiveAssetPrice` and `calculateUserProtectiveLiabilityPrice`, which you pass to `calculateAssetTransferForLiabilityTransfer` when the respective oracle is margin-invalid. No layout or IDL change |
| #245 bankruptcy-if-floor | Fix a High audit finding. The permissionless fee sweep, meaning `sweep_perp_market_fees` and the inline sweep on every `settle_pnl`, could drain `fee_ledger.pending_if_fee`, which is `resolve_perp_bankruptcy`'s first-loss tranche, ahead of a bankruptcy resolution. That converted a tranche-covered loss into a shared-IF draw or a socialized funding loss. New `PerpMarket.bankruptcy_if_floor_pct: u32`, repurposing the trailing padding before `market_stats` with sizes and offsets unchanged, makes the sweep's IF drain leave that fraction of open-interest notional, at the oracle TWAP, behind as a standing tranche. New markets default to 10 bps, and existing markets read 0 (disabled) until set via the new warm-admin `update_perp_market_bankruptcy_if_floor_pct`. SDK `AdminClient.updatePerpMarketBankruptcyIfFloorPct`, CLI `perp-market set-bankruptcy-if-floor`, and `PerpMarketAccount.bankruptcyIfFloorPct` added (§5). Later completed by `bankruptcy-claim-freeze` |
| #252 if-request-remove-settle | Fix a High audit finding. `request_remove_insurance_fund_stake` settles already-due protocol revenue into the IF vault before freezing the staker's `last_withdraw_request_value`, mirroring `add_insurance_fund_stake`. Previously the exit value was frozen against the pre-settle vault, so a public revenue settle between request and remove shifted the exiting staker's rightful share of that already-due revenue to the remaining stakers. Instruction accounts changed (ABI). `request_remove_insurance_fund_stake` gains `state`, `spot_market_vault`, `velocity_signer` and `token_program`, and accepts the transfer-hook `remaining_accounts` and token-mint like the add path. `cancel_request_remove_insurance_fund_stake` moved to its own unchanged accounts struct (`CancelRequestRemoveInsuranceFundStake`, the same 5 accounts as before). Integrators constructing the request-remove ix manually must pass the new accounts, and SDK `VelocityClient.requestRemoveInsuranceFundStake` handles them automatically. Because the pre-freeze settle goes through `attempt_settle_revenue_to_insurance_fund`, which skips while withdraws are paused, the request itself is now rejected during a withdraw pause, with `ExchangePaused` exchange-wide or `MarketWithdrawPaused` for the market's `SpotOperation::Withdraw` bit. An accepted request therefore always freezes a post-settle exit value, and cancel stays allowed. No on-chain account layout change (§5) |
| #253 if-add-zero-shares | Fix a High audit finding. `add_insurance_fund_stake` computed shares off the pre-transfer IF vault balance, which a donation can inflate, and never required the minted share count to be nonzero. An attacker could donate into the vault before a victim's add to force `floor(amount * total_shares / vault) == 0` and capture the victim's whole deposit as price appreciation on the attacker's shares. The add path now rejects a deposit that would mint zero shares with the new `IFDepositMintsZeroShares` error (6360 / `0x18D8`), mirroring the `n_shares > 0` guard the request-remove path already had. No account layout change, and the new error variant is appended at the enum tail, so the IDL `errors` list gains one entry |
| #254 if-revenue-settle-cap | Fix two High audit findings on donation-inflatable IF vault pricing. The revenue-settle APR cap is sized off an accounted snapshot rather than the live vault, and the unstake-cancel forfeiture is re-framed as withdraw-and-restake. New `SpotMarket.if_last_settle_vault_amount: u64`. [Details](#if-revenue-settle-cap) |
| #255 revshare-reserve-net-user-pnl | Fix three High audit findings on the permissionless pnl-pool fee sweeps. Every drain now reserves user PnL, the floored IF bankruptcy tranche, and accrued revenue share. New `PerpMarket.pending_revenue_share: u64`. [Details](#revshare-reserve-net-user-pnl) |
| #256 builder-stale-order-id | Fix a High audit finding. `add_builder_order` writes a `RevenueShareOrder` keyed to `user.next_order_id` before `place_perp_order` runs. If placement soft-skips on an expired `max_ts` it returns before `next_order_id` is consumed or `HasBuilder` is set, so the row persists for an order id the next placement reuses. Fill-time builder lookup (`get_builder_escrow_info`) matched only `(sub_account_id, order_id)` with no live `HasBuilder` check, so a filler supplying the escrow could charge the stale builder fee on a later non-builder order. Fixed two ways. The fill-time builder-row lookup is gated on the taker order's live `is_has_builder()` flag, with the referral lookup unchanged, and the builder-order row is cleared when `place_perp_order` bails before committing, covering an expired `max_ts` and the `TryPostOnly` validate skip. No account-layout, IDL or SDK-API change, since this is internal controller logic and the SDK computes builder fees from explicit order params rather than an order-id escrow lookup |
| #257 lazer-max-staleness | Fix a High audit finding. `post_pyth_lazer_oracle_update` validated a signed Lazer message only for signer trust and a monotonic, non-decreasing feed timestamp against the cached account, never against `Clock::unix_timestamp`, yet always stamped `posted_slot` to the current slot, and downstream oracle staleness is derived solely from that slot. An authentic but stale or replayed message was therefore treated as slot-fresh for AMM, margin, liquidation and settlement purposes. Because the monotonic check is a strict `<`, the same message could be re-posted each slot to hold a stale price at slot-fresh indefinitely. The handler now skips any feed whose message timestamp lags `Clock::unix_timestamp` by more than the new `PYTH_LAZER_MAX_STALENESS_SECONDS` (15s). Keepers and integrators posting Lazer updates must post promptly, and legitimate updates are sub-second so they are unaffected. A message older than 15s no longer updates the cache. No account-layout or IDL change, since the constant is not IDL-exposed |
| #258 trigger-price-staleness | The trigger-price last-fill leg gains a staleness guard. `PerpMarket::get_trigger_price` (the median trigger price, `MedianTriggerPrice` feature) substitutes the oracle price for `last_fill_price` when the market's last fill (`market_stats.last_trade_ts`) is older than `TRIGGER_PRICE_LAST_FILL_MAX_AGE` (5 min). Previously a fill from hours ago voted in the median indefinitely on quiet markets. Zero-fill fulfillment steps no longer stamp `last_trade_ts` or update the volume rolling sums, so only real fills do. SDK `getTriggerPrice` mirrors the guard, and `TRIGGER_PRICE_LAST_FILL_MAX_AGE` is a new export. No layout or IDL change |
| #266 if-cancel-rebase-floor | Fix an audit finding (#34). A market-level IF rebase divides a staker's `last_withdraw_request_shares` by the rebase divisor, flooring a small pending unstake request to zero. The cancel path re-checked `last_withdraw_request_shares != 0` after applying that rebase and reverted with `InvalidIFUnstakeCancel`, stranding the stake, because `remove_insurance_fund_stake` also rejects a zero request and `add` and re-`request` are blocked by the still-in-progress request. `cancel_request_remove_insurance_fund_stake` no longer re-checks the post-rebase share count, so a zeroed request cancels successfully. It clears the request, returns the intact rebased stake to active, and abandons only the dust `last_withdraw_request_value`. The genuine "no request in progress" case is still rejected by the pre-rebase guard in the instruction handler. No ABI, layout or SDK change. `InvalidIFUnstakeCancel` (6187) is retained but no longer emitted by this path |
| #266 revenue-pool-deposit-cdi | Fix a Medium audit finding. `deposit_into_spot_market_revenue_pool` refreshes `cumulative_deposit_interest` via `update_spot_market_cumulative_interest` before crediting the revenue pool, matching the normal `deposit` path. Previously a donation into a stale market minted the scaled revenue-pool balance at the lower stored interest and revalued it upward at the next refresh, letting the revenue pool claim interest that accrued before the deposit existed. No signature, layout or IDL change |
| #267 liquidation-fee-basis | Two liquidation throttle and fee fixes (OtterSec audit). (1) `liquidate_perp_with_fill` sizes the IF and protocol fee budget and the margin-shortage base amount off the time-adjusted, grace-period-ramped liquidator fee, which is the same fee the forced order is priced with, rather than the un-aged `market.liquidator_fee`. Post-grace-period liquidations therefore no longer under-budget the insurance and protocol fees relative to the execution discount, and direct `liquidate_perp` already did this. (2) `liquidate_spot_with_swap_begin` derives its `max_asset_transfer` swap bound from the time-ramped max-pct-to-liquidate throttle (`max_liability_allowed_to_be_transferred`) rather than the uncapped `liability_transfer_to_cover_margin_shortage`, so the swap lane can no longer seize more collateral in one swap than the throttle permits, matching direct `liquidate_spot`. No layout, IDL or ABI change |
| #267 liquidation-math | Three liquidation-safety fixes (OtterSec audit). (1) `liquidate_perp_pnl_for_deposit` adds a postcondition. If seizing the deposit to cancel negative pnl would grow the account's buffered margin shortage, which happens once a market's liquidator fee exceeds the liquidation buffer and the asset premium outweighs the pnl relief, it reverts with the new error `LiquidationWorsensAccountHealth` (6361) rather than degrading the account without reporting an error. The tolerance is $1, absorbing deposit dust-rounding. The guard is skipped for markets in `MarketStatus::Settlement`, since delisting winds expired positions down at the expiry price and legitimately drives the account to bankruptcy. (2) `liquidate_spot_with_swap_end` caps its insurance-side (IF plus protocol) fee by the margin shortage via `calculate_spot_if_fee` on the swap-realized `liability_transfer`, matching the direct `liquidate_spot` path, so it no longer charges the raw rates and delivers equivalent borrow relief. `LiquidateSpotRecord.if_fee` and `protocol_fee` shrink accordingly, with the wire type unchanged. (3) `resolve_spot_bankruptcy` enforces a deterministic perp-before-spot precedence, reverting with the new error `PerpBankruptcyMustPrecedeSpot` (6362) while the user has any pending cross-margin perp bankruptcy, so a public caller can no longer pick which resolver drains the shared quote insurance fund first and shift socialized loss. This matches the keeper bots, which already resolve all perp bankruptcies before spot. No layout or IDL change (§5) |
| #268 prelaunch-oracle-delay | Fix a Medium audit finding. `get_prelaunch_price` (`OracleSource::Prelaunch`) computed its staleness `delay` with the subtraction reversed, as `amm_last_update_slot - slot`, so once the clock advanced past the last AMM update the delay saturated to `0` and a stale prelaunch price always passed every freshness gate. It is now `slot - amm_last_update_slot`, matching upstream and the SDK, whose `slot.sub(oraclePriceData.slot)` in `math/oracles.ts` was already correct, so there is no SDK change. No layout or IDL change |
| #268 oracle-F4-70 | The `settle_pnl` third-party guard now extends to negative PnL. A settler who is not the user's authority or delegate can no longer push another user's negative pnl, a loss debited from that user's collateral, through a `StaleForMargin` or `InsufficientDataPoints` oracle. `SettlePnl` still admits those validities for a user settling their own pnl, and the policy is otherwise unchanged. Keepers settling other users' losses must supply a fresh oracle. No layout or IDL change |
| #268 oracle-F4-72 | The Pyth Lazer oracle handler reads the signed `Confidence` payload property, which it previously ignored, and persists `conf = max(signed_confidence, bid/ask spread, 20bps floor)`, never smaller than the prior fabricated value. No layout or IDL change |
| #268 oracle-F4-69 | `OracleMap`'s validity cache is keyed by `(oracle, market_type, market_index, risk-EMA twap, confidence multiplier, staleness overrides)` rather than the oracle pubkey alone, so two markets sharing one oracle can no longer reuse each other's validity verdict. This is an internal correctness fix with a bounded extra CU cost. No layout or IDL change |
| #268 oracle-F4-81 | `handle_withdraw` evaluates its liability-oracle margin check against a snapshot of the withdrawn market's risk-EMA TWAP taken before the in-instruction cumulative-interest refresh, so a same-instruction TWAP refresh can no longer normalize a `TooVolatile` oracle before vault tokens are released. The refreshed TWAP is still persisted. No layout or IDL change |
| #269      | AMM-quoter audit fixes, behavioral, with no layout, IDL or SDK-surface change. (1) The vAMM limit-fill cap (`calculate_base_asset_amount_to_trade_to_price`) always sizes off the spread-adjusted ask and bid reserves, dropping the `base_spread > 0` gate that fell back to the raw `base_asset_reserve`, so a take no longer executes past the taker's limit when the vol and inventory spread is non-zero while `base_spread == 0`. This matches the SDK's `calculateMaxBaseAssetAmountToTrade`, already sized off `calculateSpreadReserves` unconditionally, so no SDK change was required. (2) The fill-triggered `update_funding_rate` recomputes the mark and oracle divergence gate, and the oracle-TWAP sanitization that shares the value, against the post-fill reserve instead of the pre-fill `reserve_price_before`. (3) A zero-fill quote step no longer stamps the mark-TWAP within a `now` timestamp, so it cannot pre-empt a real same-timestamp maker trade, since mark-TWAP is first-write-wins per timestamp. (4) AMM JIT participation in a permissionless DLOB match is clamped by `calculate_amm_available_liquidity` (`max_fill_reserve_fraction`), so one match cannot move reserves past the per-fill fraction. That is a deliberate divergence from upstream Drift JIT sizing. Integrators reproducing Drift's AMM fill, funding or JIT math off-chain must adopt these. The Velocity SDK and Rust SDK already route through the corrected primitives |
| #270 ottersec-M12 | Audit fix (OtterSec Medium #12). `trigger_order` is gated by `fill_not_paused` (exchange-wide `FillPaused`) instead of `exchange_not_paused`, which only tripped when every pause bit was set, so trigger orders can no longer start their auction during a fill-pause window. No layout or IDL change |
| #270 ottersec-M27 | Audit fix (OtterSec Medium #27). `liquidate_perp_with_fill` also requires `fill_not_paused`, in addition to `liq_not_paused`. It routes a real order through `fill_perp_order`, so during the exchange-wide `FillPaused` breaker a public liquidator can no longer consume maker or AMM liquidity and advance fills. Plain `liquidate_perp`, which does no fill routing, is unaffected. No layout or IDL change |
| #270 ottersec-M40 | Audit fix (OtterSec Medium #40). `PerpMarket::can_skip_auction_duration` consults the market-scoped `PerpOperation::AmmImmediateFill` pause bit in addition to the exchange-wide `amm_immediate_fill_paused`. Setting the per-market bit now disables immediate-AMM, auction-skipping fills for that market. No layout or IDL change |
| #270 ottersec-M42 | Audit fix (OtterSec Medium #42). `settle_expired_position`, the Settlement-market path of `settle_pnl` and `settle_multiple_pnls`, honors the market's `PerpOperation::SettlePnl` bit, and `SettlePnlWithPosition` when the position still has base, mirroring `settle_pnl`. A market with settlement paused can no longer have its expired positions closed permissionlessly, and reverts with `InvalidMarketStatusToSettlePnl`. No layout or IDL change |
| #270 ottersec-M79 | Audit fix (OtterSec Medium #79). `trigger_order` rejects with `MarketFillOrderPaused` when the target perp market has `PerpOperation::Fill` paused, matching `fill_perp_order`. A caller can no longer activate or age a trigger order, and collect the keeper reward, while that market's fills are paused. No layout or IDL change |
| #270 ottersec-M43 | Audit fix (OtterSec Medium #43). `settle_multiple_pnls(..., TrySettle)` no longer sweeps a market's pnl pool when settlement was soft-skipped. `settle_pnl` returns `bool`, where `true` means truly settled and `false` means soft-skipped under `TrySettle`, covering a paused `SettlePnl` or `SettlePnlWithPosition` op, a degraded oracle, no unsettled pnl, an empty pnl pool and similar cases. `handle_settle_pnl` and `handle_settle_multiple_pnls` gate `sweep_completed_revenue_share_for_market` on that signal, so builder and referrer fees are no longer moved out of the paused or otherwise skipped market's pnl pool. `revoke_completed_orders` is unchanged. No layout or IDL change, since `settle_pnl` is an internal controller fn rather than an instruction |
| #271 signed-msg-hardening | Two OtterSec-Medium fixes to the signed-message ("swift") taker path. (1) `place_signed_msg_taker_order` is gated by `exchange_not_paused` and rejects with `ExchangePaused` when the exchange is fully paused, matching normal `place_perp_order`, so signed-msg orders no longer bypass the global pause. (2) `resize_signed_msg_user_orders`. The `SignedMsgUserOrders` account is authority-scoped and shared across all of an authority's sub-accounts, so a per-sub-account delegate may no longer shrink it, because a shrink evicts other sub-accounts' active replay-protection UUIDs and re-enables replay. Only the `authority` itself may shrink, and anyone may still grow. The now-redundant `user` account was removed from the `ResizeSignedMsgUserOrders` accounts struct, which is an IDL and ABI change, and SDK `resizeSignedMsgUserOrders` / `getResizeSignedMsgUserOrdersInstruction` drop the trailing `userSubaccountId` parameter accordingly. No account-layout change. OtterSec #78, on signed-message deployment and domain separation, is tracked separately as a signed-digest format change (§5) |
| #272 deposit-revenue-pause-enforcement | Close pause and status-gating bypasses on the deposit, withdraw and revenue-settle paths (OtterSec Mediums). (1) `transfer_pools` applies direct-`deposit()` admission on each deposit-side credit, covering the market-scoped `SpotOperation::Deposit` pause, active status for a positive deposit balance, and the `max_token_deposits` cap. (2) `liquidate_spot_with_swap_begin` rejects when the global Deposit or Withdraw status is paused, when the asset market's `SpotOperation::Withdraw` is paused, or when the liability market's `SpotOperation::Deposit` is paused, where before it checked only `liq_not_paused`. (3) `deposit_into_spot_market_revenue_pool` honors the market's `SpotOperation::Deposit` bit. (4) The direct `settle_revenue_to_insurance_fund` honors the market's `SpotOperation::Withdraw` bit in addition to the global withdraw pause. (5) The opportunistic revenue-to-IF settle folded into IF-add, liquidations and pnl-deficit resolution skips rather than errors when the global `WithdrawPaused` status or the market's `SpotOperation::Withdraw` bit is set. (6) The builder and referrer revenue-share sweep respects the perp market's `PerpOperation::SettleRevPool` pause like the direct fee sweep. It reuses the existing error codes `MarketActionPaused`, `MarketWithdrawPaused` and `ExchangePaused`. Calls that previously succeeded while the relevant pause was set now revert. No layout or IDL change |
| #273 revenue-bankruptcy | Three OtterSec audit fixes with no layout change and one new error. (a) `resolve_spot_bankruptcy` accrues the borrow market's cumulative interest to `now` before reading the borrow, so the debt is cleared at its current value rather than a stale-low one. Previously that under-drew the revenue-pool and IF tranches and under-socialized the remainder. (b) `resolve_perp_pnl_deficit` refreshes the quote market's cumulative interest before valuing the PnL pool, so the "pool already covers user PnL" gate cannot be satisfied by a stale-low pool and draw the insurance fund when a current-interest pool would suffice. (c) Revenue-share settlement (`load_revenue_share_map`) requires each builder or referrer recipient `User` to have `sub_account_id == 0`, the canonical recipient of the stored authority, rejecting with the new `InvalidRevenueShareRecipient` (6363). Previously a permissionless settlement caller could redirect accrued rewards to any sibling subaccount of that authority (§5) |
| #273 revenue-bankruptcy-settlement | Two OtterSec audit fixes to the perp fee sweep and expiry settlement, with no layout, IDL or error change. (a) `sweep_perp_market_fees` values its user-claim reserve (`net_user_pnl`) at the market's fixed `expiry_price` while the market is in `Settlement` status. Expired positions settle at `expiry_price` rather than the live oracle, so a live oracle below `expiry_price` for a net-long expired market under-reserved and made later expiry settlements revert with `InsufficientPerpPnlPool`. The live-oracle validity gates are skipped in that branch, since `expiry_price` is fixed, mirroring the `Settlement` branch liquidation already uses. Active and ReduceOnly markets are unchanged and still valued at the gated live oracle. (b) The permissionless expired-position closeout (`settle_expired_position`) routes its taker closeout fee through `fee_ledger.accrue_fill_fees` with the standard IF and protocol split, zeroing the AMM provision because there is no AMM counterparty on an expiry closeout so its share folds into the protocol residual. The fee is therefore materialized to the protocol and IF pools at the sweep, instead of being dumped wholesale into the revenue pool and insurance fund at delisting. The SDK `sweepPerpMarketFees` doc was updated to note the `Settlement` reserve valuation |
| #274 order-amm-correctness | Fix three OtterSec audit findings in the perp order and fill path, with no layout, IDL or ABI change. (1) A `ReduceOnly` perp market forces every order it fills to be risk-reducing. `fill_perp_order` (taker) and `get_maker_orders_info` (makers) re-derive the market's reduce-only status at fill time and stamp `order.reduce_only`, so a legacy order placed while the market was `Active` can no longer increase exposure after the market is flipped to `ReduceOnly`. Previously the fill keyed only off the flag stored at placement. (2) The AMM fallback-price premium (`AMM::get_fallback_price`) clamps the seconds-to-expiry operand before multiplying, so an order with an unbounded `max_ts`, for example `i64::MAX`, no longer overflows and aborts every fill routed through it. The divisor is unchanged for all in-range expiries. (3) The perp DLOB matcher builds its `QuoteContext` with the real, safe oracle instead of a zero-price default, so oracle-offset resting makers are requoted at the same price maker discovery froze them at, rather than reverting in `validate_fill_price`. The SDK is unaffected, since the TS DLOB matcher already threads the oracle consistently and the fallback-price math is not mirrored |
| #276 funding-pause-enforcement | Fix two OtterSec Medium findings that let the funding pause be bypassed. (1) Spot interest accrual (`update_spot_market_cumulative_interest`) honors the exchange-wide `FundingPaused` bit on every call path, covering deposit, withdraw and transfer, fills, PnL settlement, liquidation, IF revenue-settle, protocol-fee withdraw, and LP and hedge paths, rather than only the dedicated `update_spot_market_cumulative_interest` crank. Previously the shared helper checked only the market-scoped `SpotOperation::UpdateCumulativeInterest` bit, so interest kept accruing during a global funding pause. TWAP stats still advance and `last_interest_ts` freezes, so on resume the next accrual covers the full elapsed interval, which is the unchanged crank semantics now applied everywhere. (2) `update_perp_bid_ask_twap` no-ops when the market's `PerpOperation::UpdateFunding` bit is paused, mirroring the market-scoped gate the direct `update_funding_rate` path enforces, since the crank already blocked the exchange-wide `FundingPaused` via `funding_not_paused`. A single paused market's mark, bid and ask funding-input TWAP can therefore no longer keep advancing and feed a stale jump into funding on resume. No layout, IDL or SDK-surface change |
| #282 auction-floor-client-spread | The auction-duration floor in order sanitization (`update_perp_auction_params`, the market/oracle and crossing-limit variants) paces the narrower of the requested and sanitized price ranges instead of always the sanitized range. Previously, pulling the start price toward baseline, which happens on tail-tier markets and on any non-signed market order, widened the range and inflated the duration floor. A tier-C signed-msg order asking for a 0.2% spread over 20 slots could be floored to 60 to 100 slots or more by the market's baseline spread. Orders whose auction prices are left untouched, or whose end price is sanitized inward, keep today's durations. Orders whose start is improved toward baseline keep the client-requested duration when within the 10-slot signed-msg grace. This applies to signed-msg and regular orders alike, and fully-derived auctions with no client prices are unaffected. It is a program-only behavior change, since the swift server's `will_sanitize` simulation calls the program function and inherits it. No SDK logic mirror exists, and there is no layout or IDL change |
| #297 spot-market-idl-padding | Fix the IDL's `SpotMarket` layout. `_padding_align_pfp` widened from `[u8; 8]` to `[u8; 13]` so the borsh and IDL packed offset of `protocol_fee_pool` matches the real `#[repr(C)]` struct offset of 752. The on-chain byte layout is unchanged, because the 5 bytes were implicit repr(C) padding, but every borsh decoder using the previous IDL read the three tail fields (`protocol_fee_pool`, `protocol_liquidation_fee`, `protocol_fee_factor`) 5 bytes early and returned garbage or zeros. Integrators must regenerate clients from the new IDL, which `@velocity-exchange/sdk` ships, and re-read any cached values of those fields. Compile-time `offset_of!` guards now pin the tail offsets. New read-only admin CLI command `show fees` prints the full user-facing fee schedule, covering trading tiers, filler reward, fee split, and per-market fee adjustments, liquidation fees and interest carveouts (§5) |
| #305 revshare-liability-accounting | Fix two High audit findings on revenue-share accounting. #88: the fill-time builder-order lookup matched an escrow row on `(sub_account_id, order_id)` only, via `find_order_index`, and never the market. Order ids are per-subaccount and reused across markets, and a builder row can linger past its order, so a stale market-A row could attach to a same-id fill in market B. The market-B taker was charged a builder fee that accrued to, and was later swept from, market A's PnL pool, which is cross-market value movement, and the bite is larger when the beneficiary is a vault PDA. The fill path now uses a new `find_builder_order_index` that also requires the row's `market_index` and `market_type` to equal the fill's, that the row still be `Open` rather than a `Completed` row with a stale id, and that it not be a referral row. #90: `calculate_perp_market_amm_summary_stats`, the balance-sheet recompute an `AmmCrank` commits into `amm.total_fee_minus_distributions`, subtracted `net_user_pnl` and the pending protocol and IF counters but not accrued-but-unswept builder and referrer revenue share, so it counted that pnl-pool liability as retained AMM equity and inflated the funding and curve budget by the owed amount. It now also subtracts `PerpMarket.pending_revenue_share`, matching the reservation the fee sweep already applies, so there is one balance-sheet source of truth. No account-layout, IDL, error or SDK-API change, since this is internal controller and math logic and the SDK does not reimplement the summary recompute or the escrow-row fee lookup |
| #306 bankruptcy-amm-funding-resync | Fix a High audit finding (#89). `resolve_perp_bankruptcy` socializes residual bad debt by bumping `cumulative_funding_rate_long` and `_short`, so surviving longs and shorts both owe funding covering the loss, but it never resynced the AMM's own `amm.last_cumulative_funding_rate_long` and `_short`. On the next `update_funding_rate`, `calculate_amm_funding_payment` derives the AMM payment from `(cumulative_rate − amm.last_cumulative_rate)`, which then carried the bankruptcy bump on both legs. For a balanced book the AMM received phantom revenue into `total_fee_minus_distributions`, roughly `D·G1/G0`, which can exceed the socialized loss `D` as gross open interest grows, payable to survivors or spendable as curve budget. The socialization block now first fully settles the AMM's own funding through the current, pre-socialization cumulative rates against its net position, then resyncs the AMM stamp to the bumped cumulative rates. Only the socialization delta is therefore excluded from the AMM's next funding payment, no genuine funding is dropped, and loss recovery is left to surviving user open interest. No account-layout, IDL, error or SDK-API change, since this is internal controller logic. The SDK predicts user funding from the market cumulative rates, which are unchanged, and does not predict AMM funding across a bankruptcy |
| #309 builder-fee-integrity | Fix two Medium audit findings on builder-code fee integrity. #82: builder fees were droppable without any error. `add_builder_order` returned `Ok(None)`, meaning no `HasBuilder` bit and no fee charged, when the escrow was full, and `revoke_completed_orders` matched an order via the row's stored `user_order_index`, which can go stale and clear a still-open fee-bearing row. The modify path also stripped attribution. Now `add_builder_order` propagates `RevenueShareEscrowOrdersAccountFull`, rejecting the placement rather than downgrading it to no-builder, `revoke_completed_orders` matches by `(sub_account_id, order_id)` across the whole order list, and `modify_order` rejects a builder-coded order with the new error `CannotModifyBuilderOrder` (6366), so attribution cannot be stripped without an error. Cancel and re-place to change a builder order. #83: a self-approved builder could route up to about 65.5% of notional, a `u16::MAX` fee with no global ceiling, as a maintenance-margin-gated position-decreasing fill, moving value the taker could not withdraw under initial margin. The new global cap `MAX_BUILDER_FEE_TENTH_BPS` (1000, meaning 1%, tunable) bounds the fee charged regardless of the builder's own configured max. New error variant only, and no account-layout change |
| #317 stale-curve-fill-routing | Perp fill routing quotes the vAMM off the projected post-refresh curve instead of the stored one. This is behavioral, with no layout, IDL or SDK-surface change. `fulfill_perp_order` previously passed the raw stored `reserve_price_before`, plus the refreshed spread cache, to `determine_perp_fulfillment_methods`, while the curve refresh, a peg snap toward oracle, only ran inside `Quoter::setup`, after a fulfillment method was already selected. On a quiet market whose stored curve had drifted from oracle, a taker priced at or near oracle failed to cross the stale quote, no method was selected, `setup` never ran, and the curve stayed stale, blocking fills until an `update_amms` crank or a taker aggressive enough to cross the stale price. Routing now projects the refresh (`project_post_refresh_scalar` plus `update_amm_quote_state`) onto a scratch AMM copy under the same slot-idempotency gate as `setup`, where `last_update_slot >= slot` skips, so the routing price equals the price the first fill step quotes and the real curve still mutates only in `setup`. This matches what the TS SDK already predicted, since `calculateBidPrice`, `calculateAskPrice` and `calculateBidAskPrice` route through `calculateUpdatedAMM`, so no SDK change was required. Off-chain integrators predicting fills off the raw stored reserves should switch to the projected helpers. Superseded by `amm-refresh-dedup`, which projects the real AMM in routing instead of a scratch copy |
| #318 reserve-usdt-name | The name `USDT` is reserved for the quote spot market at index 0. `initialize_spot_market` and `update_spot_market_name` reject any other market whose name decodes to `USDT` with the new error `ReservedSpotMarketName` (6365 / `0x18DD`), appended at the enum tail. The on-chain check mirrors the SDK's `decodeName`, a UTF-8 decode plus a JS `trim()`, by trimming a superset of the JS trim set (all Unicode whitespace, BOM, NUL) before comparing, so padded variants cannot evade it. Invalid UTF-8 is never reserved. No account-layout or SDK-API change, and the IDL gains the error entry. It exists so off-chain monitoring keyed on the decoded market name can trust the name-to-index binding |
| #326 account-extension | New `extend_account` instruction plus a devnet-only `extend_account_devnet(new_len)`, the migration crank for growing zero-copy accounts after a future struct-extending upgrade (see §3 and [`ACCOUNT-EXTENSION.md`](./ACCOUNT-EXTENSION.md)). Gated on the new `HotRole::AccountExtension`, a variant appended to the enum, with a new `State.hot_account_extension: Pubkey` carved from tail padding and `State` unchanged at 1752. New error `InvalidAccountExtension` (6367 / `0x18DF`). SDK `VelocityClient.extendAccount` / `getExtendAccountIx` plus the devnet variants, `HotRole.AccountExtension`, `StateAccount.hotAccountExtension`, and an admin CLI `extend-account` command with a `--type` batch crank. Companion client hardening landed in #325 dynamic-size-clients, where velocity-rs `deser_zero_copy` and `try_deser_zero_copy` trim to `size_of::<T>()` instead of panicking on extended accounts, and swift's local sim trims `State` bytes before the aligned copy. Integrators must not assume fixed account data lengths |
| #328 equity-floor-net-equity | Fix two audit findings on the equity floor (OtterSec #119, #120). #120: every floor check compared the margin numerator (`total_collateral`), which never subtracts spot borrow value, so a floored account could hollow out its equity via borrow-withdrawals without ever becoming trippable, and which understates healthy accounts through asset weights, strict `min(oracle, TWAP)` pricing and the $100 positive-PnL clamp, so a permissionless keeper could freeze a healthy authority. All gates and the trip proof now measure net equity via `calculate_user_equity`, and `trip_equity_floor_breaker` rejects with `InvalidOracle` when any of the subaccount's oracles is invalid. #119: the breaker check sat at the top of `handle_end_swap`, before the reduce classification, so a tripped account could not swap an existing deposit into repaying an existing borrow and was forced to wait for liquidation. A post-classification gate replaces it, exempting a strictly reducing swap and bounding its execution value against oracle with the new constant `EQUITY_FLOOR_SWAP_MAX_VALUE_LOSS_BPS` (100, meaning 1%), rejecting with `InvalidSwap`. Two follow-ups from the same review: `trigger_order` evaluates the risk-increasing floor, breaker and margin cancel condition before paying the keeper's flat trigger reward, so a canceled trigger pays no reward; and `transfer_equity_floor` carries a proportional share of the debited side's `equity_floor_buffer` along with the floor, rounded up on the debited side, conserving the sums of floors and buffers. SDK breaking changes: `User.isBelowEquityFloor`, `isBelowBufferedEquityFloor`, `getEquityAboveFloor` and `getEquityAboveBufferedFloor` switch to `getNetUsdValue()` and drop their `strict` param; `calculateEquityFloorAutoDelta` and `getEquityFloorLevel` take net equity; `EquityFloorManager`, `transferDepositByDelegate('auto')` and the guard bot size and report against net equity. velocity-rs `math::equity_floor::calculate_equity_floor_auto_delta` and `equity_floor_level` rename their `total_collateral` param to `net_equity`, so feed them the program crate's `calculate_user_equity` rather than a margin calculation's numerator. No account-layout, IDL or error-code change (§3.1) |
| #331 swap-provider-interface | SDK-only. Jupiter and Titan sit behind one `SwapProvider` interface, and a quote carries the route it was quoted for. This removes a client-state cache that could execute a swap against an unrelated cached route. [Details](#swap-provider-interface) |
| #339 sole-owner-cancel-burn | Fix two audit findings where canceling a pending full unstake or withdraw request is broken for a sole owner (OtterSec #108 velocity, #126 vaults). Both stem from the withdraw-and-restake forfeiture model (#30) pricing its restake leg against the pool that remains once the request is removed. When the request covers the entire share supply, that pool has zero shares, so the retained-share calculation returns a proportion of nothing. #108 (`calculate_if_shares_lost`): the sole IF staker's retained shares came back as 0, and `cancel_request_remove_insurance_fund_stake` then subtracted the full request from the stake, from `user_shares` and from `total_shares`, burning the staker's entire position while the vault kept their tokens. #126 (`WithdrawRequest::calculate_shares_lost`): the zero-share conservation guard added for #93 fires on that same structural zero, so a depositor holding 100% of `total_shares` could not cancel at all. That was permanent, since re-requesting and depositing are both blocked while a request is pending, leaving only a `withdraw` at the stale frozen value, which forfeits every subsequent gain to synthesized manager shares. Both paths now short-circuit to zero shares lost when the pending request covers the whole supply, since a sole owner has no remaining stakers to forfeit to. `cancel_withdraw_request`'s `user_owns_entire_vault` check is retained, covering the wider case of owning 100% while requesting only part of it. Program-internal math only, with no account-layout, IDL, error-code or SDK change, since neither SDK mirrors the forfeiture math |
| #340 spot-interest-accrual | Fix two audit findings where a spot market's `last_interest_ts` was left lagging across an interval in which no interest was charged (OtterSec #117 High, #115). `calculate_accumulated_interest` bills the entire `now - last_interest_ts` span at whatever rate prevails when it finally runs, so an un-stamped interval is billed retroactively to whoever holds debt at that later moment. #117: while utilization is zero the accrual returns zero and the old code fell through without stamping, so a zero-borrow epoch stayed on the clock and the first accrual after a borrow appeared billed that whole epoch at the newly non-zero rate. That was farmable by depositing into an idle market, waiting for the first borrower, cranking the permissionless accrual, and withdrawing the retroactive lender interest. #115: the interest-pause branch, for the exchange-wide `funding_paused` or the market's `UpdateCumulativeInterest` op, returned early without stamping, so the first accrual after resume applied the whole paused window to whatever balances existed then. A deposit made just before the unpause earned interest for time it was not deposited, and a borrow opened during the pause was charged for time it did not exist. Both now stamp the clock forward without accruing, via a shared `stamp_interest_ts_without_accrual` that never moves the stamp backwards. A pause therefore means interest does not accrue for that window, rather than accruing and being billed later to a different set of balances. The fix is deliberately narrow, since only the genuinely-nothing-owed cases stamp. When utilization is non-zero but the interval is too short for the split to clear a unit, the clock is left alone, so the accrual is deferred rather than forgiven, because stamping there would let frequent cranking zero out borrowers' interest. Every borrow-creating path already cranks the accrual before touching balances, so the stamp is current at the instant debt appears. The SDK `calculateInterestAccumulated` doc now notes that a projection from `lastInterestTs` no longer spans a paused or zero-borrow window. No account-layout, IDL, error-code or SDK-API change |
| #340 if-carveout-floor | Fix an audit finding (OtterSec #127) where the insurance-fund and protocol carveouts on lending interest could be floored to zero and dropped, plus a related conservation defect found while verifying it. [Details](#if-carveout-floor) |
| #341 deposit-cap-swap-output | Fix a High audit finding where the #185 daily deposit cap acted as a one-sided exit lock rather than a growth throttle (OtterSec #118). `check_deposit_limits` is a market-wide level predicate with no notion of the operation being performed, and it was validated unconditionally in `update_spot_balances_and_cumulative_deposits_with_limits`, the shared path behind `handle_withdraw`, `handle_transfer_deposit` and the swap's in leg. A market whose deposit level sat above its cap for any reason therefore rejected every caller of that path with `DailyDepositLimit`, including withdrawals and borrow repayments, which lower or do not touch the level and are exactly what brings a market back under its cap. Liquidation does not use that path, so users could not exit or repay while staying liquidatable. The swap's out leg supplied the lever, since it credits deposits through the plain `update_spot_balances_and_cumulative_deposits` rather than the `_with_limits` variant, so the cap never applied to it while the in leg was checked, and a swapper could lift the level arbitrarily far above the cap. Both are closed by a new `math::spot_withdraw::validate_deposit_cap_after_increase`, which enforces the cap only when the market's deposit token amount actually grew across the update. It replaces the unconditional check on the shared path and is applied to the swap's out credit, after the revenue-pool fee credit so the whole out-side increase is accounted. A swap whose out leg merely repays an existing borrow no longer grows the level and is never rejected. Integrator-visible: `DailyDepositLimit` can no longer surface on a withdrawal, repayment or borrow-reducing swap, and it can now surface on `end_swap`. SDK `checkDepositLimits` is unchanged as a predicate, but its contract is documented, since a `false` result no longer implies a withdrawal will revert and should only be used to predict deposit-increasing flows. No account-layout, IDL, error-code or SDK-API change |
| #343 referral-escrow-zero-capacity | `initialize_revenue_share_escrow` rejects `num_orders == 0` (OtterSec #114). An escrow with no order slots can hold neither a builder nor a referral row, since `find_or_create_referral_index` and `add_builder_order` both fail to claim one and the sweep loop iterates over nothing, so every fee, discount and reward computation fell back to its no-revenue-share value with no error. `authority` is an `UncheckedAccount` on that instruction and only `payer` signs, so a third party could create any user's escrow PDA at zero capacity and suppress their revenue share until someone called the permissionless resize. `RevenueShareEscrow::validate` already claimed orders "must be between 1 and 128" while only checking the upper bound. The minimum is now enforced in the init handler and that message is corrected. It is not enforced in `validate` itself, which `resize` and `change_approved_builder` also call, because an escrow already at zero capacity on chain must stay able to resize its way out. Integrator-visible: `initializeRevenueShareEscrow(authority, 0)` reverts with `DefaultError` instead of creating an inert escrow, so `numOrders` must be at least 1. No account-layout, IDL, error-code or SDK-API change |
| #386, #387 vamm-maker-rebate | See the `#387 vamm-maker-rebate` row below. An earlier revision of this document carried the same entry twice, once under #386, and #387 is the merged PR |
| #387 vamm-maker-rebate | New feature-flagged option for the vAMM to earn the maker rebate on fills it makes (§3). Adds `FeatureBitFlags::VammMakerRebate` (bit 8) and the admin instruction `update_feature_bit_flags_vamm_maker_rebate`, which is an IDL addition. When the bit is on, `calculate_fee_for_fulfillment_with_amm` carves the maker rebate off the taker-fee remainder, clamped to it, before the protocol, IF and AMM split, and folds it into `amm_fee`, so the rebate is booked into the AMM's fee ledger at fill and tokenized by the existing `sweep_market_fees` provision drain. The taker fee, user-maker rebates and the post-only path, where the AMM pays the rebate out of spread surplus, are unchanged, and leaving the bit off keeps the previous distribution exactly. SDK: `FeatureBitFlags.VAMM_MAKER_REBATE`, `AdminClient.updateFeatureBitFlagsVammMakerRebate`. Admin CLI: `feature-flags vamm-maker-rebate`. No account-layout or error-code change |
| #388 fee-schedule | Rework the perp fee schedule (§3). Tiers cut from 6 to 3 (Regular, VIP 1, VIP 2 at indices 0, 1, 2) with new hardcoded 30d-volume thresholds of $5M and $80M and new defaults of 4, 3 and 2 bps taker with a -0.25 bp rebate via `maker_rebate_denominator` 1e6. `determine_perp_fee_tier` reads the rolling 30d volume through `UserStats::get_total_30d_volume_at(now)`, projecting the lazy leaky-sum decay to the current timestamp, so demotion follows the live trailing window instead of the stale stored sum. The write path is unchanged, and promotion was already instant, since each fill lands in the sum before the next fill's tier read. New per-market `taker_fee_addon_tenth_bps` (u16 in the former `_padding_buffer`, same offsets and size), so `taker fee = (tier fee + add-on) * (1 ± fee_adjustment%)`. It is an absolute surcharge the multiplicative `fee_adjustment` cannot express across tiers, and it is unsigned so the taker fee can never drop below the maker rebate it funds. Promo discounts go through `promo_fee_tier` instead. Maker rebates and the post-only path never see it. New ix `update_perp_market_taker_fee_addon` (warm admin, add-on at most 100 tenth-bps via the new constant `MAX_TAKER_FEE_ADDON_TENTH_BPS`). New `State.promo_fee_tier` (u8 from padding, 0 disables and gives legacy reads), where the effective tier is `max(volume tier, promo tier)`, applied to taker and maker tier selection. New ix `update_promo_fee_tier` (warm admin, validated against the highest populated tier `PERP_FEE_TIER_MAX_INDEX`). Two IDL instruction additions, and no account-size, seed or error-code change. SDK and admin CLI surface per §3 |
| #389 lazer-future-bound | Bound a signed Lazer message's feed timestamp above the wall clock as well as below it. `post_pyth_lazer_oracle_update` skips a feed whose message timestamp leads `Clock::unix_timestamp` by more than the new `PYTH_LAZER_MAX_FUTURE_SECONDS` (60s). The monotonic gate skips every message at or below the stored `publish_time`, so one message stamped ahead of the clock stopped all later messages until real time reached that stamp, and the bound holds that freeze to 60s. The bound is wider than the 15s staleness bound because a future stamp only delays the feed, while `Clock::unix_timestamp` is a stake-weighted median that can lag slot progression by more than 15s and would then make every legitimate message read as future. The monotonic gate also tightens from `<` to `<=`, since an equal timestamp carries the same signed content, so re-posting it added no price information but refreshed `posted_slot` and held the feed at slot-fresh. Keepers must post a message stamped no more than 15s behind the chain clock and no more than 60s ahead of it, and a repeat of the stored timestamp is a no-op. No account-layout or IDL change, since the constant is not IDL-exposed |
| #429 accelerated-referrals | Two referrer reward rates. Standard stays per-fee-tier (`FeeTier.referrer_reward_numerator`, fresh default cut from 15% to 10%), and Accelerated is the fixed `ACCELERATED_REFERRER_REWARD_PERCENT` constant, independent of the tier. The referee discount still comes from the tier. New `UserStats.accelerated_referral_status` (padding carve-out, no layout break) plus the `AcceleratedReferralStatus` flags. Automatic enrollment happens on account init, on perp fills for taker and maker excluding a liquidation's liquidatee, and on completed swaps, gated by the beta-scoped `ACCELERATED_REFERRAL_ENROLLMENT_ENABLED` constant rather than a state field, so ending the beta is a program upgrade. Warm and cold admin `update_user_accelerated_referral_status`, where a revoke blocks reenrollment until a grant clears it. New `AcceleratedReferralStatusChangedRecord` event. The referred taker's fill paths accept an optional readonly referrer `UserStats` after the taker's `RevenueShareEscrow`, and omitting it applies the Standard rate rather than failing the fill |
| #470 swift-slot-gate (plus follow-up) | Off-chain only. Fillers no longer burn their single signed-msg place-and-fill attempt on a future-stamped message slot. `place_signed_msg_taker_order` rejects `order_slot > clock.slot` with `InvalidSignedMsgOrderParam` (6288), and takers stamp the message a few slots ahead as a signing buffer, so the TS filler and keep-rs defer the place-and-fill until the slot arrives instead of attempting on arrival. New SDK exports `signedMsgOrderMaxSlot` and `signedMsgOrderSlotReached` (`math/orders`, §4.6) mirror the program's placement window for any consumer building on `DLOB.insertSignedMsgOrder`, which does no slot gating of its own. No program, layout, IDL or error-code change |
| amm-refresh-dedup | CU optimization of the #317 projected-routing fill path, a behavioral refinement with no layout, IDL or SDK-surface change. #317 projected the post-refresh curve onto a scratch AMM copy for routing and left the real curve to mutate inside `Quoter::setup`, so a fill ran the projection twice, once for routing and once in setup, plus an AMM struct clone and a third `update_amm_quote_state`. The projection and spread math are the heavy part of a fill on SBF, and duplicating them raised median fill CU by about 2.5x. Routing now projects and applies the refresh on the real AMM before selecting a fulfillment method, through a shared `project_and_apply` helper that both routing and `Quoter::setup` call. It is slot-idempotent, since `last_update_slot >= slot` skips, so the projection runs at most once per market per slot. Routing does it, and the first fill step's `setup` then skips it. The net op count returns to the pre-#317 baseline of one projection, one pre-route spread refresh and one per-step spread refresh, with no AMM clone. One behavioral refinement follows. Because routing now mutates the real AMM, a stale-market fill attempt that finds no crossing method still snaps the curve toward oracle before returning zero, where previously the scratch projection was discarded on a no-fill. That is identical to what the permissionless `update_amms` crank already does, being budget-floored, oracle-gated and direction-toward-oracle only, so it grants no new capability and reduces reliance on the staleness crank. Routing decisions and fills are unchanged, and the TS SDK already projects via `calculateUpdatedAMM`, so no SDK change was required |
| bankruptcy-admission-dust | Fix a Medium audit finding (OtterSec #151) where a zero-token deposit residue vetoed cross-margin bankruptcy admission. `math::bankruptcy::is_cross_margin_bankrupt` returned `false` for any deposit row with `scaled_balance > 0`, but a full spot-market socialization floors `cumulative_deposit_interest` at 1 and leaves each wiped depositor's scaled row positive, so the row survives while its token value is zero. For a user with unrelated cross-margin debt, that worthless row blocked both bankruptcy admission and PnL liquidation, stalling the next bad-debt repair indefinitely. The deposit branch now converts the row through `get_token_amount` and only vetoes when it is worth at least one token. The predicate therefore takes `&SpotMarketMap`, threaded through the `LiquidatePerpMode::should_user_enter_bankruptcy` trait method and its two implementors, and the isolated mode ignores it since isolated positions carry their own collateral. The change is deliberately narrow, since a deposit worth at least 1 token still vetoes, so it only ever admits bankruptcy for a row that cannot be realized at all. Mirrored in the SDK by `math::bankruptcy::isUserBankrupt`, the keeper's bankruptcy predicate that the liquidator bot uses to decide when to send a resolver, which applies the same two value-aware vetoes. Its exported signature is unchanged, but it now reads market state, meaning the deposit index and PnL pool, as well as the user account, so every spot and perp market referenced by a nonzero position must be loaded on the client. Keeping that mirror current matters, because the resolvers self-admit and an under-reporting keeper is the only thing standing between a newly admissible account and its repair. No account-layout, IDL, error-code or SDK-API-shape change |
| bankruptcy-claim-freeze | Complete the #245 fix for the permissionless-sweep front-run of a bankruptcy resolution. New `PerpMarket.pending_bankruptcy_claims: u16` books the debt itself, the standing floor is on by default, and delisting waits on the counter. [Details](#bankruptcy-claim-freeze) |
| bankruptcy-estate-setoff | Fix a High audit finding (OtterSec #130) where assets arriving after the bankruptcy latch escaped setoff, so insurance and depositors covered a debt the estate could have paid itself. [Details](#bankruptcy-estate-setoff) |
| bankruptcy-recover-then-forfeit | Close three gaps left by `bankruptcy-unfundable-claim-setoff` and `bankruptcy-estate-setoff` (OtterSec #130, #145). Includes an account-set change on `resolve_spot_bankruptcy`. [Details](#bankruptcy-recover-then-forfeit) |
| bankruptcy-unfundable-claim-setoff | Fix a High audit finding (OtterSec #145) where a positive perp `quote_asset_amount` vetoed bankruptcy admission unconditionally, stranding a resolvable loss in another market. [Details](#bankruptcy-unfundable-claim-setoff) |
| bid-ask-twap-quote-rest-age | Fix a High audit finding (OtterSec #146) in `update_perp_bid_ask_twap`, where a caller could move the mark TWAP with quotes it placed and cancelled in one transaction. Adds a minimum quote rest age and clamps the baseline auction start offset. [Details](#bid-ask-twap-quote-rest-age) |
| breaker-liquidation-followups | Close the last liquidator route the authority-wide equity breaker did not reach (OtterSec #68 follow-up). `liquidate_spot_with_swap_begin` and `_end` had no `liquidator_stats` in their account context, so a tripped authority could keep collecting liquidation fees through swap-backed spot liquidations while the four direct routes were barred. The shared `LiquidateSpotWithSwap` context gains `liquidator_stats`, validated with `is_stats_for_user` against the liquidator, and `begin` rejects with `EquityBelowFloor` when `equity_breaker_tripped` is set, before any flash-loan state opens. `end` runs in the same transaction, so gating `begin` covers the pair. Integrator-visible instruction-accounts change: both instructions require the liquidator's `UserStats` account, so keepers building them by hand must pass it. The SDK's `getLiquidateSpotWithSwapIx` and `getJupiterLiquidateSpotWithSwapIxV6`, and velocity-rs' `liquidate_spot_with_swap_begin` and `_end` builders, resolve it automatically. Also adds the breaker-freeze regression suite the audit issues asked for, covering trigger activation, order placement, every liquidation route and `transfer_perp_position` driven from a tripped authority. No account-layout or error-code change, and the IDL is updated for the new instruction accounts (§3.1, §5) |
| builder-fee-margin-gate | Close the remainder of a Medium audit finding (OtterSec #83). PR #309 capped the builder fee at 1% of notional, which rate-limits the transfer but does not stop it, because a taker below initial margin can reduce the position in slices, and each slice both routes up to 1% of its notional to a builder the taker approves and lowers the maintenance requirement that gates the next slice. `fulfill_perp_order` now charges the builder fee only while the taker meets initial margin, the same gate a withdrawal clears. The gate applies the withdraw gate's oracle rules too, meaning strict TWAP-bounded prices, no collateral for a deposit with an invalid oracle, and every liability oracle valid, so one oracle push, or one stale oracle on an unrelated position, cannot open it for the instant the fill needs. The fee is waived rather than the fill, so the taker still closes the position and the builder is paid nothing for that fill. Waiving is what this gate does on its own. A spot borrow oracle the calculation cannot value fails the fill outright on the separate `all_spot_liability_oracles_valid` gate in the same handler (see [fill-stale-margin-bad-debt](#fill-stale-margin-bad-debt)), so the waiver is the observable outcome for an invalid perp or deposit oracle. It is perp only, and the referrer reward is carved out of the taker fee rather than added to it, so it is unaffected. SDK: new `User.isBuilderFeeCharged()`, consulted by `User.calculatePerpTakerFee` and `VelocityClient.getMarketFees`. No account-layout, IDL or error-code change |
| ci-cargo-deny-offline-db | CI only, with no program or SDK surface change. The `Cargo deny` job's cargo-deny 0.20.x fetches the RustSec advisory DB by shelling out to the git CLI, and GitHub rejects that unauthenticated clone from the CI runner, since no `GITHUB_TOKEN` is given to this job. `cargo-audit` fetches the same DB successfully via the `rustsec` crate's gix-library path, so the job runs `cargo audit --db <tmp>` purely to populate a clone, symlinks it under cargo-deny's own hashed DB directory name, and runs both `cargo deny --offline` checks against it. `test-scripts/ci-local.sh` mirrors the same handoff in a `mktemp` scratch dir. `cargo-deny` and `cargo-audit` are pinned to exact versions, 0.20.2 and 0.22.2, since the handoff depends on both tools' on-disk layout, and both now gate the required `ci-gate` check. `deny.toml`'s policy for sources, licenses, bans and advisory ignores, and `.cargo/audit.toml`, are unchanged |
| delete-user-orphan-builder-rows | Fix a Medium audit finding (OtterSec #128) where `delete_user` permanently orphaned fee-bearing builder rows. `RevenueShareEscrow::revoke_completed_orders` only transitions rows whose `sub_account_id` matches the `User` it is handed, and `delete_user` retires that id for good, since `number_of_sub_accounts_created` has no decrement site, so the id is never reissued and no future `User` can match those rows again. A row left `open && !completed` therefore became unreachable, the builder's accrued fee was stranded, and the market's `pending_revenue_share` stayed inflated for the life of the market. The window is not the open-order case, since `validate_user_deletion` already requires every order closed. It is the filled-but-not-yet-revoked row, which is the state `revoke_completed_orders` exists to resolve. `delete_user` now resolves rather than blocks. It runs `revoke_completed_orders`, so each row for the subaccount becomes `Completed`, or is cleared when it carries no fees, which is the state the permissionless sweep pays out of, and the sweep needs no `User` so it still pays after the account is gone. Blocking instead would hold a user's rent hostage until a keeper cranked. A defensive `has_outstanding_orders_for_sub_account` check then refuses the deletion if anything for that id somehow remains outstanding, with `UserCantBeDeleted`. `force_delete_user` retires the id the same way and so orphaned rows identically, and it gets the same settlement, placed after its `cancel_orders` sweep so every row for the subaccount is already closed and therefore revocable. ABI change (§5): `DeleteUser` gains a required fifth account and `ForceDeleteUser` a required seventh, in both cases the authority's `RevenueShareEscrow` PDA. It is required rather than optional so a caller cannot skip the settlement by omitting it, and it is safe for the majority who have no escrow because the address is `seeds`-pinned, making absence provable via `data_is_empty()`. The TS SDK derives both automatically so SDK callers are unaffected, and manual ix builders must add it. `getForceDeleteUserIx` drops the escrow it used to append to `remaining_accounts`, which the program never read. No account-layout or error-code change, since it reuses `UserCantBeDeleted` |
| equity-breaker-hardening | Two recovery-path changes to the authority-wide equity breaker. Cure transfers: `transfer_deposit_by_delegate` gains a cure-only exemption, so while `UserStats.equity_breaker_tripped` is set the previously blanket-rejected instruction is allowed if and only if `equity_floor_delta == 0`, meaning the floor cannot move under a tripped breaker, and the credited subaccount's net equity is below its `equity_floor + equity_floor_buffer` before the transfer, verified with all of the credited side's oracles valid and rejecting with `InvalidOracle` otherwise, the same validity the trip and the reset require. A delegate can therefore top a breached subaccount back up out of internal surplus, and partial cures compose so several subaccounts can each contribute, instead of being forced to deposit fresh external funds. Every other delegate transfer under a tripped breaker still rejects with `EquityBelowFloor` (6358). The debited side stays gated at its own floor plus buffer by the withdraw margin check inside the transfer, so a cure cannot create a new breach, and once the credited side clears its buffered floor the exemption closes. The transfer never clears the flag. Self-verifying reset: `reset_equity_floor_breaker` (warm admin) is no longer a blind flag clear. `remaining_accounts` must carry every live subaccount of the authority, with the count pinned to `UserStats.number_of_sub_accounts` and every account's `authority` required to match so none can be omitted or passed twice, followed by the markets and oracles their positions reference. The handler reverts unless every floored subaccount's net equity clears its floor plus buffer with all oracles valid, using the new tail error `InvalidEquityBreakerReset` (6368 / `0x18E0`), and `InvalidOracle` on a bad price, mirroring the trip. A reset approved against state that later drifted back into breach, for example under a multisig timelock, now fails instead of unfreezing a breached authority. To resume a maker whose equity does not clear the floors anyway, lower the floors first via `update_user_equity_floor`. No instruction-signature or account-layout change, since the requirements are on `remaining_accounts`. SDK: `EquityFloorManager.planCureTransfers()` and `cureBreaches()` plus the pure `planCureMoves` plan the fund-only cure transfers, topping deficits to a haircut above their gate, deepest breach first, and drawing donors down no further than a haircut above their own gate. `AdminClient.resetEquityFloorBreaker` and `getResetEquityFloorBreakerIx` fetch the authority's subaccounts and build the remaining accounts automatically, with an optional `userAccounts` override for connections without `getProgramAccounts`, and admin CLI `user reset-equity-breaker` inherits this and needs no flag changes (§3.1) |
| equity-breaker-lazy-trip | The authority-wide equity breaker arms lazily on touch, not only via the permissionless `trip_equity_floor_breaker` transaction. The paths allowed to run while a floored subaccount sits below its raw floor, meaning reducing perp fills on both taker and maker side, strictly reducing swaps in `end_swap`, and the trigger-cancel path of `trigger_order`, set `UserStats.equity_breaker_tripped` inline when they observe net equity below the raw floor with all oracles valid. They mirror the permissionless trip's valuation and never arm off an invalid price. Gated paths cannot host a trip, since a rejected instruction reverts its own writes, and they never need to, since they reject at `floor + buffer`. Net effect: a delegate transacting on a below-floor subaccount freezes the authority at the first touch instead of waiting for the guard keeper's trip to land, and only passive drawdown with no interaction still relies on the keeper. ABI note (§5): `trigger_order`'s `user_stats` account is now writable, so clients built from an older IDL fail its `mut` constraint. No account layout or error-code change, and the SDK ships the regenerated IDL, which differs only in writability (§3.1) |
| equity-breaker-trip-dust-tolerance | Fix the residual of OtterSec #139. The breaker trip's all-or-nothing oracle-validity requirement let a worthless position in an invalid-oracle market veto the authority-wide freeze for as long as the outage lasted, however provable the breach on the rest of the portfolio. `trip_equity_floor_breaker` reverted with `InvalidOracle` and the lazy trip skipped without reporting an error. Deposits are permissionless, so the delegate the breaker constrains controlled the trigger. Both trip paths decided with a new program-internal walk, `calculate_user_equity_for_trip` returning `TripNetEquity { equity_upper_bound, provable }` and the shared predicate `proves_breach`. Positions with valid oracles were valued live as before. An invalid-oracle position worth no more than the new `EQUITY_FLOOR_TRIP_DUST_ALLOWANCE` ($100, QUOTE_PRECISION) at its own last twap was conceded its most favorable value, meaning assets took the full allowance and liabilities zero, and for a perp only the base leg needed the concession since entry quote and funding count exactly and a short's base leg is bounded by zero. A larger invalid position, or an invalid quote oracle, kept the trip blocked with `InvalidOracle`. The floor gates, the reset and cure transfers keep the strict all-or-nothing verdict, because the reset proves the opposite direction where conceding dust would be unsound, and a dead dust oracle blocking a reset is routed around by lowering floors first. No account-layout, instruction-signature, IDL or error-code change. SDK: new `User.getTripNetEquity(slot?)` and `User.provesEquityFloorBreach(slot?)` mirror the walk and the predicate, and `isBelowEquityFloor` documents that it compares the point value only. The equity guard bot needs no change, since it simulates the trip and classifies the chain's answer. Partly reverted by the next row (§3.1) |
| equity-breaker-trip-remove-twap-concession | Remove the unsound part of `equity-breaker-trip-dust-tolerance`. An invalid oracle and its stored twap are not independent sources, so a stalled twap can size a real asset or perp long below the $100 allowance, which makes the permissionless trip understate equity and freeze a healthy authority permanently. The breaker no longer uses twap or size for invalid-oracle assets and longs, and any such leg makes the trip unprovable until its oracle recovers. Invalid-oracle liabilities and shorts retain their unconditional zero upper bound, and stored perp quote and funding legs still count exactly. The existing strict floor gates are unchanged and continue blocking the affected floored subaccount during the outage. Sibling subaccounts are not given a new temporary authority-wide restriction, and that residual window is accepted for the manually enrolled, monitored user set. The program and SDK derive observed equity, strict oracle validity, and the trip upper bound through one shared position walk, deleting the duplicate trip walks and `EQUITY_FLOOR_TRIP_DUST_ALLOWANCE`. No instruction, account-layout, IDL, error-code, keeper lifecycle, or other authorization behavior change (§3.1) |
| equity-floor-buffer | New `User.equity_floor_buffer: u64` (QUOTE_PRECISION, struct offset 4480, consuming the last 8 tail-padding bytes, `User` unchanged at 4496) adds admin-set headroom above the equity floor. Every risk-increasing gate, meaning order placement and fills on taker and maker, withdrawals, `end_swap`, deposit and position transfers out, trigger-order activation, and the floor-transfer to-side check, enforces `floor + buffer`, while the permissionless `trip_equity_floor_breaker` still requires equity below the raw floor. No permitted action can therefore leave a subaccount trippable, and a passive drawdown must burn through the whole buffer before the breaker can fire. The anti-defuse pre-check in `transfer_deposit_by_delegate` stays at the raw floor, so a subaccount inside its band may still shed floor. `update_user_equity_floor(equity_floor, equity_floor_buffer)` sets both, which is a signature change. SDK: `UserAccount.equityFloorBuffer`, `User.isBelowBufferedEquityFloor` / `getBufferedEquityFloor` / `getEquityAboveBufferedFloor`, `calculateEquityFloorAutoDelta` and `getEquityFloorLevel` (`math/margin`), and the new `EquityFloorManager` giving aggregate status and levels, haircut-padded transfer planning, `getMaxWithdrawable` / `getMaxQuoteTransferable`, and proportional floor rebalancing via zero-amount transfers. `transferDepositByDelegate` `'auto'` targets `floor + buffer`. Admin CLI `user set-equity-floor <user> <floor> <buffer>` (signature change), plus new `user equity-floor-status` and `user close-positions`. The keeper guard bot gains prometheus headroom and level metrics plus drop and level-transition alerts. velocity-rs gains `math::equity_floor` (`calculate_equity_floor_auto_delta`, `equity_floor_level`, `EquityFloorLevel`) mirroring the TS helpers, and the generated bindings and the program-crate predicates pick the new field up automatically (§3.1, §5) |
| equity-floor-exemption-hardening | Fix two follow-ups from the OtterSec review of the equity floor. (1) The strictly-reducing swap exemption in `end_swap`, the only swap a below-floor or breaker-tripped account may perform, priced its 1% value-loss bound with raw oracle prices and no validity gating, so a stale-low input oracle could let a materially loss-making swap pass. Both legs now require margin-valid oracles, rejecting with `InvalidOracle` otherwise, and the bound values the in leg at the strict max and the out leg at the strict min of the live price and the 5min twap. (2) The four balance-acquiring liquidation paths (`liquidate_perp`, `liquidate_spot`, `liquidate_borrow_for_perp_pnl`, `liquidate_perp_pnl_for_deposit`) checked the liquidator authority's breaker and post-liquidation initial margin but not the liquidator subaccount's own floor plus buffer, so a delegate-controlled liquidator inside its buffer band could still acquire exposure through liquidation. They now apply the same buffered-floor admission check as risk-increasing fills, with `EquityBelowFloor`, skipped entirely when the liquidator has no floor set. Program-internal only, with no account-layout, IDL or error-code change (§3.1) |
| equity-floor-fail-closed | Harden the equity-floor oracle handling after external review of the bounds approach. The two-sided equity bound introduced by `equity-floor-oracle-validity` could collapse onto a single invalid price, because an invalid oracle's live price and its own `last_oracle_price_twap_5min` can share the same stale value, and a non-positive candidate left the pair pinned to the survivor. A "lower bound" built from invalid data could therefore still authorize an action through the floor. The bounds machinery (`calculate_user_equity_bounds`, `bound_prices`, `UserEquityBounds`) is removed. The floor metric is exact net equity plus the validity verdict (`FloorNetEquity`), and every gate fails closed on the verdict. The gates that authorize an action, meaning risk-increasing placement, taker and maker fills, trigger activation, withdrawals, deposit, perp-position and isolated transfers out, and the four liquidator admission checks, reject with `InvalidOracle` while any oracle the subaccount depends on is invalid, and with `EquityBelowFloor` when a fully valid value sits below `floor + buffer`. `force_cancel_orders` keeps its direction, treating below-floor as grounds only with all oracles valid and equity below the raw floor. Separately, DLOB match fills gain the `FillOrderMatch` oracle-validity rule that upstream applies and the fork had dropped, so a NonPositive, TooVolatile or TooUncertain oracle withholds match fulfillment. The fill returns without matching, mirroring the TWAP-divergence guard, the same way `FillOrderAmmLowRisk` and `FillOrderAmmImmediate` already gate AMM fills. That also closes the window where an exact-close reducing fill could execute under a TooUncertain oracle while the lazy breaker trip, which never arms off an invalid price, could not observe it. No account-layout, instruction-signature, IDL or error-code change. SDK breaking changes: `User.getNetUsdValueBounds` is replaced by `User.getFloorNetEquity(slot)` returning `{ value, allOraclesValid }`; `boundPrices`, `NetUsdValueBounds`, `I128_MIN` and `I128_MAX` are removed; `isBelowBufferedEquityFloor(slot)` predicts the fail-closed gates, so any invalid oracle reads as gated; and `getEquityAboveFloor(slot)` and `getEquityAboveBufferedFloor(slot)` report zero headroom on an invalid oracle. The admin CLI `user equity-floor-status` reports "invalid oracle: floor gates blocked" instead of a lower bound (§3.1) |
| equity-floor-gaps | Fix audit findings closing gaps in the per-user equity floor and authority-wide breaker enforcement. (1) `trigger_order` cancels a risk-increasing trigger order, instead of activating it and paying the keeper, when the owning authority's `equity_breaker_tripped` flag is set. The instruction gains a required `user_stats` account, the order owner's `UserStats`, which is an ABI and IDL change (§5). (2) Generic spot swaps (`end_swap`) reject with `EquityBelowFloor` while the breaker is tripped, matching withdrawals and transfers out, since the per-subaccount floor was already enforced. (3) `transfer_perp_position` also checks the recipient's equity floor, where it previously checked only the sender's, so exposure cannot be pushed into a sibling subaccount that passes initial margin but lands below its warm-admin floor. (4) `transfer_deposit_by_delegate` rejects with `InvalidEquityFloorTransfer` a floor-delta that would reduce a sub-account's floor while that sub-account is already below the floor being reduced, since an owner could otherwise shed floor off a breached sub-account with a zero-amount transfer and drop it out of breach before the permissionless breaker trips. (5) A tripped authority is barred with `EquityBelowFloor` from every balance-acquiring liquidation, meaning `liquidate_perp`, `liquidate_spot`, `liquidate_borrow_for_perp_pnl` (takes over a borrow) and `liquidate_perp_pnl_for_deposit` (takes a deposit against negative pnl), all of which acquire the liquidatee's risk and earn a fee. `liquidate_perp_with_fill` stays allowed, since its liquidator routes the position to the book and never acquires a balance, as do bankruptcy resolutions. No account-layout change, and the `trigger_order` and `liquidate_spot` account lists changed, each gaining a `UserStats` account (§5). SDK `getTriggerOrderIx` and `buildTriggerOrderInstruction` supply the owner's `userStats`, `getLiquidateSpotIx` supplies the liquidator's `liquidatorStats`, and the velocity-rs `trigger_order` and `liquidate_spot` builders were updated (§3.1, §5) |
| equity-floor-oracle-validity | Fix three OtterSec findings (#131, #139, #142) where an equity-floor decision was taken off an oracle price the program had already judged invalid. Introduced a two-sided equity bound, later replaced by `equity-floor-fail-closed`. [Details](#equity-floor-oracle-validity) |
| expiry-price-conservation | Fix three Medium audit findings (OtterSec #116, #125, #147) on opposite sides of the same expiry-settlement conservation equation, where aggregate user claims must fit the value that backs them. [Details](#expiry-price-conservation) |
| expiry-settlement-guards | Fix two High audit findings (OtterSec #149, #133). #149: a time-expired perp position could still be liquidated at the live oracle before its fixed settlement price existed. Every ordinary user path already refuses past `expiry_ts` via `is_in_settlement(now)`, so placing, filling, triggering, transferring and settling all gate on it, but `liquidate_perp` and `liquidate_perp_with_fill` did not. For the whole window between `expiry_ts` and a warm admin flipping the status to `Settlement`, a liquidator could take the position at a live price the committed `expiry_price` then supersedes, while the owner had no way to act. Both now reject with `InvalidLiquidation` when the market has expired but is not yet `Settlement` or `Delisted`. They are deliberately not gated on `is_in_settlement` itself, because that is also true once the status is `Settlement` or `Delisted`, by which point `expiry_price` is committed and liquidating during the wind-down is a legitimate way to resolve bad debt. An existing delisting test caught the over-broad first attempt. `resolve_perp_bankruptcy` is likewise untouched, so bad debt on an expired market can always still be cleared. Integrator-visible: liquidating a perp market between `expiry_ts` and its `Settlement` flip now reverts, so run `settle_expired_market` first, then close positions via `settle_expired_position`. #133: a negative committed `expiry_price` was clipped out of margin and equity. `calculate_base_asset_value_and_pnl_with_oracle_price` clamps a non-positive price to zero, which is correct for a live oracle where a negative print is nonsense, but margin reused it for the `Settlement` valuation, so a long's signed base loss was clipped to zero and the position read as merely worthless instead of underwater, letting the owner withdraw collateral. `settle_expired_position` values the same position through `calculate_base_asset_value_with_expiry_price`, which never clamped, and later booked the real negative value as an unsecured quote borrow, and that divergence was the bug. The new `calculate_base_asset_value_and_pnl_with_expiry_price` keeps the sign, and both `Settlement` branches in `math/margin.rs` use it. The live-oracle clamp is deliberately left intact, since it is a real guard against a bogus oracle print and only the expiry-price path is legitimately allowed to be negative. No account-layout, IDL, error-code or SDK-API change, since it reuses `InvalidLiquidation` |
| feat/propamm | PropAMM order flow. Perps fill through one router across the vAMM, an on-chain CLOB book and external quoter programs. The DLOB, order auctions, AMM JIT and jit-proxy are removed, and every live order is ephemeral: only its unfilled remainder rests, on the book. New accounts `QuoterV0`, `QuoterSlabV0`, `ClobCrankConditionsV0`, `UserConditionsV0` and `CrankTreasuryV0`, new error codes 6375 to 6458, and relay cranks for expiry, eviction, crosses, triggers and liquidations. Velocity holds every attached book's config authority, and `update_perp_market_clob_book_config` / `resize_perp_market_clob_book` are the only paths that change it. §2, §3, §4 and §5 carry the surface. [Details](#propamm-order-flow) |
| fee-tier-vip3 | Adds a fourth perp fee tier, VIP 3, at $200M trailing-30d volume (§3). `PERP_FEE_TIER_MAX_INDEX` goes from 2 to 3, `VOLUME_THRESHOLDS` gains `TWO_HUNDRED_MILLION_QUOTE`, `FeeStructure::perps_default` seeds `fee_tiers[3]`, and `update_promo_fee_tier` accepts 3. No ix, layout, IDL or error-code change, since the slot already existed in the 10-wide array. SDK mirror: `VIP_FEE_TIER_THREE_VOLUME_QUOTE` in `PERP_FEE_TIER_VOLUME_THRESHOLDS`, so `getPerpFeeTierIndex`, `getUserFeeTier` and `getMarketFees` pick tier 3 above $200M. Admin CLI `fees set-schedule` takes four tier fees |
| fill-stale-margin-bad-debt | Fix four High audit findings (OtterSec #143, #144 on oracle validity, and #135, #148 on unaccrued interest) in the perp-fill path's post-fill margin checks. Adds the new error `SpotMarketInterestStaleForMargin` (6371). [Details](#fill-stale-margin-bad-debt) |
| if-add-exact-share-pricing | Follow-up to #253 on the same High finding. Rejecting only the zero-share case still let a deposit be partly captured. Shares are indivisible, so a request worth 1.5 shares minted 1 and donated the remaining half to existing shareholders, and with a donation-inflated share price the forfeited fraction approaches 100%, so the zero-share guard bounded the loss rather than removing it. `add_insurance_fund_stake` now transfers only the portion of the requested amount that prices to whole shares, via `deposit_amount_and_shares_for_if_stake`, which floors the shares and ceils their cost so the fund never sells a share below price, and leaves the remainder, always less than one share price, in the depositor's token account. `IFDepositMintsZeroShares` (6360) now means the request is below the price of one share. `InsuranceFundStakeRecord.amount`, `InsuranceFundStake.cost_basis`, `UserStats.if_staked_quote_asset_amount` and `SpotMarket.if_last_settle_vault_amount` all track the accepted amount rather than the request. Integrators must treat the `amount` argument as an upper bound and read the staked amount from `InsuranceFundStakeRecord`, since the SDK's `fromSubaccount` path leaves any remainder in the wallet's token account. The `vaults` program's `add_insurance_fund_stake` stakes the whole balance of `vault_if_token_account` rather than the requested `amount`, so a remainder left by an earlier add folds into the next one. That account holds nothing else and no instruction sweeps it, so the amount staked can exceed the manager's transfer. No layout, IDL or error-code change |
| if-cancel-settle-first | Fix a High audit finding (OtterSec #141) where the IF unstake cancel priced its forfeiture against an unsettled vault. `cancel_request_remove_insurance_fund_stake` implements the anti-free-option rule, withdrawing at the frozen `last_withdraw_request_value` and restaking at the live vault price so escrow-window appreciation is forfeited to the stakers who stayed, but it read `insurance_fund_vault.amount` with no settle first. A staker could order their signed cancel ahead of an already-due signerless revenue settle, making the restake price against a pre-settle vault so the cancel burned no shares, or too few, then withdraw revenue the rule assigns to the remaining stakers. It now settles first, mirroring the treatment `request_remove` received in #31 and the add path. Settling rather than gating the cancel is deliberate, because #34 exists precisely so a pending request can always be cancelled, and refusing the cancel until someone else cranked a settle would reintroduce a cancel-blocking condition. Revenue accruing after the settle is still forfeited by the freeze, which is the intended escrow tradeoff. ABI change (§5) to both the velocity ix and the `vaults` CPI wrapper, and both SDKs were updated so their callers are unaffected |
| if-revenue-settle-snapshot (#254 follow-up) | Close the remaining donation path in the #254 fix. `SpotMarket.if_last_settle_vault_amount` was an accounted shadow balance whose `0` meant both uninitialized and empty, and both readers fell back to the live vault on `0`, so the first post-upgrade settle or stake on any existing market imported whatever sat in the vault, and a `saturating_sub` on a loss draw could return the field to `0` and let the next stake import a donation again. The field is now the lowest IF vault balance since the end of the last revenue settle, and the cap base is `min(live_if_vault, snapshot)` with no fallback. `settle_revenue_to_insurance_fund` starts each period by writing the balance it leaves behind, and the new `record_insurance_fund_outflow` lowers the field at each of the four outflow paths (`remove_insurance_fund_stake`, `resolve_perp_pnl_deficit`, `resolve_perp_bankruptcy`, `resolve_spot_bankruptcy`) so a mid-period dip is still priced at the next settle. Otherwise a draw followed by a donation would read the pre-draw balance again. A donation must therefore stay in the fund for a whole period to count, by which point it belongs to the stakers pro rata. The growth bookkeeping in `add_insurance_fund_stake` is removed, so a stake lifts the cap one settle period later. A market with a `0` snapshot settles nothing for one period and records the endpoint instead, and `NoRevenueToSettleToIF` is suppressed for that one settle so the endpoint write is not reverted. The field name, offset and account size are unchanged, so no decoder change is needed, and SDK `SpotMarketAccount.ifLastSettleVaultAmount` keeps its type and gains the new meaning. `nextRevenuePoolSettleApr` is still a display estimate and does not mirror the cap |
| interest-carveout-dust-accumulator | Replace the #127 carveout deferral with carried remainders, and fix the part of #127 that the deferral hid. Adds two fields to `PoolBalance`. [Details](#interest-carveout-dust-accumulator) |
| jupiter-swap-api-v2 | SDK-only and opt-in. `JupiterClient` gains `apiVersion: 'v1' \| 'v2'`, where `'v2'` talks to Jupiter Swap API v2. [Details](#jupiter-swap-api-v2) |
| jupiter-swap-api-v2-rust | velocity-rs only, and breaking. The Rust SDK's Jupiter path moves to Swap API v2 outright, with no v1 mode and no version toggle. [Details](#jupiter-swap-api-v2-rust) |
| lazer-conf-crossed-quotes | Fix the second half of a Medium audit finding on the Pyth Lazer confidence (OtterSec #72). `handle_update_pyth_lazer_oracle` persists the widest of three signals as `conf`, being a 20bps floor on the price, the distance between the best bid and the best ask, and the signed `Confidence` property of the message. It measured that distance as `ask - bid`, so a crossed book produced a negative value, the `max` discarded it, and the 20bps floor stood. A crossed book carries more uncertainty than an uncrossed one, so the floor understates it, which is what #72 forbids. The distance is now a magnitude, so a crossed book widens the confidence by the size of the disagreement. The subtraction also moves to i128 and saturates at `i64::MAX`, because an i64 subtraction of two extreme mantissas overflows and one feed's `MathError` aborts every other feed in the same message. The computation moves into `calculate_lazer_conf`, which is unit tested. Integrator-visible: a Lazer feed whose book crosses reports a wider confidence, which widens every confidence-derived guard on that oracle. No account-layout, IDL, error-code or SDK-API change |
| liquidation-throttle-tolerance | Tighten two bounds left loose by #267 (`liquidation-fee-basis` and `liquidation-math`). (1) `liquidate_perp_pnl_for_deposit` rejects a loss-making transfer up front instead of after the fact. The seizure premium (`asset_weight × liquidator premium / liquidator discount`) must stay below the buffered pnl liability weight, otherwise the call reverts with `LiquidationWorsensAccountHealth` (6361) before any balance moves. The "shortage must not grow" postcondition is now exact, and the $1 tolerance is gone. It existed because `calculate_asset_transfer_for_liability_transfer` rounds the seizure up to the user's whole deposit when the remainder is worth under $1, taking collateral the pnl relief does not pay for, and a liquidator picks the transfer size so any tolerance is an amount to stay under and repeat. The seizure now uses a new exact conversion, `calculate_asset_transfer_for_liability_transfer_exact` with no round-up, and takes the whole deposit only when the deposit is what limited the transfer, where the round-up is base-unit truncation rather than value. A liquidation that previously swept a sub-$1 remainder now leaves it with the user, so `LiquidatePerpPnlForDepositRecord.asset_transfer` can be smaller and the user can keep a dust spot position. Market pairs whose combined liquidator fees exceed the liquidation margin buffer are refused by this path entirely, so route them through `liquidate_spot` or `liquidate_borrow_for_perp_pnl`. `MarketStatus::Settlement` stays exempt. (2) `liquidate_spot_with_swap_begin` drops the 25 bps headroom it added on top of the max-pct-to-liquidate throttle and derives the bound with the exact conversion, so `max_asset_transfer` equals the throttled asset transfer rather than the throttle plus 25 bps, or the whole deposit when the throttle lands within $1 of it. Begin and end run in one transaction off the same oracle prices, and `swap_end` bounds the exchange rate on its own. No layout, IDL or error-code change |
| mark-twap-reseed-after-gap | Complete the funding-pause work PR #276 began, on the perp side. `calculate_new_twap` weights an incoming mark-TWAP sample by the time since the last write and floors the opposing weight at 1, so past one funding period a single sample replaces the TWAP outright and sets that period's funding premium alone. The gap is longest, and its end most predictable, after a funding pause, because `handle_update_funding_rate` and `handle_update_perp_bid_ask_twap` both carry `funding_not_paused`, so in a market that does not trade nothing writes the mark TWAP while the pause is set, and `on_the_hour_update` makes funding fire on the first crank after it lifts. Two partial defenses predate this. Every writer refreshes the oracle TWAP earlier in the same instruction and the correction in `update_mark_twap` replaces the stale bid and ask TWAPs with `last_oracle_price_twap`, but a fill-path sample then overwrote that correction at nearly 100% weight. And the crank sample-weight cap (#416, `max_mark_twap_sample_elapsed`) bounds a crank sample's weight but leaves fills uncapped, healing a post-gap TWAP only through a slow sequence of capped samples. `MarketStats::update_mark_twap` now discards the mark TWAPs and re-seeds `last_bid_price_twap`, `last_ask_price_twap` and `last_mark_price_twap` from `last_oracle_price_twap`, `last_mark_price_twap_5min` from `last_oracle_price_twap_5min`, and stamps `last_mark_price_twap_ts`, whenever the TWAPs went unwritten for more than `max(funding_period * MARK_TWAP_RESEED_FUNDING_PERIODS, ONE_HOUR)`. The multiplier is 3 because a market whose only writer is the funding crank writes once per funding period in the steady state, and `on_the_hour_update` can stretch one legitimate interval to about 1.67 periods. The `ONE_HOUR` floor covers markets configured with a zero funding period. The re-seed sits on the shared core rather than in `update_funding_rate`, because after a pause the first writer may be a fill or the bid and ask crank instead, and whichever arrives first consumes the gap. That sample lands on the next call, so the first funding update after a long gap sees a zero price spread and charges the baseline `FUNDING_RATE_OFFSET_DENOMINATOR` offset alone, and the real premium returns the following period from accumulated samples. `mark_std` is left untouched by a re-seed, which observed no trade. Integrator-visible: funding for the first period after a pause or a multi-period keeper gap is the offset alone rather than a premium derived from one quote. SDK `calculateLiveMarkTwap` mirrors the re-seed so `calculateAllEstimatedFundingRate` does not predict a premium the program will not charge, and exports `MARK_TWAP_RESEED_FUNDING_PERIODS`. No account-layout, IDL, error-code or instruction-signature change |
| market-account-padding | `PerpMarket` and `SpotMarket` each append 256 reserved bytes for future fields. Account sizes grow from 1304 to 1560 and from 808 to 1064 respectively, so existing accounts must be extended after deployment with the existing `extend-account` crank. The unused 32-byte `SpotMarket.spot_fee_pool` slot is retained as explicit padding (`padding_former_spot_fee_pool`) at the same offset, and all later field offsets are unchanged. SDK and IDL decoders expose the reserved bytes, and no program behavior changes (§5.1) |
| market-fees-for-tier | SDK-only. `getMarketFees` could only price the tier the account was on, so a client had no way to ask what a market would charge at another tier, which is needed to show what a promo saves against a volume tier. The modifier pipeline (surcharge, `feeAdjustment`, referee discount, builder fee) is extracted as the exported `getMarketFeesForFeeTier` (`math/fees`), and `getMarketFees` delegates to it and accepts a `feeTierOverride` fifth argument (§4.6). No program or layout change, and no behavior change for existing calls |
| mm-oracle-batch-native | New native fast-path instruction at dispatch opcode 2, `update_mm_oracle_batch_native`, which writes the MM oracle for up to 64 perp markets in one instruction. The motivation is transaction efficiency rather than behavior, since a caller cranking many markets on a fixed slot interval previously needed one transaction per market and the per-signature fee is flat regardless of how little the instruction does, so collapsing N markets into a single transaction amortises it across them. Measured CU also improves: `bun run bench:native-cu` puts the batch at 1627 CU for one market, 2145 for two and 3181 for four, which is about a 1109 CU fixed authentication prologue plus about 518 CU per market, against 6320 CU for the same four markets as four separate instructions. Accounts are `[0] signer, [1] state, [2..2+n] perp markets (writable)`. The slot comes from the Clock sysvar syscall, so no clock account is passed and none can be forged. The payload after the 5-byte native prefix is `u8 n` then `n × { u16 market_index_le, i64 price_le, u64 sequence_id_le, u64 source_slot_le }`, and each entry's `market_index` is re-checked against the market account it was paired with, so a misordered account list is a hard error rather than a write to the wrong market. Per-market rate-limit and sanity rejections skip that market and leave the rest of the batch intact, covering a non-positive price, a non-advancing sequence id, a non-advancing slot, a slot gap below `MM_ORACLE_MIN_SLOT_GAP`, and a source slot more than `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` from the landing slot in either direction. A throttled market must not destroy its neighbours' writes. One `msg!` is emitted per non-zero bitmask, for rejected entries and for entries the step cap altered. Structural problems fail the whole instruction, covering malformed framing, `n == 0` or `n > 64`, too few accounts, state or market failing owner and discriminator checks, a non-writable market, the kill switch being off, and a signer that is not `State.hot_mm_oracle_crank`, since those can only be caller bugs. Two new errors are appended at the enum tail, `InvalidNativeInstructionData` (6369) and `MmOracleUpdateDisabled` (6370), and both handlers return the typed error in place of the old `assert!` panic. The per-market gating is a single shared core, `apply_mm_oracle_update`, used by opcode 0 and the batch, so the two cannot drift, and `native_batch_tests::batch_matches_single_market_handler` pins them to identical accept and reject decisions at the wire level. An earlier revision duplicated the gating and left opcode 0 untouched, and the mm-oracle-freshness-fixes PR folded them together once it had to modify both anyway. One deliberate divergence remains: opcode 0 returns `Err` on a non-positive price, while opcode 2 skips it, since a hard error in a batch would destroy every other market's write. SDK gains `VelocityClient.getUpdateMmOracleBatchNativeIx` and `updateMmOracleBatchNative` (§4.6). No account-layout change. Native instructions do not appear in the IDL, but the IDL's `errors` list gains the two entries |
| mm-oracle-freshness-fixes | Five fixes to the MM-oracle path, behavioral, with no account-layout, IDL or error-code change beyond the errors added in mm-oracle-batch-native. Includes a breaking SDK signature change on `updateMmOracleNative`. [Details](#mm-oracle-freshness-fixes) |
| off-chain-slot-clock | The off-chain half of `slot-duration-scaling`. TypeScript clients read the live slot length from a shared resolver, so no client holds a slot length of its own. New SDK `currentSlotClock(source, currentSlot)` returning `{ slotDurationMs, isLive }`, and `currentSlotDuration(...)` (`math/time`), both delegating the staged-flip decision to the existing `activeSlotDurationFromState` so a client's prediction matches the on-chain value across a gate boundary. `source` is duck-typed on `{ getStateAccount() }`, keeping `math/time` importing only `BN`. When state or the slot feed is unavailable, or the decoded staging fields are not valid, both helpers fall back to the hardcoded 400ms `SLOT_DURATION_BASELINE` and report `isLive: false`, and callers do not pass a fallback. New `SLOT_DURATION_SCHEDULE_MS` mirrors the program's schedule, and `SLOT_DURATION_FLOOR` (200ms) is what a slots-to-ms call site substitutes on a dead feed, where the 400ms baseline would widen the window instead of tightening it. A missing or `0` `currentSlot` resolves to the baseline instead of being read as slot zero, since a failed slot subscription reports `0` and slot zero precedes every effective slot, which would otherwise return the pre-flip base while looking live. `keeper-bots-v2` drops its local copy for the shared helper. `SLOT_TIME_ESTIMATE_MS` stays exported and deprecated. No program, layout, IDL or error-code change |
| oracle-account-binding | Two oracle-account checks. (1) `recenter_perp_market_amm_crank` set the peg from any oracle the caller passed. The `AdminUpdatePerpMarketAmmSummaryStats` accounts struct now carries `has_one = oracle @ ErrorCode::InvalidOracle`, so the account must be `perp_market.oracle`. That binds both handlers on the struct, and `update_perp_market_amm_summary_stats` drops its now-redundant `valid_oracle_for_perp_market` access control. The crank runs under the `AmmCrank` hot role, so before this the hot key could repeg a market to a price of its choosing, which is warm-admin power. (2) A pyth push price account was read without checking that it is one. `OracleMap` admits any pyth-program-owned account on owner alone, and `pyth_client::cast` then reinterprets whatever bytes it gets, so `magic`, `ver` and `atype` decided nothing. The new `state::oracle::load_pyth_push_price` requires the length and the alignment of a `Price`, magic `0xa1b2c3d4`, version 2, and account type 3 (`AccountType::Price`) before the cast, and both push-oracle readers (`get_pyth_price`, `AMM::get_pyth_twap`) go through it. A failing account returns `InvalidOracle`, or `UnableToLoadOracle` when short or unaligned. The alignment check matters because `pyth_client::cast` discards the unaligned head of the slice and indexes the first whole `Price` in the rest, so an unaligned account would read from a different offset than the header. `OracleMap::load` and `load_one` also stop reading an 8-byte discriminator out of a shorter velocity-owned account. SDK: `PythClient.getOraclePriceDataFromBuffer` mirrors the header check and throws instead of returning a decoded number for an account the program refuses |
| placement-settlement-gating | Fix four Medium audit findings on order placement and settlement gating. #84: signed-message bundles are atomic around the main order, since `place_signed_msg_taker_order` pre-checks the main taker order's `max_ts` and skips the whole bundle if it has already expired, so the reduce-only TP and SL sidecars, which are exempt from `max_ts` expiry, are no longer left standing as standalone triggers when the entry would soft-skip. Order ids are unchanged, with sidecars first and the main order trailing. #85: the signed-message taker path rejects IOC orders with `InvalidOrderIOC`, matching the direct and batch place paths, so a signed IOC limit order cannot be stored as an indefinitely-resting order. #86: `trigger_order` enforces the `is_in_settlement` gate the place and fill paths use, so a keeper cannot trigger a dormant order, and collect the flat reward, on an expired or settling market. #87: `transfer_perp_position` rejects transfers once the market is expired or in settlement, so an authority cannot split a live-oracle gain from the matching fixed-`expiry_price` loss across two of its own subaccounts. Program-internal behavior changes, with no account-layout, IDL or error-code change, since they reuse existing error variants |
| pre-refresh-twap-band | Fix five Medium audit findings (OtterSec #109 through #112, and #134) that share one shape: an instruction advanced an oracle TWAP and then evaluated its own gate against that just-moved value. [Details](#pre-refresh-twap-band) |
| promo-fee-tier | SDK-only follow-up to #388. `VelocityClient.getMarketFees` ignored the `State.promoFeeTier` floor when called without a `user`, so the generic schedule quoted the entry tier (4bps) while a promo had every account on a better one, disagreeing with the same call made with a user. The floor plus the volume ladder now live in one place, `getPerpFeeTierIndex` (`math/fees`), which `getUserFeeTier`, `getMarketFees` and `DLOB.getMakerRebate` all select through. `PERP_FEE_TIER_VOLUME_THRESHOLDS`, `PERP_FEE_TIER_MAX_INDEX` and `User.getUserPerpFeeTierIndex` are exported with it so consumers that rank the tier stop re-deriving the ladder (§4.6). No program or layout change |
| protective-price-pre-refresh-twap | Fix a Medium audit finding (OtterSec #14) in the shared spot-liquidation helper, which refreshed the market's oracle TWAPs before judging the oracle and pricing against them. [Details](#protective-price-pre-refresh-twap) |
| referrer-snapshot-init-gate | Fix a Medium audit finding (OtterSec #129). `initialize_revenue_share_escrow` snapshots `escrow.referrer` from `UserStats.referrer` and nothing in the program ever rewrites that field, while `UserStats.referrer` is only ever set by an authority's first `initialize_user`. Because `authority` on the escrow init is an `UncheckedAccount` and only `payer` signs, any third party could create another authority's escrow PDA in the window between `initialize_user_stats` and that first `initialize_user`, freezing `Pubkey::default()` into the escrow. That leaves `has_referrer()` false, creates no referral order row, and suppresses referral rewards and the referee discount permanently with no repair path, since the permissionless resize only changes slot capacity. `initialize_revenue_share_escrow` now rejects with `UserNotFound` (6234 / `0x185A`) unless `UserStats.number_of_sub_accounts_created > 0`, which places every escrow strictly after the point where the referrer becomes immutable, so the snapshot cannot be taken too early. Integration note: an escrow can no longer be created for an authority that has only `UserStats`, so create sub-account 0 first, which is the ordering every existing client already uses. No account-layout, IDL or error-code change, and the SDK change is doc-only, since `initializeRevenueShareEscrow` and `getInitializeRevenueShareEscrowIx` state the precondition |
| revshare-settle-liveness | Fix three ways accrued builder and referrer revenue share became permanently uncollectable while `PerpMarket.pending_revenue_share` kept reserving the pnl-pool tokens behind it. Adds two permissionless instructions and two error variants. [Details](#revshare-settle-liveness) |
| same-slot-oracle-sample-check | Fix a Medium audit finding. A cached AMM oracle attestation (`last_oracle_valid` plus a same-slot `last_update_slot`) was trusted by `settle_pnl`, the signerless `sweep_perp_market_fees` and `resolve_perp_pnl_deficit` without checking that the oracle sample being consumed is the one the AMM update validated, so a Pyth Lazer write landing later in the same slot could swap in a `TooUncertain` price that then settled PnL or sized the fee sweep. AMM and funding oracle-stat updates now stamp `historical_oracle_data.last_oracle_conf` for perp markets, where it was previously always 0. The layout is unchanged, since the field existed. `PerpMarket::is_recent_oracle_valid` additionally requires the consumed sample's price and confidence to match the stamped pair, so a same-slot rewrite falls through to a full validity re-evaluation at the point of use and is rejected with the sample's own validity error when invalid. No account-layout, IDL, instruction or error-code change |
| settle-expired-no-position-sweep | Audit fix follow-up on the #270 revenue-share sweep gate (OtterSec Medium #43). The Settlement-market path reported settlement unconditionally, because `settle_expired_position` returns early when the user holds no position in the market and that no-op runs before the market's `PerpOperation::SettlePnl` pause check, so `handle_settle_pnl` and `handle_settle_multiple_pnls` still ran `sweep_completed_revenue_share_for_market` on a market whose settlement an operator paused. `settle_expired_position` now returns `bool` on the same contract as `settle_pnl`, meaning `true` when it settled and `false` for the no-position no-op, and both handlers gate the sweep on it. No layout or IDL change, since `settle_expired_position` is an internal controller fn rather than an instruction |
| slot-duration-scaling | `State.slot_duration_ms` plus `update_state_slot_duration_ms` (warm admin, staging the exact successor on the 400, 350, 300, 250, 200 schedule during the target IBRL gate warmup, with State auto-switching at the effective slot via `pending_slot_duration_ms` and `slot_duration_effective_slot`). All wall-clock windows, ramps and rates become typed durations, using `math::time::Millis` plus `SlotDuration` in the program and branded `Millis` plus `SlotDurationMs` in `math/time.ts` in the SDK, expressed in actual slots at read time. Code constants are defined in milliseconds, and legacy stored fields decode from their 400ms-unit encoding via typed getters, with `DelayOverride` typing the i8 sentinels. `block_operation`'s funding gate keeps its current wall-clock width, about 40% of the funding period, across gates instead of widening with faster slots, so the pre-existing period-versus-seconds behavior is preserved rather than corrected. SDK `SLOT_TIME_ESTIMATE_MS` is deprecated, `IDLE_TIME_SLOTS` becomes `IDLE_TIME`, `MM_ORACLE_MIN_SLOT_GAP` becomes `MM_ORACLE_MIN_WRITE_GAP`, and `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` becomes `MM_ORACLE_MAX_SOURCE_AGE`. The oracle, liquidation and idle math takes an optional `SlotDurationMs`. Admin CLI `exchange set-slot-duration-ms`. Superseded in part by `slot-duration-sync` |
| slot-duration-sync | Audit follow-up to `slot-duration-scaling`. New `State.slot_duration_transition_slots: [u64; 4]`, the permissionless `sync_state_slot_duration` replacing the warm-admin setter, and auction durations redefined as wall-clock 400ms units. [Details](#slot-duration-sync) |
| spot-bankruptcy-revenue-pool | See the `#238 spot-bankruptcy-revenue-pool` row above |
| spot-oracle-twap-ts-init | Fix a Medium audit finding (OtterSec #121): a freshly initialized spot market carried `last_oracle_price_twap_ts == 0`, which collapsed both `StrictOraclePrice` bounds on its first refresh. [Details](#spot-oracle-twap-ts-init) |
| stale-curve-fill-routing | See the `#317 stale-curve-fill-routing` row above |
| swap-twap-write-after-check | Restore the oracle-TWAP refresh that the #110 and #111 fix dropped from the two split begin and end swap lanes, on the far side of their own gates. `begin_swap` and `liquidate_spot_with_swap_begin` pass `None` so neither can refresh the anchor its band check reads, which closed the finding but left both lanes contributing nothing to the oracle EMA, so a swap-heavy market depended on other paths and on the permissionless crank to keep its TWAP fresh. `end_swap` now advances both markets' oracle TWAPs through `update_spot_market_twap_stats` after `validate_price_bands_for_swap`, and `liquidate_spot_with_swap_end` does the same after every check in its lane. The check still reads the pre-swap value, and `begin_swap`'s instruction introspection forbids any Velocity instruction after the end instruction, so nothing else in the transaction can read the new value. `begin_swap` leaves `last_oracle_price_twap_ts` alone, so the deferred update still weights the full elapsed interval, and the deposit, borrow and utilization TWAPs were already advanced in the begin instruction and are a no-op in the end instruction. This grants no capability a caller did not already have, since `update_spot_market_cumulative_interest` is permissionless and advances the same TWAPs. Program-internal ordering only, with no account-layout, IDL, error-code or SDK change |
| swift-resting-limit-placement | `place_signed_msg_taker_order` accepts a resting limit, meaning a limit order with no auction, ahead of its message slot. For such an order the message slot is the placement deadline rather than an auction start, so `max_slot = order_slot + 0`, and with the #470 gate rejecting `order_slot > clock.slot` the order was placeable in exactly one slot and never landed. Clients stamp a no-auction limit its whole signing budget, about 14s, ahead (`@velocity-exchange/common` `MINIMUM_SWIFT_NON_AUCTION_ORDER_SIGNING_BUDGET_MS`), which under Drift was placed before the stamp arrived. The future-slot rejection now applies only to orders with an auction. A resting limit stamped ahead is accepted while the lead is within 30s (`max_resting_limit_lead`, and the UI stamps about 14s), and still rejected with `InvalidSignedMsgOrderParam` (6288) beyond it. The stored order slot is unchanged at `min(clock.slot, message slot)`, and the `max_slot < clock.slot` no-op still applies after the stamp. keep-rs places a resting limit on arrival instead of deferring it, since its 10s deferral bound dropped the roughly 14s stamp outright. The TS filler mirrors the gate via the new SDK `signedMsgOrderPlaceable` (§4.6) but still ignores no-auction signed-msg orders in `dlobBuilder`, so keep-rs remains the placer of resting swift limits. Rollout: deploy the program upgrade before the keep-rs release, because keep-rs against the old program sends a place tx per resting swift limit that fails with 6288, which is the same net outcome as dropping it plus the fee. Side effect: a resting limit's `SignedMsgOrderId.max_slot` is now its future stamp, so the entry occupies the per-user id ring for the lead plus the eviction buffer, about 18s for the UI's stamp and up to about 34s at the bound, instead of about 4s. A burst of resting swift limits can therefore reach `SignedMsgUserOrdersAccountFull` sooner. No account-layout, IDL or error-code change |
| tokenized-pooled-basis-gate | Fix a High audit finding (OtterSec #140) in the `vaults` program, where a newcomer tokenizing into an under-water tokenized depositor captured part of the existing holders' loss shelter. [Details](#tokenized-pooled-basis-gate) |
| tokenized-rebase-backing | Fix a Medium audit finding (OtterSec #122) in the `vaults` program, where the signerless `apply_rebase_tokenized_depositor` could floor a tokenized depositor's backing shares to zero while the mint supply was live. [Details](#tokenized-rebase-backing) |
| vault-nav-interest-refresh | Fix two High audit findings on vault NAV pricing (OtterSec #136, #137), where a vault priced shares off a stale spot-market interest index. Changed 19 vault instruction account lists (ABI). [Details](#vault-nav-interest-refresh) |
| vault-nav-spot-market-refresh | Follow-up to `vault-nav-interest-refresh`, which fixed OtterSec #136 and #137 for one market only. Changed 20 vault instruction account lists (ABI) and added `refresh_spot_market_interest`. [Details](#vault-nav-spot-market-refresh) |
| vault-share-pricing-hardening | Fix four High audit findings on vault share pricing (OtterSec #91 through #94). Adds `UserStatus::VaultOwned` and the CPI-only velocity instruction `update_user_vault_owned`. [Details](#vault-share-pricing-hardening) |
| vault-token-transfer-basis | Fix a Medium audit finding (OtterSec #138) in the `vaults` program, where a token-denominated share transfer moved cost basis by the caller's raw request rather than by the value of the shares actually transferred. `WithdrawUnit::get_withdraw_value_and_shares` returns `withdraw_value = withdraw_amount` verbatim for `WithdrawUnit::Token` while flooring `n_shares` out of it, so `transfer_shares` credited the recipient with more `net_deposits` than their new shares were worth, sheltering that much future profit from the manager and protocol performance fees, and symmetrically over-debited the sender. `transfer_shares` now re-derives the moved value from the floored `n_shares`, as `depositor_shares_to_vault_amount(..).min(vault_equity)`, which is exactly what the `Shares` and `SharesPercent` units already did, so all three units agree. The `ShareTransferRecord.value` field reports the same actual figure. Program-internal only, with no account-layout, IDL, error-code or SDK-API change |
| vaults-fee-policy-grandfathering | Follow-up to `vaults-fee-rebase-hardening`, replacing its #98 fix. That fix stamped `last_fee_update_ts` to the activation instant, which forfeited the manager's pre-activation accrual and left profit share and the hurdle rate untouched, since both are priced off a depositor's high-water mark rather than a clock so no timestamp can slice them. Management fee: `apply_fee` accrues the closing interval at the policy in force while it accrued, stamps `last_fee_update_ts`, and only then installs a matured update. `try_update_vault_fees` rejects an install on an unsettled vault, making `apply_fee` the single installer. The window between maturity and the first vault interaction is charged at the old rate. `manager_update_fees` therefore settles through `apply_fee` instead of writing the policy directly, and gains a `velocity_user` account plus the spot market and its oracle in `remaining_accounts`, which SDK `getManagerUpdateFeesIx` passes, while protocol vaults still append `VaultProtocol`. Profit share and hurdle: `VaultDepositor` and `TokenizedVaultDepositor` gain `profit_share_at_basis` and `hurdle_rate_at_basis`, recording the policy in force when the high-water mark was last set. Gain above that mark is priced at `min(vault.profit_share, profit_share_at_basis)` and sheltered by `max(vault.hurdle_rate, hurdle_rate_at_basis)`, so a raised profit share or a lowered hurdle never prices gain earned before it, while a policy better for the depositor applies at once. A realization that leaves no unpriced gain advances the stamps to the live policy, so the manager moves depositors onto a new policy with `apply_profit_share`, which realizes their gain at the old policy first. Consequence: gain that stays unpriced, being sheltered by the hurdle, keeps its old policy until the depositor clears the old hurdle once. Also removes a dead `VaultDepositor::calculate_profit_share_and_update` that shadowed the trait implementation with gross-profit semantics. Account layout: both depositor accounts repurpose trailing padding for the two new fields and keep their existing size, and the IDL adds those fields plus `manager_update_fees`' `velocity_user` account. No error-code change, since it reuses `InvalidVaultUpdate` |
| vaults-fee-rebase-hardening | Fix eleven Medium audit findings in the `vaults` program, covering fee, rebase and share accounting. [Details](#vaults-fee-rebase-hardening) |
| withdraw-breaker-exception-budget | Fix an audit finding (OtterSec #150) on the spot withdraw circuit breaker, where a per-account exception overrode a market-level limit with no aggregate accounting. Also closes two gaps on the isolated-position withdraw path. [Details](#withdraw-breaker-exception-budget) |


### 6.2 Extended notes

Longer entries from the table above, in alphabetical order by label.

#### bankruptcy-claim-freeze

Completes the #245 fix for the permissionless-sweep front-run of a bankruptcy resolution.
#245 held back `bankruptcy_if_floor_pct` of open-interest notional, which is a proxy for the
loss rather than the loss itself. It can be smaller than the debt. It is zero when the market
has no open interest, which is the normal state of a bankruptcy, since the estate's positions
are closed before its debt resolves. And it was inert on every market created before the
field existed.

Three changes.

The latch books the debt. The new per-market counter `PerpMarket.pending_bankruptcy_claims:
u16` takes 2 of the 6 alignment-padding bytes before `last_fill_price`, leaving the size and
all other offsets unchanged, and existing accounts read 0. It is incremented when a
liquidation latches a user bankrupt while that user holds a negative quote debt in that
market, and released when the debt is discharged. While it is above zero, `sweep_market_fees`
withholds the whole `fee_ledger.pending_if_fee`, so the tranche covers the loss regardless of
open interest or configuration. The booking is marked on the position by the new
`PositionFlag::BankruptcyClaim` bit, so a repeated latch counts one debt once, and both
writers of `PerpPosition.quote_asset_amount` release it, namely `update_quote_asset_amount`
for the resolver, setoff and settle paths, and `update_position_and_market` for fills. The
freeze can therefore never outlive the debt that justified it. A cross-margin estate can hold
a debt in a market the latching instruction did not declare writable, which cannot be booked.
The standing floor covers that market, and the same-instruction admit-and-resolve case.

The floor is on by default. `bankruptcy_if_floor_pct == 0` now means
`DEFAULT_BANKRUPTCY_IF_FLOOR_PCT` (10 bps) rather than disabled, so a market written before
the field existed carries the standing tranche with no admin call. The new sentinel
`BANKRUPTCY_IF_FLOOR_DISABLED` (`u32::MAX`), accepted by
`update_perp_market_bankruptcy_if_floor_pct`, turns it off, and turning it off does not
expose a latched bankruptcy.

Delisting waits on the counter. `settle_expired_market_pools_to_revenue_pool` runs the
market's final sweep with the floor bypassed, and its wind-down checks sum the quote across
the market, so a bankrupt's settled debt nets against another user's unsettled claim and
passes them. It now rejects while `pending_bankruptcy_claims` is above zero.
`resolve_perp_bankruptcy` and `settle_pnl` both take the counter to zero without the admin.

SDK: `PerpMarketAccount.pendingBankruptcyClaims`, `PositionFlag.BankruptcyClaim`,
`DEFAULT_BANKRUPTCY_IF_FLOOR_PCT`, `BANKRUPTCY_IF_FLOOR_DISABLED`. Admin CLI:
`perp-market set-bankruptcy-if-floor <market> disabled`. No instruction-signature or
error-code change.

#### bankruptcy-estate-setoff

Fixes a High audit finding (OtterSec #130). Assets arriving after the bankruptcy latch
escaped setoff, so insurance and depositors covered a debt the estate could have paid itself.

The latch is a snapshot of "nothing left to seize", and nothing re-evaluates it before the
resolvers run, while credits can still land. The revenue-share sweep is permissionless, since
it hangs off `settle_pnl`, whose `authority` carries no `can_sign_for_user` constraint, and
keeper filler rewards (`pay_keeper_flat_reward_for_perps` and `_for_spot`) credit the filler
with no bankruptcy check at all. Once latched, every route that could apply such an asset to
the debt is shut, because `settle_pnl` and `liquidate_spot` both reject a bankrupt user and
the resolvers read only the liability row. The credit therefore sat untouchable while the
whole debt was socialized, then became withdrawable the instant the resolver cleared the
latch, which is a `min(credit, debt)` transfer from IF stakers and socialized-loss bearers to
the beneficiary.

The fix has two legs.

Primary. `resolve_perp_bankruptcy` performs the `settle_pnl` move the bankrupt user is barred
from making, before it reads `loss`. Tokens move from the quote deposit into the market's
`pnl_pool` and the perp debt shrinks by the same amount, so every tranche sees the net debt
and the insurance draw falls one for one. Those tokens land exactly where tranche 2 would
have deposited insurance money, and the move is token-neutral in the quote market so the
handler's vault-amount assertion still holds. It is bounded at `min(deposit, |debt|)`.

Fallback. For a credit in a non-quote deposit, which cannot be netted against a quote debt
without a cross-asset swap, both resolvers re-check for realizable assets via the new
`math::bankruptcy::has_realizable_spot_assets_for_setoff`. If any remain, they clear the latch
and return without drawing, handing the account back to ordinary liquidation to seize it and
re-latch for the genuine residual. Committing the un-latch rather than erroring is essential,
because erroring would leave the status bit set and `liquidate_spot` rejects a latched user,
so both paths would be stuck with no way forward.

That new predicate is deliberately far narrower than `is_cross_margin_bankrupt`, which also
vetoes on open orders and base exposure, conditions that mean "not yet resolvable" and that
the resolvers are legitimately reached with. It is token-measured for the same reason as
#151. It is scoped to spot deposits, because a positive perp `quote_asset_amount` in another
market is #145's subject, where an unfundable claim must be extinguished into the market's
insurance tranche rather than merely deferred.

A setoff that covers this market's debt outright returns `Ok(0)` rather than tripping the
negative-pnl assertion, and since `has_pending_cross_margin_perp_bankruptcy` no longer reports
the market, the spot resolver is unblocked (#52). Both new early returns precede any draw, so
neither can reorder insurance spending between perp and spot stakeholders.

Program-internal only, with no account-layout, IDL, error-code or SDK-API change.

#### bankruptcy-recover-then-forfeit

Closes three gaps left by `bankruptcy-unfundable-claim-setoff` and `bankruptcy-estate-setoff`
(OtterSec #130 and #145).

(1) A funded PnL pool still vetoed admission. `is_cross_margin_bankrupt` vetoed on a positive
perp claim while the claim's market held any pool at all, so a bad-debt repair stalled until
a keeper drained that pool through the ordinary pipeline. Ordinary trading fees flow into the
pool, so any market participant could re-arm the veto for the price of a trade. Admission is
now silent about claims apart from the net-solvency gate, and `is_cross_margin_bankrupt` and
`LiquidatePerpMode::should_user_enter_bankruptcy` drop the `&PerpMarketMap` parameter that
existed only to serve that veto.

What makes the veto safe to delete is that both resolvers now run
`recover_perp_claims_from_pnl_pools` first. For every settled positive claim they move
`min(claim, pnl_pool_tokens, debt_still_uncovered)` out of the market's `pnl_pool` into the
estate's quote deposit and debit the claim, which is the `settle_pnl` a latched estate is
barred from making. Pool excess is deliberately not the yardstick, because
`update_pool_balances` pays a positive claim out of the pool's raw balance, and `settle_pnl`'s
`user_must_settle_themself` gate lets any keeper make that call for a user who is being
liquidated, so excess governs only who may settle for a healthy user. Without the recovery
pass, deleting the veto would let insurance cover a debt the estate could have paid itself,
which is #130's finding in a new place.

`resolve_perp_bankruptcy` then sets the recovered deposit off against its market's debt, and
`resolve_spot_bankruptcy` un-latches so ordinary liquidation seizes it, since a non-quote
borrow cannot absorb quote tokens directly. Because the recovery is capped at the debt, a
claim big enough to cover it makes a deposit equal to the debt, which is the designed
outcome, and the setoff zeroes both. `resolve_perp_bankruptcy` therefore re-derives admission
on that path too, and un-latches an estate that no longer owes anything rather than leaving a
latch nothing can clear. Recovery is bounded by the debt, so it never drains a pool the
estate has no further claim against.

(2) A stale latch plus later perp credits confiscated a solvent estate. The resolvers
re-derive admission only when the latch is not already set, and keeper filler rewards
(`pay_keeper_flat_reward_for_perps`) credit a perp `quote_asset_amount` with no bankruptcy
check on the filler. An account latched against a small debt could therefore accrue large
unpayable claims and lose all of them, because the `net <= 0` gate that bounded the forfeit is
an admission-time fact the account never revisits. `extinguish_unfundable_perp_claims` now
takes a `max_forfeit` cap, which is the loss the same call is about to cover with other
people's money, meaning `|loss|` in the perp resolver and the borrow's quote value in the spot
resolver, and the running total is bounded by it. The estate can never give up more than the
payout it is funding, and the bound holds for credit routes that do not exist yet rather than
enumerating the ones that do. The forfeit consequently moved below the `loss` read, so
resolving a market carrying no deficit now fails the pre-existing negative-pnl assertion
instead of forfeiting a claim on a call that covers nothing.

(3) The stale-latch re-check un-latched isolated bankruptcies forever.
`has_realizable_spot_assets_for_setoff` reads the cross spot rows and was called for every
mode, including `IsolatedMarginLiquidatePerpMode`, whose own admission predicate ignores the
cross book by design. Any account holding a quote deposit beside a bankrupt isolated position
therefore admitted the bankruptcy and cleared it again on every call, and the isolated debt
never resolved. This was latent on mainnet, where the `isolated-position` instructions are
feature-gated off. The check is now the `LiquidatePerpMode::has_realizable_assets` trait
method. Cross mode answers with `has_realizable_spot_assets_for_setoff`, and isolated mode
with the new `has_realizable_isolated_assets`, its own collateral row.

Account-set change: `resolve_spot_bankruptcy` now requires the quote spot market writable. It
was already a required read-only account, since the claim passes read it, but a recovered
claim lands in the estate's quote deposit.

SDK: `math::bankruptcy::isUserBankrupt` drops the pool veto to match, and
`getResolveSpotBankruptcyIx` passes `QUOTE_SPOT_MARKET_INDEX` in `writableSpotMarketIndexes`.
Previously it never added index 0 at all, so the account arrived only when the user or
liquidator happened to hold a live quote row. No account-layout, IDL or error-code change.

#### bankruptcy-unfundable-claim-setoff

Fixes a High audit finding (OtterSec #145). A positive perp `quote_asset_amount` vetoed
bankruptcy admission unconditionally, so a claim on a market whose PnL pool could not pay it
stranded a real, resolvable loss in another market with no way out. The pool only fills as
counterparty losses settle, which may never happen, and until then the claim can never be
settled into a deposit to clear the veto.

`math::bankruptcy::is_cross_margin_bankrupt` now takes `&PerpMarketMap`, threaded through
`LiquidatePerpMode::should_user_enter_bankruptcy` and both implementors, with the isolated
mode ignoring it. A positive claim vetoes only while both of the following hold:

(a) the market's PnL pool can still pay some of it, because that portion belongs in the
ordinary settle, deposit and `liquidate_perp_pnl_for_deposit` pipeline, which needs no
insurance; and

(b) the estate is net solvent across its non-isolated perp positions.

Condition (b) does real work. Unpayability alone would admit an account holding a large
unfundable claim against a small debt and confiscate the whole claim to cover a fraction of
it. It is also what made an earlier payability-only attempt unsound against a user with $1050
of genuine positive PnL against a pool that was empty only because counterparty losses had
not settled. The netting is exact rather than an approximation here, because every position
reaching that check has `base_asset_amount == 0`, so its entire value is its
`quote_asset_amount`, needing no oracle and no meaningful CU. `net <= 0` is precisely what
bounds the forfeit below so it never exceeds what the estate owes.

The unfundable claim is not ignored. Ignoring it would treat an unfunded claim as worthless
when it is still owed, and would leave the user holding a live claim after insurance covered
their debt in full. Both resolvers now call `extinguish_unfundable_perp_claims`, which zeroes
the unfundable portion of each claim and adds the same amount to that market's
`pending_if_fee` via the new `FeeLedger::accrue_forfeited_claim_to_if`, moving the creditor
from the bankrupt estate to the insurance fund. That is equity-neutral by construction,
because zeroing the claim lowers `market.quote_asset_amount` and so `net_user_pnl`, raising
the market's excess by exactly what the `pending_if_fee` credit subtracts. It needs no new
state and no inter-market receivable, because `pending_if_fee` is already defined as a claim
on future PnL-pool inflows, which is precisely what the user's claim was. It is deliberately
not `accrue_liquidation_fees`, which would also bump `total_liquidation_fee` analytics for a
fee nobody charged. Only the portion the pool genuinely cannot pay is taken, recomputed at
resolution rather than trusting the admission-time view since the pool can move in between,
and positions with base exposure or a live order are never touched. It runs in
`resolve_spot_bankruptcy` too, since an account whose liability is a spot borrow reaches that
resolver directly without the perp-before-spot precedence diverting it.

Known limitation: the IF receives a claim rather than cash, so this does not reduce the draw
for the bankruptcy in progress. It is compensated later, if the pool fills, through the
existing `pending_if_fee`, `sweep_market_fees`, revenue pool and
`settle_revenue_to_insurance_fund` path.

Residual liveness note: while a claim's pool holds even dust, admission still vetoes until
that dust is settled, which for a claim the user must settle themselves depends on their
cooperation. That is strictly narrower than the previous behaviour, where any positive claim
blocked admission unconditionally.

Mirrored in the SDK by `math::bankruptcy::isUserBankrupt`, the keeper's bankruptcy predicate
that the liquidator bot uses to decide when to send a resolver, which applies the same two
value-aware vetoes. Its exported signature is unchanged, but it now reads market state, the
deposit index and PnL pool, as well as the user account, so every spot and perp market
referenced by a nonzero position must be loaded on the client. Keeping that mirror current
matters, because the resolvers self-admit and an under-reporting keeper is the only thing
standing between a newly admissible account and its repair. No account-layout, IDL,
error-code or SDK-API-shape change.

#### bid-ask-twap-quote-rest-age

Fixes one High audit finding (OtterSec #146) in `update_perp_bid_ask_twap`, plus a
defence-in-depth clamp.

The crank estimates the market's bid and ask from caller-supplied `User` accounts, and nothing
in the program checked how long an order had existed. `is_resting_limit_order` admits a
post-only order, or any order with `auction_duration == 0`, in the very slot it was placed. A
caller with the 1000-USDC IF stake could therefore place a self-crossed pair of quotes, crank,
and cancel them in one transaction, moving `last_bid_price_twap`, `last_ask_price_twap` and
`last_mark_price_twap_5min` while never being exposed to a fill. Those TWAPs feed
`get_perp_baseline_start_price_offset`, which sets the auction band for a third party's
triggered stop-loss, and at more than 50 bps divergence from the slow TWAP that offset is
driven by the 5-minute mark TWAP alone. The caller could therefore price a stranger's forced
close.

`find_bids_and_asks_from_users` now takes a `min_resting_slots` parameter and skips orders
younger than that. The crank passes the new `BID_ASK_TWAP_MIN_QUOTE_REST_SLOTS` (24 slots,
about 10s), anchored on the `min_auction_duration = 20` forced onto every triggered stop-loss
auction, so a quote must be exposed at least as long as the auction it would move.
`jit-proxy`'s `arb_perp` passes `0`, because arbitrage must act on the true live book. Age is
derived from `Order::posted_slot_tail` via the new `slots_since_order_posted`, not
`Order::slot`, because signed-message orders back-date `slot`. `posted_slot_tail` is 8 bits,
so age is exact within a 256-slot window and an older quote can only ever be treated as too
fresh, which means conservatively skipped with a fallback to the AMM quote. A fresh quote can
never appear old.

Behavioral change for keeper operators: a maker who cancels and replaces faster than about 10s
no longer contributes to the estimate, and a crank passed only freshly-placed makers gets no
DLOB estimate at all.

Defence in depth. `get_perp_baseline_start_price_offset` was the one unclamped price input on
the forced-close auction path, since its sibling `get_perp_baseline_start_end_price_offset`
clamps only the end buffer rather than the end offset's distance from oracle. Its result is
now clamped symmetrically to plus or minus `last_oracle_price_twap / max_divisor` from the
existing tier-aware `PerpMarket::get_auction_end_min_max_divisors`. That is 2% for tier A, 5%
for B and C, 10% for Speculative, and 20% for HighlySpeculative and Isolated. Both return
paths are clamped, including the low-volume and timestamp-mismatch fallback, which divides a
mark TWAP and so can leave the band when mark sits far from oracle. No new constant was
needed, because the protocol already declares, per contract tier, the widest sensible auction
width relative to price, and an auction that starts further from oracle than that is
nonsense. Clamping moves the start toward oracle, which is less aggressive for the taker so
the auction's worst price is unchanged, and it cannot invert start against end, because the
end offset is derived with a `min` and `max` against the start. Real markets sit far inside
the band, since a BTC-style tier A fixture is under 0.1% of price against a 2% bound, so only
manipulated or badly stale TWAPs bind.

SDK mirror: `math/auction.ts` `getTriggerAuctionStartPrice` applies the same clamp before its
start buffer, via the new exports `getAuctionEndMinMaxDivisors` and
`getPerpBaselineMaxPriceOffset` (§4). No instruction signature, account-layout, IDL or
error-code change, since the `min_resting_slots` parameter is internal and the clamp is
in-handler math.

#### equity-floor-oracle-validity

Fixes three OtterSec findings (#131, #139, #142) where an equity-floor decision was taken off
an oracle price the program had already judged invalid. `calculate_user_equity` prices every
position at the raw live oracle price and returns the validity verdict separately, and six
call sites discarded that verdict.

Metric. A new program-internal `calculate_user_equity_bounds` returned a lower and an upper
bound on net equity plus the verdict. A position with a valid oracle contributed one exact
value to both bounds. A position with an invalid oracle was bounded by the pair of its live
price and `last_oracle_price_twap_5min`, dropping non-positive candidates so the walk could
never error inside a shared path, where `lower` prices unpriceable assets at the min and
unpriceable liabilities at the max and `upper` mirrors it. A position with no positive
candidate at all saturated the bounds to `i128::MIN` and `i128::MAX`, so restrictions applied
and authorizations did not, still without aborting the host instruction.

All thirteen floor gates consumed the bound that fails closed for their direction. The twelve
that restrict the user, meaning risk-increasing placement, taker and maker fills, trigger
cancels, withdrawals, deposit and perp-position transfers out, and the four liquidator
admission checks, took `lower`, so a stale-high price could not buy an action through the
floor. `force_cancel_orders`, where below-floor authorizes a keeper against the user, took
`upper` and additionally required all oracles valid before the floor counted as grounds, so
oracle degradation falls back to the margin arm rather than manufacturing authorization.

A settled market's oracle no longer enters the verdict, in the bounds walk and in
`calculate_user_equity` itself. The position is valued at `expiry_price`, so a dead oracle
contributed nothing to the number while permanently blocking every validity-requiring caller,
meaning the breaker trip, the reset, and cure transfers for any account still holding the
settled position.

Because gates permitted only when `lower >= floor + buffer` and `lower <= upper`, a permitted
action still implied `upper > floor` and was therefore not trippable, so this did not
reintroduce the strict-versus-non-strict split #328 removed. When every oracle is valid,
`lower == upper` and behavior is unchanged.

Hard rejects. `transfer_deposit_by_delegate` rejects with `InvalidOracle` on both floor-delta
checks. Those are the anti-defuse pre-check, where an owner could otherwise shed the floor off
a breached subaccount in the one slot the oracle was bad, dropping it to `equity_floor = 0`
and permanently defusing a trip that `trip_equity_floor_breaker` was simultaneously refusing
to arm for the same reason, and the credited side's backing check. That matches the
cure-transfer check already there. `force_delete_user` rejects with `InvalidOracle` rather
than sizing its dust threshold off a bad price, since it sends the deleted account's
remaining spot deposits to the keeper's own token account.

Also in this change: the isolated-position deposit-transfer gate still compared the margin
numerator (`total_collateral`), which never subtracts borrows, and is converted to net equity,
completing the #328 sweep this document already claimed was complete. And
`force_cancel_orders`' handler loads its maps under the live `State` oracle guard rails, as
about 50 other handlers do, so the same account cannot get a different floor verdict there
than from `withdraw` or the trip.

`calculate_user_equity` keeps its signature, since it is called cross-program from the vaults
program and re-exported through velocity-rs, so the change is additive with no account-layout,
instruction-signature, IDL or error-code change.

SDK mirror: `User.getNetUsdValueBounds(slot)` walked positions under the same rules, with pure
helpers `boundPrices`, `getSpotOracleValidity` and `isOracleValidForMarginCalc` and the
saturation sentinels `I128_MIN` and `I128_MAX`. `isBelowBufferedEquityFloor(slot?)` predicted
the gates on the lower bound when given a slot, and was unchanged otherwise. The equity guard
bot alerts distinctly, once per outage, when a trip is blocked by `InvalidOracle`, instead of
reporting it as a generic sim failure.

The bounds machinery was later removed by `equity-floor-fail-closed`, which replaced it with
exact net equity plus a fail-closed verdict. See §3.1 for the current behavior.

#### expiry-price-conservation

Fixes three Medium audit findings (OtterSec #116, #125, #147) on opposite sides of the same
expiry-settlement conservation equation, where aggregate user claims must fit the value that
backs them. Fixing either side alone still yields a wrong settlement price, so they move
together.

#116: `settle_expired_market` solved the expiry price against `pnl_pool + fee_pool`, but only
`min(total_fee_minus_distributions, fee_pool)` is ever transferred into the PnL pool, and
expired-position settlement pays exclusively out of the PnL pool, since
`update_pnl_pool_and_user_balance` caps there and reverts `InsufficientPerpPnlPool`. Whenever
`tfmd < fee_pool`, the un-moved remainder inflated the price by value no claim could draw on,
so the tail of the winners reverted and the market could never finish winding down.
`total_excess_balance` is now the post-transfer PnL pool only. This is deliberately not fixed
by transferring the whole fee pool instead, because the fee pool can hold more than the AMM's
own accounted equity (`tfmd`), and that excess is protocol and IF fee revenue awaiting the
sweep rather than AMM surplus payable to perp winners. It still routes to the revenue pool via
`settle_expired_market_pools_to_revenue_pool` at delisting.

#125: `calculate_expiry_price` was the only consumer in the program solving against
`quote_asset_amount` alone, omitting `net_unsettled_funding_pnl`. Because
`settle_expired_position` runs `settle_funding_payment` before computing each payout, pending
funding is already inside the quote users are paid on, so aggregate claims exceeded the pools
by exactly the market's unsettled funding. `calculate_expiry_price` now takes
`net_unsettled_funding_pnl` and folds it in via the shared `calculate_net_user_cost_basis`,
the same helper `calculate_net_user_pnl` uses, which structurally prevents the two cost-basis
consumers from diverging again.

Integrator-visible: a delisting market's `expiry_price` is strictly more conservative, meaning
lower for a net-long user base and higher for net-short, whenever the fee pool holds un-moved
value or the market carries unsettled funding. Expired-position settlement correspondingly
pays less per winner but no longer strands the tail.

#147: the same solver treated the gross PnL-pool balance as backing for expired winners
without subtracting `pending_revenue_share`, a booked builder and referrer liability where the
taker's quote is debited at fill and a matching payable recorded, which every ordinary fee
sweep reserves and `calculate_perp_market_amm_summary_stats` already subtracts. The expiry
solver was the one consumer treating it as payable, over-pricing winner claims by the amount
owed. And because `settle_expired_market_pools_to_revenue_pool` validates that base amounts
and net user cost basis are zero but never that the revenue share is paid, winners draining
the pool made the accrued builder fee unrecoverable, since the tokens left for the revenue
pool and the escrow rows stayed outstanding. It is now reserved, saturating so a corrupt
counter floors the backing at zero rather than solving against a negative balance.

`pending_protocol_fee` and `pending_if_fee` are deliberately not reserved. They are the
protocol's own revenue and are junior to user claims by design, since `sweep_market_fees`
reserves `max(net_user_pnl, 0)`, valued at `expiry_price` while the market is in `Settlement`,
ahead of both drains, so a short pool pays winners first and the carveouts absorb the loss.
Reserving them in the solver would invert that ladder and pay protocol revenue ahead of
expiring traders.

No account-layout, IDL, error-code or SDK-API change. The SDK's `calculateNetUserPnl` already
included `netUnsettledFundingPnl` in its cost basis, and there is no SDK mirror of
`calculate_expiry_price`, which only reads `market.expiryPrice` for valuation.

#### fill-stale-margin-bad-debt

Fixes four High audit findings (OtterSec #143 and #144 on oracle validity, #135 and #148 on
unaccrued interest). Two unrelated mechanisms in the same margin checks.

##### Oracle validity (#143, #144)

The perp-fill path's post-fill margin checks recorded spot-oracle validity but nothing
consumed it, so a risk-increasing fill could be admitted on margin derived from a stale spot
oracle.

#143: a `StaleForMargin` positive deposit was still credited at its stale weighted price,
letting phantom collateral open an in-band losing DLOB trade whose counterparty settles a real
profit out of the PnL pool. #144: a `StaleForMargin` spot borrow was still priced at its stale
low value, so an account insolvent at the refreshed price passed and became protocol bad debt.

The two sides are fixed differently, because a deposit and a borrow fail in opposite
directions. For the deposit, both the taker and the maker fill contexts set
`ignore_invalid_deposit_oracles`, so a deposit whose oracle is invalid for margin contributes
zero collateral instead of its stale weighted value. That is the treatment
`meets_withdraw_margin_requirement` and its two siblings already apply, and a fill is the same
value-releasing decision. Dropping the deposit rather than rejecting the fill keeps the honest
test, since an account with enough valid collateral still fills, and an account that needs the
stale deposit fails on `InsufficientCollateral`. For the borrow there is no counterpart,
because dropping it understates the debt, which is the error being closed, so both sides
reject on `all_spot_liability_oracles_valid` instead.

Neither gate is keyed on risk direction. The transfer both findings describe needs two
accounts and works with both seats reducing. One seat closes into the worst in-band price and
leaves bad debt its misvalued collateral was never able to cover, while the other settles the
matching profit out of the PnL pool, so a gate that exempted reducing fills would close
neither seat. `meets_withdraw_margin_requirement` draws the same line and exempts no
direction.

Liquidations are excluded on both sides. The taker block is already
`if !fill_mode.is_liquidation()`, and the maker loop runs for liquidation fills too so its
gate carries the same condition. Otherwise one maker's stale spot oracle, or one maker's
un-cranked borrow market, would block the liquidation of an unrelated account.

The new in-memory `MarginCalculation::all_spot_liability_oracles_valid` is a plain computed
struct field with no on-chain layout implication. It exists because
`all_liability_oracles_valid` is also cleared by an invalid perp oracle, which the fill path
already handles deliberately via `oracle_stale_for_margin`, applying a 100% margin override
for the taker and reject-unless-reducing for the maker. Gating on the broad field would have
replaced that design with a hard reject, without any error to signal it. The new setter folds
into the broad flag too, so every pre-existing consumer keeps its current meaning.

Integrator-visible: a perp fill can now revert with `InvalidOracle` when one of the filling
account's spot borrow oracles is stale for margin, and can revert with `InsufficientCollateral`
where a stale spot deposit oracle previously supplied the collateral that carried it.

##### Unaccrued interest (#135, #148)

Margin values a scaled spot borrow through the market's stored `cumulative_borrow_interest`,
so interest accrued since `last_interest_ts` is omitted and the debt understated. Nothing on
these paths refreshes it, since `handle_withdraw` cranks only the market being withdrawn
(#135) and the perp-fill handler cranks none (#148), and the account's other borrow markets
arrive read-only so they cannot be refreshed in place.

Every value-releasing path now requires recent accrual on any market carrying one of the
account's borrows. Those paths are `handle_withdraw`, the perp fill for the taker and every
maker whichever direction each moves, `handle_transfer_deposit`, `handle_transfer_pools` for
both accounts since the transfer moves debt onto the recipient, `handle_end_swap`, and
`withdraw_from_isolated_perp_position`. The last four reach the same check through
`meets_withdraw_margin_requirement*` and crank only the market they touch, so #135 applies to
them unchanged. New error `SpotMarketInterestStaleForMargin` (6371 / `0x18E3`), appended at
the enum tail.

The quantity held down is the share of the debt the omission hides, not the elapsed time. The
omission is `debt x rate x elapsed / year`, and the rate is per-market configuration with no
upper bound, since `validate_borrow_rate` constrains `max_borrow_rate` only against
`optimal_borrow_rate`. A fixed window would therefore hide an arbitrary share on a high-rate
market. `MAX_SPOT_INTEREST_UNDERSTATEMENT_FOR_MARGIN`, one basis point and far inside the
initial-versus-maintenance margin gap, is turned into a per-market window by
`math::margin::max_spot_interest_staleness_for_margin`, which divides it by the ceiling that
market's curve cannot exceed. That ceiling is the larger of `max_borrow_rate` and
`min_borrow_rate`, since `calculate_borrow_rate` interpolates up to the first and then floors
at the second. Using the ceiling rather than the current rate costs two divisions instead of a
utilization and rate computation per borrow per margin check.
`MAX_SPOT_INTEREST_STALENESS_FOR_MARGIN` (equal to `ONE_HOUR`) caps the result, so a low-rate
market cannot go un-cranked indefinitely on a rate the admin may raise over the same interval.
An un-cranked market, by contrast, drifts arbitrarily far.

Only borrows are gated, because a stale deposit index understates collateral, which errs the
protocol's way. A borrow whose un-booked interest converts to less than one token is also
exempt, measured with `calculate_accumulated_interest` on the market and
`get_interest_token_amount` on the position. That exemption is required for liveness rather
than being a softening. `update_spot_market_cumulative_interest` defers an interval whose
split or configured carveout rounds below one token and leaves `last_interest_ts` where it is,
so on a dust-sized market the clock can sit past the bound however often the crank runs, and a
clock-only gate would make every fill and withdrawal for such an account fail permanently. The
clock is the cheaper test, and the size of the omission is the property the gate is actually
about. This mirrors the perp side's existing `amm.is_fresh_at` precondition.

The approach was chosen, as the owner's call, over projecting the index inside the margin
calculation, which costs CU on every fill's margin loop and needs an SDK mirror, and over
refreshing every position's market, which would require clients to pass them writable and so
break the ABI.

Integrator-visible: a withdrawal, transfer, swap, isolated-position withdrawal, or perp fill
can now revert with `SpotMarketInterestStaleForMargin`. Recovery needs no privileges, since
`update_spot_market_cumulative_interest` is permissionless and can be bundled into the same
transaction. Both SDKs gained helpers that name the markets and build the cranks (§4.6), and
both fillers (`apps/keeper-bots-v2` and `keep-rs`) bundle them ahead of every
`fill_perp_order`. The IDL gains error 6371, with no instruction or account-layout change, and
no SDK mirror of the margin validity flags exists.

#### if-carveout-floor

Fixes an audit finding (OtterSec #127) where the insurance-fund and protocol carveouts on
lending interest could be floored to zero and dropped, plus a related conservation defect
found while verifying it. Part of #340.

#127: `update_spot_market_cumulative_interest` withholds `insurance_fund.if_fee_factor` and
`protocol_fee_factor` cuts from lenders in index terms, but each cut only reaches its pool if
it converts to at least one token, via `deposit_balance * cut / 10^(19 - decimals)`. When a
cut converted to zero, the value was withheld from lenders and credited to nobody, becoming
unattributed slack in the vault, and `last_interest_ts` advanced anyway so the interval could
never be retried. Because the accrual is permissionless, any caller could keep every cut under
one token indefinitely by cranking often enough, permanently forfeiting the IF's and the
protocol's entire share of lending yield.

A configured, non-zero-factor carveout must now convert to at least 1 token before the
interval is committed. Otherwise the whole interval is deferred, leaving the clock unmoved and
adding nothing to either cumulative index, and it is retried later against a longer span. That
is the same defer-rather-than-lose treatment lenders' share already got. Deferral converges
because the cut grows with the un-stamped interval while the clock only moves on commit, so
frequent cranking cannot hold the interval short. `deposit_balance == 0` is exempt, being the
one case where the conversion is structurally zero however long the interval grows. Tradeoff:
interest lands in coarser steps on very small markets, since a $1M market at a 0.1% factor
clears a token in about 16s while a dust-sized market can defer for hours.

Conservation clamp, unreported and pre-existing on master: the deposit side of an interval is
never credited more tokens than the borrow side is charged for it. The two are equal by
construction, since the deposit rate is the borrow rate scaled by
`utilization = borrow_tokens / deposit_tokens`, but utilization is computed from rounded token
amounts and sampled once at the start of the interval. On a long interval at a high rate that
sub-token overstatement is multiplied into whole tokens of deposit credit no borrower paid
for. It is included here because the #127 deferral makes intervals longer. `deposit_interest`
is scaled down proportionally when it exceeds the borrow charge, and this is mirrored in the
SDK's `calculateInterestAccumulated`, whose returned `depositInterest` is clamped the same
way.

No account-layout, IDL, error-code or SDK-API change. The deferral was later replaced by
carried remainders, see [interest-carveout-dust-accumulator](#interest-carveout-dust-accumulator).

#### if-revenue-settle-cap

Fixes two High audit findings on donation-inflatable IF vault pricing. Part of #254.

First finding. `settle_revenue_to_insurance_fund` sized the per-period APR cap from the live
IF-vault token balance, which anyone can inflate with a direct SPL donation, letting a
dominant IF staker spike the vault right before a settle to lift the cap toward the
10%-of-revenue-pool bound and capture accelerated revenue. The cap is now sized off
`min(live_if_vault, if_last_settle_vault_amount)`, where the new
`SpotMarket.if_last_settle_vault_amount: u64`, repurposing 8 bytes of trailing padding with
offsets unchanged, is an accounted balance a donation cannot inflate. It grows with stakes and
settled revenue and shrinks with withdrawals, so a raw SPL donation is not reflected while
legitimate stakes are. Existing accounts read 0 and self-seed on the first post-upgrade add or
settle, and new markets init to 0. It is maintained automatically by every IF-vault movement
with no new instruction, growing on add-stake and settled revenue and shrinking on
remove-stake and on all three loss-draws (`resolve_perp_pnl_deficit`,
`resolve_perp_bankruptcy`, `resolve_spot_bankruptcy`), so it never drifts from the real vault
after a bankruptcy or deficit. Only raw SPL donations are excluded. SDK
`SpotMarketAccount.ifLastSettleVaultAmount: BN` was added. The display-only
`nextRevenuePoolSettleApr` estimate is unchanged, since it disclaims being a program mirror
and steady-state APR is unaffected.

Second finding, same family. The unstake-cancel share forfeiture
(`cancel_request_remove_insurance_fund_stake` and `calculate_if_shares_lost`) valued a
canceling staker's requested shares off the live IF-vault balance, so an attacker holding a
residual IF share could sandwich a victim's signed cancel with an SPL donation, manufacture
appreciation, and burn the victim's pending shares into their own. It is now framed and
documented as withdraw-and-restake at the current active share price. A cancel completes the
withdrawal of the requested shares, paying out the value frozen at request time, and
immediately re-stakes the resulting tokens at the live price. Genuine escrow-window
appreciation is therefore forfeited, which is the anti-free-option rule, while the path stays
immune to donations because the withdraw leg is bounded by the request-time snapshot
`last_withdraw_request_value`, and a raw donation spread pro-rata across all shareholders
cannot manufacture forfeiture an attacker could profitably capture. This path does not read
`if_last_settle_vault_amount`, because the accounted-balance coupling considered for cancel was
dropped in favour of the clearer withdraw-and-restake model. Cancel is the only affected path,
since `remove` already caps its payout at the frozen request-time value and the SDK does not
mirror the forfeiture, and the cancel instruction's accounts and token flows are unchanged so
the vaults-program CPI wrapper is unaffected (§5).

The `0`-means-two-things gap in the snapshot was closed later, see the
`if-revenue-settle-snapshot (#254 follow-up)` row.

#### interest-carveout-dust-accumulator

Replaces the #127 carveout deferral with carried remainders, and fixes the part of #127 that
the deferral hid.

A delayed interval does not keep its own terms. `calculate_accumulated_interest` bills the
whole `now - last_interest_ts` span at the rate that applies when it runs, and commits it with
an index move that credits every balance existing at that moment. Every spot instruction that
changes balances cranks the accrual first, so a delayed interval settles against later
balances. A deposit made in the gap earns interest for time before the deposit. A borrow
opened in the gap pays interest for time before the borrow. That reintroduced findings #115
and #117 through the path added for #127.

An interval that is owed now commits on the interval it belongs to, once it reaches a whole
index unit on both sides. An interval under that floor stays on the clock and is retried on
the next crank, instead of being stamped away. The accrual carries what it cannot pay in whole
units.

A cut passes two divisions. First `deposit_interest * fee_factor / IF_FACTOR_PRECISION` in
index space, then `deposit_balance * cut / 10^(19 - decimals)` into tokens. A short interval
floors the first, and a small market floors the second. Both remainders are carried on the
carveout pools and added back on the next interval. The index-space floor is the one that
affects a realistic market, and #340 did not address it, since at a 0.1% factor a $1M market's
one-second gain rounds to zero in index space whatever the market size.

The two cuts are also taken by splitting twice in order, lenders first and then the insurance
fund against the protocol, so they can never sum past the interval gain. Two independent cuts
could instead each round up, take the whole gain, leave lenders at zero, and stop the interval
from committing at all. The second division's divisor is the combined factor, which
`update_spot_market_if_factor` can lower, so the accrual reduces the carry it reads below the
divisor in force. A lower pair therefore costs less than one index unit and cannot strand the
market.

Layout: `PoolBalance` gains `pending_interest_split_dust: u32` at struct offset 20 and
`pending_interest_dust: u64` at struct offset 24, taken from its 14-byte `padding`, which
shrinks to 2 bytes and keeps its name. The struct size (32) and every other field offset are
unchanged, so the `PerpMarket` and `SpotMarket` layouts are untouched. Existing accounts read
0, which is the correct starting value. Only a spot market's `revenue_pool` and
`protocol_fee_pool` use the new fields. `PoolBalance` in `sdk/src/types.ts` mirrors them, and
`calculateInterestAccumulated` documents that a projection no longer spans a delayed window.
No error-code or SDK-API change.

#### jupiter-swap-api-v2

SDK-only and opt-in. `JupiterClient` gains `apiVersion: 'v1' | 'v2'`, where `'v2'` talks to
Jupiter Swap API v2 (`GET /swap/v2/build`), which answers the quote and its raw instructions
in one request, replacing v1's `/quote` plus `POST /swap` pair and the transaction
deserialization that followed it. `getRouteInstructions` and `getSwapTransaction` then build
locally with no further HTTP call. The on-chain lookup-table fetch is deliberately kept,
because the build lists each table's addresses inline but synthesizing an
`AddressLookupTableAccount` would have to invent the `state` metadata compiling reads, and
would not notice a table deactivated since the build.

Two semantic differences the SDK enforces rather than papers over. First, v2 builds for a
named `taker`, so `getQuote` requires `userPublicKey` and records it as `quotedFor`, and a v2
quote swapped by another wallet is rejected, as Titan's already was. Second, `autoSlippage`
throws under v2, because the API accepts the params, ignores them, and answers
`slippageBps: 0` with `otherAmountThreshold == outAmount`, which is zero slippage tolerance
where any adverse move reverts the swap. `swapMode: 'ExactOut'` throws for the same class of
reason, since `/build` is ExactIn-only and, sent ExactOut, answers `200` with
`swapMode: 'ExactIn'` having spent the amount as the input, inverting a trade that
`getJupiterSwapIx` and `getLpJupiterSwapIx` size off the quote.

The bracket instruction list is selected from the route's own instructions (compute budget,
setup, swap, cleanup) rather than filtered out of the full build, so a tip is excluded by
construction instead of by `filterRouteInstructions` recognizing it as a System transfer. An
address lookup table that cannot be read from chain throws by name instead of being dropped.
`getSwapTransaction` accepts an optional `computeUnitLimit`, because `/build` returns a CU
price but no CU limit where v1's `/swap` supplied one.

The former v1-only `getSwap` (`POST /swap`) is removed, so v1 callers use `getSwapTransaction`,
which posts `/swap` under the hood, or `getRouteInstructions`. v2's three unrelated error-body
shapes (`{ error: { issues, name: 'ZodError' } }`, `{ code, message }`, and v1's
`{ error, errorCode }`) are normalized into readable messages, where the first previously
rendered as `[object Object]`. `SwapInfo.feeAmount` and `feeMint` became optional (§4.4), and
`UnifiedSwapClient` forwards `jupiterApiVersion`.

No program, account-layout, IDL or error-code change. Every observed v2 route executes through
the Jupiter v6 program (`JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4`), which is already
whitelisted, and the setup, cleanup and tip instructions it wraps around the route target the
ATA, Token, System and ComputeBudget programs the shared `filterRouteInstructions` already
strips. The default stays `'v1'` in this change. See §4.6.

#### jupiter-swap-api-v2-rust

velocity-rs only, and breaking. The Rust SDK's Jupiter path moves to Swap API v2 outright, a
hard cut with no v1 mode left and no version toggle.

`jupiter_swap_query` drops its `swap_mode`, `transaction_config` and `only_direct_routes`
params and gains `max_accounts: Option<usize>`. `GET /swap/v2/build` is ExactIn-only, since it
removed `swapMode` from its contract and, sent ExactOut, answers `200` with the amount spent
as the input. It has no analogue for v1's `POST` body config, and it removed
`onlyDirectRoutes`, answering `200` for a parameter it does not recognize, so a v1-style
`onlyDirectRoutes=true` would have been inert without any error and would have let multi-hop
routes into a transaction that also carries the swap bracket. `max_accounts` is the remaining
lever and is always sent, defaulting to 50 to match the TS SDK's `DEFAULT_SWAP_MAX_ACCOUNTS`
rather than Jupiter's own 64, which assumes the swap has the transaction to itself.

The `jupiter-swap-api-client` git dependency is dropped. v2 is a single `GET`, so the request
and its response types live in `jupiter::` directly, and velocity-rs gains a `reqwest`
dependency that was previously only a dev-dependency. `SwapMode` is therefore no longer
re-exported from `jupiter`, so use `titan::SwapMode` since Titan still supports ExactOut.
`JupiterSwapInfo.quote` and `.ixs` change type from the crate's `QuoteResponse` and
`SwapInstructionsResponse` to `JupiterQuote` and `JupiterRouteInstructions`, keeping the field
names the builders read (`quote.in_amount`, `quote.out_amount`, `ixs.swap_instruction`, and so
on).

Lookup tables are no longer fetched from chain. v2 lists each table's addresses inline, which
is all a Rust `AddressLookupTableAccount` holds, unlike web3.js's which also carries `state`
metadata, which is why the TS path keeps its fetch. The tables are built from the response in
a deterministic order. A table deactivated between build and send now surfaces at simulation
rather than at the fetch, replacing a `.expect("deser LUT")` that panicked the caller when a
table account came back missing. `TransactionBuilder::build_jupiter_swap_ixs` no longer panics
on a Jito tip.

What goes in the bracket is the swap instruction plus a cleanup instruction filtered to
non-token programs, which is the SOL unwrap. Jupiter's compute-budget instructions are unused,
since the caller budgets the whole transaction, and its `setupInstructions` are read only as a
signal that token accounts are needed, with the SDK emitting its own idempotent ATA creations
before `begin_swap`. A build carrying `otherInstructions` or a `tipInstruction` is rejected at
parse time instead of dropped, because neither can go in the bracket (`end_swap` rejects any
instruction it does not recognize with `InvalidSwap`), so forwarding a route that needs one is
impossible and discarding it silently would build a transaction missing a step. Neither should
ever be populated, since the SDK opts into no Jupiter feature that produces one.
Correspondingly `JupiterRouteInstructions` has no `other_instructions` or `tip_instruction`
fields.

Because a `200` is not by itself evidence that the route answers the request, every build is
checked against it before being returned. The checks are matching input and output mints,
`in_amount` equal to the requested amount since it is what `begin_swap` releases from the
vault, the slippage it was priced at, `swapMode == ExactIn`, and a `swapInstruction` executing
on the Jupiter v6 program. A mismatch is an `SdkError` at quote time, which callers can fall
back from, rather than an `InvalidSwap` revert or a correctly-executed wrong swap on-chain.

v2's three error-body shapes (`{ error: { issues, name: 'ZodError' } }`, `{ code, message }`,
and v1's `{ error, errorCode }`) are normalized into one readable message, and checked before
the status because v2 also reports failures on a `200`. A body that is neither an error nor a
build, an HTML gateway page for example, is reported with its truncated content.

keep-rs's spot liquidator, the only internal caller, was ExactIn already. It drops the
`Some(true)` it passed for `only_direct_routes` and takes the default account cap. No program,
account-layout, IDL or error-code change, since v2 routes execute through the same
already-whitelisted Jupiter v6 program.

#### mm-oracle-freshness-fixes

Five fixes to the MM-oracle path. All behavioral, with no account-layout, IDL or error-code
change, since the errors it uses were added in mm-oracle-batch-native.

(1) Unsatisfiable immediate-fill gate. `PerpMarket.oracle_slot_delay_override` bounds the
oracle delay tolerated by immediate JIT and auction-skipping AMM fills. A negative value means
unset, and `oracle_validity` clamped it to `max(override, 0)`, which is a threshold of zero
requiring the price to have been written in the same slot as the fill. For an
MM-oracle-sourced price that is unsatisfiable by construction, because the program refuses MM
oracle writes closer together than `MM_ORACLE_MIN_SLOT_GAP` (2) slots, so the price is at best
zero slots old on alternating slots and staler on the rest. A market left at the init default
of `-1` therefore failed `VelocityAction::FillOrderAmmImmediate` on roughly half of all slots
no matter how aggressively it was cranked, surfacing as
`PlaceAndTakeOrderSuccessConditionFailed` and the log `AMM cannot fill order: oracle not valid
for immediate fills`. The field's own doc comment claimed `-1` meant "use state default",
which it never did. Unset now resolves by price source. It is `MM_ORACLE_MIN_SLOT_GAP` when
the safe price is MM-oracle-sourced, which is the tightest window a crank can satisfy, and the
strict zero threshold when it fell back to the exchange oracle, which can be same-slot fresh.
The widening therefore does not extend to the fallback path, which engages exactly when the MM
oracle is stale or diverged and latency arbitrage pays most. `0` is unchanged as the explicit
"no immediate AMM fills" sentinel, and any explicit positive threshold is unchanged in both
directions. Only markets that never had an override set are affected.

(2) Step-cap freeze. `update_mm_oracle_native` rejected any write more than 1%
(`MM_ORACLE_MAX_STEP_PCT_PRECISION`) from the last accepted price. Because the rejection left
the stored price untouched, a gap larger than the cap was unrecoverable, since the next write
was still beyond the cap against the same stale value, and so was every one after it. The
oracle froze at its pre-gap price until an admin called `zero_mm_oracle_fields`, while the
read path sat in permanent exchange-oracle fallback. Out-of-range steps are now clamped to the
cap instead of rejected, which enforces the identical invariant of at most one cap-width of
movement per accepted write, already rate-limited to one write per `MM_ORACLE_MIN_SLOT_GAP`
slots. It grants no new capability, since a caller could already move the price at the cap
rate by sending cap-sized steps. The clamp is floored at one price unit so markets priced
below the cap's integer resolution still make progress. The batch instruction
(`update_mm_oracle_batch_native`, opcode 2) adopts the same clamp. A beyond-cap entry
previously skipped that market, and is now written at the cap and no longer appears in the
reject mask, keeping the two handlers' gating identical, pinned by
`native_batch_tests::batch_matches_single_market_handler`. Integrators simulating MM-oracle
writes must expect a large step to land at the cap rather than be dropped, and it consumes the
sequence id.

(3) Panic paths. The native handler runs before Anchor, so malformed instruction data reaches
it verbatim. A short account list, a short payload, short state or clock data, or an aliased
`borrow_mut` panicked rather than returning an error, aborting the transaction with no
identifiable code. All are now bounds-checked and typed. A short account list or payload
returns `InvalidNativeInstructionData` (6369), short state data returns
`InvalidNativeStateAccount`, and the feature-bit kill switch returns `MmOracleUpdateDisabled`
(6370) instead of `assert!`-panicking, so a disabled MM oracle is distinguishable from any
other abort rather than surfacing as `Program failed to complete`.

(4) Non-positive prices are a hard error on opcode 0. The handler rejected only an exact-zero
price. With the step cap clamping instead of rejecting, a negative target was clamped against
the stored price and written, for example -1 against a stored 1,000,000 landing as 990,000 and
consuming the sequence id, and repeated negatives could walk the price to zero, resetting the
bootstrap path and with it the step cap. Any `price <= 0` now returns `Err`, and the batch
handler already skipped non-positive entries. The SDK builders validate price, sequence-id and
source-slot widths before serialization, since `BN`'s little-endian encoding drops a sign
without reporting it.

(5) Source-slot freshness bound. Both native payloads gain a trailing `u64 source_slot_le`,
which is 24 bytes after the prefix on opcode 0 and 26-byte entries on opcode 2. It is the slot
the crank observed the price at. The stored `mm_oracle_slot` is the landing slot, so without
this a signed update landing late, and a recent blockhash allows about 150 slots, made an old
observation read as slot-fresh downstream. That is the same class of defect as the Pyth Lazer
max-age fix (OtterSec #50). An update whose source slot is more than
`MM_ORACLE_MAX_SOURCE_AGE_SLOTS` (2) from the landing slot in either direction is skipped,
softly like the other rate limits. Behind means it landed too late to be fresh, and ahead means
a wrong-unit source value that must not disable the gate quietly. At the bound a crank may
still estimate its landing slot. The constant is pinned at or below `MM_ORACLE_MIN_SLOT_GAP`
by a `const_assert!`, because the landing-slot stamp makes `oracle_delay` understate true
observation age by up to this bound, and at 2 the immediate-fill gate's effective window is at
most twice the gap. The slot is checked rather than stored. SDK: `updateMmOracleNative` and
`getUpdateMmOracleNativeIx` take a required `oracleSourceSlot` param and `MmOracleBatchUpdate`
gains the field, which is breaking (§4.6), and `MM_ORACLE_MAX_SOURCE_AGE_SLOTS` is exported.

Review follow-ups in the same PR. Both native MM-oracle handlers read the slot via the Clock
sysvar syscall instead of a passed clock account, so opcode 0 accounts are now
`[market, signer, state]` and batch accounts are `[signer, state, markets..]`, which is one
account fewer per transaction and removes the forged-clock possibility. The per-market gating
is extracted into the shared `apply_mm_oracle_update` core. Batch entries altered by the step
cap are reported in a separate clamped bitmask. And opcode 1
(`update_amm_spread_adjustment_native`) received the same panic hardening as opcode 0, meaning
bounds-checked accounts, payload and state reads, `try_borrow`, and typed errors.

#### pre-refresh-twap-band

Fixes five Medium audit findings (OtterSec #109 through #112, and #134) that share one shape.
An instruction advanced an oracle TWAP and then evaluated its own gate against that just-moved
value, so the operation normalized away the check meant to stop it. This generalizes the #81
withdraw fix to the remaining four sites.

#109: `handle_update_funding_rate` called the composed `PerpMarket::update_oracle_derived_stats`
before `update_funding_rate`, whose gate (`oracle::block_operation` then `get_oracle_status`)
reads `last_oracle_price_twap` for the too-volatile ratio and `last_oracle_price_twap_5min` for
the mark divergence. The refresh dragged both toward the live price, so a `TooVolatile` oracle
cleared its own gate and mutated cumulative funding. `update_oracle_derived_stats` is now split
into a private `refresh_oracle_twaps` and the new public `refresh_amm_quote_state`, which
updates the AMM quote-state cache and the `last_oracle_valid` stamp with no TWAP writes, and
the funding crank calls only the latter. Nothing is lost, because `update_funding_rate`
advances the TWAPs itself on the path where funding actually updates, and the handler reverts
with `FundingWasNotUpdated` on every path where it does not.

#112: `fill_perp_order` captured `oracle_twap_5min` after its `update_oracle_derived_stats`
call and fed it to both `is_oracle_too_divergent_with_twap_5min` and
`validate_fill_price_within_price_bands`. The capture moved before the refresh, and the
refresh itself stays, because a fill legitimately advances the TWAPs and does not gate on
them.

#110 and #111: `begin_swap` and `liquidate_spot_with_swap_begin` both passed
`Some(oracle_price_data)` to `update_spot_market_cumulative_interest`, refreshing
`last_oracle_price_twap_5min` in the same transaction as the `validate_price_bands_for_swap`
or `is_oracle_too_divergent_with_twap_5min` check that reads it. Both now pass `None`, which
still accrues interest and advances the deposit, borrow and utilization TWAPs but leaves the
oracle TWAPs, and `last_oracle_price_twap_ts`, alone. Because begin and end are separate
instructions there is nowhere to hold an in-memory snapshot, so not moving the value is the
fix. #111 also brings the swap-backed liquidation lane in line with the direct `liquidate_spot`
lane, which already ran that check with no pre-refresh.

#134: `transfer_spot_deposit`, the shared core of `transfer_deposit` and
`transfer_deposit_by_delegate`, likewise refreshed the transferred market's oracle TWAP before
calling `meets_withdraw_margin_requirement` on the source. `get_strict_token_value` prices a
liability at `StrictOraclePrice::max()`, so dragging the 5-minute TWAP down toward a
temporarily depressed live price lowers that upper bound, under-values the debt, admits the
transfer, and frees sibling collateral for withdrawal, leaving depositor-socialized debt once
the oracle recovers. It is fixed the same way as #110 and #111, passing `None` for the oracle
data.

Integrator-visible: a funding crank, a perp fill, an `end_swap`, a swap-backed spot
liquidation, or a spot deposit transfer can now revert with `FundingWasNotUpdated` or
`PriceBandsBreached` in cases where the in-instruction refresh previously let it through, and
spot oracle TWAPs stopped advancing on the swap paths. The `swap-twap-write-after-check` change
restores that, after the check. No account-layout, IDL, error-code or SDK-API change, since
the SDK's `isOracleTooDivergent` already measured against the stored pre-refresh TWAP.

#### propamm-order-flow

This is the largest ABI change since the fork. §2, §3 and §5 carry the feature and layout
detail, and §4 the SDK surface.

##### Fills

Every perp fill goes through one router pass. Each source publishes discrete price levels and
the split walks priority tiers ascending (vAMM, then a book, then customs), pro rata within a
tier. AMM JIT is gone along with `PerpFulfillmentMethod`, so off-chain fill prediction must
model the vAMM as a level ladder rather than a curve swap (`splitAcrossQuoters` /
`vammQuoteLevels` mirror the program).

The DLOB is gone. `place_perp_order`, `place_orders`, `place_scale_orders`,
`place_and_take_perp_order` (v0), `place_and_make_perp_order` (v0), `fill_perp_order`,
`fill_legacy_dlob_order`, `revert_fill`, `trigger_order` and `resolve_trigger_order` are
deleted, and a transaction that names one fails as an unknown instruction.
`place_and_take_perp_order_v1` and `place_and_make_perp_order_v1` are the taker and maker
routes, `place_trigger_orders_v1` arms a trigger, and `trigger_market_order_v1` /
`trigger_limit_order_v1` fire one. `User.orders` holds unfired conditionals only: armed
triggers, the SL/TP sidecars a signed-message order writes, and the shadow a triggered
trigger-limit keeps. The cancel and modify endpoints are unchanged, so an order left over from
the old venue is inert but still cancellable and its `open_bids`/`open_asks` unwind correctly;
nothing strands, but each one reserves margin until its owner pulls it. The SDK's off-chain
book goes with the venue: `DLOB`, `DLOBNode`, `NodeList`, `DLOBSubscriber`, `OrderSubscriber`,
`AuctionSubscriber`, `UserMap.getDLOB` and the vAMM ladder generators that fed them are
removed, `L2Level`/`L2OrderBook`/`L3Level`/`L3OrderBook`/`groupL2`/`uncrossL2` move from
`dlob/orderBookLevels` to `orderBookLevels` (same barrel exports), a level's `sources` reports
`'vamm'`/`'clob'`/`'propamm'` only, and `calculateEstimatedPerpEntryPrice` takes an
`L2OrderBook` in place of a `DLOB`. Read a market's book from the dlob-server's `/l2`, `/l3`
and `/topMakers`, a user's own orders from `UserClobOrdersClient`, and what a taker of a given
size would get from `quoteRouter`.

Signed-message orders no longer rest in a slot. `place_signed_msg_taker_order` routes the order
at placement and rests what it cannot fill on the market's book as a taker-origin remainder, so
the highest-volume order type in the protocol gets the activation-slot auction instead of
resting where a landing race decides who fills it. `place_and_make_signed_msg_perp_order` is
deleted. It existed only to match a signed-message order already resting in `User.orders`. The
taker-facing signed message and its broadcast to swift are unchanged; the ABI break is on the
keeper side, which velocity controls. Signed-msg orders carry a `network` tag and an optional
signed route that binds the filler; that route now rides `SignedMsgUserOrders` next to the CLOB
order id it rests under, rather than five spare bytes on `Order`, and is eight bytes wide. A
quoter reports depth it could not reach and the router reserves it, so a worse price cannot
take what a book was standing on. IOC is exempt.

##### The book

A standalone CLOB program holds plain limit orders, reached only through velocity adapters that
gate margin and unwind aggregates; trigger orders migrate onto it and leave a shadow in
`User.orders`, and a fired trigger rests taker-origin. It came to trade, so a cross settles at
the counterparty's price rather than picking it off at its own, and its owner cannot cancel it
until its claim on the depth it crosses lapses, `reservation_grace_slots` after activation; an activation-slot speed bump replaces JIT, and `jit-proxy` is
deleted. The book gained `fill_v0`: velocity reports base it settled against a resting order
and the order shrinks in place, keeping its id and its queue position, so a cross no longer
costs a taker the place in line it waited for. Cancel takes a `force` flag, which is how
liquidation force-cancel reaches an order bound inside its window while an ordinary cancel
cannot. Order identity is stored derivable (`authority`, `sub_account_id`), so a reader reaches
every user-derived account from a node. Velocity never reads the book's arena: what to remove
and what a set of refs still names come back from the book's own read-only `next_removal_v0`
and `orders_v0` (both describing an order in one shape, `OrderViewV0`), and depth comes from
`quote_v0` / `quote_l3_v0`, the same interface every other source answers on, so the CLOB gets
no special reader. The book's rules an order has to satisfy come from `order_rules_v0`, and
where a relay watch on its top of book registers is reported by `set_crank_conditions_v0`.
Velocity therefore knows the CLOB's instruction wire and nothing about its account layout: the
`clob-spec` crate is deleted, and the market's layout is the book's own. The book also hosts
the relay conditions for its own state (an expired order, a side at its eviction threshold, a
crossed book, an order reaching its activation slot) and keeps their wakes current as it places
and removes; `set_crank_conditions_v0` is where velocity registers which of its resolvers
answers each. One resolver, `resolve_clob_crank`, for all of them: relay hands a resolver the
condition that fired, so `resolve_crank_clob_evict`, `resolve_crank_clob_remove_expired` and
`resolve_crank_cross_match` collapse into it and each condition keeps its own payment floor.
Placement, modify, cancel and every taker route therefore take no conditions account:
`place_and_take_perp_order_v1`, `place_and_make_perp_order_v1`, `cancel_orders_v1` and
`place_signed_msg_taker_order` carry no optional `crank_conditions`.

##### Quoters

`QuoterV0` registers an external quoter per (market, program, user) with its CPI surface, its
admin-assigned tier and a reprice watch; entries are born unapproved, the maker keeps a kill
switch, and velocity signs every external quoter CPI as the market's quoter-slab PDA rather
than the vault authority (see the slab paragraphs below). The wire is `quote_v0`, `execute_v0`
and the optional `quote_l3_v0`; responses are validated rather than trusted, and a `Custom`
entry may only move the account its registration consented for.

##### Cranking

Expiry, eviction, crossed books, crossed taker remainders, trigger arming and liquidations land
with nobody submitting them, out of relay conditions with a keeper-payment reservoir. A
condition's wake lives on the account whose state it describes and its resolver is registered
by the program that owns the flow, so the book's four sit on the book and velocity's per-market
and per-user accounts hold the rest. Each crank's payment is derived, not set:
`State.transaction_fee_rails` says what a transaction costs to land and a market's attach
prices every crank from it and the cost units the admin measured, so a two-legged cross and a
book removal are not paid the same. Two cranks then price themselves above that base, because a
flat figure covers a quiet market and nothing more. A keeper paying any priority fee is out of
pocket, and declines exactly when congestion is what stopped the crank. An expiry's offer
climbs linearly with how long it went unclaimed, to 5,000 lamports over five minutes. A
liquidation repays the priority fee its keeper paid. It is read back from the transaction's own
compute-budget instructions, and priced on the lesser of the limit the transaction asked for
and the crank's measured cost units, so a keeper is made whole without profiting either by
inflating its limit or by requesting less than it is repaid for. It is capped at
`State.liquidation_crank_reimbursement_bps` of what the liquidation recovered, converted
through the SOL market named by `State.sol_spot_market_index`. Reimbursing a keeper-chosen cost
is safe because the keeper does not keep it: a priority fee goes to the validator. A
liquidation that fills nothing pays nothing, so an account that cannot be liquidated cannot be
cranked for its reservoir. New `update_liquidation_crank_reimbursement` sets both fields; both
default to zero, which leaves the flat payment. Every reservoir is funded from one account:
`CrankTreasuryV0` (`["crank_treasury"]`) is the protocol's single crank pool, and a market
refills itself from it through the permissionless `refill_crank_reservoir` crank rather than
being topped up by hand. A reservoir mirrors its spendable lamports into
`ClobCrankConditionsV0.spendable_mirror`, because a relay watch reads account data and a
lamport balance is metadata, and a condition on that value wakes the refill; the mirror sits on
the same account as the block, so no second watch is registered. A user's
liquidation-conditions resync is paid from the treasury too, because a stale threshold is a
protocol problem before it is a user's, so an underfunded user account can no longer stop its
own liquidation from firing. Cranks are still paid by the market reservoir they crank rather
than from the treasury directly, because a crank writes whatever pays it and a writable account
has a fixed compute budget per block. What a refill pays is priced from the rails like every
other crank and stored on the market it fills (`CrankPaymentsV0.refill`), because a relay
turner drops any condition advertising less than its own configured floor. The treasury holds
the lamports and both levels a reservoir is held between, counted in cranks so one setting fits
every market: the target is read at refill time, and the watermark is resolved to lamports at
attach and stored on the market because it is the threshold that market's wake condition
carries. `sweep_crank_reservoir` moves lamports back out of a reservoir, so a retired market
does not hold them for good. The self-sync fee a user states for its own liquidation-conditions
resync is capped and drawn at most once per fallback interval, because the treasury pays it and
opting in is permissionless. New `initialize_crank_treasury`, `update_crank_treasury` and
`withdraw_crank_treasury`; funding it needs no instruction. `initialize_user` now requires the
`user_conditions` account rather than accepting `None`, so every user carries relay liquidation
coverage and pays its rent with the account. The moment coverage matters is the moment another
party's transaction gave the user a position, where the user signs nothing and no rent can be
charged to them. `initialize_vault` and `initialize_vault_with_protocol` gain
`velocity_user_conditions` for the same reason. `UserConditionsV0` no longer stores
per-exposure liquidation thresholds (`LiqSlotMetaV0` and its `slots` array are removed, 6,040
bytes rather than 7,864): a precomputed threshold is a second, approximate implementation of
the margin engine, and a `LIQ_LIVENESS_POLL` condition plus a resolver that runs the real
maintenance-margin calculation covers the same ground exactly. Spot-only distress remains a
keeper-bot path, because `liquidate_spot` hands the liquidator the borrow and its collateral
and a protocol keeper has no venue to unwind that inventory. `ClobCrankConditionsV0` gains a
second condition, the mirror field and a seventh crank price, so it is 808 bytes rather than
616, and `CrankCostUnitsV0` / `CrankPaymentsV0` gain a `refill` field.

##### Books off-chain

`quote_router` answers what a taker of a direction and size can get, per source, in fill order,
by simulating the real fill (including the orders behind each ladder and the users a fill has
to carry) into `RouterQuoteBufferV0`, read out of post-simulation state.

##### Client surface

A book order has no `User.orders` slot, so two things replace what `user.getOpenOrders()` used
to answer. Its id is minted from `User.next_order_id`, the same counter an armed trigger draws
from, and carried through the book on every answer that names the order (`client_order_id` on
the placement args, the node, `RemovedOrderV0`, `CompletedOrderV0`, the new
`PartiallyFilledOrderV0`, and every CLOB event), so a client names an order the same way
wherever it rests and holds no map between two id spaces. Its lifecycle is on the records the
order-history pipeline already reads: velocity emits `OrderRecord` when an order starts resting
(placement, modify, a trigger firing, a taker remainder migrating) and `OrderActionRecord` with
`OrderAction::Cancel` when it stops, with two appended `OrderActionExplanation` variants,
`ClobOrderEvicted` and `ClobRemainderCulled`. Both records carry `OrderBitFlag::PlacedOnClob`,
and `OrderBitFlag::IsIsolatedPosition` when the order belongs to an isolated position, so a
reader learns a book order's margin regime from the record that opens it. A fill's
`maker_order_id` is set when the balance change names exactly one order; the order's size
fields stay absent, because velocity holds no per-order state for a book order and a wrong size
is worse than none. `cancel_orders_v1` is the exception (128 removals against a 10 KB log
budget), and its per-order detail rides the book's own `OrdersCancelRecordV0`, now listing
velocity's ids. `cancel_order_v1` carries a read-only `perp_market`, for the cached oracle
price its record is stamped with. Post-only ships as `reject_if_crossed` on
`place_and_make_perp_order_v1`/`modify_order_v1`: it refuses a placement that would rest
crossed. It is not what makes the order a maker. A resting CLOB order settles at its own price
on the maker fee schedule either way, including in a cross, where the protocol `User` is the
taker on both legs. A modify keeps the order's id and loses its queue position. Off-chain, the
book publisher indexes the market account into a per-user resting-order feed (`GET
/userOrders`, websocket channel `user_orders`), republishing a user only when that user's own
rows change; every row carries the `ClobOrderRefV0` a cancel or a modify takes.

##### Nothing live transits `User.orders`

Every v1 live order is ephemeral: swift (`place_signed_msg_taker_order`), `place_and_take_v1`
and `place_and_make_v1` build a stack `Order`, margin-check it, route it, and place only the
restable remainder on the book. No `User.orders` slot is taken and cancelled. `jit-proxy` is
deleted. A fired stop-market fills straight to the book too: new `trigger_market_order_v1`
fires the trigger and fills the now-live market order in one instruction, resting only the
remainder as a taker-origin order, where the removed `trigger_order` left a live
`TriggeredAbove` slot for a later fill crank. Relay drives this without a keeper bot:
`sync_trigger_conditions` routes a stop-market on a book market to the new
`resolve_trigger_market_order_v1` resolver and a trigger-limit to
`resolve_trigger_limit_order_v1`. The relay resolver stages no quoter tail, so
`trigger_market_order_v1` does not fill: it rests the whole fired order taker-origin and the
cross crank fills it across every source. A resolver sees only the book, not the propAMMs, so a
fill it staged would take a worse price. A keeper that read the book may still stage a tail and
fill in the same instruction. `User.orders` now holds only unfired conditionals. A reduce-only
order may rest on the book: the router carries an authoritative per-user `base_cover` cap in
the caps wire (`quoter-spec` `UserCapV0.base_cover`) and a per-order `ReduceOnly` flag on the
CLOB order, and the book clamps every reduce-only fill to the position it may reduce. It fails
closed, so an uncovered reduce-only order never fills. `PerpPosition` gains
`reduce_only_clob_orders`, a `u16` reusing the two padding bytes so the account size is
unchanged, which is the count that tells the router which makers to cap. A reduce-only order
rests at most the position it reduces: a fired reduce-only trigger-limit rests only that much
and is cancelled with `ReduceOnlyOrderIncreasedPosition` when nothing is left to reduce, and
`modify_order_v1` clamps a reduce-only replacement the same way and refuses one with nothing
left to reduce. The quoter wire's `CompletedOrderV0` and `CancelledRemainderV0` carry the
removed order's `L3_ROW_FLAG_REDUCE_ONLY` in a new `flags` byte, so the count also disarms when
a fill consumes or culls a reduce-only order. `CompletedOrderV0.change_index` narrows to `u16`
to keep that entry at 16 bytes. `PartiallyFilledOrderV0.change_index` is a `u16` too, followed
by two padding bytes, so that entry stays 24 bytes. A new `ReduceOnly` bit on the CLOB's own `OrderBitFlag` (the
node flag, not velocity's `Order.bit_flags`) and an `L3_ROW_FLAG_REDUCE_ONLY` book-row flag
carry the fact across the wire. SDK `triggerMarketOrderV1` / `getTriggerMarketOrderV1Ix` /
`VelocityCore.buildTriggerMarketOrderV1Instruction`, and `PerpPosition.reduceOnlyClobOrders`.
Limit orders no longer accept `oracle_price_offset` (§2).

##### Maker priority

On a book with a nonzero `default_activation_delay_slots`, only attested flow (a
signed-message order that carries `State.hot_flow_authority`'s attestation) fills against the
book in the same transaction. An unattested taker (`place_and_take_perp_order_v1`,
`place_signed_msg_taker_order` without an attestation, a keeper tail on
`trigger_market_order_v1`) rests its whole order taker-origin through the default window and
the cross cranks fill it; an unattested shape that demands a synchronous outcome (an IOC, a
success condition) is refused with `UnattestedSynchronousTake` (6404). Cancels are never
delayed, so a maker can always reprice ahead of unattested aggression. A book with a zero
default delay opts out entirely. The only attestation transport is the
`flow_attestation: Option<FlowAttestationV0>` argument of `place_signed_msg_taker_order`:
swift's detached signature over the order's own signature plus an expiry, verified in-program
next to the taker signature. The flow authority never signs a transaction, so there is no
second signature fee and swift never takes custody of a transaction. Swift's `/attest` returns
the attestation blob for a held order. `place_and_take_perp_order_v1`,
`place_and_make_perp_order_v1` and `modify_order_v1` take no flow-authority account and are
always unattested: a maker or a modify that asks for an activation delay below the book's
default is refused with `UnattestedFastActivation` (6382). Velocity verifies the attestation
once and forwards the verdict on the quoter wire:
`QuoteArgsV0`/`ExecuteArgsV0` gain `taker_served_window` (true for attested flow, and for the
protocol cranks only when the orders they settle measurably rested (`SERVED_WINDOW_MIN_SLOTS`,
two slots), so a zero-delay book cannot launder fresh flow into the flag). The midpoint's
`require_attested_flow` now checks that flag instead of introspecting the instructions sysvar,
so its `quote_v0`/`execute_v0` account lists drop the sysvar and velocity `State` accounts.
Re-register midpoint entries accordingly. A rested unattested order therefore reaches protected
liquidity through the cross cranks. The off-chain router view takes the same fact as an
argument (`QuoteRouterArgs.taker_served_window`): a view for unprotected flow shows no depth
from a bumped book or a protected quoter, exactly as the fill's route would. `QuoterV0` carries
the book's placement rules: the attach mirrors them onto the entry (`book_tick_size`,
`book_min_order_size`, `book_default_activation_delay_slots`), so the take gate, the route's
maker-priority skip, and the remainder rest read a loaded field instead of CPI'ing
`order_rules_v0`. The attach's `quoter` account is writable.

Velocity is the config authority of every book it attaches. The market's quoter slab holds
both the book's `place_authority` and its `authority`, and the attach refuses a book whose
`authority` is anything else, so the mirror cannot go stale. `initialize_market_v0` no longer
requires `authority` to sign, which lets a book be created with the slab as its authority.
Two warm/cold-admin instructions reach the book: `update_perp_market_clob_book_config(args:
ClobUpdateMarketArgsV0)` CPIs `update_market_v0` and rewrites the mirror in the same
instruction, and `resize_perp_market_clob_book(new_capacity: u32)` CPIs `resize_market_v0` with
the admin paying the rent. The book validates every config at init and update: tick, step and
minimum order size are nonzero and the minimum is a multiple of the step,
`unknown_user_grace_slots` is at most 150, `max_activation_delay_slots` at most 1500, and
`max_execute_users` at most 48, the user-set capacity. `order_rules_v0` reports the book's
`authority`. The book rotates its authority in two steps (`propose_market_authority_v0`, then
`accept_market_authority_v0` signed by the proposed key), and each step emits a record. A book
attached to velocity has no velocity path that proposes a rotation. Every market admin instruction logs a versioned record: `MarketInitializeRecordV0` and `MarketUpdateRecordV0` carry the full settings (the update carries them before and after), and `MarketResizeRecordV0`, `MarketCloseRecordV0`, `CrankConditionsRecordV0` (the four resolver programs it registered), `MarketAuthorityProposedRecordV0` and `MarketAuthorityAcceptedRecordV0` cover the rest. The CLOB IDL (`tests/e2e/idl/clob.json`) declares them. SDK:
`AdminClient.getUpdatePerpMarketClobBookConfigIx`, `getResizePerpMarketClobBookIx` and the
`ClobUpdateMarketArgsV0` type. Admin CLI: `clob-market update-config` goes through velocity, and
`clob-market resize` is new.

The registry moves into a per-market slab. The quoter registry's approved set moves into one
`QuoterSlabV0` per market (§2 quoter registry, §5 ABI). A router fill carries the slab instead
of one `QuoterV0` entry per quoter, so a quoter costs two unshared account locks instead of
three; a slot is consulted when its response account rides the transaction. `QuoterV0` shrinks
to a staging entry (792 bytes) with one unified 12-slot CPI account list plus per-leg index
lists; `update_quoter_accounts` takes the whole list in one call and `QuoterCpiLeg` is removed;
approval copies staging into the slab and staging edits no longer clear a live config. Every
CLOB order instruction renames its `quoter` account to `quoterSlab`; `quoteRouter` drops
`quoterCount`; `crank_cross_match` legs become slab slot indexes. New `initializeQuoterSlab`
instruction, `getQuoterSlabPublicKey`, `decodeQuoterSlab`,
`QuoterConfigV0`/`QuoterSlotV0`/`QuoterSlabV0Account` mirrors. Errors `QuoterSlabFull` (6405),
`QuoterNotOnSlab` (6406).

The slab signs everything.

The market's quoter slab signs every external quoter CPI (ABI, §5). The `["clob_authority"]`
and per-entry `["quoter_signer", entry]` PDAs are removed; a book's `place_authority`, a
midpoint instance's `execute_authority`, and the signer slot in every registered CPI account
list are the market's `QuoterSlabV0` PDA. Every CLOB instruction drops its `clob_authority`
account, and fills stop carrying per-quoter signer accounts, so a route frees one account lock
per quoter plus one per CLOB instruction. Sharing one per-market key is safe because approval
refuses a registered list that names another approved quoter's response account and every
authority-trusting callee instruction requires its response account (the same first-look
exclusion that already existed), and per-market seeds keep the signature inert elsewhere;
approving a third-party quoter program now carries the matching check.

The slab right-sizes itself (ABI, §5). `initialize_quoter_slab` takes `{ market_index }` and
creates one slot; `update_quoter_approved` grows the account by exactly the slot it needs
(admin pays; its context gains `system_program` and a writable `admin`) and returns trailing
vacancy on revocation; `extend_quoter_slab` is deleted. The slab header stores its own bump.
`crank_cross_match` names `perp_market` and `quoter_slab` in its accounts (ABI); its maps
section shrinks to the oracle and the quote spot market. Every endpoint feat/propamm added
takes a single args struct (`PlaceAndTakePerpOrderV1Args`, `TriggerMarketOrderV1Args`,
`CrankCrossMatchArgs`, `UpdateQuoterApprovedArgs`, …; the full list is in the IDL). Borsh
encoding is unchanged where the fields kept their order. SDK: `getClobAuthorityPublicKey` /
`getQuoterSignerPublicKey` and the `VelocityClient` accessors are removed. Derive
`getQuoterSlabPublicKey(programId, marketIndex)` instead; `clobAccounts` bundles lose their
`clobAuthority` field; `QuoterSlabV0Account.bump`. Admin CLI: `quoter init-slab <market>` loses
its capacity argument and `quoter extend-slab` is removed.

The bindings are stored and constraint-checked (ABI, §5). `PerpMarket.clob_quoter` is replaced
by `clob_market` (same offset, 1296): the market stores its book account directly instead of
the registry entry, written once at `initialize_quoter` (the entry stays the quoter's identity
in signed routes and relay conditions). `PerpMarket` also gains `quoter_slab` at offset 1328
(the reserved tail shrinks to 192 bytes and the account did not grow), written at `initialize_perp_market`. The
slab header gains the book's account (`clob_market` at account offset 16, inside reserved
padding), written at approval and kept through a suspension so removal paths still reach a
killed book. Every accounts struct that names the market, the slab, or the book binds them with
`has_one`, so a wrong account now fails at the accounts layer instead of in a handler.
`update_quoter_approved` gains a read-only `perp_market` account (after `quoter`): a `Clob`
approval is refused unless the staged response account is the market's designated book, since
the staging entry is maker-editable between registration and approval. `crank_cross_match` and
`crank_taker_origin_cross` gain the `fill_not_paused` exchange-status guard the fill endpoints
carry. `QuoterCrossConditionsV0` drops its unread `clob_quoter` field (later fields shift −32;
size unchanged). SDK: `PerpMarketAccount.clobQuoter` becomes `clobMarket` plus `quoterSlab`;
`QuoterSlabV0Account.clobMarket`.

Audit fixes across the PropAMM order flow.

Approval records the deploy slot (§5). `update_quoter_approved` takes the entry's program and
an optional `quoter_program_data`, and records the slot the program was last deployed at in the
new `QuoterV0.approved_program_slot` field (carved from that struct's reserved padding, so its
size and alignment are unchanged). Approval does not require or impose a frozen program: a
maker may upgrade, because a `Custom` entry can move only its own registered user, at a price
held to its own quote and the taker's limit price, sized inside its own margin, so an upgrade
can lose the maker's money and cannot take anyone else's. The recorded slot moves on an
upgrade, which is how a router or an admin learns the code changed rather than inferring it
from behaviour; no fill reads it, because that would cost an account lock per quoter on the
budget that decides how many quoters a route can hold. SDK:
`QuoterV0Account.approvedProgramSlot`.

Reservoir and treasury. `force_cancel_clob_orders` paid its reservoir whenever it was reached,
and the only work gate was `open_bids`/`open_asks`, which count a user's slot orders too, so an
account holding only those swept a book that held nothing of theirs, `cancel_all_v0` removed
zero without erroring, and the payment landed anyway; it now requires a removal.
`force_cancel_clob_orders` also drops the liquidatee's `user_stats` account. Its grounds are
the two `force_cancel_orders` answers to: the account fails initial margin, or it is provably
below its equity floor. The authority-wide equity breaker authorises neither surface, so both
force-cancel routes answer one rule. The liquidation priority-fee reimbursement was charged per
instruction against a whole-transaction cost and is now divided between the claimants in the
transaction. `min_cross_surplus` must be above zero when attaching a CLOB.

Liveness. `execute_v0` and `cancel_all_v0` failed with `BookInvariantViolated` when two makers
were budget-truncated in one sweep (the response has one slot per remainder and `UserCapsV0`
carries eight budgets). Both walks now end where the response runs out of room, and `quote`
ends in the same place; the expiry-hint repair walked the whole arena per removal and now
follows the side lists once per call. `split_across_quoters` spun until the compute budget ran
out when the taker size was not a step multiple, which a reduce-only order reaches through an
unstandardised position magnitude. The stored liquidation-conditions account list fits an
account with more than five markets, and `digest_positions` no longer skips positions holding
only open orders, an isolated balance or a liquidation flag.

Fill accounting. No quoter may name the protocol `User` as a fill subject; a culled remainder
is held below the market's minimum (the attach requires the book's minimum to sit at or under
it), since the release saturates and an oversized figure collapsed a maker's whole reservation;
the router's binding band is quantized at the step the split used. New tail error
`InvalidUserConditionsSync`. SDK: `RouterAllocation.scaledQuote` mirrors the scalar the program
binds a fill to; a fill's quoter-CPI buffers are reused rather than allocated per leg, which is
what let a route reach the eight entries its account-lock budget allows.

Withheld depth becomes a filler obligation. A book stops its walk at an order whose owner the
transaction omits, and reports the depth behind it as withheld. That report used to reserve
taker size, which was then discarded, so a taker underfilled while liquidity it could have
reached went untraded. The size now goes to the sources that can fill it, and
`split_across_quoters` loses its `reserve` argument (SDK: `splitAcrossQuoters` loses the
parameter, `RouterReserve` is removed). What the report drives instead is an obligation on
whoever assembled the transaction, applied only when the taker did not sign it. A taker that
signs chose its own account list. The fill then requires that the transaction held more than
`TX_WRITABLE_LOCK_BUDGET - MAKER_ACCOUNT_COST` writable account locks (40 less the two a maker
costs), and that every loaded user either received a balance change or holds a role in the
fill: the taker, the filler, the taker's referrer, or a registered quoter's user. A loaded user
that filled nothing spent locks the missing maker needed, which is how a filler forces a
withhold and takes the flow on a worse-priced source of its own.

ABI note (§5). Every path a keeper assembles (`place_signed_msg_taker_order`, the trigger
cranks, `liquidate_perp_with_fill`) takes an optional `instructions_sysvar` account, read for
the account count and for whether the order's owner signed; a keeper that omits it is refused
whenever a book withholds. `place_and_take_perp_order_v1` is exempt, because its taker signs
and chose the account list. New tail errors `FillerOmittedReachableMaker` (6396 / `0x18FC`),
`FillerPaddedTheUserSet` (6397 / `0x18FD`), `FillerObligationUncountable` (6398 / `0x18FE`).
The grace window is unchanged: an order younger than the market's `unknown_user_grace_slots` is
passed over rather than withheld, because no filler can know of it yet.

A quoter must deliver what it won. A quoter that returned no base for an allocation it won was
skipped (`if ext_base == 0 { continue }`). The allocation had already been cut from that
quoter's own quoted ladder, and for a custom entry already reduced to what its own margin
supports, so returning nothing is the same contradiction as returning part, but only the
partial case failed. The skip therefore let a quoter quote tight, win base away from a source
that would have filled it, deliver nothing, and leave the taker short by exactly that size. An
allocation of zero is skipped earlier, so a zero fill can only mean the quoter was given real
size. Both directions now fail: `QuoterOverfilled` (unchanged) for more than the allocation,
new tail error `QuoterFilledShort` (6399 / `0x18FF`) for less. No account-layout or
instruction-signature change.

Blast-radius bounds. Two bounds on what an external quoter can do with a response velocity did
not author.

##### Blast-radius bound: a book's report is held to the reservation

Every CLOB order reserves `open_bids`/`open_asks` and an open-order slot on its owner's
`PerpPosition` at placement, under that owner's signature; nothing outside velocity can write
those. Every externally-reported unwind (the fill leg in `settle_external_match_fill`, the
sub-min culls, the completed-order counts, the evict/expire/cancel removal cranks, and both
cross cranks) now fails when the report exceeds it (`QuoterReportExceedsReservation`, 6403)
instead of saturating at zero. The ceiling on what a compromised book can open for a loaded
user drops from that user's free collateral to the size they actually posted, on the side they
posted it. The owner-signed exits (`cancel_order_v1`, `cancel_orders_v1`,
`force_cancel_clob_orders`) still clamp and log rather than fail, so a maker can always leave a
book that reports garbage. `User::re_arm_placed_trigger_slot` also refuses an evict report
larger than the shadow row it re-arms.

##### Blast-radius bound: a maker can tighten its own oracle band

New `QuoterV0.max_oracle_deviation_bps: u32` + `padding: [u8; 12]` (carved from the entry's
reserved padding, §5) and `update_quoter_max_oracle_deviation` (entry authority, Custom entries
only, `u32` in MARGIN_PRECISION units where 1 = 1 bp; 0 = no declaration). It applies as
`min(declared, market.margin_ratio_initial)`, so it can only tighten a bound the admin already
vetted, and it writes through to the live slab slot without re-vetting. A declared band also
trims the entry's quoted ladder before the split, so an over-wide quote costs that maker
allocation instead of failing a fill that carries other makers. `settle_taker_origin_cross`'s
pair path gained the per-change oracle band the router fill and the cross crank already
applied. SDK: `QuoterV0Account.maxOracleDeviationBps` + `padding`; new `math/router` exports
`quoterOracleBand`, `makerPriceBreachesOracleBand`, `isReportWithinReservation`. Admin CLI:
`velocity-admin quoter set-oracle-band <quoter> <bps>`.

##### The crossing reservation

A taker-origin remainder now claims the depth it crosses, and claimed depth leaves the
matchable set of `quote_v0`, `quote_l3_v0`, `execute_v0` and `next_cross_v0` for every caller
except the crank that owes the taker its improvement. `OrderNodeV0` grows 96 -> 104 bytes
(`taker_origin_prev` / `taker_origin_next`, threading each side's taker-origin orders onto
their own rest-ordered list) and `ORDERS_OFFSET` moves 9624 -> 9648; any off-chain reader that
decodes the arena by stride or offset must move with it. `ClobHeaderV0` gains
`taker_origin_head`/`tail`/`count` and `reservation_grace_slots` (default 32, settable through
`update_market_v0` up to a 150-slot ceiling, the blockhash validity window, past which a crank
cannot land anyway). `QuoteArgsV0`, `ExecuteArgsV0` and `L3ArgsV0` gain
`include_taker_origin_reservations`, which only `crank_taker_origin_cross` sets; `L3RowV0`
gains `L3_ROW_FLAG_RESERVED` within its existing width. Two rules an integrator has to price
in: a claim outranks price, so a better-priced ordinary order on the claimant's own side takes
nothing from claimed depth while the claim stands, and with a maker in front of a remainder
both cross cranks go quiet until the claim lapses; and attested flow reaches unclaimed depth
only, so a synchronous take can fill nothing and rest as a remainder itself.

##### Cross-match

`crank_cross_match` takes `{ market_index, size }` (`buy_quoter_index` and `sell_quoter_index`
are gone) and runs as two ordinary router fills of the protocol `User`, so the vAMM's last
look, PropAMM crossing and the pre-execute margin clamp all apply. It carries the quoter slab
on the tail and an `instructions_sysvar`. Its cost rose from ~75k to ~328k CU, past one
instruction's 200,000 default, so a caller must request a budget. New errors
`CrossMatchLegsDoNotCross` (6407) and `TakerExposureNotProtocolOwned` (6408);
`CrossedTakerRemainderPending` (6390) is deprecated in place and no longer emitted. CLOB place
records now carry `IsIsolatedPosition`, matching the cancel and fill records.

##### Liquidation and the funding mark read the book

`liquidate_perp_with_fill` fills its forced order through the router, so a liquidation reaches
the market's CLOB and its PropAMM quoters instead of matching only the makers the caller
loaded. The named account list does not change: the quoter section rides `remaining_accounts`
after the maker pairs, and a market that names a book refuses the call without it. The
liquidator is still only the filler and acquires no position. A liquidation carries no
attestation, so the route vouches for protected flow the way the cross cranks do. It measures
that the depth it can reach has rested for `SERVED_WINDOW_MIN_SLOTS`, rather than asserting a
window it did not serve. Without that a book with a speed bump quotes a liquidation nothing and
the position reaches the vAMM alone. The relay resolver stages the book and the owners resting
on the side the liquidation sweeps, read through the book's own `quote_l3_v0` leg;
`sync_liq_conditions` stores the book and its program beside the slab so the resolver can reach
them. `update_perp_bid_ask_twap` gains three optional accounts (`quoter_slab`, `clob_market`,
`clob_program`) and estimates each side from the book alone; a market that names a book refuses
the crank without them. It reads no `remaining_accounts`, so `updatePerpBidAskTwap` and
`getUpdatePerpBidAskTwapIx` no longer take a `makers` argument. A quote that has not rested for
`BID_ASK_TWAP_MIN_QUOTE_REST` is dropped, and a suspended book moves no mark. SDK:
`getUpdatePerpBidAskTwapIx` resolves the book accounts from the market, and
`getLiquidatePerpWithFillIx` appends the quoter section and takes an `extraQuoterAccounts`
argument.

##### Hardening of the quoter registry, the book and the signed-message envelope

Several instructions gained accounts and one envelope field became mandatory.

Registry. `initialize_quoter` takes `quoter_slab` (required for a `Clob` entry), so a market's
slab must exist before its book is registered; `update_quoter_approved` takes `clob_market`,
required for a `Clob` entry, bound to the book the market designated and asked for its own
`order_rules_v0` so an approval cannot name an account that is not that book.
`update_quoter_active`, `update_quoter_config`, `update_quoter_accounts` and
`update_quoter_watch` take `state`: a non-`Custom` entry is now gated on the State's warm admin
rather than on the individual key that registered it, so rotating the admin also rotates
control of a book slot. A `Custom` entry is unchanged and still answers only to the key that
created it, because `is_active` is the maker's own kill switch. A registered account list may
no longer name the market's designated book, whether or not that book has been approved yet.

Signed messages. The `network` tag is now required. A message that names no cluster is refused
exactly as one naming the wrong cluster is, because both replay the same way. In the SDK
`network` becomes a required field on both message types, `signedMsgNetworkForEnv` is exported,
and the client stamps the tag from its configured `env`, so a caller that signs through the
client states nothing. Two input aliases, `SignedMsgOrderParamsMessageInput` and
`SignedMsgOrderParamsDelegateMessageInput`, keep the field optional at the client's edge.
`VelocityClient.env` defaults to `mainnet-beta`, so a devnet integrator that never set `env`
now signs a mainnet tag that devnet refuses; the program names both clusters in its log.

Events. `AcceleratedReferralStatusChangedRecord` becomes
`AcceleratedReferralStatusChangedRecordV0` and carries a new discriminator.

Errors. New velocity variant `ClobRestUnavailable`. The CLOB adds `MaxTsBeforeActivation` and
`MarketNotEmpty`; the midpoint keeps `InvalidInstructionsSysvar` and `InvalidVelocityState` as
deprecated in place, since a variant's number is its on-chain identity.

CLOB program. `initialize_market_v0` now requires the market account to sign, which closes a
window where a pre-created account could be initialized by somebody else and its rent stranded;
`close_market_v0` returns the rent of an empty book to its authority. `base_precision` must
equal `1_000_000_000`, and `evict_threshold_per_side` must sit between zero and half the arena,
so a market cannot be created in a shape that fails every fill or that can never be evicted.
`OrderRulesV0` gains three trailing fields, `side_order_counts`, `arena_capacity` and
`evict_threshold_per_side`, so a caller predicts a full side instead of discovering it as a
failed CPI. Existing fields keep their order and the discriminator is unchanged.

Midpoint. `QuoterConfigV0` gains `max_mid_deviation_ppm`, which must be nonzero at creation, so
a fresh instance already bounds how far its mid may sit from the oracle; `UpdateQuoterArgsV0`
gains `mid_sequence` so a runaway sequence is recoverable; a zero mid is exempt from the
sequence guard, so the withdrawal that stops an instance quoting can never lose a race;
`MidpointExecuteRecordV0.market_index` becomes `configured_market_index`, which names the
market the instance was created for rather than the market the fill settled.
`MidpointQuoterV0::validate` now also rejects a zero or above-`MAX_MID_STALENESS_SLOTS_CEILING`
`max_mid_staleness_slots`, a zero `price_tick_size` or `size_step`, and, on every config path
rather than only at creation, a zero `max_mid_deviation_ppm`. `update_quoter_v0` emits a new
`MidpointConfigRecordV0` with the resulting config. Two new instructions,
`propose_authority_v0` and `accept_authority_v0`, rotate the config `authority` in two steps and
emit the same record; `MidpointQuoterV0` gains `pending_authority: Address`, carved from the
trailing padding, so the account size is unchanged but a decoder that reads that offset as
padding must add the field.

Behaviour an integrator can observe. A liquidation refuses while the account holds orders on a
book, and that refusal now covers `set_user_status_to_being_liquidated` and the four spot and
pnl liquidation entries as well as the perp ones, so `force_cancel_clob_orders` runs first; a
fired trigger order counts as book-resident for that rule. `modify_order_v1` applies the same
placement preconditions a placement does. An eviction charges the evicted maker the flat
removal reward, as an expiry already did. A fired stop that cannot rest emits a cancel record
rather than disappearing, and a reducing remainder is no longer held to a margin gate the old
matching path never applied. A reduce-only order is sized against the position as each leg
leaves it, so several of one maker's orders in one fill cannot flip that maker's position. The
vAMM's last look shades only the depth a rival actually offers. A fill that withholds book
depth is excused only by locks velocity can attribute to accounts it verified. A signed-message
order id is reclaimed from a full account only once it is past `SIGNED_MSG_EVICTION_BUFFER`;
the entry carries the uuid the replay guard matches on, and an entry still inside its own
`max_slot` guards a message placement would still accept. Both trigger endpoints refuse a
fill-paused market, because firing commits the order and pays the keeper, and `MarketStatus`
carries no fill-paused variant. A crossed pair of taker-origin remainders settles outside the
router, so it runs the router's own market gates and re-derives `reduce_only` from the market:
a row that rested while the market was `Active` must not grow a position after a flip to
`ReduceOnly`. `modify_order_v1` gates its replacement on the risk test every other placement
uses, which counts the reservations the account already holds rather than the bare position, so
a replacement behind other resting orders takes initial margin and the buffered equity floor.
`calculateAmmAvailableLiquidity` is the SDK mirror of the per-fill reserve throttle, and
`vammQuoteLevels` caps its ladder at it; the room to the hard reserve bound is far wider and
quoting that over-allocates the vAMM. The protocol-owned `User` is exempt from the
external-payer allowlist, because its authority is `State::signer` and a program address can
neither sign nor pay. An armed trigger past its `max_ts` is no longer payable work on either
trigger endpoint, and relay's discovery skips it, so a dead stop neither pays a keeper nor
starves the triggers behind it. `crank_taker_origin_cross` accepts the taker's
`RevenueShareEscrow`: without it a referred taker's cross failed outright, and with it the
referee discount and referrer reward bind. A builder fee does not bind on that path, because
the book row carries its own handle rather than the velocity order id.
`force_cancel_clob_orders` cancels during a full exchange halt but pays no keeper fee.
`initialize_quoter_cross_conditions` bounds `expire_fallback_slots` at 9,000 slots, because the
endpoint is permissionless. Every resolver refuses a call that marks an account writable beyond
its staging region, so landing one is inert by construction rather than by review; the region
is the relay scratch account, `quote_buffer` for `quote_router`, plus the book's own account
where `quote_l3_v0` streams its answer. `quote_router` takes the perp market read-only and no
longer writes to the makers it sizes.

##### Auctions are gone

An order carries one worst price instead of a ramp, and `Order.price` holds it for every type.
A market order's bound is clamped to 0.5 percent of the oracle at placement. An oracle-relative
order uses `oracle_price_offset`. `auction_start_price`, `auction_end_price` and
`auction_duration` are renamed on `Order`, removed from `OrderParams` and `ModifyOrderParams`,
and replaced there by `activation_delay_slots`. Off-chain fill prediction must stop
interpolating a price against elapsed slots and read the order's own bound. The §2 row has the
full surface.

#### protective-price-pre-refresh-twap

Fixes a Medium audit finding (OtterSec #14) that is the same defect as #109 through #112 and
#134, in the shared spot-liquidation helper.
`controller::spot_balance::update_spot_market_and_check_validity` refreshed the market's oracle
TWAPs first, then judged the oracle against `last_oracle_price_twap` and returned, leaving
callers to read `last_oracle_price_twap_5min` off the just-refreshed account. Both readers were
therefore self-normalizing.

The verdict: a `TooVolatile` oracle could pull the 1-hour risk EMA toward itself far enough to
clear its own `VelocityAction::Liquidate` gate.

The protective prices added for #14 in the pnl-versus-spot lanes
(`calculate_user_protective_asset_price` = `max(oracle, 5min twap, oracle + conf)` and
`calculate_user_protective_liability_price` = `min(oracle, 5min twap, oracle − conf)`) are a
bound against a reference the refresh had already turned into a copy of the price being
bounded. With the 5-minute TWAP dragged onto the oracle, the `max` and `min` collapse to the
raw oracle price and the protection is gone. The drag is not marginal.
`calculate_new_twap` gives the incoming price about 99.7% weight once 300s have elapsed since
`last_oracle_price_twap_ts`, and `update_spot_market_cumulative_interest` is a permissionless
crank, so a liquidator can move the TWAP onto a stale price for the cost of transaction fees.
The only brake is `sanitize_new_price`'s step clamp of plus or minus 10%, 20% and 50% of the
1h TWAP, and the 1-hour TWAP drifts under the same cranking.
`liquidate_borrow_for_perp_pnl` and `liquidate_perp_pnl_for_deposit` have no 5-minute-TWAP
divergence band at all, so there the protective price was the only guard.

The helper now computes the verdict and captures the 5-minute TWAP from the market as it
stands on entry, then refreshes, returning both in a new
`SpotMarketOracleRefresh { validity, pre_refresh_twap_5min }`. Each lane then reads a TWAP it
did not move. `liquidate_spot`, `liquidate_borrow_for_perp_pnl` and
`liquidate_perp_pnl_for_deposit` keep the refresh and price off the snapshot, which is #109's
shape where a gate reads unmutated state, rather than #110 and #111's `None`.
`liquidate_spot`'s two `is_oracle_too_divergent_with_twap_5min` bands read the snapshot as
well, instead of the field the same instruction had just dragged toward the oracle price the
band measures.

The swap-backed lane takes the #110 and #111 shape instead, because
`liquidate_spot_with_swap_end` prices against the same 5-minute TWAP from a separate
instruction where a snapshot cannot be carried. A new
`controller::spot_balance::check_spot_oracle_validity` judges a spot oracle without advancing
anything, `liquidate_spot_with_swap_begin` uses it in place of the refreshing helper, and both
halves of the swap price against the unmoved TWAPs. `end` also drops two hand-rolled copies of
the validity check onto the same helper, which adds the `VelocityAction::Liquidate` gate
`begin` already applies. The deposit, borrow and utilization TWAPs still advance in
`handle_liquidate_spot_with_swap_begin`.

Integrator-visible: a spot, swap-backed spot, or pnl-versus-spot liquidation can now revert
with `InvalidOracle` where the in-instruction refresh previously normalized a `TooVolatile`
oracle into range. A spot liquidation can now revert with `PriceBandsBreached` where the
refresh previously widened its own band. A swap-backed spot liquidation no longer advances
either market's oracle TWAPs. And protective seizure and repayment prices are strictly more
user-favorable than before. Liveness is unaffected in the steady state, since these TWAPs
advance on every other spot path and via the permissionless
`update_spot_market_cumulative_interest` crank, so a genuinely volatile market clears once
those catch up. No account-layout, IDL, error-code or SDK-API change, since the SDK's
protective-price helpers are pure functions over fetched, and therefore pre-refresh, market
state.

#### revshare-reserve-net-user-pnl

Fixes three related High audit findings on the permissionless pnl-pool fee sweeps. All three
are about reserving tokens that other claimants are owed before draining. Part of #255.

#48: the builder and referrer revenue-share sweep
(`sweep_completed_revenue_share_for_market`, run on every permissionless `settle_pnl` or
`settle_multiple_pnls` with a `RevenueShareEscrow`) paid rows out of a market's PnL pool
checking only the raw pool balance against `fees_accrued`, never reserving
`max(net_user_pnl, 0)`. A caller could therefore move tokens backing a third party's positive
unsettled PnL. It now draws only `pnl_pool_token_amount − reserved`, with `net_user_pnl` valued
at the market's oracle price and validity-gated in-slot by the preceding settle.

#53: the protocol fee sweep (`sweep_market_fees`) let its buffer-exempt protocol-fee drain move
the tokens backing the #245 floored `pending_if_fee` bankruptcy tranche into
`protocol_fee_pool`, outside the insurance backstop, without touching the counter. A later
`resolve_perp_bankruptcy` then cancelled the loss counter-only against an unbacked tranche,
leaving surviving-trader PnL short. Every drain, including the protocol one and the
revenue-share sweep, now reserves `min(pending_if_fee, get_bankruptcy_if_floor())` on top of
user PnL.

#73: `sweep_market_fees` drained protocol fees without reserving already-accrued builder and
referrer revenue share, briefly leaving those claims unpayable. The new per-market counter
`PerpMarket.pending_revenue_share: u64` (QUOTE_PRECISION) is incremented as builder and
referrer fees accrue on fills and decremented as the revenue-share sweep pays them, and
`sweep_market_fees` reserves it too.

`sweep_market_fees`'s reservable total is therefore `max(net_user_pnl, 0)` plus the floored IF
tranche plus `pending_revenue_share`.

Layout: `pending_revenue_share` reuses the 8-byte alignment padding before `amm`
(`_padding_align_amm`), so all offsets are unchanged and existing accounts read 0 (§5).
`PerpMarketAccount.pendingRevenueShare` was added. There is no instruction or SDK-API change,
since the sweeps are program-internal and not reimplemented client-side.

#### revshare-settle-liveness

Fixes three ways accrued builder and referrer revenue share became permanently uncollectable
while `PerpMarket.pending_revenue_share` kept reserving the pnl-pool tokens behind it. That
value was withheld from the protocol, IF and AMM fee drains (`sweep_market_fees`) indefinitely,
since the counter is only ever decremented by `sweep_completed_revenue_share_for_market`.

No payer. That sweep ran only from `settle_pnl` and `settle_multiple_pnls`, and only when the
settle actually moved PnL, so a row was payable only while its escrow owner still had
settleable PnL on the market. Once they flattened and stopped trading, the beneficiary had no
way to collect. The new permissionless instruction `settle_revenue_share` (§3) runs the sweep
for one escrow and one market on its own, applying the oracle-validity gating the settle
handlers previously supplied. That is the same preamble `sweep_perp_market_fees` uses, being
the other permissionless third-party drain of this pool valuing the same
`max(net_user_pnl, 0)` reserve, including its `MarketStatus::Settlement` branch that values at
`expiry_price`. On a live market the `Completed` precondition on builder rows is kept, because
paying an `Open` row zeroes it and destroys the `order_id` and `sub_account_id` that
`find_builder_order_index` needs, which would let a third party unilaterally end a builder's
fee accrual on a live order. The caller passes the owner's sub-accounts read-only so the
program can complete those rows itself. In `Settlement` the precondition is dropped, as
described below.

Head-of-line block. A row the pool could not currently afford ended the sweep loop instead of
being skipped, so one oversized row blocked every beneficiary behind it, and permanently,
because escrow row order is stable across calls. It is now skipped. The reservation is
computed once per call and the pool re-read per row, so the floor guarantee is unchanged.

Delisting paid out the wrong party. `settle_expired_market_pools_to_revenue_pool` drains the
entire pnl pool, reservation included, to the revenue pool and sets `Delisted`, after which
`settle_pnl` rejects the market and no sweep can ever run. Yet it never inspected the counter,
so identified third parties' earned fees went to the protocol's own revenue pool and a stale
liability sat on the market indefinitely. That value is provably present and provably payable
right up to the delist, because the expiry solver prices winner claims against
`pnl_pool - pending_revenue_share` (#147), so the tokens behind the counter are withheld from
winners, and the handler's own wind-down validations put `net_user_pnl` at 0, leaving the whole
pool available. The handler now requires the counter to be drained before it will delist,
rejecting with `UnsettledRevenueShareOnDelist`, with no time-based escape, because every row
has a terminal resolution. `settle_revenue_share` pays anything payable, and the new
`forfeit_revenue_share_order` writes off anything provably unpayable.

Two supporting changes make that exhaustive. In `Settlement` the sweep no longer requires a
builder row to be `Completed`. That gate exists so paying does not destroy the `order_id`
binding a live order still accrues against, and `fill_perp_order` rejects any market that is
not Active or ReduceOnly, so nothing can accrue. Without this, an owner who deleted a
sub-account could never have its rows completed and the market could never delist. And the
forfeit covers the three ways a row can be unpayable, namely no beneficiary payout account, a
wound-down pool too short, and no reachable beneficiary at all, each proven on-chain rather
than inferred from a missing account. The missing-payout-account reason is the one a
beneficiary can still undo by calling `initialize_user`, so it is accepted only after
`expiry_ts + escrow_period_before_transfer`, which is the first instant the market may delist
anyway, giving the beneficiary the whole escrow window to onboard and adding no delay to the
wind-down. The escrow window itself moved onto `State::escrow_period_before_transfer` so the
delist and the forfeit cannot disagree about it.

`RevenueShareEscrowMap.getEscrowsOwingRevenueShare(marketIndex)` and
`velocity-admin fees settle-revenue-share <market> --all`, which settles and then forfeits the
stragglers, make clearing a market a one-command crank. The delist itself no longer discharges
anything, because the gate guarantees the counter is already zero when the final `force` sweep
runs, so the reservation that sweep applies contributes nothing and `pending_protocol_fee`
reaches the still-withdrawable `protocol_fee_pool` in full. This also makes the pre-delist
`Settlement` window, whose reservation `expiry-price-conservation` (#147) added precisely so
the payable survives wind-down, usable by a beneficiary.

Layout: none. Two appended `Error` variants, `UnsettledRevenueShareOnDelist` (6372) and
`RevenueShareOrderNotForfeitable` (6373), plus the two new instructions. New SDK exports in
§4.6, and a new CLI command `fees settle-revenue-share`.

#### slot-duration-sync

Audit follow-up to `slot-duration-scaling`.

New `State.slot_duration_transition_slots: [u64; 4]` at struct offset 1512, carved from
padding with `State` unchanged at 1752 bytes, records the first slot of each post-baseline IBRL
regime. `math::time::SlotClock` resolves the duration at any slot from it and integrates
elapsed intervals piecewise, so a measurement spanning a transition converts to its exact
wall-clock length. Previously the whole delta was priced at one endpoint duration. Every
elapsed-time rule measures through it, covering oracle ages, the AMM staleness gate, the
liquidation fee and ramp, filler time rewards, idle, eviction and quote-rest windows, and VLP
hedge uncertainty fees.

The warm-admin `update_state_slot_duration_ms` was removed and replaced by the permissionless
`sync_state_slot_duration`, whose accounts are `state` writable plus one of the four IBRL
feature-gate accounts with the key and owner constrained, and which takes no args and no signer
authority. The effective slot derives from the `EpochSchedule` sysvar as the first slot of the
epoch after activation, mirroring Agave, which is correct for warmup epochs and mid-epoch
activations. Syncs are ordered and idempotent, and the legacy staging trio is kept coherent for
older readers.

SDK: `StateAccount.slotDurationTransitionSlots` added. `activeSlotDurationFromState` consults
the archive first. New `elapsedMillis` and `elapsedMillisFromSlotDelta` mirrors.
`getOracleValidity`, `isOracleValid`, `getSpotOracleValidity`, `blockOperation`,
`getLiquidationFee`, `calculateMaxPctToLiquidate` and `User.canMakeIdle` now take a trailing
`SlotDurationState`, the decoded `State`, instead of a `SlotDurationMs`.
`AdminClient.updateStateSlotDurationMs` and `getUpdateStateSlotDurationMsIx` become
`syncStateSlotDuration` and `getSyncStateSlotDurationIx`, with no admin param.
`IBRL_FEATURE_WARMUP_SLOTS` is removed.

velocity-rs: `slot_clock_from_state` and `VelocityClient::slot_clock` added.
`VelocityAccounts.slot_duration_ms` becomes `slot_clock`. The Rust `DLOB` holds a slot clock
(`DLOB::update_slot_clock`, `DLOBNotifier::slot_clock_update`, and the new
`DLOBEvent::SlotClockUpdate` variant), pushed each slot by the builder and bot slot handlers.
CLI: `exchange set-slot-duration-ms` becomes `exchange sync-slot-duration`.

Auction durations became wall-clock 400ms units. `Order.auction_duration` and
`OrderParams.auction_duration` (u8, layout unchanged) now mean 400ms units rather than live
slots. The raw values are identical at the 400ms baseline, so existing orders and clients
migrate for free while the chain is at 400ms. Auction interpolation and completion measure
elapsed wall-clock through the slot clock, so the u8 keeps the full historical 72s range at
every gate, with a ceiling of 255 units equal to 102s, and the previous compression to about
51s at 200ms is gone. The signed-msg `max_slot` converts the duration to actual slots, rounding
up, at the live slot duration. SDK mirrors (`isAuctionComplete`, `getAuctionPrice*`,
`getLimitPrice`, `hasLimitPrice`, `hasAuctionPrice`, `isRestingLimitOrder`,
`DLOBNode.getPrice`) take a trailing optional `SlotDurationState`, and
`DLOB.slotDurationState` carries it, set from the `State` account and wired by
`UserMap.getDLOB` and `DLOBSubscriber`. Integrators that convert auction durations from
milliseconds must divide by 400, rounding up, never by the live slot duration.

#### spot-oracle-twap-ts-init

Fixes a Medium audit finding (OtterSec #121). A freshly initialized spot market carried
`last_oracle_price_twap_ts == 0`, because `HistoricalOracleData::default_with_current_oracle`
had that one assignment commented out, while the perp initializer has always stamped it.

With a zero timestamp the first `update_spot_market_twap_stats` computes `since_last = now - 0`,
which dwarfs any TWAP period, so `from_start` saturates to 0 and the TWAP is replaced by the
live price outright. That collapses both `StrictOraclePrice` bounds, the `min` and `max` of
current against the 5-min TWAP, onto a single number, and leaves the first price-banded
operation on the market unguarded in both directions.

`default_with_current_oracle` now takes `now` and stamps it, and drops its `..default()` so a
future field addition is a compile error rather than a silent zero, which is how the omission
went unnoticed.

The fix is init-only, with no retroactive handling for accounts already on chain.
`update_spot_market_twap_stats` runs on every deposit, withdraw and interest accrual, so every
live market took its one degenerate refresh long ago and already carries a real timestamp.

`QuoteAsset` markets are excluded, and `default_quote_oracle` still leaves the timestamp at
zero. That source returns a constant `PRICE_PRECISION`, so such a market's TWAP and live price
are always the same number and its band is degenerate by construction, and the mechanism #121
describes cannot exist there. Touching it is not harmless either. It flips which way
`calculate_weighted_average`'s plus-or-minus-1 rounding bias falls on the first crank,
`999999` against `1000001`, because a zero timestamp saturates `from_start` to 1 and inverts
the sign, and every collateral valuation reads the quote TWAP. The Anchor suite caught this via
`maxLeverageOrderParams`, which asserts an exact leverage for a USDC-only account. Both values
are noise around a definitionally constant 1.0, so the quote market's behavior is left as it
was rather than trading one artifact for another.

Integrator-visible: on a spot market's first oracle-bearing crank the 5-min TWAP no longer
jumps to the live price, and `initialize_spot_market` writes a non-zero
`historicalOracleData.lastOraclePriceTwapTs`. No account-layout, IDL, error-code or SDK-API
change, since the SDK reads the stored TWAP for the spot band rather than projecting it, and
its `lastOraclePriceTwapTs` consumers are all perp-side (`marketStats`) where the timestamp was
always stamped.

#### swap-provider-interface

SDK-only, from #331. Jupiter and Titan now sit behind one `SwapProvider` interface, calling
`getQuote` and then `getRouteInstructions`, and a quote carries the route it was quoted for.

Previously `UnifiedSwapClient.getSwapInstructions` branched on client type into two unrelated
implementations. The Jupiter branch built from the `quote` argument. The Titan branch ignored
every argument except `userPublicKey` and replayed a route held in private client state,
populated by whichever `getQuote` had last run on that instance and cleared after a single use.
A caller passing a quote got the route it named on Jupiter and an arbitrary cached one on
Titan, indistinguishable at the call site. When the cached route was for a different pair, the
swap executed and deposited its output into a token account `end_swap` was not watching,
reverting with `InvalidSwap: amount_out must be greater than 0` after funds had already moved.

Route building is now a pure function of the quote, with no client state, no ordering
requirement and no single-use cache. Three further route and swap mismatches are rejected up
front rather than on-chain. A quote for the wrong pair is checked against the spot markets
`beginSwap` and `endSwap` bracket. A Titan route quoted for a different wallet is rejected,
because Titan resolves token accounts at quote time. And a quote executed at a slippage other
than the one it was priced at is rejected, since Jupiter's `/swap` defaulted to 50bps. A quote
is also rejected unless it is for the amount being swapped, because `beginSwap` releases funds
sized off the quote.

The duplicated per-client instruction filter collapses into one shared
`filterRouteInstructions`, where the Jupiter copy indexed `keys[3]` unguarded and threw where
the Titan copy did not. The three per-provider `VelocityClient` swap builders collapse into one
provider-generic `getProviderSwapIx`.

This is a breaking SDK surface change only, with no program, account-layout, IDL or error-code
change. See §4.3 and §4.6.

#### tokenized-pooled-basis-gate

Fixes a High audit finding (OtterSec #140) in the `vaults` program. A newcomer tokenizing into
an under-water tokenized depositor captured part of the existing holders' loss shelter.

A `TokenizedVaultDepositor` carries one cost basis
(`net_deposits + cumulative_profit_share_amount`) for every holder of its mint, and the
profit-share fee is collected by shrinking the pool's own shares, so it dilutes every token
equally regardless of who accrued the loss. Both `transfer_shares` legs move basis by the
current value of the shares moved, so the pooled shelter (basis minus value) is invariant to
supply changes while being consumed per-token. Minting therefore transfers shelter from
incumbents to the newcomer, which is a pure holder-to-holder transfer since the manager
collects the same either way. And draining a pool leaves the shelter unattached, so whoever
tokenizes next inherits it at no cost, with no victim and no timing race.

Working the fee-incidence algebra through shows a single pooled basis can be fair only when
basis equals value at the moment supply changes, and `apply_profit_share` already forces that
equality whenever value exceeds basis. `tokenize_shares` therefore now rejects the
value-below-basis case with the existing `InvalidTokenization`, which is provably the tightest
condition reachable without a per-holder state model. Per-holder basis is blocked twice over.
The fee comes out of shared pool shares so it cannot be charged to one cohort over another, and
the mint is a classic `anchor_spl::token` mint whose transfers the program never observes,
since a transfer hook would need Token-2022 and a different mint PDA.

The gate is evaluated post-transfer, which is algebraically identical to the pre-transfer test
because a mint raises basis and value by the same amount, so no plumbing of `withdraw_value` is
needed. Using `>=` rather than `==` keeps it compatible with #104's deferred sub-share fee and
immune to the plus-or-minus 1 that `WithdrawUnit::Token` can introduce.

Separately, `redeem_tokens` now calls the new `reset_orphaned_cost_basis()` when a redemption
empties the pool of both shares and tokens. That also fixes an honest-user bug in the mirror
direction, where a basis left above value, reachable when `hurdle_rate > 0` leaves a sub-hurdle
profit without advancing the mark, or when #104 defers a fee and rolls it back, made the next
honest tokenizer owe profit share on gains they never made.

Cost to accept: a pool that has been under water stays closed to new tokenizations until the
vault recovers past the pooled high-water mark. Redemptions are unaffected, so existing holders
can always exit. No account-layout change, `SIZE` unchanged, no new error variant, and no IDL
or migration change. The SDK gains a changeset documenting the new failure mode.

#### tokenized-rebase-backing

Fixes a Medium audit finding (OtterSec #122) in the `vaults` program. The signerless
`apply_rebase_tokenized_depositor` instruction could floor a tokenized depositor's
`vault_shares` to zero while the tokenized mint's supply was still live.

Those shares are the shared backing for the entire token supply, and the base rebase floors
them by integer division. The instruction carries no signer at all, so any caller could commit
the lazy rebase at a moment when the divisor zeroes the backing. Every holder then computes
zero redeemable shares and `redeem_tokens` aborts before burning, leaving the tokens
permanently unredeemable. Unlike a paper loss it does not heal when the portfolio recovers,
because the backing shares are gone.

The new `TokenizedVaultDepositor::apply_rebase_public` rejects a rebase that would take nonzero
backing shares to zero, with `InvalidVaultRebase`, and the instruction calls it instead of the
unguarded path. This is the tokenized analogue of the #106 guard already applied to
`VaultDepositor::apply_rebase_public` in #307, and it makes the same liveness trade: a refusal
is recoverable, since the signed lifecycle actions `tokenize_shares` and `redeem_tokens` still
rebase through the unguarded path, whereas floored backing is not.

Program-internal only, with no account-layout, IDL, error-code or SDK-API change, since it
reuses `InvalidVaultRebase`.

Unrelated observation recorded but not changed: the inherent
`TokenizedVaultDepositor::apply_rebase` is private, so method resolution already sent this
instruction to the trait method, which means the signerless path has never refreshed the
`last_vault_shares` checkpoint that the signed paths maintain. `apply_rebase_public`
deliberately preserves that behavior rather than altering it.

#### vault-nav-interest-refresh

Fixes two High audit findings on vault NAV pricing (OtterSec #136, #137).

`Vault::calculate_equity` values the vault's velocity spot deposit off the denomination spot
market's stored `cumulative_deposit_interest`, and only the owning program, velocity, may write
that account, so a vault instruction can only get a current index by CPI-ing velocity.

#136: `deposit` refreshed the market only afterwards, as a side effect of the deposit CPI, so
an entrant priced its shares against a stale index. NAV was understated by the accrued but
unbooked lender interest and the entrant overminted, capturing a slice of interest the
incumbents had already earned.

#137: `request_withdraw` and `cancel_withdraw_request` never refreshed at all, so the recorded
request value understated NAV, leaking pre-request lender interest to the remaining
shareholders, and `calculate_shares_lost` saw no request-window gain, letting request-window
interest escape the cancellation share-forfeiture rule.

Every vault instruction that snapshots NAV now CPIs velocity's
`update_spot_market_cumulative_interest` for `vault.spot_market_index` as its first statement,
before any account is borrowed, since `invoke` rejects a CPI whose writable accounts have live
borrows, and before `load_maps`, so the maps and `calculate_equity` read post-refresh data. The
refresh is a shared helper (`velocity_cpi::refresh_denomination_spot_market` plus the
`refresh_velocity_spot_market!` macro) and is idempotent within a slot, so paths that later CPI
`deposit` or `withdraw` pay nothing extra. This was chosen over a freshness gate that would
reject the instruction when the index is stale, because a gate would make deposits, withdrawals
and cancels fail until someone cranked the market, reintroducing a liveness hazard, whereas the
CPI makes every path self-sufficient.

Instruction accounts changed (ABI). 19 instructions gained `velocity_spot_market` (writable)
and `velocity_oracle`, plus `velocity_spot_market_vault`, `velocity_state` and
`velocity_program` where absent. They are appended only, with nothing reordered or removed, and
discriminators are unchanged. SDK `VaultClient.getSpotMarketRefreshAccounts` feeds every
affected builder, so SDK callers need no change. Manual builders must append the accounts and
pass the two PDAs explicitly, because Anchor's TS resolver cannot derive seeds that read a
`vault` field and substitutes the default pubkey without reporting an error.

Behavioral side effect: these instructions inherit velocity's `exchange_not_paused`,
`spot_market_valid` and spot-vault-solvency checks, so a paused exchange or a delisted
denomination market blocks withdraw-request and cancel too, not only the paths that already
CPI'd `deposit` or `withdraw`.

`manager_borrow`, `manager_repay` and `manager_update_borrow` are left alone, because their
equity snapshot only populates event fields. No account-layout change in either program, and
the `velocity` IDL is unchanged.

The three extra named accounts this change introduced were removed again by
[vault-nav-spot-market-refresh](#vault-nav-spot-market-refresh). See §5.4 for the current
account lists.

#### vault-nav-spot-market-refresh

Follow-up to `vault-nav-interest-refresh`, which fixed OtterSec #136 and #137 for one market
only.

`Vault::calculate_equity` delegates to velocity's `calculate_user_equity`, which converts every
held spot position through that position's own market's cumulative index. A vault that also
lends or borrows outside its denomination market therefore still priced those positions off
whatever index the last unrelated crank left. For a borrow the sign flips, since a stale
`cumulative_borrow_interest` understates the liability, reads NAV high, and overpays a
withdrawer out of the vault rather than out of another depositor.

The new velocity instruction `refresh_spot_market_interest` books several markets in one call
(§5.5), and both it and `force_delete_user` walk one shared
`controller::spot_balance::refresh_spot_market_interest` helper, so the per-market work has a
single owner. The vaults program derives the market list on chain from the vault and its
velocity user, so a caller cannot leave a market out.

Three further corrections.

Oracle TWAP: the refresh passes no oracle. `calculate_equity` gates the denomination oracle on
`is_oracle_valid_for_action(MarginCalc)`, whose `TooVolatile` arm measures the live price
against `last_oracle_price_twap`, and the previous refresh advanced that TWAP toward the live
price immediately before the check read it, which is the shape OtterSec #110 and #111 closed
elsewhere.

Delisting: the refresh no longer carries `spot_market_valid`, so a vault whose denomination
market is delisted is no longer frozen out of every instruction with no way back, since
`handle_update_spot_market_status` carries `spot_market_valid` itself and makes `Delisted`
terminal. The paths that move no tokens work again, namely `request_withdraw`,
`cancel_withdraw_request`, `apply_rebase`, `apply_profit_share` and `liquidate`. The
token-moving paths still fail, because velocity's own withdraw admits only `Active`,
`ReduceOnly` and `Settlement` (`controller/spot_position.rs:159`), and that gate is unchanged.
Nothing about delisted markets changes, since `deposit`, `force_delete_user` and
`resolve_spot_bankruptcy` already book interest on one, so the guard blocked callers without
stopping the accrual.

Isolated perp positions: their collateral prices through the perp market's quote spot market,
which the position does not name, so the market list reads it off the perp market accounts
already present for the equity walk. That walk runs only when the user holds an isolated
position.

Instruction accounts changed (ABI, §5.4): 20 vault instructions, `manager_update_fees`
included. No account-layout or error-code change.

#### vault-share-pricing-hardening

Fixes four High audit findings on vault share pricing.

#91, #92 and #93 share a cause. Builder and referral rewards owed to a vault PDA accrue in
arbitrary third-party escrows and only enter the vault-owned Velocity User's equity via a
permissionless revenue-share sweep, at an attacker-controlled time. That enables late-entrant
dilution (#91), a stranded pending withdrawer (#92), and a reward donation that burns a
canceller's claim (#93). Since the vault cannot enumerate those rewards, and no legitimate flow
has a vault earn revenue share because a third party can name a vault PDA as their builder with
no signature, the reward is blocked at the source. The new `UserStatus::VaultOwned` bit
(SDK `UserStatus.VAULT_OWNED = 32`) marks a vault-owned User. The vaults program sets it at
`initialize_vault` via a new CPI to the new velocity instruction `update_user_vault_owned`,
which is CPI-only, authority-gated and set-only. And `sweep_completed_revenue_share_for_market`
now skips crediting a vault-owned User, draining the liability counter and clearing the row
without transferring, so the reward stays in the market's pnl pool and never enters vault NAV.

Defense in depth: `VaultDepositor::deposit` rejects a positive deposit that mints zero shares,
and `WithdrawRequest::calculate_shares_lost` rejects a cancel that would floor a positive
claim's retained shares to zero on an equity increase.

#94: `Vault::calculate_equity` fetches the denomination-market oracle with
`get_price_data_and_validity` plus `VelocityAction::MarginCalc`, instead of a raw unchecked
`get_price_data`, so a stale-high denomination oracle can no longer shrink NAV and overmint
shares when the vault holds no denomination position.

New `update_user_vault_owned` instruction and `UserStatus::VaultOwned` in the IDL, and
`UserStatus.VAULT_OWNED` in the SDK. No account-layout change, since `VaultOwned` reuses a
spare `status` bit and existing accounts read 0, and no error-code change.

#### vaults-fee-rebase-hardening

Fixes eleven Medium audit findings in the `vaults` program, covering fee, rebase and share
accounting.

- #96: a management-fee-only rebase now scales protocol shares, by passing the real
  `vault_protocol`.
- #97: a shared `validate_fee_policy` enforces the init bounds when queueing and on both
  maturity paths (`apply_fee` and the pending branch of `manager_update_fees`), so the
  timelocked fee-update path cannot install an out-of-bounds policy. For protocol vaults,
  `manager_update_fees` with a pending update now takes the `VaultProtocol` account in
  `remaining_accounts`, and SDK `getManagerUpdateFeesIx` appends it.
- #98: a matured fee update stamps `last_fee_update_ts` to the activation instant, so rate
  epochs are clean and a raised rate never prices the pre-activation interval. Replaced by
  `vaults-fee-policy-grandfathering`.
- #99: `apply_rebase` scales `last_protocol_withdraw_request.shares`, so no protocol request is
  stranded.
- #100: `redeem_tokens` snapshots a complete share domain including protocol shares, and keeps
  the `VaultProtocol` provider alive across the conservation check.
- #101: `transfer`, `tokenize` and `redeem` thread and apply a matured `FeeUpdate`, closing a
  basis-reset escape.
- #102: the protocol-vault combined fee is capped to `equity - 1` before splitting, so accrual
  cannot underflow and freeze public actions.
- #104: a positive profit-share fee that floors to zero shares is deferred, so
  `apply_profit_share` transfers nothing and leaves the high-water mark and
  `profit_share_fee_paid` untouched, charging the fee later once accrued profit makes it worth
  at least one share. Previously it was rounded up to a whole share, which could confiscate
  value far exceeding the fee at a high share price.
- #105: `redeem_tokens` refreshes the tokenized depositor's `last_vault_shares` checkpoint
  post-transfer, so future `tokenize_shares` calls are no longer bricked.
- #106: the signerless `apply_rebase` rejects flooring an active depositor's, or a pending
  request's, shares to zero.
- #107: depositors and pending requests are re-synced after a fee-induced vault rebase, so
  there is no `InvalidVaultRebase` freeze.
- #95 (a late reward creating manager shares) is closed at the root by the #307 revenue-share
  sweep block.

Program-internal only, with no account-layout, IDL or error-code change, since it reuses
`InvalidVaultUpdate` and `InvalidVaultRebase`.

#### withdraw-breaker-exception-budget

Fixes an audit finding (OtterSec #150) on the spot withdraw circuit breaker, plus two gaps in
the same subsystem.

`check_user_exception_to_withdraw_limits` let any account whose pre-withdraw position in a
market was below `withdraw_guard_threshold / 10` override the market-level breaker outright,
with no aggregate accounting. A per-account allowance therefore overrode a market-level limit,
so N prepared subaccounts got N bypasses. At the shipped quote-market (USDT) config, with
`withdraw_guard_threshold = 9_500_000_000`, that is a 950 token allowance each and about 527
subaccounts to drain the 500,000 token market past a tripped breaker, at refundable rent.

`check_withdraw_limits` now treats that predicate as an eligibility filter only, and bounds the
whole eligible cohort with a market-level
`exception_floor = min_deposit_token - withdraw_guard_threshold`. The exception may take a
market one `withdraw_guard_threshold` below the breaker floor and no further, however many
accounts join. `validate_withdraw_guard_threshold` caps that field at
`MAX_WITHDRAW_GUARD_THRESHOLD_NOTIONAL` ($10k notional) on both `initialize_spot_market` and
`update_withdraw_guard_threshold`, so total exception outflow per market per TWAP window is
bounded at $10k, and the budget regenerates as the deposit TWAP decays. Note that
`withdraw_circuit_breaker_bps` never reached the exception before this change, so tightening
the breaker did not shrink the bypass.

No instruction-signature, account-layout or IDL change. SDK: `calculateWithdrawLimit` returns a
new `exceptionWithdrawLimit`, the shared budget, always at least `withdrawLimit`, and
`User.getWithdrawalLimit` caps the bypass by it. Previously it raised the withdraw limit to the
user's full deposit unconditionally, over-predicting a withdrawal that reverts with
`DailyWithdrawLimit` (6128). `User.canBypassWithdrawLimits` keeps its behavior but is
documented as eligibility only. The `AdminClient` doc comments for `withdrawGuardThreshold` are
corrected to say that it is the level below which the withdraw guards stop binding, rather than
a cap above which withdraws are blocked.

The same PR closes the second half of the finding. `withdraw_from_isolated_perp_position` sent
tokens out of the spot market vault with the bare balance helpers and never called
`check_withdraw_limits`, so a single account of any size defeated the breaker with no split
needed. It now applies the market-level check, with `user = None` so the small-depositor
exception does not apply to an isolated balance, and reverts with `DailyWithdrawLimit` (6128).
This was latent on mainnet, because all three isolated-position instructions are behind the
`isolated-position` feature, which is not in the mainnet default feature set. The sibling
`transfer_isolated_perp_position_deposit` stays unchecked by design and now documents the
reason. It is vault-neutral, both legs land inside one market, and the breaker rate limits
vault outflow.
SDK: `getWithdrawFromIsolatedPerpPositionIxsBundle` documents the new `DailyWithdrawLimit`
failure mode, and its clamp is still against the position's own balance and does not shrink to
the market limit.

Two further gaps in the same subsystem are closed. `withdraw_from_isolated_perp_position`
applies the per-market withdraw status gate and the `SpotOperation::Withdraw` pause gate
(`MarketWithdrawPaused`, 6149), copying the cross path's admitted set of
`Active | ReduceOnly | Settlement`, so a market closed for withdrawals is closed on every route
out of the vault while a wound-down market in `Settlement` stays exitable. And
`deposit_into_isolated_perp_position` applies `check_deposit_limits` (`DailyDepositLimit`,
6364), so the daily deposit cap can no longer be stepped around by depositing through an
isolated position. No new `Error` variants. SDK: JSDoc on `depositIntoIsolatedPerpPosition` and
the withdraw bundle records both revert codes.

---

## 7. Migration checklist

1. **Swap the dependency**: `@drift-labs/sdk` to `@velocity-exchange/sdk`.
2. **Find and replace the renames** (§4.2). There are no runtime aliases, so TypeScript will
   surface every site as a compile error.
3. **Update the program ID** everywhere it is hardcoded:
   `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P`.
4. **Re-derive all PDAs and cached addresses.** Nothing derived against the Drift program ID is
   valid on Velocity. User accounts must be re-initialized, and balances do not migrate.
5. **Replace the IDL** if you load it yourself. Use `sdk/src/idl/velocity.json`, which is
   Anchor 1.0 format, with an Anchor 1.0 client. Update any string-keyed coder calls to the new
   camelCase account names (§4.1).
6. **Wrap `oraclePriceOffset` values in `BN`.** Limit orders no longer accept an offset at all
   (§2), so move oracle-floating maker quotes to a PropAMM quoter or a repriced fixed limit.
7. **Delete integrations with removed features** (§2): spot DLOB orders, Serum, Phoenix and
   OpenBook fulfillment, fuel, LP shares, protected maker, high leverage mode, prediction
   markets, Switchboard and Pyth-pull oracles, the gov-token stake fee discount, the perp DLOB
   and order auctions, and jit-proxy. Drop the `@velocity-exchange/jit-proxy` dependency. There
   is no successor package, so rest orders on the CLOB instead.
8. **Update account decoders and indexers** to the new `User`, `PerpMarket`, `SpotMarket`,
   `State` and `UserStats` layouts and the shifted `MarketStatus` discriminants (§5).
   Discriminators match Drift's, so guard by program ID rather than by discriminator.
9. **Stop assuming fixed account data lengths.** `PerpMarket` and `SpotMarket` carry 256
   reserved bytes, and `extend_account` can grow any zero-copy account after a program upgrade.
   Drop `dataSize` filters and exact-size decodes (§3, §5.1).
10. **Re-test error handling.** Codes are stable, but retired codes now decode to `Deprecated*`
    names and new codes exist past the old end of the enum, through 6458 (§5.6).
11. **Adopt builder codes** (optional). Approve builders via `changeApprovedBuilder(...)` and
    set `builderIdx` and `builderFeeTenthBps` on `OrderParams`. No action is needed if you do
    not use builders. If you are a filler, attach the taker's `RevenueShareEscrow` in remaining
    accounts when the taker has a builder order or a referred escrow (§3, fill-time
    enforcement).
12. **Re-pull the IDL and types** for the fee redesign. `PerpMarket` is now 1560 bytes and the
    fee fields live in `feeLedger` (§4.4, §5.1). IF stakers receive 100% of settled revenue,
    with no protocol share mint. If you index fees, the authoritative flow description is
    [`FEES.md`](./FEES.md).
