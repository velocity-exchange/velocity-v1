# Velocity Protocol v1 architecture

A navigation map for `programs/velocity` and `packages/sdk`. It covers which module owns what,
five worked execution flows, where each account type is defined, the design patterns you will hit
immediately, and how SDK methods map onto program instructions. Use it to find the right file
before you start reading code. It is not an exhaustive index of the program.

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

### Place perp order

1. A user calls `place_perp_order`, which enters the program at `lib.rs:241`.
2. `instructions::user::handle_place_perp_order` (`instructions/user.rs:2710`) runs under the
   `PlaceOrder` accounts context (`instructions/user.rs:5417`).
3. It delegates to `controller::orders::place_perp_order` (`controller/orders.rs:109`).
4. That checks `math::liquidation::validate_user_not_being_liquidated`
   (`math/liquidation.rs:291`), then sizes and standardizes the order with
   `math::orders::calculate_max_perp_order_size` (`math/orders.rs:753`) and
   `math::orders::standardize_base_asset_amount` (`math/orders.rs:238`), and derives auction
   parameters with `controller::orders::get_auction_params` (`controller/orders.rs:513`).
5. `validation::order::validate_order` (`validation/order.rs:21`) approves the finished order.
6. The order is written into the first free slot on the `User` account by direct assignment,
   `user.orders[new_order_index] = new_order` (`controller/orders.rs:396`). There is no
   `add_order` method; the free slot comes from an inline `is_available()` scan at
   `controller/orders.rs:156`.
7. The handler then calls `controller::position::increase_open_bids_and_asks`
   (`controller/position.rs:600`), confirms
   `math::margin::meets_place_order_margin_requirement` (`math/margin.rs:821`), and emits
   `OrderActionRecord` and `OrderRecord` through `state::events::emit_stack`
   (`state/events.rs:674`).

### Fill perp order (keeper crank)

1. A keeper calls `fill_perp_order`, which enters at `lib.rs:477`.
2. `instructions::keeper::handle_fill_perp_order` (`instructions/keeper.rs:110`) runs under the
   `FillOrder` accounts context (`instructions/keeper.rs:3927`).
3. It delegates to `controller::orders::fill_perp_order` (`controller/orders.rs:1046`).
4. `controller::orders::get_maker_orders_info` (`controller/orders.rs:1669`) selects the makers.
5. `controller::orders::fulfill_perp_order` (`controller/orders.rs:2045`) orchestrates the fill.
   It runs the margin check through
   `math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info`
   (`math/margin.rs:358`) for the taker (`controller/orders.rs:2452`) and each maker
   (`controller/orders.rs:2602`).
6. `math::fulfillment::determine_perp_fulfillment_methods` (`math/fulfillment.rs:16`) picks the
   methods, and `controller::orders::fulfill_perp_order_step` (`controller/orders.rs:3501`)
   executes each one, settling through `settle_amm_house_fill` (`controller/orders.rs:2869`) or
   `settle_dlob_match_fill` (`controller/orders.rs:3172`).
7. `controller::position::update_position_and_market` (`controller/position.rs:101`) applies the
   position change, then `controller::orders::update_order_after_fill`
   (`controller/orders.rs:3969`) and `controller::position::decrease_open_bids_and_asks`
   (`controller/position.rs:626`) do the bookkeeping.
8. `controller::orders::emit_perp_action_record` (`controller/orders.rs:2807`) emits the
   `OrderActionRecord` event, whose struct is at `state/events.rs:233`.

### Liquidate perp

1. A keeper calls `liquidate_perp`, which enters at `lib.rs:565`.
2. `instructions::keeper::handle_liquidate_perp` (`instructions/keeper.rs:1286`) runs under the
   `LiquidatePerp` accounts context (`instructions/keeper.rs:4085`).
3. It delegates to `controller::liquidation::liquidate_perp` (`controller/liquidation.rs:99`).
4. `math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info`
   (`math/margin.rs:358`), called with `MarginContext::liquidation(...)`, confirms the account is
   liquidatable (`controller/liquidation.rs:199`).
5. The liquidation math sizes the action: `math::liquidation::calculate_perp_if_fee`
   (`math/liquidation.rs:491`),
   `math::liquidation::calculate_base_asset_amount_to_cover_margin_shortage`
   (`math/liquidation.rs:38`), and `LiquidationMode::calculate_max_pct_to_liquidate`
   (`math/liquidation.rs:453`).
