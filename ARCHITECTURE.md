# Drift Protocol v2 — Architecture

Navigation map for `programs/drift` and `sdk/`. Start here to find the right file for any query.

---

## Module Responsibility Matrix

| Module | Owns | Does NOT own |
|---|---|---|
| `programs/drift/src/instructions/` | Account constraint structs, Anchor deserialization, input validation, delegation to `controller` | Business logic, math |
| `programs/drift/src/controller/` | Stateful mutations: fills, liquidations, position updates, funding | Account loading (done by `instructions`), pure math |
| `programs/drift/src/math/` | Pure numeric functions: margin, fees, funding, AMM pricing, oracle checks | Any account I/O or state mutation |
| `programs/drift/src/state/` | Account struct definitions and accessor/mutation methods | Instruction routing, math |
| `programs/drift/src/validation/` | Pre-mutation precondition checks (called by `instructions` before `controller`) | Post-trade checks (those live in `math/margin`) |

---

## Execution Flows

### Place Perp Order
1. User calls `place_perp_order` → `instructions/user.rs` (`PlacePerpOrder` context)
2. → `validation::order::validate_order` (`validation/order.rs`)
3. → `controller::orders::place_perp_order` (`controller/orders.rs`)
4. → `math::orders::standardize_base_asset_amount` + auction parameter derivation
5. → `state::user::add_order` writes order to `User` account

### Fill Perp Order (keeper crank)
1. Keeper calls `fill_perp_order` → `instructions/keeper.rs` (`FillPerpOrder` context)
2. → `controller::orders::fill_perp_order` (`controller/orders.rs`)
3. → `math::margin::calculate_margin_requirement` (pre-fill margin check)
4. → `controller::orders::fulfill_perp_order_with_match` (maker matching loop)
5. → `controller::position::update_position_and_market` (`controller/position.rs`)
6. → `controller::orders::update_order_after_fill` + bookkeeping
7. → emits `OrderActionRecord` event (`state/events.rs`)

### Liquidate Perp
1. Keeper calls `liquidate_perp` → `instructions/keeper.rs`
2. → `controller::liquidation::liquidate_perp` (`controller/liquidation.rs`)
3. → `math::margin::calculate_margin_requirement` (confirms liquidatable)
4. → `math::liquidation::calculate_perp_liquidation_price` + fee
5. → `controller::position::update_position_and_market`
6. → emits `LiquidationRecord`

### Settle PnL
1. Keeper calls `settle_pnl` → `instructions/keeper.rs`
2. → `controller::pnl::settle_funding_payment` (`controller/pnl.rs`)
3. → `math::pnl::calculate_per_lp_position`
4. → mutates `PerpMarket.pnl_pool` and `User` position

### Update Funding Rate
1. Keeper calls `update_funding_rate` → `instructions/keeper.rs`
2. → `controller::funding::update_funding_rate` (`controller/funding.rs`)
3. → `math::funding::calculate_funding_rate` (TWAP-based)
4. → writes `PerpMarket.amm.last_funding_rate`

---

## Fee & Revenue Flow

Classifies every fee the protocol charges by destination: **protocol-retained revenue**, **pass-through** (forwarded to a user, keeper, or builder), **liability-offsetting** (insurance-fund inflows that pre-fund bankruptcy payouts), or **LP revenue**. This fork diverges from upstream Drift in the ways noted below.

### Differences from upstream Drift

- **Spot trading charges no fee.** The swap fee is hardcoded to zero (`let fee = 0_u64;`, `instructions/user.rs:3949`), and there is no spot order-book fill path (`fulfill_spot_order` does not exist); spot trades route through `begin_swap`/`end_swap` and `lp_pool_swap`. `SpotMarket.total_spot_fee`, `spot_fee_pool`, and `total_swap_fee` are therefore inert.
- **Perp taker fees are the only trading-fee revenue.**
- **The AMM lives in `src/vlp/`** (the decoupled AMM). Its fee counters (`total_fee`, `total_mm_fee`, `total_fee_minus_distributions`, `total_fee_withdrawn`, `fee_pool`) are on `vlp/amm/state.rs`, not `PerpMarket`; `lp_fee_transfer_scalar` is now `HedgeConfig.fee_transfer_scalar` (`vlp/hedge/state.rs:80`).
- **The protocol's automatic fee share is ½ of taker fees** (`SHARE_OF_FEES_ALLOCATED_TO_DRIFT = 1/2`, `math/constants.rs:111-112`). This bounds only the continuous revenue-pool sweep; AMM spread surplus and trading PnL are excluded from it and are realized at market wind-down instead (see [Protocol fee share](#protocol-fee-share-streaming-vs-wind-down)).

### Diagram 1 — Perp taker-fee decomposition

Carve-out order, in `math/fees.rs` (`calculate_fee_for_fulfillment_with_amm` `:36`, `calculate_fee_for_fulfillment_with_match` `:263`):

