# Velocity Protocol v1 architecture

A navigation map for `programs/velocity` and `packages/sdk`. It covers which module owns what,
six worked execution flows, where each account type is defined, the design patterns you will hit
immediately, and how SDK methods map onto program instructions. Use it to find the right file
before you start reading code. It is not an exhaustive index of the program. The line numbers
drift with every change, so treat them as a starting point.

---

## Module responsibility matrix

| Module | Owns | Does not own |
|---|---|---|
| `programs/velocity/src/instructions/` | Account constraint structs, Anchor deserialization, input validation, delegation to `controller` | Business logic, math |
| `programs/velocity/src/controller/` | Stateful mutations: fills, liquidations, position updates, funding | Account loading, which `instructions` does, and pure math |
| `programs/velocity/src/math/` | Pure numeric functions: margin, fees, funding, oracle checks | Any account I/O or state mutation |
| `programs/velocity/src/state/` | Account struct definitions and their accessor and mutation methods | Instruction routing, math |
| `programs/velocity/src/validation/` | Precondition checks, called by `instructions` before `controller` | Post-trade checks, which live in `math/margin` |
| `programs/velocity/src/vlp/` | The Velocity Liquidity Provider product: the constant-product vAMM (`vlp/amm/`), the LP pool that hedges its inventory (`vlp/hedge/`), and the `amm_cache` that bridges them | Order routing, user accounting |

---

## Execution flows

Each flow lists the ordered call chain from the instruction entry point down to the state write.

### Place and take perp order

1. A user calls `place_and_take_perp_order_v1`, which enters the program at `lib.rs:306`.
2. `handle_place_and_take_perp_order_v1` (`instructions/clob/place_and_take_v1.rs:82`) runs under
   the `PlaceAndTakeV1` accounts context (`instructions/clob/place_and_take_v1.rs:31`). It
   delegates to `place_and_take_perp_order_v1` (`instructions/user/place_and_take.rs:484`).
3. `create_detached_take` (`instructions/user/place_and_take.rs:179`) calls
   `controller::orders::create_detached_perp_order` (`controller/orders/placement.rs:685`). That
   checks `math::liquidation::validate_user_not_being_liquidated` (`math/liquidation.rs:284`),
   sizes and standardizes the order with `math::orders::calculate_max_perp_order_size`
   (`math/orders.rs:717`) and `math::orders::standardize_base_asset_amount`
   (`math/orders.rs:233`), and approves it with `validation::order::validate_order`
   (`validation/order.rs:21`).
4. The order is detached. It is built on the stack and never written into `User.orders`. It is
   admitted as if it rested: `math::margin::meets_place_order_margin_requirement`
   (`math/margin.rs:806`) runs at `controller/orders/placement.rs:91`, and `emit_place_records`
   (`controller/orders/placement.rs:625`) emits `OrderActionRecord` and `OrderRecord`.
5. `fill_detached_take` (`instructions/user/place_and_take.rs:359`) quotes the market's quoters
   through `instructions::router::quote_route` (`instructions/router/quoted_route.rs:258`), then
   calls `controller::orders::fill_perp_order` (`controller/orders/perp_fill/order.rs:226`). When
   `synchronous_take_allowed` (`instructions/clob/helpers/placement.rs:157`) refuses the take, the
   fill is skipped and the whole order rests.
6. `fill_within_taker_risk_limits` (`controller/orders/perp_fill/taker_risk.rs:62`) runs the gates
   before the fill. `fill_from_liquidity_sources` (`controller/orders/perp_fill/liquidity.rs:1281`)
   quotes the vAMM and every quoter book, splits the size with `math::router::split_across_quoters`
   (`math/router.rs:267`), and settles each source. After the fill,
   `TakerRiskLimits::check_after_fill` (`controller/orders/perp_fill/taker_risk.rs:275`) runs
   `math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info`
   (`math/margin.rs:339`) for the taker.
7. Settlement lives in `controller/orders/settle.rs`. `controller::position::update_position_and_market`
   (`controller/position.rs:96`) applies each position change, and `emit_perp_action_record`
   (`controller/orders/settle.rs:77`) emits the `OrderActionRecord` event, whose struct is at
   `state/events.rs:234`. A fill settles only for makers whose `(User, UserStats)` pair the
   transaction carries.
8. `settle_take_remainder` (`instructions/user/place_and_take.rs:425`) rests the unfilled remainder
   of an order that is not immediate-or-cancel on the market's CLOB, through
   `try_place_remainder_on_clob` (`instructions/clob/helpers/placement.rs:284`).

### Place and make perp order

