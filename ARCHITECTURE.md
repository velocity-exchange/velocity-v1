# Velocity Protocol v1 architecture

Navigation map for `programs/velocity` and `packages/sdk`. Start here to find the right file for a
question.

Several trees are a module root file beside a directory of the same name, such as
`controller/orders.rs` with `controller/orders/`. The root holds the doc comment, the `mod`
declarations, and the re-exports. The subject code is in the directory.

---

## Module responsibility matrix

| Module | Owns | Does NOT own |
|---|---|---|
| `programs/velocity/src/instructions/` | Account constraint structs, Anchor deserialization, input validation, delegation to `controller` | Business logic, math |
| `programs/velocity/src/controller/` | Stateful mutations: fills, liquidations, position updates, funding | Account loading (done by `instructions`), pure math |
| `programs/velocity/src/math/` | Pure numeric functions: margin, fees, funding, AMM pricing, oracle checks | Any account I/O or state mutation |
| `programs/velocity/src/state/` | Account struct definitions and accessor/mutation methods | Instruction routing, math |
| `programs/velocity/src/validation/` | Pre-mutation precondition checks (called by `instructions` before `controller`) | Post-trade checks (those live in `math/margin`) |

---

## Execution flows

### Place perp order

1. A user calls `place_perp_order`, context `PlaceOrder` (`instructions/user/orders.rs`)
2. → `controller::orders::place_perp_order` (`controller/orders/placement.rs`)
3. → `math::orders::standardize_base_asset_amount` and auction parameter derivation
4. → `validation::order::validate_order` (`validation/order.rs`)
5. → `commit_order_to_slot` writes the order into `User.orders` and reserves its exposure

### Fill perp order (keeper crank)

1. A keeper calls `fill_perp_order`, context `FillOrder` (`instructions/keeper/fill.rs`)
2. → `controller::orders::fill_perp_order` (`controller/orders/perp_fill/order.rs`)
3. The fill runs in three layers, each one calling the layer below as a step
   (`controller/orders/perp_fill/`):
   - `order` finds the order, decides whether the market and the taker admit a fill, and applies the
     bookkeeping the fill leaves behind
   - `taker_risk` runs the gates the fill must pass before it moves anything, and the checks both
     seats are held to after it does
   - `liquidity` quotes, splits, executes and settles
4. → `controller::position::update_position_and_market` (`controller/position.rs`)
5. → emits `OrderActionRecord` (`state/events.rs`)

### Liquidate perp

1. A keeper calls `liquidate_perp`, context in `instructions/keeper/liquidation.rs`
2. → `controller::liquidation::liquidate_perp` (`controller/liquidation.rs`)
3. → `math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info` confirms
   the account is liquidatable
4. → `math::liquidation::calculate_base_asset_amount_to_cover_margin_shortage` sizes the transfer
5. → `controller::position::update_position_and_market`
6. → emits `LiquidationRecord`

### Settle PnL

1. A keeper calls `settle_pnl`, context `SettlePNL` (`instructions/keeper/settle_pnl.rs`)
2. → `controller::pnl::settle_pnl` (`controller/pnl.rs`)
3. → `PerpPosition::get_claimable_pnl` against the oracle price and the pool's excess
4. → `controller::update_pnl_pool_and_user_balance` moves `PerpMarket.pnl_pool` and the user's quote
   balance

### Update funding rate

1. A keeper calls `update_funding_rate`, context in `instructions/keeper/funding.rs`
2. → `controller::funding::update_funding_rate` (`controller/funding.rs`)
3. → `math::funding::calculate_funding_rate_long_short` from the TWAPs
4. → writes `PerpMarket.last_funding_rate`

---

## Fee and revenue flow

The per-fee flow, the pool-movement diagrams, the protocol and staker insurance-fund split, and a
snapshot of the old-program per-market fee parameters are in [FEES.md](./FEES.md).

---

## Account type locations

| Type | File | Notes |
|---|---|---|
| `User` | `state/user.rs` | Zero-copy, `AccountLoader`. Holds positions, open orders, margin info. |
| `UserStats` | `state/user.rs` | Companion to `User`, tracks volume, fees and referrals. |
| `PerpMarket` | `state/perp_market.rs` | Zero-copy. Embeds the `AMM` struct for AMM state. |
| `SpotMarket` | `state/spot_market.rs` | Zero-copy. Tracks deposits, borrows, oracle and insurance. |
| `State` | `state/state.rs` | Global protocol config: fees, admin pubkey, number of markets. |
| `InsuranceFundStake` | `state/insurance_fund_stake.rs` | Per-user insurance-fund stake position. |
| `OracleMap` | `state/oracle_map.rs` | Per-instruction oracle account loader, built from `remaining_accounts`. |
| `OrderParams` | `state/order_params.rs` | Shared input struct for place and modify order instructions. |
| All events | `state/events.rs` | `OrderActionRecord`, `DepositRecord`, `LiquidationRecord`, `FundingPaymentRecord`, and the rest. |

---

## Key design patterns

### Custom high-frequency entrypoint

