# jitter-example

A minimal JIT (just-in-time) maker bot built on `velocity-rs`.

It watches JIT auctions and swift orders and tries to fill them via the on-chain
`jit-proxy` program (`velocity_rs::jit_client::JitProxyClient`). The program
enforces fills at the configured prices and max/min positions (otherwise the tx
fails). Prices here are set at a fixed margin from oracle, and fills are retried
each slot until the auction completes.

This is illustrative only — a production maker needs price/position tuning and
tx-inclusion strategy. The `Jitter` orchestrator plus the `Shotgun` (retry every
slot) and `Sniper` (stub) strategies live here in the example, not in the SDK.

## Run

```sh
RPC_URL=<https rpc>       \
PRIVATE_KEY=<base58 key>  \
RUST_LOG=info             \
cargo run --release
```