```mermaid
flowchart TD
    Taker["Gross taker fee<br/>ceil(notional × fee_numerator/fee_denominator)<br/>tier by 30d volume + gov-token stake<br/>then ± per-market fee_adjustment"]
    Taker -->|"− referee_discount (5%): fee lowered, never collected"| Disc(["not collected"])
    Taker -->|"− filler_reward = min(size-based, time-based)"| Filler["Filler perp position (quote PnL)"]
    Taker -->|"− referrer_reward (15%)"| RefEsc["Referrer RevenueShareEscrow.fees_accrued"]
    Taker -->|"− maker_rebate (match path only, 2 bps)"| Maker["Maker perp position"]
    Taker ==>|"remainder = fee_to_market"| FTM["AMM.total_fee / total_fee_minus_distributions (AMM fills)<br/>PerpMarket.total_exchange_fee (all fills)"]
    Builder["builder_fee = notional × fee_tenth_bps / 100_000<br/>(ADDED on top of taker fee — NOT carved out)"] --> BldEsc["Builder RevenueShareEscrow.fees_accrued"]
    Taker -.->|charged together with builder_fee| Builder
    RefEsc -->|"sweep_completed_revenue_share on settle_pnl, drawn from PerpMarket.pnl_pool"| RefBal["Referrer quote spot balance"]
    BldEsc -->|"sweep on settle_pnl, drawn from PerpMarket.pnl_pool"| BldBal["Builder quote spot balance"]

    classDef revenue fill:#cfe8cf,stroke:#2e7d32,color:#000;
    classDef passthru fill:#ffe0b2,stroke:#e65100,color:#000;
    class FTM revenue;
    class Filler,RefEsc,RefBal,BldEsc,BldBal,Maker,Disc passthru;
```

`fee_to_market` is the only protocol-retained component (green); the rest is forwarded to a user, keeper, or builder. `builder_fee` is additive — it does not reduce `fee_to_market` — and is paid from the perp `pnl_pool` at settlement.

### Diagram 2 — Value flow across pools

Each flow passes through an accounting ledger (tracks an amount, holds no tokens), one or more token pools (actual balances), and a settlement instruction (moves tokens).

Legend: gray = accounting ledger · blue = token pool · orange hexagon = settlement instruction · green = protocol-retained · light-orange = pass-through · red = liability/outflow. Solid arrow = token movement; dashed arrow = a ledger that bounds a settlement amount.

```mermaid
flowchart LR
    classDef ledger fill:#eceff1,stroke:#607d8b,color:#000;
    classDef pool fill:#e3f2fd,stroke:#1565c0,color:#000;
    classDef step fill:#fff3e0,stroke:#ef6c00,color:#000;
    classDef revenue fill:#cfe8cf,stroke:#2e7d32,color:#000;
    classDef passthru fill:#ffe0b2,stroke:#e65100,color:#000;
    classDef liability fill:#f8d7da,stroke:#c62828,color:#000;

    TK["Perp taker"]
    BR["Spot borrower"]
    PLQ["Perp liquidatee"]
    SLQ["Spot liquidatee"]

    TEF["total_exchange_fee +<br/>AMM.total_fee / total_fee_minus_distributions / total_fee_withdrawn"]:::ledger
    TLF["PerpMarket.total_liquidation_fee"]:::ledger
    ESC["RevenueShareEscrow.fees_accrued"]:::ledger
    QOL["AmmCache.quote_owed_from_lp_pool"]:::ledger

    FP["AMM.fee_pool"]:::pool
    PP["PerpMarket.pnl_pool"]:::pool
    RP["SpotMarket.revenue_pool (Deposit)"]:::pool
    IFV["Insurance Fund vault"]:::pool
    LPV["LP quote constituent vault"]:::pool

    S_FILL{{"fulfill_perp_order"}}:::step
    S_INT{{"update_spot_market_cumulative_interest"}}:::step
    S_SET{{"settle_pnl → update_pool_balances<br/>→ transfer_revenue_to_pool"}}:::step
    S_REV{{"settle_revenue_to_insurance_fund"}}:::step
    S_SWP{{"settle_pnl → sweep_completed_revenue_share"}}:::step
    S_LP{{"update_amm_cache + SettleAmmPnlToLp"}}:::step
    S_WD{{"admin_withdraw_from_insurance_fund_vault"}}:::step
    S_BK{{"resolve_perp / spot_bankruptcy"}}:::step
    S_EXP{{"settle_expired_market_pools_to_revenue_pool<br/>(market Settlement → Delisted)"}}:::step

    COLD["External protocol wallet"]:::revenue
    BLD["Builder / referrer spot balance"]:::passthru
    LIQ["Liquidator"]:::passthru
    LPH["LP token holders"]:::passthru
    STK["Staker IF claim"]:::passthru
    BD["Bankruptcy bad debt"]:::liability

    %% perp trading fee
    TK --> S_FILL
    S_FILL -->|"+= user_fee (gross)"| TEF
    S_FILL -->|"builder/referrer carve"| ESC
    S_FILL -.->|"hedge settle realizes AMM PnL+fees as tokens"| FP

    %% builder/referrer pass-through
    ESC --> S_SWP
    PP --> S_SWP
    S_SWP -->|"transfer_spot_balances out of pnl_pool"| BLD

    %% lending skim
    BR --> S_INT
    S_INT -->|"deposit_interest × total_factor / 1e6"| RP

    %% liquidations
    PLQ -->|"liquidator_fee (symmetric transfer)"| LIQ
    PLQ -->|"if_fee accrues"| TLF
    SLQ -->|"if_fee → update_revenue_pool_balances (direct)"| RP

    %% shared revenue-pool sweep
    TEF -.->|"½ floor gates amount"| S_SET
    TLF -.->|"min(·, quote_settled + quote_max_insurance) gates"| S_SET
    FP ==>|"reclassify Borrow→Deposit; only if surplus > 250Q,<br/>capped by ½×total_exchange_fee (excl. spread/PnL) & period cap"| S_SET
    S_SET ==> RP

    %% wind-down: protocol recovers 100% of residual (NOT capped at ½)
    FP ==>|"on expiry: withdraw_from_fee_pool (100% residual)"| S_EXP
    PP ==>|"on expiry: 100% residual"| S_EXP
    S_EXP ==> RP

    %% revenue → IF → protocol
    RP ==>|"reclassify Borrow + SPL transfer spot_vault→IF_vault"| S_REV
    S_REV ==> IFV
    IFV ==>|"protocol shares = total_shares − user_shares"| S_WD
    S_WD ==>|"cold_admin only, must leave ≥1 share"| COLD
    IFV -->|"user_factor slice raises staker claim"| STK
    IFV -.-> S_BK
    S_BK -.->|"SPL payout → pnl_pool / spot_vault; residual socialized"| BD

    %% LP routing
    TEF -.->|"fee_transfer_scalar % gates"| S_LP
    FP --> S_LP
    PP --> S_LP
    S_LP -->|"accrues, then settles to vault"| QOL
    QOL --> LPV
    LPV --> LPH
```