6. `controller::position::update_position_and_market` (`controller/position.rs:101`) applies the
   transfer to the user (`controller/liquidation.rs:568`) and then the liquidator
   (`controller/liquidation.rs:588`).
7. A `LiquidationRecord` event (`state/events.rs:433`) is emitted at
   `controller/liquidation.rs:770`.

### Settle PnL

1. A keeper calls `settle_pnl`, which enters at `lib.rs:537`.
2. `instructions::keeper::handle_settle_pnl` (`instructions/keeper.rs:962`) runs under the
   `SettlePNL` accounts context (`instructions/keeper.rs:4039`).
3. It delegates to `controller::pnl::settle_pnl` (`controller/pnl.rs:71`).
4. That brings the accounts current with
   `controller::spot_balance::update_spot_market_cumulative_interest`
   (`controller/spot_balance.rs:158`) and `controller::funding::settle_funding_payment`
   (`controller/funding.rs:50`), then requires
   `math::margin::meets_settle_pnl_maintenance_margin_requirement` (`math/margin.rs:888`).
5. `vlp::amm::math::amm::calculate_net_user_pnl` (`vlp/amm/math/amm.rs:459`) values the position,
   and `controller::perp_pools::update_pool_balances` (`controller/perp_pools.rs:211`) moves the
   market's pnl pool.
6. The user side is written by `controller::spot_balance::update_spot_balances`
   (`controller/spot_balance.rs:377`), `controller::position::update_quote_asset_amount`
   (`controller/position.rs:498`), and `controller::position::update_settled_pnl`
   (`controller/position.rs:579`).
7. A `SettlePnlRecord` event (`state/events.rs:544`) is emitted at `controller/pnl.rs:426`.

### Update funding rate

1. A keeper calls `update_funding_rate`, which enters at `lib.rs:699`.
2. `instructions::keeper::handle_update_funding_rate` (`instructions/keeper.rs:2662`) runs under
   the `UpdateFundingRate` accounts context (`instructions/keeper.rs:4445`).
3. It delegates to `controller::funding::update_funding_rate` (`controller/funding.rs:218`).
4. `controller::funding::refresh_amm_for_funding_gate` (`controller/funding.rs:177`) refreshes the
   AMM and checks the oracle gate, and `math::helpers::on_the_hour_update` (`math/helpers.rs:73`)
   enforces the once-an-hour cadence.
5. `math::funding::calculate_funding_premium_with_offset` (`math/funding.rs:79`) produces the
   premium, and `math::funding::calculate_funding_rate_long_short` (`math/funding.rs:112`) splits
   it across the two sides.
6. The result is written onto `PerpMarket`: `cumulative_funding_rate_long` and
   `cumulative_funding_rate_short` (`controller/funding.rs:422` and `425`), `last_funding_rate`,
   `last_funding_rate_long`, `last_funding_rate_short`, `net_unsettled_funding_pnl` and
   `last_funding_rate_ts` (`controller/funding.rs:464` to `478`), plus the `market_stats` TWAP
   fields. The AMM side goes through `AmmQuoter::for_amm(...).on_market_event(...)`
   (`controller/funding.rs:461`).
7. A `FundingRateRecord` event (`state/events.rs:161`) is emitted at `controller/funding.rs:485`.

---

## Fee and revenue flow

[docs/FEES.md](./docs/FEES.md) holds the full fee and revenue documentation: the per-fee flow, the
pool-movement diagrams, the protocol and staker insurance-fund split, and a snapshot of the old
program's per-market fee parameters.

---

## Account type locations

| Type | File | Notes |
|---|---|---|
| `User` | `state/user.rs:89` | Zero-copy, loaded with `AccountLoader`. Holds positions, open orders, margin info. |
| `UserStats` | `state/user.rs:1915` | Companion to `User`. Tracks volume, fees, referrals. |
| `PerpMarket` | `state/perp_market.rs:243` | Zero-copy. Embeds the `AMM` struct. |
| `SpotMarket` | `state/spot_market.rs:44` | Zero-copy. Tracks deposits, borrows, oracle, insurance. |
| `State` | `state/state.rs:35` | Global protocol config: fees, admin pubkey, market counts. |
| `InsuranceFundStake` | `state/insurance_fund_stake.rs:14` | Per-user insurance fund stake position. |
| `OracleMap` | `state/oracle_map.rs:50` | Per-instruction oracle account loader, built from `remaining_accounts`. |
| `OrderParams` | `state/order_params.rs:32` | Shared input struct for the place and modify order instructions. |
| All events | `state/events.rs` | `OrderActionRecord`, `DepositRecord`, `LiquidationRecord`, `FundingRateRecord`, `SettlePnlRecord`, and the rest. |

