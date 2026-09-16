# Rust keeper bots

Keeper bots for Velocity. Every mode ships in the one `keeprs` binary and is picked with a
flag: `--filler` (on by default), `--liquidator`, `--quoter`, `--taker`, `--relayer`. Add
`--dry` to simulate transactions instead of sending them, and `--init-user` to create the
bot's subaccounts before the selected mode starts.

## Configuration

Copy `.env.example` to `.env` and substitute valid RPC credentials.

Required environment variables:

- `BOT_PRIVATE_KEY`: base58 encoded private key
- `RPC_URL`: Solana RPC endpoint
- `GRPC_ENDPOINT`: Velocity gRPC endpoint
- `GRPC_X_TOKEN`: authentication token for gRPC
- `PYTH_LAZER_TOKEN`: Pyth Lazer access token. The filler, liquidator, and relayer all
  read it at startup and panic when it is unset.

Optional: `MARKET_IDS` (default `0,1,2`), `MAINNET` (default `true`), `DRY_RUN`,
`SUBACCOUNTS` (default `0`), `METRICS_PORT` (default `9898`), and `SWIFT_WS_URL` to point
the filler's swift order feed at a different swift ws server. `.env.example` lists the
per-bot knobs. Every bot serves `/metrics`, `/health`, and a dashboard at `/` on
`METRICS_PORT`.

`--mainnet` is a switch, so `--mainnet false` is rejected as an unexpected argument. Set
`MAINNET=false` in the environment to run against devnet.

## Run perp filler

The perp filler matches swift orders and onchain auction orders against resting liquidity.
It also attempts to uncross resting limit orders.

```shell
RUST_LOG=filler=info,dlob=info,swift=info \
    cargo run --release -- --mainnet --filler
```

- use `--dry` flag for tx simulation only

## Run 'perp with fill' liquidator

The liquidator closes liquidatable perp positions against resting limit orders, and spot
borrows with atomic swaps.

```shell
RUST_LOG=liquidator=info,dlob=info,swift=info \
    cargo run --release -- --mainnet --liquidator
```

## Simulate trading on devnet (market maker + taker)

Two bots that together generate realistic two-sided flow. They warm mark TWAPs toward the
live oracle and build AMM inventory on a quiet devnet, which is what the e2e scenarios in
`velocity-rs/tests/devnet_e2e.rs` need. Both ship in the same `keeprs` binary and ECR
image, selected by flag.

**Market maker.** The `--quoter` bot posts resting post-only two-sided limits at
`±--quote-spread-bps` (default **20** bps) around the oracle and refreshes them. For
continuous "always quote ±20bps around oracle" behaviour, set `--quote-refresh-bps 0` so it
re-centres whenever the oracle moves at all. Prefer `--quote-size-notional` (USD,
QUOTE_PRECISION 1e6) over `--quote-size-base` so each quote is the same dollar size on
every market (converted via the oracle):

```shell
RUST_LOG=quoter=info \
    cargo run --release -- --quoter --market-ids 0 \
    --quote-spread-bps 20 --quote-refresh-bps 0 --quote-size-notional 25000000
```

**Taker.** The `--taker` bot sends randomized small **market** orders that cross the
spread, filling the quoter's resting quotes (or the AMM). Direction is a coin-flip, forced
to the inventory-reducing side once `|position|` reaches `--taker-max-base-per-market`
(mean-reverting; `0` disables the bound):

```shell
RUST_LOG=taker=info \
    cargo run --release -- --taker --market-ids 0 \
    --taker-size-base 100000000 --taker-interval-secs 15 \
    --taker-max-base-per-market 1000000000
```

Run the quoter, taker, and a `--filler` (to match them) as separate processes against
devnet. Use `--dry` on either to log intended orders without sending. Both default to
mainnet, so set `MAINNET=false` for devnet.

Taker env knobs: `TAKER_INTERVAL_SECS`, `TAKER_SIZE_BASE`, `TAKER_MAX_BASE_PER_MARKET`
(mirror the flags above).

## Event flow diagram (filler)

```mermaid
flowchart TD
    subgraph Event_Sources
        A1["gRPC Slot Update"]
        A2["gRPC Account Update"]
        A3["gRPC Transaction Update"]
        A4["Swift Order Websocket"]
    end

    subgraph DLOB_and_Notifier
        B1["DLOB (Orderbook State)"]
        B2["DLOBNotifier"]
    end

    subgraph FillerBot_MainLoop
        C1["Slot Receiver (from gRPC)"]
        C2["Swift Order Stream"]
        C3["Find Crosses"]
        C4["try_auction_fill / try_swift_fill"]
    end

    subgraph Transaction_Worker
        D1["TxWorker.send_tx"]
        D2["TxWorker.confirm_tx"]
    end

    %% Event flow
    A1 -- "on_slot_update_fn" --> B2
    A1 -- "on_slot_update_fn" --> C1
    A2 -- "on_account_update_fn" --> B2
    A3 -- "on_transaction_update_fn" --> D2
    A4 -- "New Swift Order" --> C2

    B2 -- "DLOBEvent::SlotOrPriceUpdate / Order" --> B1
    B1 -- "Orderbook State" --> C3
    C1 -- "New Slot" --> C3
    C2 -- "New Swift Order" --> C3
    C3 -- "Crosses Found?" --> C4
    C4 -- "Build Transaction" --> D1
    D1 -- "Send Transaction" --> Solana["Solana Network"]
    Solana -- "Transaction Update" --> A3
    D2 -- "Confirm & Metrics" --> FillerBot_MainLoop
    D1 -- "PendingTxs" --> D2

    %% Feedback
    D2 -- "Update Metrics, PendingTxs" --> FillerBot_MainLoop
```