1. A maker calls `place_and_make_perp_order_v1`, which enters at `lib.rs:318`.
2. `handle_place_and_make_perp_order_v1` (`instructions/clob/place_and_make_v1.rs:81`) runs under
   the `PlaceAndMakeV1` accounts context (`instructions/clob/place_and_make_v1.rs:32`).
3. It builds and admits the order through `controller::orders::create_detached_perp_order`, as in
   steps 3 and 4 of the take flow.
4. `try_place_remainder_on_clob` (`instructions/clob/helpers/placement.rs:284`) rests the whole
   order on the market's CLOB as a maker quote. The order never enters `User.orders` and matches
   nothing on placement. A later taker removes it from the book.

### Liquidate perp

1. A keeper calls `liquidate_perp`, which enters at `lib.rs:556`.
2. `instructions::keeper::handle_liquidate_perp` (`instructions/keeper/liquidation.rs:12`) runs
   under the `LiquidatePerp` accounts context (`instructions/keeper/liquidation.rs:660`).
3. It delegates to `controller::liquidation::liquidate_perp` (`controller/liquidation.rs:121`).
4. `math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info`
   (`math/margin.rs:339`), called with `MarginContext::liquidation(...)`, confirms the account is
   liquidatable (`controller/liquidation.rs:208`).
5. The liquidation math sizes the action: `math::liquidation::calculate_perp_if_fee`
   (`math/liquidation.rs:476`),
   `math::liquidation::calculate_base_asset_amount_to_cover_margin_shortage`
   (`math/liquidation.rs:36`), and `LiquidationMode::calculate_max_pct_to_liquidate`
   (`math/liquidation.rs:438`).
6. `controller::position::update_position_and_market` (`controller/position.rs:96`) applies the
   transfer to the user (`controller/liquidation.rs:578`) and then the liquidator
   (`controller/liquidation.rs:598`).
7. A `LiquidationRecord` event (`state/events.rs:445`) is emitted at
   `controller/liquidation.rs:776`.

### Settle PnL

1. A keeper calls `settle_pnl`, which enters at `lib.rs:528`.
2. `instructions::keeper::handle_settle_pnl` (`instructions/keeper/settle_pnl.rs:13`) runs under
   the `SettlePNL` accounts context (`instructions/keeper/settle_pnl.rs:335`).
3. It delegates to `controller::pnl::settle_pnl` (`controller/pnl.rs:69`).
4. That brings the accounts current with
   `controller::spot_balance::update_spot_market_cumulative_interest`
   (`controller/spot_balance.rs:153`) and `controller::funding::settle_funding_payment`
   (`controller/funding.rs:50`), then requires
   `math::margin::meets_settle_pnl_maintenance_margin_requirement` (`math/margin.rs:865`).
5. `vlp::amm::math::amm::calculate_net_user_pnl` (`vlp/amm/math/amm.rs:462`) values the position,
   and `controller::perp_pools::update_pool_balances` (`controller/perp_pools.rs:169`) moves the
   market's pnl pool.
6. The user side is written by `controller::spot_balance::update_spot_balances`
   (`controller/spot_balance.rs:341`), `controller::position::update_quote_asset_amount`
   (`controller/position.rs:406`), and `controller::position::update_settled_pnl`
   (`controller/position.rs:488`).
7. A `SettlePnlRecord` event (`state/events.rs:556`) is emitted at `controller/pnl.rs:414`.

### Update funding rate

1. A keeper calls `update_funding_rate`, which enters at `lib.rs:690`.
2. `instructions::keeper::handle_update_funding_rate` (`instructions/keeper/funding.rs:18`) runs
   under the `UpdateFundingRate` accounts context (`instructions/keeper/funding.rs:463`).
3. It delegates to `controller::funding::update_funding_rate` (`controller/funding.rs:221`).
4. `controller::funding::refresh_amm_for_funding_gate` (`controller/funding.rs:179`) refreshes the
   AMM and checks the oracle gate, and `math::helpers::on_the_hour_update` (`math/helpers.rs:73`)
   enforces the once-an-hour cadence.
5. `math::funding::calculate_funding_premium_with_offset` (`math/funding.rs:79`) produces the
   premium, and `math::funding::calculate_funding_rate_long_short` (`math/funding.rs:112`) splits
   it across the two sides.
6. The result is written onto `PerpMarket`: `cumulative_funding_rate_long` and
   `cumulative_funding_rate_short` (`controller/funding.rs:425` and `428`), `last_funding_rate`,
   `last_funding_rate_long`, `last_funding_rate_short`, `net_unsettled_funding_pnl` and
   `last_funding_rate_ts` (`controller/funding.rs:467` to `481`), plus the `market_stats` TWAP
   fields. The AMM side goes through `AmmQuoter::for_amm(...).on_market_event(...)`
   (`controller/funding.rs:464`).