The protocol's only path to an external wallet is `fee_pool → revenue_pool → IF vault → protocol IF shares → cold_admin`, via `admin_withdraw_from_insurance_fund_vault` (`if_staker.rs:1241`, restricted to `state.cold_admin`, must leave ≥1 protocol share). `revenue_pool` has no direct admin withdrawal; it exits only to the IF (`settle_revenue_to_insurance_fund`) or to an underwater perp market (`update_pool_balances`; the negative branch is currently a no-op, `perp_pools.rs:179`).

### Protocol fee share: streaming vs wind-down

The ½ share bounds the continuous sweep, not the protocol's total claim on a market's fees.

| Path | When | Amount | Mechanism |
|---|---|---|---|
| Streaming sweep | Continuously, during `settle_pnl` | ≤ `½ × total_exchange_fee + liq_fees` lifetime (`total_fee_withdrawn`) | `proposed_revenue_outflow` (`amm/state.rs:409`); cap `total_fee_for_if = total_exchange_fee × ½` (`perp_pools.rs:81`, `repeg.rs:433`) |
| Fee↔PnL rebalance | Admin | None (internal) | `transfer_fee_and_pnl_pool` (`admin.rs:1141`) moves `fee_pool`↔`pnl_pool`; does not reach the revenue pool |
| Wind-down sweep | Market expired, balanced, after escrow | 100% of residual `fee_pool + pnl_pool` | `settle_expired_market_pools_to_revenue_pool` (`admin.rs:1083`, sweep `:1164-1190`) |

The streaming cap is keyed to `total_exchange_fee`, which accumulates only the explicit taker fee (`user_fee`, `orders.rs:2267`). AMM spread surplus (`taker_surplus → AMM.total_mm_fee`, `quoter.rs:170`) and AMM trading PnL accrue to `total_fee`/`total_fee_minus_distributions` and remain in `fee_pool` as retained equity, reaching the revenue pool only at delist. Higher AMM profitability raises `fee_pool` but not the streaming cap.

### Pools and ledgers

