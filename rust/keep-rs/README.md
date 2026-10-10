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
per-bot knobs. Every bot serves `/metrics` and `/health` on `METRICS_PORT`.

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

## How the bots work

[`ARCHITECTURE.md`](./ARCHITECTURE.md) describes the source layout, how the filler and the
liquidator decide what to send, the transaction lifecycle from signing to confirmation, the
liquidator's collateral reservations, and the `tx_event` events each bot logs.

## Tx summary

Print recent tx stats for a pubkey (the last 256 signatures).

```bash
 RUST_LOG=info cargo run --release --bin=tx_history <PUBKEY> --rpc-url <RPC_URL>
 ```