7. A `FundingRateRecord` event (`state/events.rs:162`) is emitted at `controller/funding.rs:488`.

---

## Fee and revenue flow

[docs/FEES.md](./docs/FEES.md) holds the full fee and revenue documentation: the per-fee flow, the
pool-movement diagrams, the protocol and staker insurance-fund split, and a snapshot of the old
program's per-market fee parameters.

---

## Account type locations

| Type | File | Notes |
|---|---|---|
| `User` | `state/user.rs:88` | Zero-copy, loaded with `AccountLoader`. Holds positions, open orders, margin info. |
| `UserStats` | `state/user.rs:2032` | Companion to `User`. Tracks volume, fees, referrals. |
| `PerpMarket` | `state/perp_market.rs:231` | Zero-copy. Embeds the `AMM` struct. |
| `SpotMarket` | `state/spot_market.rs:44` | Zero-copy. Tracks deposits, borrows, oracle, insurance. |
| `State` | `state/state.rs:35` | Global protocol config: fees, admin pubkey, market counts. |
| `InsuranceFundStake` | `state/insurance_fund_stake.rs:14` | Per-user insurance fund stake position. |
| `OracleMap` | `state/oracle_map.rs:54` | Per-instruction oracle account loader, built from `remaining_accounts`. |
| `OrderParams` | `state/order_params.rs:22` | Shared input struct for the place and modify order instructions. |
| All events | `state/events.rs` | `OrderActionRecord`, `DepositRecord`, `LiquidationRecord`, `FundingRateRecord`, `SettlePnlRecord`, and the rest. |

---

## Key design patterns

### Custom high-frequency entrypoint

`program_entry` (`lib.rs:59`) inspects the instruction data before Anchor sees it. Data starting
with `[0xFF, 0xFF, 0xFF, 0xFF, opcode]` goes to a native handler that skips Anchor's account
deserialization. Three opcodes are wired up today: `0` for
`handle_update_mm_oracle_native`, `1` for `handle_update_amm_spread_adjustment_native`, and `2`
for `handle_update_mm_oracle_batch_native`. Everything else, including the rest of the keeper
surface, falls through to the standard Anchor `#[program]` entry.

### `remaining_accounts` convention

Variable-length account lists arrive through `remaining_accounts` rather than fixed Anchor context
fields:

- Oracles: one oracle account per market the instruction references.
- Spot markets: for instructions that touch multiple spot positions.
- Maker accounts: a `(User, UserStats)` pair for every maker a fill settles against. A fill
  settles only for users the transaction carries, so a book stops at the first maker it was not
  handed.
- Referrer: an optional `(User, UserStats)` pair at the end.

### Zero-copy account loading

`User`, `PerpMarket`, and `SpotMarket` load through `AccountLoader<'info, T>`. Call `.load()` or
`.load_mut()` instead of deserializing directly, which is what keeps these large structs off the
stack.

### Feature flags

Default features are `mainnet-beta` and `no-entrypoint` (`programs/velocity/Cargo.toml:26`).

| Flag | Purpose |
|---|---|
| `mainnet-beta` | Production gates: mainnet `ids.rs` constants, devnet-only instructions compiled out |
| `anchor-test` | Enables the test helper instructions the TypeScript integration tests use. Implies `isolated-position` and `vlp-hedge` |
| `no-entrypoint` | Compiles out the Anchor `#[program]` entrypoint, for use as a library dependency |
| `cpi` | Exposes the CPI client only. Implies `no-entrypoint` |
| `isolated-position` | The isolated perp position instruction surface, held back from mainnet builds pending audit |
| `vlp-hedge` | The VLP hedge and LP-pool instruction surface, held back from mainnet builds pending audit |
| `fuzz-fixtures` | Exposes `pub mod test_utils` to the host crates in `fuzz/`. Never enabled by an SBF build |

---

## SDK structure (`packages/sdk/src/`)

### Key files

Line counts are approximate and drift with every change.

| File | Size | Purpose |
|---|---|---|
| `velocityClient.ts` | ~14.8k lines | Main client. All trading and keeper instruction builders. |
| `adminClient.ts` | ~9.1k lines | Admin instruction builders. Extends `VelocityClient`. |
| `user.ts` | ~5.7k lines | `User` account abstraction: margin queries, position accessors, PnL. |
| `types.ts` | ~3.3k lines | Shared TypeScript types mirroring the on-chain structs. |
| `idl/velocity.json` | generated | Anchor IDL. The source of truth for instruction interfaces and account layouts. Do not edit it by hand. |