| Node | Kind | Holds tokens? | Role / classification |
|---|---|---|---|
| `total_exchange_fee`, `AMM.total_fee`, `total_fee_minus_distributions`, `total_fee_withdrawn` | Accounting ledger | No | Gate the revenue-pool sweep (protocol's ½ floor); `total_fee_withdrawn` = realized-revenue meter |
| `PerpMarket.total_liquidation_fee` | Accounting accumulator | No | Gates how much liq-fee may sweep, capped by `quote_max_insurance` |
| `RevenueShareEscrow.fees_accrued` | Accounting ledger | No | Builder/referrer amount owed; paid from `pnl_pool` |
| `AmmCache.quote_owed_from_lp_pool` | Accounting accumulator | No | Perp↔LP amount owed; settled by `SettleAmmPnlToLp` |
| `InsuranceFund.total_shares` / `user_shares` | Share ledger | No | Claims on IF vault; protocol = `total − user` |
| `AMM.fee_pool` | Token pool | Yes | AMM/protocol buffer; **source** of revenue-pool sweeps; funded by hedge settle |
| `PerpMarket.pnl_pool` | Token pool | Yes | Backs user PnL; **source** of builder/referrer sweeps; tapped in perp bankruptcy |
| `SpotMarket.revenue_pool` | Token pool (Deposit) | Yes | **Protocol-revenue staging**; exits only to IF or underwater perp |
| `Insurance Fund vault` | Token pool | Yes | **At-risk**: protocol + staker claims; pays bankruptcies |
| `LP quote constituent vault` | Token pool | Yes | LP-holder owned (not protocol) |

### Flow classification

Each row is the chain `ledger → token pool(s) → settlement step(s) → destination`, with its class. "→IF→cold" abbreviates the shared tail `revenue_pool —settle_revenue_to_insurance_fund→ IF vault —admin_withdraw_from_insurance_fund_vault (cold_admin)→ wallet`. User-account initialization charges `State.max_initialize_user_fee` (Solana rent, not revenue).

| Flow | Ledger(s) | Intermediary token pool(s) | Settlement step(s) | Destination | Class |
|---|---|---|---|---|---|
| Perp trading fee (protocol ½, live) | `total_exchange_fee`, `AMM.total_fee*` | `fee_pool` → `revenue_pool` → IF vault | `fulfill_perp_order` → (hedge settle funds `fee_pool`) → `update_pool_balances`/`transfer_revenue_to_pool` → →IF→cold | Protocol IF shares → wallet | **Protocol revenue** (capped ½×`total_exchange_fee`) |
| AMM spread surplus + trading PnL | `AMM.total_mm_fee`, `total_fee_minus_distributions` | `fee_pool` (retained equity) → `revenue_pool` | accrues live (excluded from sweep cap); realized only via `settle_expired_market_pools_to_revenue_pool` (100% residual) | →IF→cold (at delist) | **Protocol revenue, deferred to wind-down** |
| Lending skim | `total_factor` (vs `cumulative_deposit_interest`) | `revenue_pool` → IF vault | `update_spot_market_cumulative_interest` → →IF→cold | Protocol IF shares → wallet (staker slice via `user_factor`) | **Protocol revenue** (shared) |
| Perp `if_liquidation_fee` | `total_liquidation_fee` | (retained in market) → `fee_pool` → `revenue_pool` → IF vault | `liquidate_perp` (accrue) → `update_pool_balances`/`transfer_revenue_to_pool` (cap `quote_max_insurance`) → `settle_revenue_to_insurance_fund` | IF vault | Liability-offset |
| Spot `if_liquidation_fee` | — (direct write) | liability `revenue_pool` → IF vault | `liquidate_spot`→`update_revenue_pool_balances` → `settle_revenue_to_insurance_fund` | IF vault | Liability-offset |
| Builder fee / referrer reward | `RevenueShareEscrow.fees_accrued` | `pnl_pool` | `fulfill_perp_order` (accrue) → `settle_pnl`/`sweep_completed_revenue_share` | Builder/referrer spot balance | Pass-through |
| Filler reward | — | — (credited to filler `PerpPosition` at fill) | `fulfill_perp_order` | Keeper | Pass-through |
| Maker rebate | `UserStats.total_rebate` | — (credited to maker `PerpPosition`) | `fulfill_perp_order` | Maker | Pass-through |
| Perp `liquidator_fee` | — | — (symmetric quote transfer) | `liquidate_perp` | Liquidator | Pass-through |
| Spot `liquidator_fee` | — | — (asset/liability multiplier spread) | `liquidate_spot` | Liquidator | Pass-through |
| Referee discount | `UserStats.total_referee_discount` | — | — (taker fee reduced) | n/a | Not collected |
| AMM → LP routing | `quote_owed_from_lp_pool` (gated by `fee_transfer_scalar`, `exchange_fee_exclusion_scalar`) | `fee_pool`/`pnl_pool` ↔ LP constituent vault | `update_amm_cache` (accrue) → `SettleAmmPnlToLp` | LP token holders | LP revenue |
| LP swap / mint / redeem fees | `total_swap_fees`, `total_mint_redeem_fees_paid` | LP constituent vaults (stay) | `lp_pool_swap` / `add`/`remove_liquidity` | LP token holders | LP revenue |
| Bankruptcy payout (outflow) | `quote_settled_insurance`, `total_social_loss` | IF vault → `pnl_pool` / `spot_market_vault` | `resolve_perp`/`spot_bankruptcy` (+ socialize residual via `cumulative_funding_rate` / `cumulative_deposit_interest`) | Bad debt | Liability |

### Lending and insurance-fund revenue

- **Borrower-interest skim.** In `update_spot_market_cumulative_interest` (`controller/spot_balance.rs:130-185`), `total_factor × deposit_interest / 1e6` is split off before lenders are paid and deposited into `revenue_pool`; only the remainder compounds into `cumulative_deposit_interest`. The `revenue_pool` balance, being a `Deposit`, also earns the lender rate, but the skim is the primary lending revenue.
- **Settlement split.** When `revenue_pool` settles to the IF (`controller/insurance.rs:758-775`), the protocol's slice `(total_factor − user_factor)/total_factor` is minted as new shares to `total_shares` only; the `user_factor` slice raises existing stakers' share value.
- **Two-way book.** `resolve_perp_bankruptcy` (`liquidation.rs:3268`) and `resolve_spot_bankruptcy` (`liquidation.rs:3491`) pay out of the IF vault (`send_from_program_vault`, `keeper.rs` ~`:2026` spot / ~`:2155` perp). Liquidation `if_fee`s and protocol IF shares absorb bad debt before they are withdrawable.

### Insurance-fund staker economics

The IF is a share-based vault. Value accrues as vault-balance growth against a fixed share count; there is no per-staker interest field.

- **Stake:** `add_insurance_fund_stake` (`controller/insurance.rs:83`) mints `amount × total_shares / vault_balance` shares (`vault_amount_to_if_shares`, `math/insurance.rs:16`) to both `user_shares` and `total_shares`.
- **Claim:** `vault_balance × shares / total_shares` (`if_shares_to_vault_amount`, `math/insurance.rs:44`).
- **Unstake (request + cooldown):** `request_remove_insurance_fund_stake` (`:235`) records the share count and value at request; after `unstaking_period`, `remove_insurance_fund_stake` (`:385`, cooldown `:396`) pays `min(current_value, requested_value)` (`:429`) and burns from both share counters. Losses during the cooldown reduce the payout; gains above the requested value are not captured.
- **First-loss capital:** bankruptcy payouts shrink the vault and reduce every share's value; `apply_rebase_to_insurance_fund` (`:158`) handles a near-zero vault.

Two directions exist between the IF and the revenue pool:
1. `revenue_pool → IF vault` — `settle_revenue_to_insurance_fund` (`:685`). Throttled to `min(1/10 of revenue pool, MAX_APR cap)` per settle when stakers exist (`:719-739`), half-rate at high utilization (`:714-717`).
2. `protocol IF shares → revenue_pool` — `transfer_protocol_if_shares_to_revenue_pool` (`:1150`), limited to protocol shares (`:1167`) and to `IfRebalanceConfig.max_transfer_amount`. Staker capital does not flow to the revenue pool.

The split is set by `total_factor` and `user_factor` (`u32`, `IF_FACTOR_PRECISION = 1e6`, `spot_market.rs:681-682`) via `handle_update_spot_market_if_factor` (`admin.rs:1636`, requiring `user_factor ≤ total_factor ≤ 1e6`). Market init defaults to `user_factor = total_factor / 2` (`admin.rs:384-385`). On each settle, the protocol's slice `(total_factor − user_factor)/total_factor` is minted as new shares to `total_shares` only (`get_protocol_shares = total_shares − user_shares`, `spot_market.rs:686`); the `user_factor` slice appreciates existing shares.

#### Diagram 3 — Insurance-fund revenue split

```mermaid
flowchart TD
    classDef pool fill:#e3f2fd,stroke:#1565c0,color:#000;
    classDef step fill:#fff3e0,stroke:#ef6c00,color:#000;
    classDef revenue fill:#cfe8cf,stroke:#2e7d32,color:#000;
    classDef passthru fill:#ffe0b2,stroke:#e65100,color:#000;

    RP["SpotMarket.revenue_pool"]:::pool
    SET{{"settle_revenue_to_insurance_fund<br/>settled = min(1/10 pool, MAX_APR cap)"}}:::step
    IFV["Insurance Fund vault<br/>(ALL settled tokens land here)"]:::pool
    PSH["Protocol IF shares = total_shares − user_shares"]:::revenue
    USH["Staker IF shares (user_shares)<br/>→ value-per-share rises"]:::passthru
    WALL["cold_admin wallet"]:::revenue

    RP ==>|"SPL transfer spot_vault → IF_vault"| SET
    SET ==> IFV
    IFV -.->|"slice = settled × (total_factor − user_factor)/total_factor:<br/>MINT new shares to total_shares only (insurance.rs:764-775)"| PSH
    IFV -.->|"slice = settled × user_factor/total_factor:<br/>NO new shares → existing shares appreciate"| USH
    PSH ==>|"admin_withdraw_from_insurance_fund_vault (cold_admin)"| WALL
    PSH -.->|"transfer_protocol_if_shares_to_revenue_pool (rate-limited)"| RP
```

Dashed arrows are the factor split of the just-settled tokens (all tokens already sit in the vault): the protocol leg materializes as newly minted shares, the staker leg as appreciation of existing shares.

### Gross vs net trading-fee fields

- **Gross taker fees (perp):** `UserStats.total_fees` per user; `PerpMarket.total_exchange_fee` per market. `total_exchange_fee` accumulates gross `user_fee` on AMM fills (`orders.rs:2267`) but net `fee_to_market` on DLOB-matched fills (`orders.rs:2577`). Spot contributes zero.
- **Deductions:** maker rebate (`UserStats.total_rebate`), referrer reward (`RevenueShare.total_referrer_rewards`), referee discount (`UserStats.total_referee_discount`), filler reward (`OrderActionRecord.filler_reward` events), builder fee (`RevenueShare.total_builder_rewards`).
- **Net trading fee** = `fee_to_market`: taker fee minus filler and referrer rewards (and, on the match path, the maker rebate); referee discount is removed upstream and builder fee is excluded. `AMM.total_fee` captures this for AMM fills only — DLOB-matched fills accrue to `total_exchange_fee` and skip `apply_fill_fees` — so no single field equals net trading fees across both fill types; event-level aggregation from `OrderActionRecord` is required.
- **Protocol-retained** = ½ of net, realized as growth in `AMM.total_fee_withdrawn` as it settles to `revenue_pool` (the remainder is deferred AMM equity; see [Protocol fee share](#protocol-fee-share-streaming-vs-wind-down)).

### Source reference index

Symbol and line for each claim above (line numbers shift with edits; search the symbol if stale).

**Constants** (`programs/drift/src/math/constants.rs`)
| Constant | Value | Line |
|---|---|---|
| `FEE_DENOMINATOR` | `10 * ONE_BPS_DENOMINATOR` = 100_000 | `:158` (`ONE_BPS_DENOMINATOR`=10000 `:155`) |
| `FEE_PERCENTAGE_DENOMINATOR` | 100 | `:159` |
| `SHARE_OF_FEES_ALLOCATED_TO_DRIFT_NUMERATOR/DENOMINATOR` | 1 / 2 | `:111-112` |
| `SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_*` | 1 / 1 | `:117-118` |
| `FEE_POOL_TO_REVENUE_POOL_THRESHOLD` | `TWO_HUNDRED_FIFTY_QUOTE` (250 QUOTE) | `:152` (`:143`) |
| `LIQUIDATION_FEE_PRECISION` | `PERCENTAGE_PRECISION` = 1e6 | `:74` |

**Trading fee math** (`programs/drift/src/math/fees.rs`)
| Claim | Symbol | Line |
|---|---|---|
| Fee tiers / defaults (tier0 fee_num 100=10bps, rebate 20=2bps, referrer 15/100, referee 5/100; lowest 50=5bps) | `FeeStructure::perps_default` | `state/state.rs:461` |
| Tier selection (30d volume + gov stake) | `determine_user_fee_tier` | `:334` (called from `:49`) |
| AMM-fill fee fn; **post-only branch** sets `user_fee=0`, `referrer_reward=0`, `referee_discount=0` | `calculate_fee_for_fulfillment_with_amm` | `:36` (post-only `:52-90`) |
| `fee_to_market = fee − filler − referrer (+ surplus)` | (taker branch) | `:112-116` |
| `builder_fee = quote × fee_tenth_bps / 100_000` (additive, off notional) | (taker branch) | `:120-128` |
| Taker fee = `ceil(notional × fee_num/fee_den)` then `± fee_adjustment` | `calculate_taker_fee` | `:144` |
| Maker rebate (2 bps) | `calculate_maker_rebate` | `:172` |
| Referrer 15% / referee 5% | `calculate_referee_fee_and_referrer_reward` | `:200` |
| DLOB-match fee fn | `calculate_fee_for_fulfillment_with_match` | `:263` |

**Fill accrual / payout** (`programs/drift/src/controller/orders.rs`)
| Claim | Line |
|---|---|
| AMM path: `total_exchange_fee += user_fee` (gross) | `:2267` |
| AMM path: `apply_fill_fees(amm, fee_to_market, surplus)` | `:2268` |
| AMM path: builder `fees_accrued +=` / referrer `fees_accrued +=` | `:2250` / `:2280` |
| AMM path: filler reward → filler position | `credit_filler_perp_pnl` `:2304` |
| Match path: `total_exchange_fee += fee_to_market` (net) — **no `apply_fill_fees`** | `:2577` |
| Match path: taker debited `-(taker_fee + builder_fee)` | `:2580` |
| Match path: `maker_rebate` → maker position; filler → filler; referrer → escrow | `:2587` / `:2599` / `:2622` |

**AMM accounting & revenue sweep** (`programs/drift/src/vlp/...`)
| Claim | Symbol | Line |
|---|---|---|
| AMM fee fields | `fee_pool`/`total_fee`/`total_mm_fee`/`total_fee_minus_distributions`/`total_fee_withdrawn`/`net_revenue_since_last_funding` | `amm/state.rs:82/113/116/119/122/141` |
| `apply_fill_fees`: `total_fee += fee_to_maker`, `total_mm_fee += surplus` | `amm/quoter.rs` | `:170` |
| `transfer_revenue_to_pool` (reclassify fee_pool Borrow → revenue_pool Deposit) | `amm/quoter.rs` | `:177` |
| `fee_pool` funded with tokens ONLY here (+ admin) | `deposit_to_fee_pool` caller | `hedge/math.rs:197` |
| protocol floor = `total_fee × ½ − total_fee_withdrawn` | `total_fee_lower_bound` / `protocol_floor` | `amm/state.rs:358` / `:376` |
| `transfer = (protocol_floor + liq_fees − withdrawn).max(0).min(fee_pool_thresh).min(cap)` | `proposed_revenue_outflow` | `amm/state.rs:409` |
| `get_total_fee_lower_bound = total_exchange_fee × ½` (live sweep cap; **excludes** spread/PnL) | — | `amm/math/repeg.rs:433` |
| Internal fee↔pnl rebalance (no net extraction) | `handle_transfer_fee_and_pnl_pool` | `instructions/admin.rs:1141` (`lib.rs:2042`) |
| Wind-down: 100% of residual `fee_pool + pnl_pool` → revenue pool | `handle_settle_expired_market_pools_to_revenue_pool` | `instructions/admin.rs:1083` (sweep `:1164-1190`, `lib.rs:1032`) |

**Pool plumbing** (`programs/drift/src/controller/perp_pools.rs`)
| Claim | Symbol | Line |
|---|---|---|
| Revenue-pool transfer decision; runs only if `terminal_state_surplus > 250 QUOTE` | `calculate_revenue_pool_transfer` | `:33` (gate `:47-50`) |
| `total_liq_fees = min(total_liquidation_fee, quote_settled + quote_max_insurance)` | — | `:59-68` |
| Executes the transfer; **negative (revenue→perp) branch is a no-op** | `update_pool_balances` | `:133` (no-op `:179`) |
| Called from settle_pnl | — | `controller/pnl.rs:264` |

**Liquidation** (`programs/drift/src/...`)
| Claim | Symbol | Line |
|---|---|---|
| Perp `liquidator_fee` / `if_fee` = `base_value × fee / 1e6` | `liquidate_perp` | `controller/liquidation.rs:459-469` |
| Perp: user pays `if_fee` (no liquidator credit); `total_liquidation_fee += if_fee` | — | `:502` / `:537` |
| Perp IF-fee rate = `min(if_liquidation_fee, (margin_ratio − liquidator_fee − shortage) × 19/20)` | `calculate_perp_if_fee` | `math/liquidation.rs:394` |
| Spot IF-fee → **direct** `update_revenue_pool_balances(if_fee, Deposit, liability_market)` | `liquidate_spot` / `_with_swap` | `controller/liquidation.rs:1653` / `:2231` |
| Spot IF-fee rate | `calculate_spot_if_fee` | `math/liquidation.rs:438` |
| Perp bankruptcy: `if_payment` → `fee_pool` draw → socialize via `cumulative_funding_rate_long/short` | `resolve_perp_bankruptcy` | `:3268` (if_payment `:3346`, fee_pool `:3407`, socialize `:3435-3440`) |
| Spot bankruptcy: `if_payment` → socialize via `cumulative_deposit_interest` | `resolve_spot_bankruptcy` | `:3491` (if_payment `:3574`, socialize `:3578`) |
| IF-vault SPL payout | `send_from_program_vault` (keeper handlers) | `keeper.rs` ~`:2026` spot / ~`:2155` perp |

**Revenue pool, lending skim, insurance fund**
| Claim | Symbol | Line |
|---|---|---|
| `revenue_pool` is a `PoolBalance` whose `balance_type()` is hardcoded `Deposit`; can't change | `impl SpotBalance for PoolBalance` | `state/perp_market.rs:1094` (`update_balance_type` errs `:1112`) |
| Revenue-pool mutation entrypoint | `update_revenue_pool_balances` | `controller/spot_balance.rs:192` |
| Lending skim: `deposit_interest_for_stakers = deposit_interest × total_factor / 1e6`; only lender share compounds; staker share → revenue_pool | `update_spot_market_cumulative_interest` | `spot_balance.rs:130` (`:147` / `:151` / `:168`) |
| Revenue → IF settlement (100% eligible, period/APR capped) | `settle_revenue_to_insurance_fund` | `controller/insurance.rs:685` |
| Protocol slice = `if_amount × (total_factor − user_factor)/total_factor` minted to `total_shares` only | `protocol_if_factor` | `insurance.rs:758` (mint `:764-775`) |
| Split knobs (`u32`, `IF_FACTOR_PRECISION`=1e6) | `InsuranceFund.total_factor` / `user_factor` | `state/spot_market.rs:681-682` (`constants.rs:68`) |
| Admin sets the split (`user_factor ≤ total_factor ≤ 1e6`) | `handle_update_spot_market_if_factor` | `instructions/admin.rs:1636` |
| Default split = 50/50 (`user_factor = total_factor / 2`) | (market init) | `instructions/admin.rs:384-385` |
| Protocol withdraw: `cold_admin` only, must leave ≥1 protocol share | `handle_admin_withdraw_from_insurance_fund_vault` | `instructions/if_staker.rs:1241` (guard `:1281`, `cold_admin` `:1347`) |
| Stake → shares = `amount × total_shares / vault` | `vault_amount_to_if_shares` / `add_insurance_fund_stake` | `math/insurance.rs:16` / `controller/insurance.rs:83` |
| Share claim = `vault × shares / total_shares` | `if_shares_to_vault_amount` | `math/insurance.rs:44` |
| Unstake: request locks value, cooldown, pay `min(current, requested)` | `request_remove_insurance_fund_stake` / `remove_insurance_fund_stake` | `insurance.rs:235` / `:385` (cooldown `:396`, payout `:429`) |
| Staker yield = vault grows, share count fixed; rebase on near-zero | `apply_rebase_to_insurance_fund` | `insurance.rs:158` (`calculate_rebase_info` `math/insurance.rs:71`) |
| Reverse: protocol shares → revenue pool (protocol shares only, rate-limited) | `transfer_protocol_if_shares_to_revenue_pool` | `insurance.rs:1150` (guard `:1167`) |
| `get_protocol_shares = total_shares − user_shares` | — | `state/spot_market.rs:686` |

**LP / VLP pool** (`programs/drift/src/vlp/...`)
| Claim | Symbol | Line |
|---|---|---|
| Perp→LP routing scalars | `HedgeConfig.fee_transfer_scalar` / `exchange_fee_exclusion_scalar` | `hedge/state.rs:80` / `:78` |
| Routing accrual into `quote_owed_from_lp_pool` | `update_amount_owed_from_lp_pool` | `amm_cache.rs:308` (field `:50`, uses scalar `:351`) |
| Token settlement perp↔LP | `SettleAmmPnlToLp` (`SettlementDirection::To/FromLpPool`) | `hedge/settle.rs:36` (`:158-186`) |
| Constituent swap fee bounds 0.3%–37.5% | `BASE_SWAP_FEE` / `MAX_SWAP_FEE` | `hedge/state.rs:37` / `:38` |
| Mint/redeem fee + counter | `min_mint_fee` / `total_mint_redeem_fees_paid` | `hedge/state.rs:133` / `:118` |

**Spot fees are dead in this fork**
| Claim | Symbol | Line |
|---|---|---|
| Swap fee hardcoded zero | `let fee = 0_u64;` | `instructions/user.rs:3949` |

---

## Account Type Locations

| Type | File | Notes |
|---|---|---|
| `User` | `state/user.rs` | Zero-copy, `AccountLoader`. Holds positions, open orders, margin info. |
| `UserStats` | `state/user.rs` | Companion to `User`, tracks volume/fees/referrals. |
| `PerpMarket` | `state/perp_market.rs` | Zero-copy. Embeds `AMM` struct for AMM state. |
| `SpotMarket` | `state/spot_market.rs` | Zero-copy. Tracks deposits/borrows, oracle, insurance. |
| `State` | `state/state.rs` | Global protocol config: fees, admin pubkey, number of markets. |
| `InsuranceFundStake` | `state/insurance_fund_stake.rs` | Per-user IF stake position. |
| `OracleMap` | `state/oracle_map.rs` | Per-instruction oracle account loader, built from `remaining_accounts`. |
| `OrderParams` | `state/order_params.rs` | Shared input struct for place/modify order instructions. |
| All events | `state/events.rs` | `OrderActionRecord`, `DepositRecord`, `LiquidationRecord`, `FundingPaymentRecord`, etc. |

---

## Key Design Patterns

### Custom High-Frequency Entrypoint
Keeper instructions (`fill_perp_order`, `update_funding_rate`, etc.) use a custom native entrypoint with discriminator `[0xFF, 0xFF, 0xFF, 0xFF, opcode]` that bypasses Anchor's account deserialization overhead. Standard user and admin instructions use the normal Anchor `#[program]` entrypoint.

### `remaining_accounts` Convention
Variable-length account lists are passed via `remaining_accounts` to avoid fixed Anchor context sizes:
- **Oracles**: one oracle account per market referenced in the instruction
- **Spot markets**: for instructions touching multiple spot positions
- **Maker accounts**: `(User, UserStats)` pairs for each DLOB maker in a fill
- **Referrer**: optional `(User, UserStats)` pair at the end of remaining_accounts

### Zero-Copy Account Loading
`User`, `PerpMarket`, and `SpotMarket` are loaded via `AccountLoader<'info, T>` (zero-copy). Call `.load()`/`.load_mut()` rather than direct deserialization. This avoids stack overflow on large structs.

### Feature Flags
| Flag | Purpose |
|---|---|
| `mainnet-beta` | Production gates (program IDs, conservative limits) |
| `anchor-test` | Enables test helper instructions used by TS integration tests |
| `no-entrypoint` | Excludes native entrypoint (for use as CPI dependency) |
| `cpi` | Exposes CPI client only (implies `no-entrypoint`) |

---

## SDK Structure (`sdk/src/`)

### Key Files
| File | Size | Purpose |
|---|---|---|
| `driftClient.ts` | ~13k lines | Main client. All trading + keeper instruction builders. |
| `adminClient.ts` | ~6.5k lines | Admin instruction builders (extends `DriftClient`). |
| `user.ts` | ~4.7k lines | `User` account abstraction: margin queries, position accessors, PnL. |
| `types.ts` | ~45k lines | All shared TypeScript types mirroring on-chain structs. |
| `idl/drift.json` | — | Generated Anchor IDL. Source of truth for instruction interfaces and account layouts. **Do not edit manually.** |

### Key Directories
| Directory | Purpose |
|---|---|
| `accounts/` | Account subscription infrastructure: WebSocket, polling, bulk loaders. |
| `addresses/` | `pda.ts` — all PDA derivation helpers. |
| `dlob/` | Decentralized Limit Order Book: order matching, price levels, maker selection. |
| `math/` | TypeScript mirrors of on-chain math (margin, funding, AMM pricing). |
| `oracles/` | Oracle client adapters (Pyth, Switchboard, Pyth Lazer). |
| `events/` | Event parsing and subscription from program logs. |
| `tx/` | Transaction building utilities, compute unit estimation. |
| `constants/` | Market indices, precision constants, numeric limits. |

### SDK ↔ On-Chain Instruction Mapping
| SDK Method | On-Chain Instruction | Handler File |
|---|---|---|
| `driftClient.placePerpOrder` | `PlacePerpOrder` | `instructions/user.rs` |
| `driftClient.cancelOrder` | `CancelOrder` | `instructions/user.rs` |
| `driftClient.modifyOrder` | `ModifyOrder` | `instructions/user.rs` |
| `driftClient.deposit` | `Deposit` | `instructions/user.rs` |
| `driftClient.withdraw` | `Withdraw` | `instructions/user.rs` |
| `driftClient.fillPerpOrder` | `FillPerpOrder` | `instructions/keeper.rs` |
| `driftClient.settlePnl` | `SettlePnl` | `instructions/keeper.rs` |
| `driftClient.liquidatePerp` | `LiquidatePerp` | `instructions/keeper.rs` |
| `driftClient.updateFundingRate` | `UpdateFundingRate` | `instructions/keeper.rs` |
| `adminClient.initializePerpMarket` | `InitializePerpMarket` | `instructions/admin.rs` |
| `adminClient.updatePerpMarket*` | `UpdatePerpMarket*` | `instructions/admin.rs` |
| `adminClient.updateOracleGuardRails` | `UpdateOracleGuardRails` | `instructions/admin.rs` |

---

## Ancillary Programs

These are stubs/wrappers used by Drift for oracle and DEX integrations. No core logic lives here.

| Program | Purpose |
|---|---|
| `programs/pyth/` | Pyth V1 oracle account layout definitions |
| `programs/pyth-lazer/` | Pyth Lazer type definitions and utilities |
| `programs/switchboard/` | Switchboard V2 oracle account type definitions |
| `programs/switchboard-on-demand/` | Switchboard On-Demand oracle type definitions |
| `programs/openbook_v2/` | OpenBook V2 account types for spot fulfillment |
| `programs/token_faucet/` | Devnet/test token minting utility (not on mainnet) |

---

## Build & Test Quick Reference

See `CLAUDE.md` for full commands. Key entry points:

```bash
# Verify program compiles after Rust changes
cargo build -p drift

# Run Rust unit tests
cargo test -p drift

# Run a single TS integration test
ts-mocha -t 300000 ./tests/<test_file>.ts

# Full TS integration suite
bash test-scripts/run-anchor-tests.sh --skip-build
```