Keeper instructions such as `fill_perp_order` and `update_funding_rate` use a custom native
entrypoint with discriminator `[0xFF, 0xFF, 0xFF, 0xFF, opcode]`, which bypasses Anchor's account
deserialization overhead. Standard user and admin instructions use the normal Anchor `#[program]`
entrypoint.

### The `remaining_accounts` convention

Variable-length account lists travel in `remaining_accounts`, so an Anchor context does not have to
fix their number:

- **Oracles**: one oracle account per market the instruction references
- **Spot markets**: for instructions touching several spot positions
- **Maker accounts**: a `(User, UserStats)` pair for each DLOB maker in a fill
- **Referrer**: an optional `(User, UserStats)` pair at the end

### Zero-copy account loading

`User`, `PerpMarket` and `SpotMarket` load through `AccountLoader<'info, T>`. Call `.load()` or
`.load_mut()` rather than deserializing directly. Direct deserialization overflows the stack on
structs this large.

### Feature flags

| Flag | Purpose |
|---|---|
| `mainnet-beta` | Production gates (program IDs, conservative limits) |
| `anchor-test` | Enables test helper instructions used by the TypeScript integration tests |
| `no-entrypoint` | Excludes the native entrypoint, for use as a CPI dependency |
| `cpi` | Exposes the CPI client only (implies `no-entrypoint`) |

---

## SDK structure (`packages/sdk/src/`)

### Key files

| File | Purpose |
|---|---|
| `velocityClient.ts` | Main client. All trading and keeper instruction builders. |
| `adminClient.ts` | Admin instruction builders. Extends `VelocityClient`. |
| `user.ts` | `User` account abstraction: margin queries, position accessors, PnL. |
| `types.ts` | Hand-maintained TypeScript mirrors of the on-chain structs. |
| `idl/velocity.json` | Generated Anchor IDL. The source of truth for instruction interfaces and account layouts. **Do not edit manually.** |

### Key directories

| Directory | Purpose |
|---|---|
| `accounts/` | Account subscription infrastructure: websocket, polling, bulk loaders. |
| `addresses/` | `pda.ts`, which holds every PDA derivation helper. |
| `clob/` | The user-orders feed client for orders resting on a CLOB. |
| `dlob/` | Decentralized limit order book: order matching, price levels, maker selection. |
| `math/` | TypeScript mirrors of the on-chain math (margin, funding, AMM pricing). |
| `oracles/` | Oracle client adapters (Pyth, Pyth Lazer, Prelaunch, QuoteAsset). |
| `events/` | Event parsing and subscription from program logs. |
| `tx/` | Transaction building, compute unit estimation. |
| `constants/` | Market indexes, precision constants, numeric limits. |

### SDK to on-chain instruction mapping

| SDK method | Instruction | Handler file |
|---|---|---|
| `velocityClient.placePerpOrder` | `place_perp_order` | `instructions/user/orders.rs` |
| `velocityClient.cancelOrder` | `cancel_order` | `instructions/user/orders.rs` |
| `velocityClient.modifyOrder` | `modify_order` | `instructions/user/orders.rs` |
| `velocityClient.deposit` | `deposit` | `instructions/user/deposit.rs` |
| `velocityClient.withdraw` | `withdraw` | `instructions/user/deposit.rs` |
| `velocityClient.fillPerpOrder` | `fill_perp_order` | `instructions/keeper/fill.rs` |
| `velocityClient.settlePNL` | `settle_pnl` | `instructions/keeper/settle_pnl.rs` |
| `velocityClient.liquidatePerp` | `liquidate_perp` | `instructions/keeper/liquidation.rs` |
| `velocityClient.updateFundingRate` | `update_funding_rate` | `instructions/keeper/funding.rs` |
| `adminClient.initializePerpMarket` | `initialize_perp_market` | `instructions/admin.rs` |
| `adminClient.updatePerpMarket*` | `update_perp_market_*` | `instructions/admin.rs` |
| `adminClient.updateOracleGuardRails` | `update_oracle_guard_rails` | `instructions/admin.rs` |

---

## Ancillary programs

These are type definitions and test utilities. No core protocol logic lives here.

| Program | Purpose |
|---|---|
| `programs/pyth-lazer/` | Pyth Lazer type definitions, linked into `velocity` as a library dependency rather than a CPI target |
| `programs/pyth/` | Pyth V1 oracle account layout definitions. An optional dependency, pulled in only by `velocity`'s `fuzz-fixtures` feature and by tests |
| `programs/token_faucet/` | Devnet and test token minting utility, not deployed on mainnet |

Switchboard oracle support and the external spot-fulfillment venues (Serum, Phoenix, OpenBook) are
removed. There is no `programs/switchboard*` and no `programs/openbook_v2`. `OracleSource` keeps its
`DeprecatedSwitchboard` and `DeprecatedSwitchboardOnDemand` variants only to preserve the ABI
discriminants.

---

## Build and test quick reference

`CLAUDE.md` holds the full commands. The entry points:

```bash
# Verify the program compiles after Rust changes
cargo build -p velocity

# Run the Rust unit tests
cargo test -p velocity

# Run a single TypeScript integration test
ts-mocha -t 300000 ./tests/<test_file>.ts

# Full TypeScript integration suite
bash test-scripts/run-anchor-tests.sh --skip-build
```