### Key directories

| Directory | Purpose |
|---|---|
| `accounts/` | Account subscription infrastructure: WebSocket, polling, bulk loaders. |
| `addresses/` | `pda.ts` holds every PDA derivation helper. |
| `clob/` | The user-orders feed client for orders resting on a CLOB. |
| `orderBookLevels.ts` | The `L2` and `L3` book shapes the dlob-server serves, plus `groupL2` and `uncrossL2`. The SDK builds no book. The rust `book-publisher` quotes every source through the program's router view. |
| `math/` | TypeScript mirrors of the on-chain math: margin, funding, AMM pricing. |
| `oracles/` | Oracle client adapters for Pyth, Pyth Lazer, Prelaunch, and QuoteAsset. |
| `events/` | Event parsing and subscription from program logs. |
| `tx/` | Transaction building utilities and compute unit estimation. |
| `constants/` | Market indices, precision constants, numeric limits. |

### SDK method to on-chain instruction mapping

| SDK method | Program instruction | Accounts context | Handler file |
|---|---|---|---|
| `velocityClient.placeAndTakePerpOrder` | `place_and_take_perp_order_v1` | `PlaceAndTakeV1` | `instructions/clob/place_and_take_v1.rs` |
| `velocityClient.placeAndMakePerpOrder` | `place_and_make_perp_order_v1` | `PlaceAndMakeV1` | `instructions/clob/place_and_make_v1.rs` |
| `velocityClient.cancelOrderV1` | `cancel_order_v1` | `CancelOrderV1` | `instructions/clob/cancel_order_v1.rs` |
| `velocityClient.modifyOrderV1` | `modify_order_v1` | `ModifyOrderV1` | `instructions/clob/modify_order_v1.rs` |
| `velocityClient.cancelOrder` | `cancel_order` | `CancelOrder` | `instructions/user/orders.rs` |
| `velocityClient.modifyOrder` | `modify_order` | `CancelOrder` | `instructions/user/orders.rs` |
| `velocityClient.deposit` | `deposit` | `Deposit` | `instructions/user/deposit.rs` |
| `velocityClient.withdraw` | `withdraw` | `Withdraw` | `instructions/user/deposit.rs` |
| `velocityClient.settlePNL` | `settle_pnl` | `SettlePNL` | `instructions/keeper/settle_pnl.rs` |
| `velocityClient.liquidatePerp` | `liquidate_perp` | `LiquidatePerp` | `instructions/keeper/liquidation.rs` |
| `velocityClient.updateFundingRate` | `update_funding_rate` | `UpdateFundingRate` | `instructions/keeper/funding.rs` |
| `adminClient.initializePerpMarket` | `initialize_perp_market` | `InitializePerpMarket` | `instructions/admin.rs` |
| `adminClient.updatePerpMarket*` | `update_perp_market_*` | `AdminUpdatePerpMarket` and friends | `instructions/admin.rs` |
| `adminClient.updateOracleGuardRails` | `update_oracle_guard_rails` | `AdminUpdateState` | `instructions/admin.rs` |

The settle-PnL method is spelled `settlePNL`, not `settlePnl`. Related methods follow the same
casing: `settlePNLs`, `settleMultiplePNLs`.

---

## Ancillary programs

These are type definitions and test utilities. No core protocol logic lives in them.

| Program | Purpose |
|---|---|
| `programs/pyth-lazer/` | Pyth Lazer type definitions, linked into `velocity` as a real library dependency rather than a CPI target |
| `programs/pyth/` | Pyth V1 oracle account layout definitions. An optional dependency, pulled in only by `velocity`'s `fuzz-fixtures` feature and by tests |
| `programs/token_faucet/` | Devnet and test token minting utility. Not on mainnet |

Switchboard oracle support and the external spot-fulfillment venues (Serum, Phoenix, OpenBook)
have been removed, so there is no `programs/switchboard*` or `programs/openbook_v2`. `OracleSource`
keeps its `DeprecatedSwitchboard` and `DeprecatedSwitchboardOnDemand` variants only to preserve the
ABI discriminants.

---

## Build and test quick reference

`CLAUDE.md` has the full command list. These are the entry points you need most:

```bash
# Verify program compiles after Rust changes
cargo build -p velocity

# Run Rust unit tests
cargo test -p velocity

# Run a single TS integration test
ts-mocha -t 300000 ./tests/<test_file>.ts

# Full TS integration suite
bash test-scripts/run-anchor-tests.sh --skip-build
```
