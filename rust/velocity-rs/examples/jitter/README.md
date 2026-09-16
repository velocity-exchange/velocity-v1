# jitter-example

A minimal JIT (just-in-time) maker bot built on `velocity-rs`.

It watches JIT auctions and swift orders and tries to fill them through the onchain
`jit-proxy` program (`velocity_rs::jit_client::JitProxyClient`). The program enforces fills
at the configured prices and max/min positions, and the tx fails otherwise. Prices here sit
at a fixed margin from the oracle, and fills are retried each slot until the auction
completes.

This is illustrative only. A production maker needs price and position tuning plus a
tx-inclusion strategy. The `Jitter` orchestrator and the `Shotgun` (retry every slot) and
`Sniper` (stub) strategies live here in the example, not in the SDK.

This crate declares its own `[workspace]`, so it carries its own `Cargo.lock` and builds
independently of the `rust/` workspace.

## Run

```sh
RPC_URL=<https rpc>       \
PRIVATE_KEY=<base58 key>  \
RUST_LOG=info             \
cargo run --release
```