---

## Key design patterns

### Custom high-frequency entrypoint

`program_entry` (`lib.rs:53`) inspects the instruction data before Anchor sees it. Data starting
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
- Maker accounts: a `(User, UserStats)` pair for each DLOB maker in a fill.
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
| `velocityClient.ts` | ~15.6k lines | Main client. All trading and keeper instruction builders. |
| `adminClient.ts` | ~8.7k lines | Admin instruction builders. Extends `VelocityClient`. |
| `user.ts` | ~5.8k lines | `User` account abstraction: margin queries, position accessors, PnL. |
| `types.ts` | ~2.7k lines | Shared TypeScript types mirroring the on-chain structs. |
| `idl/velocity.json` | generated | Anchor IDL. The source of truth for instruction interfaces and account layouts. Do not edit it by hand. |

### Key directories

| Directory | Purpose |
|---|---|
| `accounts/` | Account subscription infrastructure: WebSocket, polling, bulk loaders. |
| `addresses/` | `pda.ts` holds every PDA derivation helper. |
| `dlob/` | Decentralized limit order book: order matching, price levels, maker selection. |
| `math/` | TypeScript mirrors of the on-chain math: margin, funding, AMM pricing. |
| `oracles/` | Oracle client adapters for Pyth, Pyth Lazer, Prelaunch, and QuoteAsset. |
| `events/` | Event parsing and subscription from program logs. |
| `tx/` | Transaction building utilities and compute unit estimation. |
| `constants/` | Market indices, precision constants, numeric limits. |

### SDK method to on-chain instruction mapping

| SDK method | Program instruction | Accounts context | Handler file |
|---|---|---|---|
| `velocityClient.placePerpOrder` | `place_perp_order` | `PlaceOrder` | `instructions/user.rs` |
| `velocityClient.cancelOrder` | `cancel_order` | `CancelOrder` | `instructions/user.rs` |
| `velocityClient.modifyOrder` | `modify_order` | `CancelOrder` | `instructions/user.rs` |
| `velocityClient.deposit` | `deposit` | `Deposit` | `instructions/user.rs` |
| `velocityClient.withdraw` | `withdraw` | `Withdraw` | `instructions/user.rs` |
| `velocityClient.fillPerpOrder` | `fill_perp_order` | `FillOrder` | `instructions/keeper.rs` |
| `velocityClient.settlePNL` | `settle_pnl` | `SettlePNL` | `instructions/keeper.rs` |
| `velocityClient.liquidatePerp` | `liquidate_perp` | `LiquidatePerp` | `instructions/keeper.rs` |
| `velocityClient.updateFundingRate` | `update_funding_rate` | `UpdateFundingRate` | `instructions/keeper.rs` |
| `adminClient.initializePerpMarket` | `initialize_perp_market` | `InitializePerpMarket` | `instructions/admin.rs` |
| `adminClient.updatePerpMarket*` | `update_perp_market_*` | `AdminUpdatePerpMarket` and friends | `instructions/admin.rs` |
| `adminClient.updateOracleGuardRails` | `update_oracle_guard_rails` | `AdminUpdateState` | `instructions/admin.rs` |

The settle-PnL method is spelled `settlePNL`, not `settlePnl`. Related methods follow the same
casing: `settlePNLs`, `settleMultiplePNLs`.

---

## Ancillary programs

These are stubs and wrappers used for oracle integrations, JIT fills, and testing. No core
protocol logic lives in them.

| Program | Purpose |
|---|---|
| `programs/pyth-lazer/` | Pyth Lazer type definitions, linked into `velocity` as a real library dependency rather than a CPI target |
| `programs/pyth/` | Pyth V1 oracle account layout definitions. An optional dependency, pulled in only by `velocity`'s `fuzz-fixtures` feature and by tests |
| `programs/jit-proxy/` | Just-in-time fill and arb proxy. CPIs into `velocity` |
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