## Transaction lifecycle diagram

Covers the paths a fill transaction can take, including failures and losing the race to
another filler.

```mermaid
flowchart TD
    subgraph "Transaction Creation"
        A1["Cross Detection"]
        A2["Build Transaction"]
        A3["Calculate Priority Fee"]
        A4["Set CU Limit"]
    end

    subgraph "Transaction Sending"
        B1["TxWorker.send_tx"]
        B2["Sign & Send to RPC"]
        B3["Add to PendingTxs"]
        B4["Increment Metrics"]
    end

    subgraph "Transaction Confirmation"
        C1["gRPC Transaction Update"]
        C2["TxWorker.confirm_tx"]
        C3["Get Transaction Details"]
        C4["Parse Transaction Logs"]
        C5["Update Metrics"]
    end

    subgraph "Success Path"
        D1["Transaction Success"]
        D2["Parse Fill Events"]
        D3["Compare Expected vs Actual Fills"]
        D4["Record Performance Metrics"]
    end

    subgraph "Error Paths"
        E1["Send Error"]
        E2["Confirmation Error"]
        E3["Transaction Failed"]
        E4["Insufficient Funds"]
        E5["Compute Unit Exceeded"]
    end

    subgraph "Competition Scenarios"
        F1["AMM Fill Beats Us"]
        F2["Higher Priority Fee"]
        F3["Order Already Filled"]
        F4["Partial Fill"]
    end

    %% Main flow
    A1 --> A2 --> A3 --> A4 --> B1 --> B2 --> B3 --> B4
    B2 --> C1 --> C2 --> C3 --> C4 --> C5

    %% Success path
    C4 --> D1 --> D2 --> D3 --> D4

    %% Error paths
    B2 --> E1
    C3 --> E2
    C4 --> E3
    E3 --> E4
    E3 --> E5

    %% Competition scenarios
    D2 --> F1
    D2 --> F2
    D2 --> F3
    D3 --> F4

    %% Styling
    classDef success fill:#d4edda,stroke:#155724,color:#155724
    classDef error fill:#f8d7da,stroke:#721c24,color:#721c24
    classDef competition fill:#fff3cd,stroke:#856404,color:#856404
    classDef process fill:#d1ecf1,stroke:#0c5460,color:#0c5460

    class D1,D2,D3,D4 success
    class E1,E2,E3,E4,E5 error
    class F1,F2,F3,F4 competition
    class A1,A2,A3,A4,B1,B2,B3,B4,C1,C2,C3,C4,C5 process
```

## Liquidator overview

### 1. Initialization (`LiquidatorBot::new`)

`new` builds the DLOB the strategy matches against and spawns its notifier thread, a
`TxWorker` thread that signs, sends, and confirms transactions, a `MarketState` cache of
market metadata and oracle prices, gRPC subscriptions for users, oracles, and markets, and
a liquidation worker thread that consumes liquidatable users. It skips markets whose name
contains "bet" and markets still in `Initialized` status.

### 2. Main event loop (`LiquidatorBot::run`)

The loop drains up to 64 gRPC events per pass, keeps every user account in memory, and
tracks the ones at high risk. It rechecks high-risk users whenever an oracle price moves,
and rechecks every user once every 1024 cycles or every 30 seconds, whichever comes first.

### 3. Margin status checking

Each user lands in one of three states: **liquidatable** when
`total_collateral < margin_requirement`, **high risk** when free margin is under 10% of the
margin requirement, and **safe** otherwise.

### 4. Liquidation worker thread

The worker receives liquidatable users on a channel and attempts one liquidation per user
at most every 2 seconds, converted to slots at the current slot duration. After a failed
attempt it backs off, starting at 5 seconds and doubling up to a 5 minute cap, and it
prices each transaction at the 60th percentile priority fee.

### 5. Liquidation strategy (`PrimaryLiquidationStrategy`)

For perps the strategy takes the position with the largest notional, asks the DLOB for the
top makers on the side that absorbs it (bids for a long liquidatee, asks for a short), and
builds a `liquidate_perp_with_fill` transaction. With no eligible makers it falls back to
taking the position over with `liquidate_perp`, which it only does when the subaccount
holds enough free collateral.

For spot it takes the largest borrow that is not dust (below twice the market's minimum
order size), uses the largest deposit as collateral, and quotes Jupiter and Titan in
parallel, sending whichever route returns more output tokens. The swap is wrapped in
`liquidate_spot_with_swap_begin` and `liquidate_spot_with_swap_end`. It also runs
`liquidate_perp_pnl_for_deposit` and `liquidate_borrow_for_perp_pnl` where those apply.

### 6. Transaction lifecycle

The strategy builds each transaction with a priority fee and a compute limit and hands it
to the `TxWorker` over the tx sender channel. The worker signs it, sends it, and records it
as pending. gRPC transaction updates then confirm it, update the metrics, and drop it from
the pending set.

## Tx summary

Print recent tx stats for a pubkey (the last 256 signatures).

```bash
 RUST_LOG=info cargo run --release --bin=tx_history <PUBKEY> --rpc-url <RPC_URL>
 ```
